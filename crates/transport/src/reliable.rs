//! Reliable, ordered message stream on top of the encrypted link.
//!
//! Video tolerates a lost datagram; a file does not. This module carries
//! whole messages (a request, a directory listing, a block of file data) so
//! that each one arrives complete, once, and in the order it was sent.
//!
//! Wire format (inside the link's encryption):
//!   * data: `seq u64 | flags u8 | payload`   (flag bit 0: last piece of a message)
//!   * ack:  `next u64 | window u16 | bitmap u64`
//!     `next` is the first segment not yet received; bit `i` of `bitmap`
//!     says segment `next + 1 + i` has arrived (selective acknowledgement);
//!     `window` is how many more segments the receiver will take.
//!
//! Sending is windowed: at most `min(congestion window, receiver window)`
//! segments are in flight. The congestion window grows while acks come back
//! and shrinks on loss or when the round trip time rises well above its
//! floor, which is what keeps a large copy from pushing the picture aside.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;

use crate::link::SecureLink;

/// Payload bytes per segment; keeps the sealed datagram under the MTU of a
/// Tailscale tunnel (1280).
pub const SEGMENT_BYTES: usize = 1100;
/// Largest message accepted by [`Reliable::send`] and by the receiver.
pub const MAX_MESSAGE_BYTES: usize = 16 << 20;

/// `send` blocks while this much is waiting to enter the window.
const SEND_QUEUE_BYTES: usize = 2 << 20;
/// Complete messages the application has not read yet: the advertised window
/// closes as this fills.
const INBOX_BYTES: usize = 8 << 20;
/// Past this, in-order segments are dropped (the sender ignored the window).
const INBOX_HARD_BYTES: usize = 12 << 20;
/// Segments the receiver holds while waiting for an earlier one.
const RX_WINDOW: u64 = 4096;
const MIN_CWND: f64 = 8.0;
const START_CWND: f64 = 32.0;
const MAX_CWND: f64 = 2048.0;
/// Datagrams sent per lock hold, so the receive thread is never kept waiting.
const BURST: usize = 32;

const DATA_HEADER: usize = 9;
const ACK_LEN: usize = 18;

/// What the stream needs from the link below it.
pub trait Wire: Send + Sync + 'static {
    fn send_stream(&self, ack: bool, body: &[u8]) -> std::io::Result<()>;
}

impl Wire for SecureLink {
    fn send_stream(&self, ack: bool, body: &[u8]) -> std::io::Result<()> {
        SecureLink::send_stream(self, ack, body)
    }
}

/// The stream was closed (session over) before the message could be sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closed;

impl std::fmt::Display for Closed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the connection is closed")
    }
}

impl std::error::Error for Closed {}

struct Seg {
    body: Bytes,
    sent: Instant,
    retx: u8,
    sacked: bool,
    /// Marked lost by selective acks; the pump sends it again.
    resend: bool,
}

struct Tx {
    /// Segments not yet in the window.
    queue: VecDeque<Bytes>,
    queue_bytes: usize,
    next_seq: u64,
    /// In flight; `window[0]` has sequence number `base`.
    window: VecDeque<Seg>,
    base: u64,
    cwnd: f64,
    ssthresh: f64,
    rwnd: u32,
    srtt: Option<f64>,
    min_rtt: f64,
    /// A loss before this sequence number belongs to an episode already
    /// answered (one window reduction per episode).
    recover: u64,
    last_delay_backoff: Instant,
    last_send: Instant,
    retransmits: u64,
}

impl Tx {
    fn rto(&self) -> Duration {
        match self.srtt {
            Some(s) => Duration::from_secs_f64((s * 2.0 + 0.02).clamp(0.06, 2.0)),
            None => Duration::from_millis(300),
        }
    }

    fn on_loss(&mut self, seq: u64) {
        if seq >= self.recover {
            self.ssthresh = (self.cwnd / 2.0).max(MIN_CWND);
            self.cwnd = self.ssthresh;
            self.recover = self.base + self.window.len() as u64;
        }
    }
}

