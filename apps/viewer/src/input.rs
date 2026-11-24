//! The way keyboard and mouse events leave the session window.
//!
//! Everything that types or clicks on the other computer goes through here
//! (the window's own events, the paste that follows a file upload, the Esc
//! that calls off a drag), for two reasons:
//!
//! * all of it is numbered and repeated until confirmed, see
//!   `imperio_transport::input`;
//! * one place knows which keys and buttons are held down over there, so
//!   they can be let go when this window stops receiving the keyboard.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use imperio_protocol::{InputEvent, MouseButton, FEATURE_INPUT_BATCH};
use imperio_transport::input::InputSender;
use imperio_transport::link::SecureLink;

/// Keys (scancode, extended) and mouse buttons held down as far as the other
/// computer knows.
#[derive(Default)]
struct Held {
    keys: Vec<(u16, bool)>,
    buttons: Vec<MouseButton>,
}

pub struct InputLine {
    link: Arc<SecureLink>,
    sender: InputSender,
    held: Mutex<Held>,
    /// False in a view-only session: nothing is sent.
    enabled: bool,
}

impl InputLine {
    pub fn new(link: Arc<SecureLink>, enabled: bool) -> Arc<Self> {
        Arc::new(Self { link, sender: InputSender::new(), held: Mutex::new(Held::default()), enabled })
    }

    pub fn send(&self, ev: InputEvent) {
        if !self.enabled {
            return;
        }
        {
            let mut held = self.held.lock().unwrap();
            match ev {
                InputEvent::Key { scancode, extended, down, .. } if scancode != 0 => {
                    held.keys.retain(|k| *k != (scancode, extended));
                    if down {
                        held.keys.push((scancode, extended));
                    }
                }
                InputEvent::MouseButton { button, down } => {
                    held.buttons.retain(|b| *b != button);
                    if down {
                        held.buttons.push(button);
                    }
                }
                _ => {}
            }
        }
        let msg = self.sender.push(ev, Instant::now());
        if let Err(e) = self.link.send_control(&msg) {
            tracing::debug!(error = %e, "input send failed");
        }
    }

    /// Whether the other computer has this key down (as far as we sent).
    pub fn key_held(&self, scancode: u16, extended: bool) -> bool {
        self.held.lock().unwrap().keys.contains(&(scancode, extended))
    }

    /// Let go, on the other computer, of every key and button still held
    /// there. Returns how many there were.
    pub fn release_held(&self) -> usize {
        let held = std::mem::take(&mut *self.held.lock().unwrap());
        let count = held.keys.len() + held.buttons.len();
        for (scancode, extended) in held.keys {
            self.send(InputEvent::Key { scancode, extended, down: false, unicode: None });
        }
        for button in held.buttons {
            self.send(InputEvent::MouseButton { button, down: false });
        }
        count
    }

    /// The other computer said what it understands.
    pub fn features(&self, bits: u32) {
        if bits & FEATURE_INPUT_BATCH != 0 && !self.sender.batching() {
            tracing::info!("input: the other computer confirms what it receives; lost events are sent again");
            self.sender.enable();
        }
    }

    pub fn confirmed(&self, seq: u32) {
        self.sender.on_ack(seq);
    }

    /// Called from the network thread every pass: sends unconfirmed events
    /// again once they are overdue.
    pub fn tick(&self, now: Instant) {
        if let Some(msg) = self.sender.tick(now, self.link.rtt()) {
            let _ = self.link.send_control(&msg);
        }
    }
}
