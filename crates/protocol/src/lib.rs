//! Wire protocol shared by host and viewer.
//!
//! Everything that crosses the network is defined here so that the host,
//! viewer and bench tool cannot drift apart. Outer datagram kinds and the
//! signaling / handshake messages live in [`signal`]; the encrypted session
//! layer in `imperio-transport` carries [`ControlMessage`]s and video chunks.

pub mod device_id;
pub mod signal;

use serde::{Deserialize, Serialize};

/// Protocol version negotiated at session start. Bump on any breaking change.
pub const PROTOCOL_VERSION: u16 = 3;

/// Clipboard payloads above this are not synced (plain text only for now).
pub const MAX_CLIPBOARD_BYTES: usize = 256 * 1024;

/// Fixed pairing secret used in "open" mode (no code). The session is still
/// encrypted, but anyone who can reach the host on the network can connect, so
/// open mode is only safe on a trusted private network (a Tailscale tailnet or
/// home LAN). Both host (`--open`) and a viewer given no `--code` use this.
pub const OPEN_CODE: &str = "imperio-open-mode-v1";

/// Default UDP port the host listens on.
pub const DEFAULT_PORT: u16 = 7700;

/// Default UDP port of the rendezvous server.
pub const DEFAULT_SIGNAL_PORT: u16 = 7800;

/// Viewer <-> host control traffic inside the encrypted session. Small and
/// infrequent; latency matters more than size so each message is its own
/// datagram.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ControlMessage {
    /// Viewer announces itself after the handshake; host starts streaming.
    Hello {
        version: u16,
    },
    /// Host answers with the stream geometry so the viewer can size its
    /// window, plus which of how many displays is being streamed.
    StreamInfo {
        width: u32,
        height: u32,
        codec: VideoCodec,
        monitor: u8,
        monitors: u8,
    },
    /// Host proves its persistent identity: Ed25519 public key (32 bytes) plus
    /// a signature (64 bytes) over the session channel binding. Sent once,
    /// right after the link is established, so the viewer can pin it
    /// (trust-on-first-use).
    Identity {
        device_id: u32,
        public: Vec<u8>,
        sig: Vec<u8>,
    },
    /// Viewer asks for another display; host answers with a new StreamInfo.
    SelectMonitor(u8),
    /// Clipboard text changed on the sending side (either direction).
    Clipboard(String),
    /// Second factor: viewer sends the current TOTP code after Hello when the
    /// host requires one. Verified inside the encrypted channel.
    Totp(String),
    /// Viewer lost a frame and needs a keyframe to resync.
    RequestKeyframe,
    Input(InputEvent),
    /// RTT probe; the receiver echoes `t_us` back in `Pong`.
    Ping {
        t_us: u64,
    },
    Pong {
        t_us: u64,
    },
    /// Either side is leaving.
    Bye,
}

impl ControlMessage {
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("control message serialises")
    }

    pub fn decode(body: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(body)
    }
}

/// Video codec carried in a media stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VideoCodec {
    H264,
    Hevc,
    Vp9,
    Av1,
}

/// Pixel format of a raw captured frame before encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 8-bit BGRA, what DXGI Desktop Duplication hands back by default.
    Bgra8,
    /// 4:2:0 planar, the input most hardware encoders want.
    Nv12,
}

/// Mouse / keyboard events sent from viewer to host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputEvent {
    /// Position normalised to 0..=65535 across the host's stream rectangle,
    /// so the viewer never needs to know the host's pixel geometry.
    MouseMove {
        nx: u16,
        ny: u16,
    },
    MouseButton {
        button: MouseButton,
        down: bool,
    },
    /// Vertical / horizontal wheel delta in WHEEL_DELTA (120) units.
    MouseWheel {
        dx: i32,
        dy: i32,
    },
    /// Hardware scan code (Set 1), plus a Unicode fallback for keys the host
    /// layout cannot map.
    Key {
        scancode: u16,
        extended: bool,
        down: bool,
        unicode: Option<char>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_roundtrip() {
        let msg = ControlMessage::Input(InputEvent::Key {
            scancode: 0x1E,
            extended: false,
            down: true,
            unicode: Some('a'),
        });
        let bytes = msg.encode();
        assert_eq!(ControlMessage::decode(&bytes).unwrap(), msg);
    }
}