struct Rx {
    next: u64,
    ooo: BTreeMap<u64, (bool, Bytes)>,
    partial: Vec<u8>,
    out: Option<mpsc::Sender<Vec<u8>>>,
    since_ack: u32,
    /// An in-order segment has not been acknowledged yet (acks go out for
    /// every second segment; the pump sends this one shortly).
    ack_due: Option<Instant>,
    /// Send an ack at once (the window reopened).
    ack_now: bool,
}

struct Inner {
    wire: Arc<dyn Wire>,
    tx: Mutex<Tx>,
    /// Wakes the pump (new data, an ack, an ack to send).
    wake: Condvar,
    /// Wakes senders blocked on a full queue.
    space: Condvar,
    rx: Mutex<Rx>,
    /// Bytes of complete messages waiting in the inbox.
    pending: AtomicUsize,
    closed: AtomicBool,
}

/// Sending half and datagram entry point. Cheap to clone.
#[derive(Clone)]
pub struct Reliable {
    inner: Arc<Inner>,
}

/// Receiving half: complete messages, in order.
pub struct Inbox {
    rx: mpsc::Receiver<Vec<u8>>,
    inner: Arc<Inner>,
}

/// Counters for the log.
#[derive(Debug, Clone, Copy, Default)]
pub struct StreamStats {
    pub retransmits: u64,
    pub cwnd: u32,
    pub rtt_ms: f64,
}

impl Reliable {
    /// Start a stream on `wire`. The caller feeds incoming stream datagrams
    /// to [`Reliable::handle`] and reads messages from the [`Inbox`].
    pub fn start(wire: Arc<dyn Wire>) -> (Reliable, Inbox) {
        let now = Instant::now();
        let (out, rx) = mpsc::channel();
        let inner = Arc::new(Inner {
            wire,
            tx: Mutex::new(Tx {
                queue: VecDeque::new(),
                queue_bytes: 0,
                next_seq: 0,
                window: VecDeque::new(),
                base: 0,
                cwnd: START_CWND,
                ssthresh: 512.0,
                rwnd: START_CWND as u32,
                srtt: None,
                min_rtt: f64::MAX,
                recover: 0,
                last_delay_backoff: now,
                last_send: now,
                retransmits: 0,
            }),
            wake: Condvar::new(),
            space: Condvar::new(),
            rx: Mutex::new(Rx {
                next: 0,
                ooo: BTreeMap::new(),
                partial: Vec::new(),
                out: Some(out),
                since_ack: 0,
                ack_due: None,
                ack_now: false,
            }),
            pending: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
        });
        let pump = inner.clone();
        std::thread::Builder::new()
            .name("reliable".into())
            .spawn(move || pump.pump())
            .expect("reliable thread");
        (Reliable { inner: inner.clone() }, Inbox { rx, inner })
    }

    /// Queue one message. Blocks while the send queue is full, so a fast
    /// reader of a disk is slowed to the pace of the network.
    pub fn send(&self, msg: &[u8]) -> Result<(), Closed> {
        if msg.len() > MAX_MESSAGE_BYTES {
            debug_assert!(false, "message too large");
            return Err(Closed);
        }
        let inner = &self.inner;
        let mut tx = inner.tx.lock().unwrap();
        loop {
            if inner.closed.load(Ordering::Relaxed) {
                return Err(Closed);
            }
            if tx.queue_bytes < SEND_QUEUE_BYTES {
                break;
            }
            tx = inner.space.wait_timeout(tx, Duration::from_millis(100)).unwrap().0;
        }
        let count = msg.len().div_ceil(SEGMENT_BYTES).max(1);
        for i in 0..count {
            let part = &msg[(i * SEGMENT_BYTES).min(msg.len())..((i + 1) * SEGMENT_BYTES).min(msg.len())];
            let mut body = Vec::with_capacity(DATA_HEADER + part.len());
            body.extend_from_slice(&tx.next_seq.to_be_bytes());
            body.push((i + 1 == count) as u8);
            body.extend_from_slice(part);
            tx.next_seq += 1;
            tx.queue_bytes += body.len();
            tx.queue.push_back(Bytes::from(body));
        }
        drop(tx);
        inner.wake.notify_one();
        Ok(())
    }

