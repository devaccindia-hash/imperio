//! Adaptive bitrate from receiver reports.
//!
//! Loss and queuing delay say *that* the path is overloaded; how much arrived
//! says *what it carries*. On overload the target drops to a little under the
//! rate that actually arrived, in one step, instead of creeping down while the
//! queue in front of the bottleneck keeps growing. It rises again only while
//! the stream really uses the target: a quiet screen sends a fraction of it
//! and proves nothing about the path.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::link::ReceiverReport;

/// Media sent so far (cumulative), read when a report arrives.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Sent {
    pub datagrams: u64,
    pub bytes: u64,
}

/// Rates are measured over about this long: one report (100 ms) holds only a
/// few frames and says little, while a long window still shows the rate from
/// before the overload.
const RATE_WINDOW: Duration = Duration::from_millis(400);

struct Sample {
    at: Instant,
    received: u64,
    sent: Sent,
}

pub struct BitrateController {
    min_kbps: u32,
    max_kbps: u32,
    target_kbps: u32,
    last_report: Option<ReceiverReport>,
    history: VecDeque<Sample>,
    last_increase: Instant,
    last_decrease: Instant,
    /// Queuing delay at the last cut, to tell a draining queue from a growing one.
    queue_at_decrease: f64,
    /// No overload seen yet: the target climbs quickly towards the maximum.
    starting: bool,
}

impl BitrateController {
    pub fn new(start_kbps: u32, min_kbps: u32, max_kbps: u32) -> Self {
        let now = Instant::now();
        Self {
            min_kbps,
            max_kbps,
            target_kbps: start_kbps.clamp(min_kbps, max_kbps),
            last_report: None,
            history: VecDeque::new(),
            last_increase: now,
            last_decrease: now,
            queue_at_decrease: 0.0,
            starting: true,
        }
    }

    pub fn target_kbps(&self) -> u32 {
        self.target_kbps
    }

    /// (sent, arrived) in kbit/s over the last [`RATE_WINDOW`], once there is
    /// enough history. Arrived bytes are the arrived datagrams at the average
    /// size of those sent: the report counts datagrams, not bytes.
    fn rates(&self, now: Instant) -> Option<(f64, f64)> {
        let newest = self.history.back()?;
        let oldest = self.history.iter().find(|s| now.duration_since(s.at) <= RATE_WINDOW)?;
        let secs = newest.at.duration_since(oldest.at).as_secs_f64();
        let datagrams = newest.sent.datagrams.saturating_sub(oldest.sent.datagrams);
        if secs < 0.2 || datagrams == 0 {
            return None;
        }
        let bytes = newest.sent.bytes.saturating_sub(oldest.sent.bytes) as f64;
        let arrived = newest.received.saturating_sub(oldest.received) as f64 * (bytes / datagrams as f64);
        Some((bytes * 8.0 / 1000.0 / secs, arrived * 8.0 / 1000.0 / secs))
    }

