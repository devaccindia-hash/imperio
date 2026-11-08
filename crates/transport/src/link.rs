//! Encrypted, loss-aware datagram link between two peers.
//!
//! Outer datagram: `Kind::Encrypted | nonce u64 | AEAD(inner)`.
//! Inner: `inner_kind u8 | ...`
//!   * MEDIA:   seq u32 | send_ts_us u64 | payload (a video chunk)
//!   * CONTROL: ControlMessage (postcard)
//!   * NACK:    count u16 | seq u32 * count
//!   * REPORT:  ReceiverReport (postcard)
//!
//! Media datagrams are numbered and kept in a bounded send history; the
//! receiver detects gaps and NACKs them, the sender retransmits the original
//! ciphertext (same nonce, so the replay window must tolerate it: it does,
//! because duplicates are dropped there and a retransmit of a never-received
//! datagram is fresh).

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::{Buf, BufMut, Bytes, BytesMut};
use imperio_protocol::signal::Kind;
use imperio_protocol::ControlMessage;
use serde::{Deserialize, Serialize};

use crate::crypto::{Cipher, ReplayWindow, SessionKeys};

const INNER_MEDIA: u8 = 0;
const INNER_CONTROL: u8 = 1;
const INNER_NACK: u8 = 2;
const INNER_REPORT: u8 = 3;
const INNER_AUDIO: u8 = 4;

/// Bytes of sent media kept for retransmission (~1.5 s at 8 Mbps).
const HISTORY_BYTES: usize = 1_500_000;
const MAX_NACK_RETRIES: u8 = 4;
const GIVE_UP_AFTER: Duration = Duration::from_millis(300);
const REPORT_INTERVAL: Duration = Duration::from_millis(100);

/// Periodic receiver -> sender feedback.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ReceiverReport {
    pub highest_seq: u32,
    /// Cumulative counters since session start.
    pub received: u64,
    /// Given up on (never recovered).
    pub lost: u64,
    pub recovered: u64,
    /// Gaps detected before any recovery: the raw network loss signal.
    pub missing_detected: u64,
    /// Smoothed one-way queuing delay estimate (above the observed minimum).
    pub queue_delay_us: u32,
    pub rtt_us: u32,
}

pub enum Incoming {
    Media {
        seq: u32,
        send_ts_us: u64,
        payload: Bytes,
    },
    /// An audio frame. Fire-and-forget: no retransmission, because a late
    /// audio packet is worse than a lost one. `codec` is the `AudioCodec` tag.
    Audio {
        codec: u8,
        seq: u32,
        send_ts_us: u64,
        payload: Bytes,
    },
    Control(ControlMessage),
    Report(ReceiverReport),
}

struct TxState {
    cipher: Cipher,
    nonce: u64,
    seq: u32,
    history: VecDeque<(u32, Bytes)>,
    history_bytes: usize,
    retransmits: u64,
}

struct Missing {
    first_seen: Instant,
    last_nack: Option<Instant>,
    retries: u8,
}