    /// Feed one stream datagram from the link (`Incoming::Stream`).
    pub fn handle(&self, ack: bool, body: Bytes) {
        if self.inner.closed.load(Ordering::Relaxed) {
            return;
        }
        if ack {
            self.inner.on_ack(&body);
        } else {
            self.inner.on_data(body);
        }
    }

    /// Stop the stream: senders get [`Closed`], the inbox ends.
    pub fn close(&self) {
        self.inner.closed.store(true, Ordering::Relaxed);
        self.inner.rx.lock().unwrap().out = None;
        self.inner.wake.notify_all();
        self.inner.space.notify_all();
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Relaxed)
    }

    pub fn stats(&self) -> StreamStats {
        let tx = self.inner.tx.lock().unwrap();
        StreamStats {
            retransmits: tx.retransmits,
            cwnd: tx.cwnd as u32,
            rtt_ms: tx.srtt.unwrap_or(0.0) * 1e3,
        }
    }
}

impl Inbox {
    /// Next message, or `None` once the stream is closed.
    pub fn recv(&self) -> Option<Vec<u8>> {
        loop {
            match self.recv_timeout(Duration::from_millis(500)) {
                Ok(m) => return Some(m),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return None,
            }
        }
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<Vec<u8>, mpsc::RecvTimeoutError> {
        let msg = self.rx.recv_timeout(timeout)?;
        let before = self.inner.pending.fetch_sub(msg.len(), Ordering::Relaxed);
        // The window was (nearly) shut and has reopened: tell the sender now
        // rather than leave it to its probe.
        let half = INBOX_BYTES / 2;
        if before >= half && before - msg.len() < half {
            self.inner.rx.lock().unwrap().ack_now = true;
            self.inner.wake.notify_one();
        }
        Ok(msg)
    }
}

impl Inner {
    fn window_now(&self, rx: &Rx) -> u16 {
        let pending = self.pending.load(Ordering::Relaxed);
        let by_inbox = INBOX_BYTES.saturating_sub(pending) / SEGMENT_BYTES;
        let by_buffer = (RX_WINDOW as usize).saturating_sub(rx.ooo.len());
        by_inbox.min(by_buffer).min(u16::MAX as usize) as u16
    }

    fn build_ack(&self, rx: &mut Rx) -> [u8; ACK_LEN] {
        let mut bitmap = 0u64;
        for (&seq, _) in rx.ooo.range(rx.next + 1..=rx.next + 64) {
            bitmap |= 1 << (seq - rx.next - 1);
        }
        let mut ack = [0u8; ACK_LEN];
        ack[..8].copy_from_slice(&rx.next.to_be_bytes());
        ack[8..10].copy_from_slice(&self.window_now(rx).to_be_bytes());
        ack[10..].copy_from_slice(&bitmap.to_be_bytes());
        rx.since_ack = 0;
        rx.ack_due = None;
        rx.ack_now = false;
        ack
    }

    fn deliver(&self, rx: &mut Rx, last: bool, payload: &[u8]) {
        rx.next += 1;
        if rx.partial.len() + payload.len() > MAX_MESSAGE_BYTES {
            // Not something this code ever sends: stop listening.
            tracing::warn!("reliable stream: oversized message; closing");
            rx.out = None;
            rx.partial = Vec::new();
            self.closed.store(true, Ordering::Relaxed);
            return;
        }
        rx.partial.extend_from_slice(payload);
        if last {
            let msg = std::mem::take(&mut rx.partial);
            if let Some(out) = &rx.out {
                self.pending.fetch_add(msg.len(), Ordering::Relaxed);
                let _ = out.send(msg);
            }
        }
    }

