//! Adaptive bitrate from receiver reports.
//!
//! Deliberately simple and conservative (loss + queuing-delay AIMD). A
//! proper delay-gradient controller with pacing is Phase 2b; this one is
//! enough to stop a WAN link from collapsing under a fixed CBR target.

use std::time::{Duration, Instant};

use crate::link::ReceiverReport;

pub struct BitrateController {
    min_kbps: u32,
    max_kbps: u32,
    target_kbps: u32,
    last_report: Option<ReceiverReport>,
    last_increase: Instant,
    last_decrease: Instant,
}

impl BitrateController {
    pub fn new(start_kbps: u32, min_kbps: u32, max_kbps: u32) -> Self {
        let now = Instant::now();
        Self {
            min_kbps,
            max_kbps,
            target_kbps: start_kbps.clamp(min_kbps, max_kbps),
            last_report: None,
            last_increase: now,
            last_decrease: now,
        }
    }

    pub fn target_kbps(&self) -> u32 {
        self.target_kbps
    }

    /// Feed a report. Returns the new target when it changed by >= 5%.
    pub fn on_report(&mut self, r: &ReceiverReport, now: Instant) -> Option<u32> {
        let prev = self.last_report.replace(*r);
        let Some(prev) = prev else { return None };
        let d_recv = r.received.saturating_sub(prev.received);
        let d_lost = r.lost.saturating_sub(prev.lost);
        let d_missing = r.missing_detected.saturating_sub(prev.missing_detected);
        let total = d_recv + d_lost;
        if total == 0 {
            return None;
        }
        // Unrecoverable loss is a hard signal; raw (pre-NACK) loss only
        // matters when heavy, so random lossy links are not starved.
        let loss = d_lost as f64 / total as f64;
        let raw_loss = d_missing as f64 / (d_recv + d_missing).max(1) as f64;
        let queue_ms = r.queue_delay_us as f64 / 1000.0;

        let old = self.target_kbps;
        if (loss > 0.02 || raw_loss > 0.15 || queue_ms > 60.0)
            && now.duration_since(self.last_decrease) > Duration::from_millis(200)
        {
            self.target_kbps = ((self.target_kbps as f64) * 0.8) as u32;
            self.last_decrease = now;
        } else if loss < 0.01
            && queue_ms < 15.0
            && now.duration_since(self.last_increase) > Duration::from_millis(500)
        {
            self.target_kbps = ((self.target_kbps as f64) * 1.08) as u32;
            self.last_increase = now;
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

    #[test]
    fn backs_off_on_loss_and_recovers() {
        let t = Instant::now();
        let mut cc = BitrateController::new(8000, 500, 12000);
        assert!(cc.on_report(&rep(100, 0, 0), t).is_none());
        let r = cc.on_report(&rep(200, 20, 0), t + Duration::from_millis(300));
        assert_eq!(r, Some(6400));
        // clean reports for a while -> growth
        let mut t2 = t + Duration::from_secs(1);
        let mut last = cc.target_kbps();
        for i in 0..6 {
            if let Some(v) = cc.on_report(&rep(300 + i * 100, 20, 0), t2) {
                assert!(v > last);
                last = v;
            }
            t2 += Duration::from_millis(600);
        }
        assert!(cc.target_kbps() > 6400);
    }

    #[test]
    fn queue_delay_alone_triggers_backoff() {
        let t = Instant::now();
        let mut cc = BitrateController::new(8000, 500, 12000);
        cc.on_report(&rep(100, 0, 0), t);
        assert_eq!(
            cc.on_report(&rep(200, 0, 120), t + Duration::from_millis(300)),
            Some(6400)
        );
    }
}
