//! Send pacing.
//!
//! A 1080p keyframe is ~200 KB: fired as one burst of 180 datagrams it
//! overflows small router / Wi-Fi / socket buffers and the tail is lost
//! systematically, which NACKs then re-burst. A token bucket at a few times
//! the target bitrate spreads each frame over a few milliseconds instead.

use std::time::{Duration, Instant};

pub struct Pacer {
    rate_bytes_per_s: f64,
    burst_bytes: f64,
    tokens: f64,
    last: Instant,
}

impl Pacer {
    /// `floor_kbps` keeps pacing fast on low-bitrate links so latency is
    /// not added when the encoder is throttled.
    pub fn new(target_kbps: u32, floor_kbps: u32) -> Self {
        let mut p = Self {
            rate_bytes_per_s: 0.0,
            burst_bytes: 16_000.0,
            tokens: 16_000.0,
            last: Instant::now(),
        };
        p.set_target(target_kbps, floor_kbps);
        p
    }

    pub fn set_target(&mut self, target_kbps: u32, floor_kbps: u32) {
        let kbps = (target_kbps as f64 * 2.5).max(floor_kbps as f64);
        self.rate_bytes_per_s = kbps * 1000.0 / 8.0;
    }

    fn refill(&mut self, now: Instant) {
        let dt = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + dt * self.rate_bytes_per_s).min(self.burst_bytes);
    }

    /// Block until `bytes` may be sent. Short waits spin (Windows sleep
    /// granularity is coarse); longer ones yield.
    pub fn wait_for(&mut self, bytes: usize) {
        let need = bytes as f64;
        loop {
            self.refill(Instant::now());
            if self.tokens >= need {
                self.tokens -= need;
                return;
            }
            let deficit = need - self.tokens;
            let wait = Duration::from_secs_f64(deficit / self.rate_bytes_per_s);
            if wait > Duration::from_millis(2) {
                std::thread::sleep(Duration::from_millis(1));
            } else {
                let until = Instant::now() + wait;
                while Instant::now() < until {
                    std::hint::spin_loop();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spreads_a_burst() {
        // 25 Mbps floor = 3.125 MB/s; 160 KB beyond the 16 KB burst should
        // take roughly 46 ms.
        let mut p = Pacer::new(1000, 25_000);
        let t0 = Instant::now();
        for _ in 0..160 {
            p.wait_for(1100);
        }
        let el = t0.elapsed();
        assert!(el >= Duration::from_millis(35), "too fast: {el:?}");
        assert!(el <= Duration::from_millis(120), "too slow: {el:?}");
    }
}