struct RxState {
    cipher: Cipher,
    replay: ReplayWindow,
    highest: Option<u32>,
    missing: BTreeMap<u32, Missing>,
    received: u64,
    lost: u64,
    recovered: u64,
    missing_detected: u64,
    duplicates: u64,
    nacks_sent: u64,
    // One-way-delay tracking (host clock vs local clock; offset cancels).
    owd_min_cur: i64,
    owd_min_prev: i64,
    owd_window_start: Instant,
    queue_delay_us: f64,
    last_report: Instant,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct LinkStats {
    pub received: u64,
    pub lost: u64,
    pub recovered: u64,
    pub duplicates: u64,
    pub nacks_sent: u64,
    pub retransmits: u64,
    pub queue_delay_us: u32,
    pub rtt_us: u32,
    pub outstanding_missing: usize,
}

pub struct SecureLink {
    socket: Arc<UdpSocket>,
    peer: SocketAddr,
    keys: SessionKeys,
    tx: Mutex<TxState>,
    rx: Mutex<RxState>,
    rtt_us: AtomicU64,
    /// Bytes sent on the wire (all kinds).
    pub bytes_sent: AtomicU64,
}

impl SecureLink {
    pub fn new(socket: Arc<UdpSocket>, peer: SocketAddr, keys: SessionKeys) -> Self {
        let now = Instant::now();
        Self {
            socket,
            peer,
            keys: keys.clone(),
            tx: Mutex::new(TxState {
                cipher: Cipher::new(&keys.send),
                nonce: 1,
                seq: 0,
                history: VecDeque::new(),
                history_bytes: 0,
                retransmits: 0,
            }),
            rx: Mutex::new(RxState {
                cipher: Cipher::new(&keys.recv),
                replay: ReplayWindow::default(),
                highest: None,
                missing: BTreeMap::new(),
                received: 0,
                lost: 0,
                recovered: 0,
                missing_detected: 0,
                duplicates: 0,
                nacks_sent: 0,
                owd_min_cur: i64::MAX,
                owd_min_prev: i64::MAX,
                owd_window_start: now,
                queue_delay_us: 0.0,
                last_report: now,
            }),
            rtt_us: AtomicU64::new(0),
            bytes_sent: AtomicU64::new(0),
        }
    }

    /// The agreed session keys (for channel binding / identity proof).
    pub fn session_keys(&self) -> &SessionKeys {
        &self.keys
    }

    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    pub fn set_rtt(&self, rtt: Duration) {
        // EWMA so a single outlier does not swing deadlines.
        let prev = self.rtt_us.load(Ordering::Relaxed);
        let cur = rtt.as_micros() as u64;
        let next = if prev == 0 { cur } else { (prev * 7 + cur) / 8 };
        self.rtt_us.store(next, Ordering::Relaxed);
    }

    pub fn rtt(&self) -> Duration {
        Duration::from_micros(self.rtt_us.load(Ordering::Relaxed))
    }

    fn seal_and_send(&self, tx: &mut TxState, inner: &[u8]) -> io::Result<Bytes> {
        let nonce = tx.nonce;
        tx.nonce += 1;
        let ct = tx.cipher.seal(nonce, inner);
        let mut out = BytesMut::with_capacity(1 + 8 + ct.len());
        out.put_u8(Kind::Encrypted as u8);
        out.put_u64(nonce);
        out.put_slice(&ct);
        let out = out.freeze();
        self.socket.send_to(&out, self.peer)?;
        self.bytes_sent
            .fetch_add(out.len() as u64, Ordering::Relaxed);
        Ok(out)
    }

    /// Send one media chunk; returns its sequence number.
    pub fn send_media(&self, payload: &[u8], send_ts_us: u64) -> io::Result<u32> {
        let mut tx = self.tx.lock().unwrap();
        let seq = tx.seq;
        tx.seq = tx.seq.wrapping_add(1);
        let mut inner = BytesMut::with_capacity(13 + payload.len());
        inner.put_u8(INNER_MEDIA);
        inner.put_u32(seq);
        inner.put_u64(send_ts_us);
        inner.put_slice(payload);
        let wire = self.seal_and_send(&mut tx, &inner)?;
        tx.history_bytes += wire.len();
        tx.history.push_back((seq, wire));
        while tx.history_bytes > HISTORY_BYTES {
            if let Some((_, old)) = tx.history.pop_front() {
                tx.history_bytes -= old.len();
            }
        }
        Ok(seq)
    }

    /// Send one audio frame. Unlike media, audio is not kept for retransmit:
    /// loss is concealed by the decoder, and a stale frame only adds delay.
    /// Audio carries its own sequence number, separate from the media stream.
    pub fn send_audio(&self, codec: u8, seq: u32, send_ts_us: u64, payload: &[u8]) -> io::Result<()> {
        let mut inner = BytesMut::with_capacity(14 + payload.len());
        inner.put_u8(INNER_AUDIO);
        inner.put_u8(codec);
        inner.put_u32(seq);
        inner.put_u64(send_ts_us);
        inner.put_slice(payload);
        let mut tx = self.tx.lock().unwrap();
        self.seal_and_send(&mut tx, &inner).map(|_| ())
    }

