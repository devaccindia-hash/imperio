//! Keyboard and mouse events that arrive, once, in the order they were made.
//!
//! Control messages travel as single datagrams and are not retransmitted. For
//! input that is not good enough: a lost key release leaves the key held down
//! on the other computer, a lost press loses a letter, and a lost pointer move
//! puts the click that follows it in the wrong place. A weak connection loses
//! datagrams exactly when the person is trying to get something done.
//!
//! So every event gets a number, and each datagram carries all the events the
//! other side has not confirmed yet ([`ControlMessage::InputBatch`]). The
//! other side applies those newer than the last one it applied and answers
//! with that number ([`ControlMessage::InputAck`]). Unconfirmed events are
//! sent again with the next event, and by a timer when no next event comes.
//! An event needs one datagram out of several to arrive, instead of exactly
//! one.
//!
//! Pointer moves are the exception to "every event": only the newest one
//! matters, so an unconfirmed move is replaced by the next move rather than
//! queued behind it.
//!
//! This module only decides what to send; the caller owns the link.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use imperio_protocol::{ControlMessage, InputEvent};

/// Events per datagram: keeps a batch far below one datagram's size.
const MAX_BATCH: usize = 48;
/// Unconfirmed events kept. More than this means the link has been dead for
/// a long while; the oldest are given up.
const MAX_WINDOW: usize = 512;

struct SenderState {
    /// The host said it understands batches. Until then events go out the
    /// old way, one unnumbered datagram each, which every version accepts.
    batching: bool,
    next_seq: u32,
    /// Sent but not confirmed, oldest first.
    window: VecDeque<(u32, InputEvent)>,
    last_send: Option<Instant>,
}

/// The viewer's side.
pub struct InputSender {
    state: Mutex<SenderState>,
}

impl Default for InputSender {
    fn default() -> Self {
        Self::new()
    }
}

impl InputSender {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(SenderState { batching: false, next_seq: 1, window: VecDeque::new(), last_send: None }),
        }
    }

    /// The other computer announced that it understands batches.
    pub fn enable(&self) {
        self.state.lock().unwrap().batching = true;
    }

    pub fn batching(&self) -> bool {
        self.state.lock().unwrap().batching
    }

    /// Events sent and not yet confirmed.
    pub fn unconfirmed(&self) -> usize {
        self.state.lock().unwrap().window.len()
    }

    fn batch(st: &SenderState) -> ControlMessage {
        ControlMessage::InputBatch { events: st.window.iter().take(MAX_BATCH).copied().collect() }
    }

    /// A new event. Returns the message to send now.
    pub fn push(&self, ev: InputEvent, now: Instant) -> ControlMessage {
        let mut st = self.state.lock().unwrap();
        if !st.batching {
            return ControlMessage::Input(ev);
        }
        if matches!(ev, InputEvent::MouseMove { .. }) && matches!(st.window.back(), Some((_, InputEvent::MouseMove { .. }))) {
            st.window.pop_back();
        }
        let seq = st.next_seq;
        st.next_seq = st.next_seq.wrapping_add(1);
        st.window.push_back((seq, ev));
        while st.window.len() > MAX_WINDOW {
            st.window.pop_front();
        }
        st.last_send = Some(now);
        Self::batch(&st)
    }

    /// The other computer has applied everything up to `seq`.
    pub fn on_ack(&self, seq: u32) {
        let mut st = self.state.lock().unwrap();
        while st.window.front().is_some_and(|(s, _)| *s <= seq) {
            st.window.pop_front();
        }
    }

    /// Call every few milliseconds. Returns a batch to send again when
    /// something has stayed unconfirmed for longer than a round trip.
    pub fn tick(&self, now: Instant, rtt: Duration) -> Option<ControlMessage> {
        let mut st = self.state.lock().unwrap();
        if st.window.is_empty() {
            return None;
        }
        let wait = (rtt + rtt / 2).clamp(Duration::from_millis(20), Duration::from_millis(250));
        if st.last_send.is_some_and(|t| now.duration_since(t) < wait) {
            return None;
        }
        st.last_send = Some(now);
        Some(Self::batch(&st))
    }
}

/// The host's side: which events of a batch are new.
#[derive(Default)]
pub struct InputReceiver {
    /// Number of the last event applied (0 = none yet; numbering starts at 1).
    last: u32,
}