    fn on_data(&self, mut body: Bytes) {
        if body.len() < DATA_HEADER {
            return;
        }
        let seq = u64::from_be_bytes(body[..8].try_into().unwrap());
        let last = body[8] & 1 == 1;
        let payload = body.split_off(DATA_HEADER);
        if payload.len() > SEGMENT_BYTES {
            return;
        }
        let mut rx = self.rx.lock().unwrap();
        if rx.out.is_none() {
            return;
        }
        let mut ack_inline = false;
        let mut wake = false;
        if seq < rx.next {
            // A duplicate: our ack was lost, say it again.
            ack_inline = true;
        } else if seq >= rx.next + RX_WINDOW {
            // Beyond anything we offered to hold.
        } else if seq == rx.next {
            if self.pending.load(Ordering::Relaxed) > INBOX_HARD_BYTES {
                ack_inline = true;
            } else {
                let had_gap = !rx.ooo.is_empty();
                self.deliver(&mut rx, last, &payload);
                let mut any_last = last;
                loop {
                    let next = rx.next;
                    let Some((l, p)) = rx.ooo.remove(&next) else { break };
                    any_last |= l;
                    self.deliver(&mut rx, l, &p);
                }
                rx.since_ack += 1;
                if any_last || had_gap || rx.since_ack >= 2 {
                    ack_inline = true;
                } else if rx.ack_due.is_none() {
                    rx.ack_due = Some(Instant::now());
                    wake = true;
                }
            }
        } else {
            rx.ooo.entry(seq).or_insert((last, payload));
            ack_inline = true;
        }
        let ack = ack_inline.then(|| self.build_ack(&mut rx));
        drop(rx);
        if let Some(ack) = ack {
            let _ = self.wire.send_stream(true, &ack);
        }
        if wake {
            self.wake.notify_one();
        }
    }

    fn on_ack(&self, body: &[u8]) {
        if body.len() < ACK_LEN {
            return;
        }
        let cum = u64::from_be_bytes(body[..8].try_into().unwrap());
        let rwnd = u16::from_be_bytes(body[8..10].try_into().unwrap());
        let bitmap = u64::from_be_bytes(body[10..18].try_into().unwrap());
        let now = Instant::now();
        let mut tx = self.tx.lock().unwrap();
        if cum < tx.base || cum > tx.base + tx.window.len() as u64 {
            return; // stale (reordered) or nonsense
        }
        let acked = (cum - tx.base) as usize;
        let mut sample = None;
        for _ in 0..acked {
            if let Some(seg) = tx.window.pop_front() {
                if seg.retx == 0 {
                    sample = Some(now.duration_since(seg.sent).as_secs_f64());
                }
            }
        }
        tx.base = cum;
        tx.rwnd = rwnd as u32;

        // Selective acks: note what arrived, and treat a segment as lost once
        // three later ones have.
        let mut highest = None;
        for i in 0..64usize {
            if bitmap >> i & 1 == 1 {
                if let Some(seg) = tx.window.get_mut(1 + i) {
                    seg.sacked = true;
                    highest = Some(1 + i);
                }
            }
        }
        let mut lost = None;
        if let Some(h) = highest {
            let again_after = Duration::from_secs_f64((tx.srtt.unwrap_or(0.05) * 1.5).max(0.02));
            for idx in 0..h {
                let seg = &mut tx.window[idx];
                if !seg.sacked && !seg.resend && h - idx >= 3 && (seg.retx == 0 || now.duration_since(seg.sent) > again_after) {
                    seg.resend = true;
                    lost.get_or_insert(idx);
                }
            }
        }

        let mut delayed = false;
        if let Some(rtt) = sample {
            tx.srtt = Some(match tx.srtt {
                Some(s) => s * 0.875 + rtt * 0.125,
                None => rtt,
            });
            tx.min_rtt = tx.min_rtt.min(rtt);
            // A round trip well above the floor means a queue is building on
            // the path, the same path the picture travels: ease off before
            // anything is dropped.
            if rtt > tx.min_rtt * 2.0 + 0.025 {
                delayed = true;
                let srtt = Duration::from_secs_f64(tx.srtt.unwrap_or(rtt));
                if now.duration_since(tx.last_delay_backoff) > srtt {
                    tx.last_delay_backoff = now;
                    tx.cwnd = (tx.cwnd * 0.85).max(MIN_CWND);
                    tx.ssthresh = tx.cwnd;
                }
            }
        }
        if acked > 0 && !delayed && lost.is_none() {
            let n = acked as f64;
            tx.cwnd = if tx.cwnd < tx.ssthresh { tx.cwnd + n } else { tx.cwnd + n / tx.cwnd };
            tx.cwnd = tx.cwnd.min(MAX_CWND);
        }
        if let Some(idx) = lost {
            let seq = tx.base + idx as u64;
            tx.on_loss(seq);
        }
        drop(tx);
        self.wake.notify_one();
    }