    pub fn send_control(&self, msg: &ControlMessage) -> io::Result<()> {
        let body = msg.encode();
        let mut inner = Vec::with_capacity(1 + body.len());
        inner.push(INNER_CONTROL);
        inner.extend_from_slice(&body);
        let mut tx = self.tx.lock().unwrap();
        self.seal_and_send(&mut tx, &inner).map(|_| ())
    }

    fn send_nack(&self, seqs: &[u32]) -> io::Result<()> {
        let mut inner = BytesMut::with_capacity(3 + seqs.len() * 4);
        inner.put_u8(INNER_NACK);
        inner.put_u16(seqs.len() as u16);
        for s in seqs {
            inner.put_u32(*s);
        }
        let mut tx = self.tx.lock().unwrap();
        self.seal_and_send(&mut tx, &inner).map(|_| ())
    }

    fn send_report(&self, r: &ReceiverReport) -> io::Result<()> {
        let mut inner = vec![INNER_REPORT];
        inner.extend(postcard::to_allocvec(r).expect("report serialises"));
        let mut tx = self.tx.lock().unwrap();
        self.seal_and_send(&mut tx, &inner).map(|_| ())
    }

    fn retransmit(&self, seqs: &[u32]) {
        let mut tx = self.tx.lock().unwrap();
        for s in seqs {
            // History is in seq order; binary search by seq.
            let idx = tx.history.binary_search_by_key(s, |(q, _)| *q);
            if let Ok(i) = idx {
                let wire = tx.history[i].1.clone();
                if self.socket.send_to(&wire, self.peer).is_ok() {
                    tx.retransmits += 1;
                    self.bytes_sent
                        .fetch_add(wire.len() as u64, Ordering::Relaxed);
                }
            }
        }
    }

    /// Process one received datagram (any kind). Returns the decoded content
    /// for MEDIA / CONTROL / REPORT; NACKs are handled internally.
    pub fn handle_datagram(&self, data: &[u8], now: Instant) -> Option<Incoming> {
        if data.len() < 1 + 8 + crate::crypto::TAG_LEN || data[0] != Kind::Encrypted as u8 {
            return None;
        }
        let nonce = u64::from_be_bytes(data[1..9].try_into().unwrap());
        let mut rx = self.rx.lock().unwrap();
        let plain = rx.cipher.open(nonce, &data[9..])?;
        if !rx.replay.check_and_set(nonce) {
            rx.duplicates += 1;
            return None;
        }
        let mut body = Bytes::from(plain);
        if body.is_empty() {
            return None;
        }
        match body.get_u8() {
            INNER_MEDIA => {
                if body.len() < 12 {
                    return None;
                }
                let seq = body.get_u32();
                let send_ts_us = body.get_u64();
                rx.received += 1;
                Self::track_seq(&mut rx, seq, now);
                Self::track_owd(&mut rx, send_ts_us, now);
                Some(Incoming::Media {
                    seq,
                    send_ts_us,
                    payload: body,
                })
            }
            INNER_AUDIO => {
                drop(rx);
                if body.len() < 13 {
                    return None;
                }
                let codec = body.get_u8();
                let seq = body.get_u32();
                let send_ts_us = body.get_u64();
                Some(Incoming::Audio {
                    codec,
                    seq,
                    send_ts_us,
                    payload: body,
                })
            }
            INNER_CONTROL => {
                drop(rx);
                ControlMessage::decode(&body).ok().map(Incoming::Control)
            }
            INNER_NACK => {
                drop(rx);
                if body.len() < 2 {
                    return None;
                }
                let n = body.get_u16() as usize;
                let seqs: Vec<u32> = (0..n)
                    .filter_map(|_| (body.len() >= 4).then(|| body.get_u32()))
                    .collect();
                self.retransmit(&seqs);
                None
            }
            INNER_REPORT => {
                drop(rx);
                postcard::from_bytes::<ReceiverReport>(&body)
                    .ok()
                    .map(Incoming::Report)
            }
            _ => None,
        }
    }

