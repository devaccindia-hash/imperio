//! In-order frame assembly with a retransmit deadline.
//!
//! Chunks arrive out of order and retransmitted chunks arrive late. Frames
//! are held until complete or until `deadline` passes; a frame that misses
//! its deadline is dropped and, because H.264 P-frames depend on their
//! predecessors, everything up to the next keyframe is discarded and a
//! keyframe is requested.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use bytes::{BufMut, Bytes, BytesMut};

use crate::lan::{ChunkHeader, CompleteFrame};

struct Partial {
    header: ChunkHeader,
    chunks: Vec<Option<Bytes>>,
    received: usize,
    first_seen: Instant,
    /// Last time a new chunk of this frame arrived. The deadline counts from
    /// here, so a large paced keyframe is not abandoned while still in flight.
    last_progress: Instant,
    /// A chunk arrived well after the first one (reordering or retransmit).
    had_late_chunk: bool,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct AssemblerStats {
    /// Frames dropped because they missed the deadline.
    pub dropped_incomplete: u64,
    /// Non-key frames discarded while waiting for a keyframe to resync.
    pub discarded_waiting_key: u64,
    /// Frames that completed only after at least one chunk arrived late.
    pub completed_late: u64,
    pub emitted: u64,
}

pub struct FrameAssembler {
    pending: BTreeMap<u32, Partial>,
    next_frame: Option<u32>,
    waiting_for_keyframe: bool,
    deadline: Duration,
    pub stats: AssemblerStats,
}

impl Default for FrameAssembler {
    fn default() -> Self {
        Self::new(Duration::from_millis(40))
    }
}

impl FrameAssembler {
    pub fn new(deadline: Duration) -> Self {
        Self {
            pending: BTreeMap::new(),
            next_frame: None,
            waiting_for_keyframe: false,
            deadline,
            stats: AssemblerStats::default(),
        }
    }

    /// Deadline should cover one NACK round trip plus scheduling slack.
    pub fn set_deadline(&mut self, d: Duration) {
        self.deadline = d.clamp(Duration::from_millis(15), Duration::from_millis(400));
    }

    pub fn deadline(&self) -> Duration {
        self.deadline
    }

    pub fn push(&mut self, header: ChunkHeader, payload: Bytes, now: Instant) {
        // Ignore chunks of frames we already emitted or gave up on.
        if let Some(next) = self.next_frame {
            if header.frame_id < next {
                return;
            }
        }
        let partial = self
            .pending
            .entry(header.frame_id)
            .or_insert_with(|| Partial {
                header,
                chunks: vec![None; header.count.max(1) as usize],
                received: 0,
                first_seen: now,
                last_progress: now,
                had_late_chunk: false,
            });
        if let Some(slot) = partial.chunks.get_mut(header.index as usize) {
            if slot.is_none() {
                *slot = Some(payload);
                partial.received += 1;
                // A chunk arriving after a stall longer than pacing would
                // explain means reordering or a NACK retransmit.
                if now.duration_since(partial.last_progress) > Duration::from_millis(10) {
                    partial.had_late_chunk = true;
                }
                partial.last_progress = now;
            }
        }
        // Memory bound against a hostile or broken sender.
        while self.pending.len() > 64 {
            let oldest = *self.pending.keys().next().unwrap();
            self.pending.remove(&oldest);
            self.stats.dropped_incomplete += 1;
            self.waiting_for_keyframe = true;
        }
    }

