//! Clipboard text framing.
//!
//! Control messages travel one per datagram, so a clipboard text larger than
//! a datagram has to be cut into pieces. Small texts still go out as a single
//! [`ControlMessage::Clipboard`] (which older peers understand); larger ones
//! become a run of [`ControlMessage::ClipboardChunk`]s that the receiver puts
//! back together with a [`Reassembler`].

use imperio_protocol::{ControlMessage, MAX_CLIPBOARD_BYTES};

/// Text bytes per datagram; keeps the sealed datagram well under the MTU.
pub const CHUNK_BYTES: usize = 1000;

/// Most pieces a clipboard text may be cut into.
pub const MAX_CHUNKS: usize = MAX_CLIPBOARD_BYTES / CHUNK_BYTES + 1;

/// The control messages that carry `text`. `id` distinguishes this clipboard
/// update from the previous one on the receiving side.
pub fn messages(id: u32, text: String) -> Vec<ControlMessage> {
    if text.len() <= CHUNK_BYTES {
        return vec![ControlMessage::Clipboard(text)];
    }
    let bytes = text.as_bytes();
    let count = bytes.len().div_ceil(CHUNK_BYTES);
    if count > MAX_CHUNKS {
        return Vec::new();
    }
    bytes
        .chunks(CHUNK_BYTES)
        .enumerate()
        .map(|(i, part)| ControlMessage::ClipboardChunk {
            id,
            index: i as u16,
            count: count as u16,
            data: part.to_vec(),
        })
        .collect()
}

/// Send `text` to the peer as one or more control messages. Large texts are
/// lightly paced so a burst of datagrams does not overflow a socket buffer.
pub fn send(link: &crate::link::SecureLink, id: u32, text: String) {
    for (i, msg) in messages(id, text).iter().enumerate() {
        let _ = link.send_control(msg);
        if i % 16 == 15 {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
}

/// Collects the pieces of one clipboard update. A piece with a new `id`
/// abandons whatever was in progress.
#[derive(Default)]
pub struct Reassembler {
    id: u32,
    parts: Vec<Option<Vec<u8>>>,
    received: usize,
}

impl Reassembler {
    /// Add one piece; returns the full text once every piece has arrived.
    pub fn push(&mut self, id: u32, index: u16, count: u16, data: Vec<u8>) -> Option<String> {
        let count = count as usize;
        if count == 0 || count > MAX_CHUNKS || data.len() > CHUNK_BYTES {
            return None;
        }
        if self.parts.len() != count || self.id != id {
            self.id = id;
            self.parts = vec![None; count];
            self.received = 0;
        }
        let slot = self.parts.get_mut(index as usize)?;
        if slot.is_none() {
            *slot = Some(data);
            self.received += 1;
        }
        if self.received < count {
            return None;
        }
        let bytes: Vec<u8> = std::mem::take(&mut self.parts).into_iter().flatten().flatten().collect();
        self.received = 0;
        String::from_utf8(bytes).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rebuild(msgs: Vec<ControlMessage>) -> Option<String> {
        let mut r = Reassembler::default();
        let mut out = None;
        for m in msgs {
            if let ControlMessage::ClipboardChunk { id, index, count, data } = m {
                out = r.push(id, index, count, data);
            }
        }
        out
    }

    #[test]
    fn small_text_is_a_single_legacy_message() {
        assert_eq!(messages(1, "hi".into()), vec![ControlMessage::Clipboard("hi".into())]);
    }

    #[test]
    fn large_text_roundtrips_even_out_of_order() {
        // Multi-byte characters straddle chunk boundaries on purpose.
        let text = "héllo wörld ✓ ".repeat(700);
        let mut msgs = messages(7, text.clone());
        assert!(msgs.len() > 1);
        msgs.reverse();
        assert_eq!(rebuild(msgs).as_deref(), Some(text.as_str()));
    }

    #[test]
    fn oversized_text_is_not_sent() {
        assert!(messages(1, "x".repeat(MAX_CLIPBOARD_BYTES + CHUNK_BYTES * 2)).is_empty());
    }

    #[test]
    fn hostile_chunks_are_ignored() {
        let mut r = Reassembler::default();
        assert!(r.push(1, 0, 0, vec![]).is_none());
        assert!(r.push(1, 0, u16::MAX, vec![1]).is_none());
        assert!(r.push(1, 5, 2, vec![1]).is_none());
        assert!(r.push(1, 0, 2, vec![0; CHUNK_BYTES + 1]).is_none());
        // Invalid UTF-8 never reaches the clipboard.
        assert!(r.push(2, 0, 1, vec![0xff, 0xfe]).is_none());
    }

    #[test]
    fn a_new_id_abandons_the_old_update() {
        let mut r = Reassembler::default();
        assert!(r.push(1, 0, 2, b"old".to_vec()).is_none());
        assert!(r.push(2, 0, 2, b"new-".to_vec()).is_none());
        assert_eq!(r.push(2, 1, 2, b"text".to_vec()).as_deref(), Some("new-text"));
    }
}
