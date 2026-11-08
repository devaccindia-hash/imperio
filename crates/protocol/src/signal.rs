//! Rendezvous (signaling) protocol between clients and the imperio-cloud
//! signal server. Carries no secrets: the server learns device IDs and
//! addresses, never session keys or media.

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

/// Nine-digit device identifier shown to users (like a phone number).
pub type DeviceId = u32;

/// Random per-connection identifier agreed via the server.
pub type SessionId = u64;

/// Outer datagram kinds. Byte 0 of every datagram on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// Plaintext SPAKE2 handshake (`Handshake`).
    Handshake = 0,
    /// ChaCha20-Poly1305 sealed session traffic (`imperio_transport::link`).
    Encrypted = 1,
    /// Client <-> server signaling (`SignalMessage`).
    Signal = 2,
    /// Peer <-> peer hole-punch probe (`Punch`).
    Punch = 3,
}

impl Kind {
    pub fn from_u8(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Handshake),
            1 => Some(Self::Encrypted),
            2 => Some(Self::Signal),
            3 => Some(Self::Punch),
            _ => None,
        }
    }
}

/// Domain-separated bytes a device signs with its Ed25519 identity key to
/// prove key ownership when requesting a fresh server-assigned ID. Binding the
/// ID to the key is what stops a guessed ID from being claimed by anyone else.
pub fn new_device_context(public_key: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(16 + public_key.len());
    v.extend_from_slice(b"imperio-newdev\0");
    v.extend_from_slice(public_key);
    v
}

/// Domain-separated bytes a device signs to prove the binding of an existing
/// `device_id` to its `public_key` on re-registration. The server refuses a
/// `Register` whose key does not match the one the ID was first bound to, so a
/// device ID cannot be hijacked.
pub fn register_context(device_id: DeviceId, public_key: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(20 + public_key.len());
    v.extend_from_slice(b"imperio-register\0");
    v.extend_from_slice(&device_id.to_be_bytes());
    v.extend_from_slice(public_key);
    v
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignalMessage {
    // --- host -> server ---
    /// Ask the server to assign a fresh, non-enumerable device ID. `sig` is
    /// over [`new_device_context`], proving the sender holds the private key
    /// for `public_key`. The server binds the new ID to this key.
    RegisterNew {
        public_key: Vec<u8>,
        sig: Vec<u8>,
    },
    /// Sent every few seconds for an already-assigned ID; the server records
    /// the public address it sees and the local candidates for same-LAN
    /// shortcuts. `sig` is over [`register_context`]; the server checks it
    /// against the key the ID was bound to.
    Register {
        device_id: DeviceId,
        public_key: Vec<u8>,
        sig: Vec<u8>,
        locals: Vec<SocketAddr>,
    },
    // --- server -> host ---
    /// Reply to [`RegisterNew`] with the assigned ID.
    Assigned {
        device_id: DeviceId,
    },
    /// Registration refused (bad signature, or ID bound to a different key).
    Rejected {
        reason: String,
    },
    Registered {
        public: SocketAddr,
    },
    /// A viewer wants to connect. `token` authorises relay use for this session.
    Incoming {
        session: SessionId,
        token: u64,
        peer_public: SocketAddr,
        peer_locals: Vec<SocketAddr>,
    },

    // --- viewer -> server ---
    Connect {
        target: DeviceId,
        locals: Vec<SocketAddr>,
    },
    // --- server -> viewer ---
    PeerInfo {
        session: SessionId,
        token: u64,
        peer_public: SocketAddr,
        peer_locals: Vec<SocketAddr>,
    },
    NotFound {
        target: DeviceId,
    },
    /// Too many requests from this source address; back off.
    RateLimited,

    // --- either -> server, when punching failed ---
    /// Bind this socket to the session's relay slot. The `token` is the
    /// server-issued value from `Incoming` / `PeerInfo`; the server forwards
    /// only between the two addresses it recorded for the session, and only
    /// when the token matches, so a third party cannot attach to a relay.
    RelayBind {
        session: SessionId,
        token: u64,
    },
    RelayReady {
        session: SessionId,
    },

    // --- abuse reporting ---
    /// Report a device or session for abuse (scam, unauthorized access). The
    /// server logs it; moderation is out of band.
    Report {
        reported_device: Option<DeviceId>,
        session: Option<SessionId>,
        reason: String,
    },
}

impl SignalMessage {
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_extend(self, vec![Kind::Signal as u8]).expect("signal message serialises")
    }

    /// `body` is the datagram after the kind byte.
    pub fn decode(body: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(body)
    }
}

/// Hole-punch probe. Both peers spray these at every candidate address;
/// the first `ack` that comes back fixes the peer address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Punch {
    pub session: SessionId,
    pub ack: bool,
}

impl Punch {
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_extend(self, vec![Kind::Punch as u8]).expect("punch serialises")
    }
    pub fn decode(body: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(body)
    }
}

/// SPAKE2 handshake message. Symmetric mode: each side sends one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handshake {
    pub session: SessionId,
    pub spake_msg: Vec<u8>,
}

impl Handshake {
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_extend(self, vec![Kind::Handshake as u8]).expect("handshake serialises")
    }
    pub fn decode(body: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_roundtrip() {
        let m = SignalMessage::Incoming {
            session: 7,
            token: 0xABCD,
            peer_public: "203.0.113.5:4321".parse().unwrap(),
            peer_locals: vec!["192.168.1.2:7700".parse().unwrap()],
        };
        let b = m.encode();
        assert_eq!(b[0], Kind::Signal as u8);
        assert_eq!(SignalMessage::decode(&b[1..]).unwrap(), m);
    }
}