    fn track_seq(rx: &mut RxState, seq: u32, now: Instant) {
        match rx.highest {
            None => rx.highest = Some(seq),
            Some(h) if seq > h => {
                // Cap gap size so a corrupted seq cannot allocate unboundedly.
                let gap = (seq - h - 1).min(2048);
                for s in (seq - gap)..seq {
                    rx.missing.insert(
                        s,
                        Missing {
                            first_seen: now,
                            last_nack: None,
                            retries: 0,
                        },
                    );
                }
                rx.missing_detected += gap as u64;
                rx.highest = Some(seq);
            }
            Some(_) => {
                if rx.missing.remove(&seq).is_some() {
                    rx.recovered += 1;
                } else {
                    rx.duplicates += 1;
                }
            }
        }
    }

    fn track_owd(rx: &mut RxState, send_ts_us: u64, now: Instant) {
        let owd = crate::now_us() as i64 - send_ts_us as i64;
        if now.duration_since(rx.owd_window_start) > Duration::from_secs(2) {
            rx.owd_min_prev = rx.owd_min_cur;
            rx.owd_min_cur = i64::MAX;
            rx.owd_window_start = now;
        }
        rx.owd_min_cur = rx.owd_min_cur.min(owd);
        let base = rx.owd_min_cur.min(rx.owd_min_prev);
        let q = (owd - base).max(0) as f64;
        rx.queue_delay_us = rx.queue_delay_us * 0.9 + q * 0.1;
    }

    /// Receiver-side housekeeping: NACK missing datagrams, give up on old
    /// ones, send periodic reports. Call every few milliseconds.
    pub fn tick(&self, now: Instant) {
        let rtt = self.rtt();
        // Wait a little for reordering before the first NACK; retry after an RTT.
        let first_delay = Duration::from_millis(2).max(rtt / 4);
        let retry_delay = rtt + Duration::from_millis(5);

        let mut nacks = Vec::new();
        let mut report = None;
        {
            let mut rx = self.rx.lock().unwrap();
            let mut give_up = Vec::new();
            for (seq, m) in rx.missing.iter_mut() {
                let age = now.duration_since(m.first_seen);
                if m.retries >= MAX_NACK_RETRIES || age > GIVE_UP_AFTER {
                    give_up.push(*seq);
                    continue;
                }
                let due = match m.last_nack {
                    None => age >= first_delay,
                    Some(t) => now.duration_since(t) >= retry_delay,
                };
                if due && nacks.len() < 256 {
                    nacks.push(*seq);
                    m.last_nack = Some(now);
                    m.retries += 1;
                }
            }
            for s in give_up {
                rx.missing.remove(&s);
                rx.lost += 1;
            }
            if !nacks.is_empty() {
                rx.nacks_sent += nacks.len() as u64;
            }
            if now.duration_since(rx.last_report) >= REPORT_INTERVAL {
                rx.last_report = now;
                report = Some(ReceiverReport {
                    highest_seq: rx.highest.unwrap_or(0),
                    received: rx.received,
                    lost: rx.lost,
                    recovered: rx.recovered,
                    missing_detected: rx.missing_detected,
                    queue_delay_us: rx.queue_delay_us as u32,
                    rtt_us: rtt.as_micros() as u32,
                });
            }
        }
        if !nacks.is_empty() {
            let _ = self.send_nack(&nacks);
        }
        if let Some(r) = report {
            let _ = self.send_report(&r);
        }
    }

    pub fn stats(&self) -> LinkStats {
        let rx = self.rx.lock().unwrap();
        let tx = self.tx.lock().unwrap();
        LinkStats {
            received: rx.received,
            lost: rx.lost,
            recovered: rx.recovered,
            duplicates: rx.duplicates,
            nacks_sent: rx.nacks_sent,
            retransmits: tx.retransmits,
            queue_delay_us: rx.queue_delay_us as u32,
            rtt_us: self.rtt_us.load(Ordering::Relaxed) as u32,
            outstanding_missing: rx.missing.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (SecureLink, SecureLink, Arc<UdpSocket>, Arc<UdpSocket>) {
        let a = Arc::new(UdpSocket::bind("127.0.0.1:0").unwrap());
        let b = Arc::new(UdpSocket::bind("127.0.0.1:0").unwrap());
        a.set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        b.set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let k = [3u8; 32];
        let j = [4u8; 32];
        let la = SecureLink::new(
            a.clone(),
            b.local_addr().unwrap(),
            SessionKeys { send: k, recv: j },
        );
        let lb = SecureLink::new(
            b.clone(),
            a.local_addr().unwrap(),
            SessionKeys { send: j, recv: k },
        );
        (la, lb, a, b)
    }

    fn recv(sock: &UdpSocket) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; 2048];
        sock.recv(&mut buf).ok().map(|n| buf[..n].to_vec())
    }

