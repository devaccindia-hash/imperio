//! Two-way clipboard text sync.
//!
//! A polling thread watches the local clipboard; when the text changes (and
//! it is not text we just applied from the remote side) the callback fires so
//! the owner can send it over the link. `apply_remote` sets the local
//! clipboard and records the text so the poller does not echo it back.

use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Shared {
    /// Last text we observed or applied; compared against the clipboard.
    last: Option<String>,
    max_bytes: usize,
}

pub struct ClipboardSync {
    shared: Arc<Mutex<Shared>>,
}

impl ClipboardSync {
    /// Start watching. `on_change` runs on the poller thread.
    pub fn spawn(max_bytes: usize, on_change: impl Fn(String) + Send + 'static) -> Self {
        let shared = Arc::new(Mutex::new(Shared { last: None, max_bytes }));
        let poll = shared.clone();
        std::thread::Builder::new()
            .name("clipboard".into())
            .spawn(move || {
                let mut cb = match arboard::Clipboard::new() {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(error = %e, "clipboard unavailable; sync disabled");
                        return;
                    }
                };
                // Seed with current content so we do not send it on start.
                if let Ok(t) = cb.get_text() {
                    poll.lock().unwrap().last = Some(t);
                }
                loop {
                    std::thread::sleep(Duration::from_millis(300));
                    let Ok(text) = cb.get_text() else { continue };
                    let mut s = poll.lock().unwrap();
                    if text.len() > s.max_bytes || s.last.as_deref() == Some(text.as_str()) {
                        continue;
                    }
                    s.last = Some(text.clone());
                    drop(s);
                    on_change(text);
                }
            })
            .expect("clipboard thread");
        Self { shared }
    }

    /// Apply text received from the peer to the local clipboard.
    pub fn apply_remote(&self, text: String) {
        {
            let mut s = self.shared.lock().unwrap();
            if text.len() > s.max_bytes {
                return;
            }
            s.last = Some(text.clone());
        }
        if let Err(e) = arboard::Clipboard::new().and_then(|mut c| c.set_text(text)) {
            tracing::debug!(error = %e, "set clipboard failed");
        }
    }
}
