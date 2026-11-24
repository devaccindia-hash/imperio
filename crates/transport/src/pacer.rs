//! Send pacing.
//!
//! A 1080p keyframe is ~200 KB: fired as one burst of 180 datagrams it
//! overflows small router / Wi-Fi / socket buffers and the tail is lost
//! systematically, which NACKs then re-burst. A token bucket at a few times
//! the target bitrate spreads each frame over a few milliseconds instead.

use std::time::{Duration, Instant};

/// Frames larger than this are full pictures, or close to it.
const LARGE_FRAME_BYTES: usize = 64_000;

pub struct Pacer {
    rate_bytes_per_s: f64,
    /// Rate for ordinary frames and for large ones (see `begin_frame`).
    normal_bytes_per_s: f64,
    large_bytes_per_s: f64,
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
            normal_bytes_per_s: 0.0,
            large_bytes_per_s: 0.0,
            burst_bytes: 16_000.0,
            tokens: 16_000.0,
            last: Instant::now(),
        };
        p.set_target(target_kbps, floor_kbps);
        p
    }

    pub fn set_target(&mut self, target_kbps: u32, floor_kbps: u32) {
        let rate = |factor: f64| (target_kbps as f64 * factor).max(floor_kbps as f64).max(64.0) * 1000.0 / 8.0;
        // An ordinary frame may leave well above the average rate: it is
        // small, and holding it back only adds delay.
        self.normal_bytes_per_s = rate(2.5);
        // A full picture is tens of times larger. Sent that fast into a path
        // that carries about the target, most of it would have to wait in
        // the queue in front of the bottleneck, and the tail is dropped when
        // that queue is full. It leaves at about the rate the path carries.
        self.large_bytes_per_s = rate(1.25);
        self.rate_bytes_per_s = self.normal_bytes_per_s;
    }

    /// Choose the pace for the frame about to be sent.
    pub fn begin_frame(&mut self, frame_bytes: usize) {
        self.rate_bytes_per_s =
            if frame_bytes > LARGE_FRAME_BYTES { self.large_bytes_per_s } else { self.normal_bytes_per_s };
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
    fn a_full_picture_leaves_at_about_the_target_rate() {
        // 100 KB at a 2 Mbit/s target: about 0.32 s at 1.25 times the
        // target, where the ordinary pace would take 0.16 s.
        let mut p = Pacer::new(2000, 0);
        p.begin_frame(100_000);
        let t0 = Instant::now();
        for _ in 0..100 {
            p.wait_for(1000);
        }
        let el = t0.elapsed();
        assert!(el >= Duration::from_millis(230), "too fast: {el:?}");
        assert!(el <= Duration::from_millis(480), "too slow: {el:?}");
    }

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