impl InputReceiver {
    /// Returns the events to apply, in order, and the number to confirm.
    pub fn accept(&mut self, mut events: Vec<(u32, InputEvent)>) -> (Vec<InputEvent>, u32) {
        events.sort_by_key(|(seq, _)| *seq);
        let mut fresh = Vec::new();
        for (seq, ev) in events {
            if seq > self.last {
                self.last = seq;
                fresh.push(ev);
            }
        }
        (fresh, self.last)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(scancode: u16, down: bool) -> InputEvent {
        InputEvent::Key { scancode, extended: false, down, unicode: None }
    }

    fn mv(n: u16) -> InputEvent {
        InputEvent::MouseMove { nx: n, ny: n }
    }

    /// xorshift: the same "random" loss on every run.
    struct Rng(u64);
    impl Rng {
        fn lost(&mut self, p: f64) -> bool {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            ((self.0.wrapping_mul(0x2545F4914F6CDD1D) >> 11) as f64 / (1u64 << 53) as f64) < p
        }
    }

    #[test]
    fn before_the_host_announces_batches_events_go_the_old_way() {
        let s = InputSender::new();
        assert_eq!(s.push(key(30, true), Instant::now()), ControlMessage::Input(key(30, true)));
        assert_eq!(s.unconfirmed(), 0);
    }

    #[test]
    fn typing_arrives_complete_and_in_order_through_heavy_loss() {
        // 30 % of datagrams lost in each direction.
        let (s, mut r, mut rng) = (InputSender::new(), InputReceiver::default(), Rng(0x9E3779B97F4A7C15));
        s.enable();
        let mut applied = Vec::new();
        let mut made = Vec::new();
        let mut now = Instant::now();
        let rtt = Duration::from_millis(40);
        let deliver = |msg: ControlMessage, r: &mut InputReceiver, rng: &mut Rng, applied: &mut Vec<InputEvent>| {
            if rng.lost(0.3) {
                return;
            }
            let ControlMessage::InputBatch { events } = msg else { panic!("expected a batch") };
            let (fresh, ack) = r.accept(events);
            applied.extend(fresh);
            if !rng.lost(0.3) {
                s.on_ack(ack);
            }
        };
        for i in 0..400u16 {
            for down in [true, false] {
                let ev = key(i % 80 + 1, down);
                made.push(ev);
                deliver(s.push(ev, now), &mut r, &mut rng, &mut applied);
                now += Duration::from_millis(7);
                if let Some(again) = s.tick(now, rtt) {
                    deliver(again, &mut r, &mut rng, &mut applied);
                }
            }
        }
        // The person stops typing: the timer alone must finish the job.
        for _ in 0..200 {
            now += Duration::from_millis(10);
            if let Some(again) = s.tick(now, rtt) {
                deliver(again, &mut r, &mut rng, &mut applied);
            }
        }
        assert_eq!(applied, made);
        assert_eq!(s.unconfirmed(), 0);
    }

    #[test]
    fn an_unconfirmed_move_is_replaced_by_the_next_but_stays_in_front_of_the_click() {
        let s = InputSender::new();
        s.enable();
        let now = Instant::now();
        s.push(mv(1), now);
        s.push(mv(2), now);
        s.push(InputEvent::MouseButton { button: imperio_protocol::MouseButton::Left, down: true }, now);
        let ControlMessage::InputBatch { events } = s.push(mv(3), now) else { panic!() };
        let kinds: Vec<_> = events.iter().map(|(_, e)| *e).collect();
        assert_eq!(
            kinds,
            vec![mv(2), InputEvent::MouseButton { button: imperio_protocol::MouseButton::Left, down: true }, mv(3)]
        );
        // The receiver applies them in that order even though a number is missing.
        let (fresh, ack) = InputReceiver::default().accept(events);
        assert_eq!(fresh.len(), 3);
        assert_eq!(ack, 4);
    }

    #[test]
    fn a_repeated_or_reordered_batch_applies_nothing_twice() {
        let mut r = InputReceiver::default();
        let (a, _) = r.accept(vec![(1, key(30, true)), (2, key(30, false))]);
        assert_eq!(a.len(), 2);
        let (b, ack) = r.accept(vec![(1, key(30, true)), (2, key(30, false))]);
        assert!(b.is_empty());
        assert_eq!(ack, 2);
        let (c, ack) = r.accept(vec![(2, key(30, false)), (3, key(31, true))]);
        assert_eq!(c, vec![key(31, true)]);
        assert_eq!(ack, 3);
    }

    #[test]
    fn nothing_is_resent_once_everything_is_confirmed() {
        let s = InputSender::new();
        s.enable();
        let now = Instant::now();
        s.push(key(30, true), now);
        assert!(s.tick(now + Duration::from_millis(5), Duration::from_millis(40)).is_none(), "too early");
        assert!(s.tick(now + Duration::from_millis(100), Duration::from_millis(40)).is_some());
        s.on_ack(1);
        assert!(s.tick(now + Duration::from_secs(1), Duration::from_millis(40)).is_none());
    }
}