    /// Emit everything that is complete and in order. Returns the frames and
    /// whether the caller should request a keyframe.
    pub fn poll(&mut self, now: Instant) -> (Vec<CompleteFrame>, bool) {
        let mut out = Vec::new();
        let mut need_key = false;
        while let Some((&id, partial)) = self.pending.iter().next() {
            let complete = partial.received == partial.chunks.len();
            if !complete {
                if now.duration_since(partial.last_progress) >= self.deadline {
                    tracing::debug!(
                        frame_id = id,
                        received = partial.received,
                        total = partial.chunks.len(),
                        keyframe = partial.header.keyframe,
                        age_ms = now.duration_since(partial.first_seen).as_millis(),
                        stalled_ms = now.duration_since(partial.last_progress).as_millis(),
                        "frame missed deadline"
                    );
                    self.pending.remove(&id);
                    self.stats.dropped_incomplete += 1;
                    self.waiting_for_keyframe = true;
                    need_key = true;
                    self.next_frame = Some(id + 1);
                    continue;
                }
                break; // wait for retransmits
            }
            let partial = self.pending.remove(&id).unwrap();
            let skipped = self.next_frame.is_some_and(|n| id > n);
            if skipped && !partial.header.keyframe {
                // A whole frame vanished (no chunk ever arrived); P-frame
                // chain is broken.
                self.waiting_for_keyframe = true;
                need_key = true;
            }
            self.next_frame = Some(id + 1);
            if self.waiting_for_keyframe {
                if partial.header.keyframe {
                    self.waiting_for_keyframe = false;
                } else {
                    self.stats.discarded_waiting_key += 1;
                    need_key = true;
                    continue;
                }
            }
            if partial.had_late_chunk {
                self.stats.completed_late += 1;
            }
            let total: usize = partial
                .chunks
                .iter()
                .map(|c| c.as_ref().map_or(0, |b| b.len()))
                .sum();
            let mut data = BytesMut::with_capacity(total);
            for c in partial.chunks {
                data.put_slice(&c.unwrap());
            }
            self.stats.emitted += 1;
            out.push(CompleteFrame {
                frame_id: id,
                keyframe: partial.header.keyframe,
                timestamp_us: partial.header.timestamp_us,
                data: data.freeze(),
            });
        }
        (out, need_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lan::{parse, split};

    fn feed(a: &mut FrameAssembler, chunks: &[Bytes], now: Instant) {
        for c in chunks {
            let (h, p) = parse(c.clone()).unwrap();
            a.push(h, p, now);
        }
    }

    #[test]
    fn in_order_frames_emit_immediately() {
        let mut a = FrameAssembler::default();
        let t = Instant::now();
        feed(&mut a, &split(1, true, 0, &[1u8; 3000]), t);
        feed(&mut a, &split(2, false, 0, &[2u8; 100]), t);
        let (frames, key) = a.poll(t);
        assert_eq!(
            frames.iter().map(|f| f.frame_id).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!(!key);
    }

    #[test]
    fn late_chunk_completes_frame_within_deadline() {
        let mut a = FrameAssembler::new(Duration::from_millis(40));
        let t = Instant::now();
        let f1 = split(1, true, 0, &[1u8; 3000]); // 3 chunks
        feed(&mut a, &[f1[0].clone(), f1[2].clone()], t);
        feed(&mut a, &split(2, false, 0, &[2u8; 10]), t);
        let (frames, key) = a.poll(t + Duration::from_millis(10));
        assert!(frames.is_empty() && !key, "must wait for the missing chunk");
        feed(&mut a, &[f1[1].clone()], t + Duration::from_millis(25));
        let (frames, key) = a.poll(t + Duration::from_millis(25));
        assert_eq!(
            frames.iter().map(|f| f.frame_id).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!(!key);
        assert_eq!(a.stats.completed_late, 1);
    }

    #[test]
    fn missed_deadline_drops_until_keyframe() {
        let mut a = FrameAssembler::new(Duration::from_millis(40));
        let t = Instant::now();
        let f1 = split(1, true, 0, &[1u8; 3000]);
        feed(&mut a, &[f1[0].clone()], t);
        feed(&mut a, &split(2, false, 0, &[2u8; 10]), t);
        feed(&mut a, &split(3, false, 0, &[3u8; 10]), t);
        let (frames, key) = a.poll(t + Duration::from_millis(50));
        assert!(frames.is_empty());
        assert!(key, "keyframe must be requested");
        assert_eq!(a.stats.dropped_incomplete, 1);
        assert_eq!(a.stats.discarded_waiting_key, 2);
        feed(
            &mut a,
            &split(4, true, 0, &[4u8; 10]),
            t + Duration::from_millis(60),
        );
        let (frames, _) = a.poll(t + Duration::from_millis(60));
        assert_eq!(frames.len(), 1);
        assert!(frames[0].keyframe);
    }

    #[test]
    fn vanished_frame_breaks_chain() {
        let mut a = FrameAssembler::default();
        let t = Instant::now();
        feed(&mut a, &split(1, true, 0, &[1u8; 10]), t);
        let _ = a.poll(t);
        // frame 2 never arrives at all
        feed(&mut a, &split(3, false, 0, &[3u8; 10]), t);
        let (frames, key) = a.poll(t);
        assert!(frames.is_empty());
        assert!(key);
    }
}