    /// Send an ack the receive side owes (delayed, or a window update).
    fn flush_ack(&self) {
        let mut rx = self.rx.lock().unwrap();
        let due = rx.ack_now || rx.ack_due.is_some_and(|t| t.elapsed() >= Duration::from_millis(2));
        if !due || rx.out.is_none() {
            return;
        }
        let ack = self.build_ack(&mut rx);
        drop(rx);
        let _ = self.wire.send_stream(true, &ack);
    }

    fn pump(&self) {
        loop {
            if self.closed.load(Ordering::Relaxed) {
                return;
            }
            self.flush_ack();
            let ack_pending = self.rx.lock().unwrap().ack_due.is_some();

            let mut tx = self.tx.lock().unwrap();
            let now = Instant::now();
            let rto = tx.rto();
            let mut burst: Vec<Bytes> = Vec::new();

            // 1. Segments to send again: marked lost, or unanswered too long.
            let mut timed_out = None;
            for (i, seg) in tx.window.iter_mut().enumerate() {
                if seg.sacked {
                    continue;
                }
                let late = now.duration_since(seg.sent) > rto * (1u32 << seg.retx.min(5));
                if seg.resend || late {
                    if late && !seg.resend {
                        timed_out.get_or_insert(i);
                    }
                    seg.resend = false;
                    seg.sent = now;
                    seg.retx = seg.retx.saturating_add(1);
                    burst.push(seg.body.clone());
                    if burst.len() >= BURST {
                        break;
                    }
                }
            }
            tx.retransmits += burst.len() as u64;
            if let Some(i) = timed_out {
                let seq = tx.base + i as u64;
                tx.on_loss(seq);
            }

            // 2. New segments, as far as both windows allow.
            let mut allowed = (tx.cwnd as usize).min(tx.rwnd as usize);
            if allowed == 0 && tx.window.is_empty() && now.duration_since(tx.last_send) > Duration::from_millis(200) {
                allowed = 1; // probe a closed window in case its reopening was lost
            }
            while burst.len() < BURST && tx.window.len() < allowed {
                let Some(body) = tx.queue.pop_front() else { break };
                tx.queue_bytes -= body.len();
                burst.push(body.clone());
                tx.window.push_back(Seg { body, sent: now, retx: 0, sacked: false, resend: false });
            }
            if tx.queue_bytes < SEND_QUEUE_BYTES {
                self.space.notify_all();
            }

            if !burst.is_empty() {
                tx.last_send = now;
                drop(tx);
                for body in burst {
                    let _ = self.wire.send_stream(false, &body);
                }
                continue;
            }
            let idle = tx.window.is_empty() && tx.queue.is_empty();
            let wait = if ack_pending {
                Duration::from_millis(2)
            } else if idle {
                Duration::from_millis(50)
            } else {
                Duration::from_millis(5)
            };
            let _unused = self.wake.wait_timeout(tx, wait).unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, SeedableRng};
    use std::sync::OnceLock;

    /// A wire that hands datagrams to the other end through a thread which
    /// loses and reorders some of them.
    struct Lossy {
        to_peer: Mutex<mpsc::Sender<(bool, Vec<u8>)>>,
    }

    impl Wire for Lossy {
        fn send_stream(&self, ack: bool, body: &[u8]) -> std::io::Result<()> {
            let _ = self.to_peer.lock().unwrap().send((ack, body.to_vec()));
            Ok(())
        }
    }

    fn carrier(rx: mpsc::Receiver<(bool, Vec<u8>)>, dest: Arc<OnceLock<Reliable>>, loss: f64, seed: u64) {
        std::thread::spawn(move || {
            let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
            let mut held: Option<(bool, Vec<u8>)> = None;
            while let Ok(d) = rx.recv() {
                if rng.gen_bool(loss) {
                    continue;
                }
                let Some(dest) = dest.get() else { continue };
                // Now and then, let the next datagram overtake this one.
                if held.is_none() && rng.gen_bool(0.02) {
                    held = Some(d);
                    continue;
                }
                dest.handle(d.0, Bytes::from(d.1));
                if let Some(h) = held.take() {
                    dest.handle(h.0, Bytes::from(h.1));
                }
            }
        });
    }

    fn pair(loss: f64) -> ((Reliable, Inbox), (Reliable, Inbox)) {
        let (a_tx, a_rx) = mpsc::channel();
        let (b_tx, b_rx) = mpsc::channel();
        let a_slot = Arc::new(OnceLock::new());
        let b_slot = Arc::new(OnceLock::new());
        carrier(a_rx, b_slot.clone(), loss, 1);
        carrier(b_rx, a_slot.clone(), loss, 2);
        let a = Reliable::start(Arc::new(Lossy { to_peer: Mutex::new(a_tx) }));
        let b = Reliable::start(Arc::new(Lossy { to_peer: Mutex::new(b_tx) }));
        let _ = a_slot.set(a.0.clone());
        let _ = b_slot.set(b.0.clone());
        (a, b)
    }

    fn messages(n: usize, max: usize) -> Vec<Vec<u8>> {
        let mut rng = rand::rngs::StdRng::seed_from_u64(7);
        (0..n)
            .map(|i| {
                let len = if i % 9 == 0 { 0 } else { rng.gen_range(1..max) };
                (0..len).map(|_| rng.gen()).collect()
            })
            .collect()
    }

    fn roundtrip(loss: f64, n: usize, max: usize) {
        let ((a, _a_in), (b, b_in)) = pair(loss);
        let msgs = messages(n, max);
        let sent = msgs.clone();
        let sender = std::thread::spawn(move || {
            for m in &sent {
                a.send(m).unwrap();
            }
            a
        });
        for (i, want) in msgs.iter().enumerate() {
            let got = b_in.recv_timeout(Duration::from_secs(30)).unwrap_or_else(|_| panic!("message {i} never arrived"));
            assert!(got == *want, "message {i} differs");
        }
        let a = sender.join().unwrap();
        a.close();
        b.close();
        assert!(b_in.recv().is_none());
    }

    #[test]
    fn clean_wire_delivers_everything_in_order() {
        roundtrip(0.0, 300, 40_000);
    }

    #[test]
    fn survives_loss_and_reordering() {
        roundtrip(0.08, 150, 60_000);
    }

    #[test]
    fn large_messages_and_a_slow_reader() {
        let ((a, _a_in), (b, b_in)) = pair(0.01);
        let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        let copy = big.clone();
        let sender = std::thread::spawn(move || {
            for _ in 0..8 {
                a.send(&copy).unwrap();
            }
            a
        });
        // The reader dawdles: the window has to close and reopen.
        std::thread::sleep(Duration::from_millis(600));
        for _ in 0..8 {
            let got = b_in.recv_timeout(Duration::from_secs(30)).expect("message arrives");
            assert!(got == big);
            std::thread::sleep(Duration::from_millis(50));
        }
        sender.join().unwrap().close();
        b.close();
    }

    #[test]
    fn send_fails_once_closed() {
        let ((a, _a_in), (b, _b_in)) = pair(0.0);
        a.close();
        assert_eq!(a.send(b"x"), Err(Closed));
        b.close();
    }
}