    #[test]
    fn media_roundtrip_and_control() {
        let (la, lb, _a, b) = pair();
        la.send_media(b"frame-bytes", 123).unwrap();
        la.send_control(&ControlMessage::RequestKeyframe).unwrap();
        let now = Instant::now();
        match lb.handle_datagram(&recv(&b).unwrap(), now) {
            Some(Incoming::Media {
                seq,
                send_ts_us,
                payload,
            }) => {
                assert_eq!(
                    (seq, send_ts_us, &payload[..]),
                    (0, 123, &b"frame-bytes"[..])
                );
            }
            _ => panic!("expected media"),
        }
        match lb.handle_datagram(&recv(&b).unwrap(), now) {
            Some(Incoming::Control(ControlMessage::RequestKeyframe)) => {}
            _ => panic!("expected control"),
        }
    }

    #[test]
    fn nack_recovers_dropped_datagram() {
        let (la, lb, a, b) = pair();
        // Sender emits 0,1,2; we "drop" 1 on the wire by swallowing it.
        la.send_media(b"p0", 0).unwrap();
        la.send_media(b"p1", 0).unwrap();
        la.send_media(b"p2", 0).unwrap();
        let d0 = recv(&b).unwrap();
        let _d1_dropped = recv(&b).unwrap();
        let d2 = recv(&b).unwrap();
        let t0 = Instant::now();
        assert!(matches!(
            lb.handle_datagram(&d0, t0),
            Some(Incoming::Media { seq: 0, .. })
        ));
        assert!(matches!(
            lb.handle_datagram(&d2, t0),
            Some(Incoming::Media { seq: 2, .. })
        ));
        assert_eq!(lb.stats().outstanding_missing, 1);
        // Receiver ticks after the reorder delay -> NACK goes to sender.
        lb.tick(t0 + Duration::from_millis(10));
        let nack = recv(&a).unwrap();
        assert!(la.handle_datagram(&nack, t0).is_none());
        assert_eq!(la.stats().retransmits, 1);
        // Retransmission arrives at receiver.
        let d1 = recv(&b).unwrap();
        match lb.handle_datagram(&d1, t0 + Duration::from_millis(12)) {
            Some(Incoming::Media {
                seq: 1, payload, ..
            }) => assert_eq!(&payload[..], b"p1"),
            _ => panic!("expected recovered media"),
        }
        let s = lb.stats();
        assert_eq!((s.recovered, s.lost, s.outstanding_missing), (1, 0, 0));
    }

    #[test]
    fn duplicate_is_dropped_by_replay_window() {
        let (la, lb, _a, b) = pair();
        la.send_media(b"x", 0).unwrap();
        let d = recv(&b).unwrap();
        let now = Instant::now();
        assert!(lb.handle_datagram(&d, now).is_some());
        assert!(lb.handle_datagram(&d, now).is_none());
        assert_eq!(lb.stats().duplicates, 1);
    }

    #[test]
    fn report_is_sent_and_parsed() {
        let (la, lb, a, b) = pair();
        la.send_media(b"x", 0).unwrap();
        let now = Instant::now();
        lb.handle_datagram(&recv(&b).unwrap(), now);
        lb.tick(now + Duration::from_millis(150));
        let r = recv(&a).unwrap();
        match la.handle_datagram(&r, now) {
            Some(Incoming::Report(rep)) => assert_eq!(rep.received, 1),
            _ => panic!("expected report"),
        }
    }
}