    /// Feed a report, with the media sent up to now. Returns the new target
    /// when it changed by >= 5%.
    pub fn on_report(&mut self, r: &ReceiverReport, sent: Sent, now: Instant) -> Option<u32> {
        self.history.push_back(Sample { at: now, received: r.received, sent });
        while self.history.front().is_some_and(|s| now.duration_since(s.at) > RATE_WINDOW * 2) {
            self.history.pop_front();
        }
        let prev = self.last_report.replace(*r)?;
        let d_recv = r.received.saturating_sub(prev.received);
        let d_lost = r.lost.saturating_sub(prev.lost);
        let d_missing = r.missing_detected.saturating_sub(prev.missing_detected);
        let total = d_recv + d_lost;
        // Unrecoverable loss is a hard signal; raw (pre-NACK) loss only
        // matters when heavy, so random lossy links are not starved.
        let loss = if total == 0 { 0.0 } else { d_lost as f64 / total as f64 };
        let raw_loss = d_missing as f64 / (d_recv + d_missing).max(1) as f64;
        let queue_ms = r.queue_delay_us as f64 / 1000.0;
        let rates = self.rates(now);
        let target = self.target_kbps as f64;
        // The stream is using a good part of the target, so what the path
        // does to it says something about the path.
        let busy = rates.is_some_and(|(sent, _)| sent >= target * 0.4);

        let old = self.target_kbps;
        let overloaded = loss > 0.02 || raw_loss > 0.15 || queue_ms > 60.0;
        if overloaded {
            // A queue takes a while to drain after a cut. While it is
            // shrinking and nothing is being lost, the last cut is working:
            // cutting again would only overshoot.
            let since = now.duration_since(self.last_decrease);
            let draining = loss <= 0.02
                && since < Duration::from_secs(2)
                && queue_ms < self.queue_at_decrease * 0.9;
            if since > Duration::from_millis(200) && !draining {
                let mut next = target * 0.8;
                // What arrived while the path was overloaded is what the
                // path carries: aim a little under it.
                if let (true, Some((_, arrived))) = (busy, rates) {
                    next = next.min(arrived * 0.85);
                }
                self.target_kbps = next as u32;
                self.last_decrease = now;
                self.queue_at_decrease = queue_ms;
                self.starting = false;
            }
        } else if total > 0 && busy && loss < 0.01 && queue_ms < 15.0 {
            let (every, factor) = if self.starting {
                (Duration::from_millis(250), 1.25)
            } else {
                (Duration::from_millis(500), 1.08)
            };
            // After a cut, let the queue drain before trying for more.
            if now.duration_since(self.last_increase) > every
                && now.duration_since(self.last_decrease) > Duration::from_secs(1)
            {
                self.target_kbps = (target * factor) as u32;
                self.last_increase = now;
            }
        }
        self.target_kbps = self.target_kbps.clamp(self.min_kbps, self.max_kbps);
        let changed =
            (self.target_kbps as i64 - old as i64).unsigned_abs() as f64 >= old as f64 * 0.05;
        changed.then_some(self.target_kbps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rep(received: u64, lost: u64, q_ms: u32) -> ReceiverReport {
        ReceiverReport {
            highest_seq: 0,
            received,
            lost,
            recovered: 0,
            missing_detected: lost,
            queue_delay_us: q_ms * 1000,
            rtt_us: 0,
        }
    }

    /// Feeds one report every 100 ms of a stream of 1000-byte datagrams.
    struct Sim {
        cc: BitrateController,
        t: Instant,
        received: u64,
        lost: u64,
        sent: Sent,
    }

    impl Sim {
        fn new(start: u32, min: u32, max: u32) -> Self {
            let mut s = Self {
                cc: BitrateController::new(start, min, max),
                t: Instant::now() + Duration::from_secs(5),
                received: 0,
                lost: 0,
                sent: Sent::default(),
            };
            s.step(0, 0, 0);
            s
        }

        /// 100 ms in which `sent_kbps` left and `arrived_kbps` arrived.
        fn step(&mut self, sent_kbps: u64, arrived_kbps: u64, q_ms: u32) -> Option<u32> {
            self.t += Duration::from_millis(100);
            let sent = sent_kbps * 100 / 8 / 1000; // datagrams of 1000 bytes
            let arrived = arrived_kbps * 100 / 8 / 1000;
            self.sent.datagrams += sent;
            self.sent.bytes += sent * 1000;
            self.received += arrived;
            self.lost += sent.saturating_sub(arrived);
            self.cc.on_report(&rep(self.received, self.lost, q_ms), self.sent, self.t)
        }
    }

    #[test]
    fn overload_drops_to_what_arrives() {
        // 8 Mbit/s into a path that carries 3: the target must land a little
        // under 3 within a second, not after a dozen 20% cuts.
        let mut s = Sim::new(8000, 500, 8000);
        for _ in 0..8 {
            s.step(8000, 8000, 0);
        }
        for _ in 0..7 {
            s.step(8000, 3000, 150);
        }
        let t = s.cc.target_kbps();
        assert!((1200..=2600).contains(&t), "target {t}");
    }

    #[test]
    fn a_quiet_screen_neither_raises_nor_collapses_the_target() {
        let mut s = Sim::new(4000, 500, 8000);
        // Hardly anything is sent: no evidence the path can carry more.
        for _ in 0..30 {
            s.step(200, 200, 0);
        }
        assert_eq!(s.cc.target_kbps(), 4000);
        // A burst of loss while almost idle: an ordinary cut, not a fall to
        // the little that happened to be flowing.
        for _ in 0..3 {
            s.step(200, 100, 0);
        }
        assert!(s.cc.target_kbps() >= 2500, "target {}", s.cc.target_kbps());
    }

    #[test]
    fn climbs_fast_at_the_start_then_carefully() {
        let mut s = Sim::new(4000, 500, 8000);
        for _ in 0..12 {
            let t = s.cc.target_kbps() as u64;
            s.step(t, t, 0);
        }
        assert_eq!(s.cc.target_kbps(), 8000, "start-up reaches the maximum within about a second");
        // One overload ends the start-up phase.
        for _ in 0..5 {
            s.step(8000, 4000, 100);
        }
        let low = s.cc.target_kbps();
        assert!(low < 4000, "target {low}");
        for _ in 0..15 {
            let t = s.cc.target_kbps() as u64;
            s.step(t, t, 0);
        }
        let after = s.cc.target_kbps();
        assert!(after > low && (after as f64) < low as f64 * 1.3, "careful growth: {low} -> {after}");
    }

    #[test]
    fn queue_delay_alone_triggers_backoff() {
        let mut s = Sim::new(8000, 500, 12000);
        for _ in 0..8 {
            s.step(8000, 8000, 0);
        }
        let before = s.cc.target_kbps();
        s.step(8000, 8000, 120);
        s.step(8000, 8000, 120);
        s.step(8000, 8000, 120);
        assert!(s.cc.target_kbps() < before);
    }
}
