//! Session key agreement and datagram sealing.
//!
//! Pairing: the host shows a 6-digit code; the viewer types it. Both run
//! SPAKE2 (symmetric mode) with the code as password and
//! `imperio:<device_id>:<session>` as identity, so an attacker who sees the
//! handshake learns nothing about the code and gets one guess per attempt.
//! The SPAKE2 output is expanded with HKDF-SHA256 into one ChaCha20-Poly1305
//! key per direction. Relays and the signal server only ever see ciphertext.

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};

pub const TAG_LEN: usize = 16;

/// One side of the SPAKE2 exchange.
pub struct Handshake {
    state: Spake2<Ed25519Group>,
    pub outbound: Vec<u8>,
}

#[derive(Clone)]
pub struct SessionKeys {
    pub send: [u8; 32],
    pub recv: [u8; 32],
}

impl Handshake {
    /// `session` is the random 64-bit id agreed via the server (or chosen by
    /// the viewer in direct mode); it binds the exchange to this connection.
    pub fn new(code: &str, session: u64) -> Self {
        let identity = format!("imperio:{session}");
        let (state, outbound) = Spake2::<Ed25519Group>::start_symmetric(
            &Password::new(code.as_bytes()),
            &Identity::new(identity.as_bytes()),
        );
        Self { state, outbound }
    }

    /// Finish with the peer's message. `is_host` picks which derived key is
    /// used for sending; the viewer uses the opposite assignment.
    pub fn finish(self, inbound: &[u8], is_host: bool) -> anyhow::Result<SessionKeys> {
        let shared = self
            .state
            .finish(inbound)
            .map_err(|e| anyhow::anyhow!("SPAKE2 failed: {e:?}"))?;
        let hk = Hkdf::<Sha256>::new(Some(b"imperio-v1"), &shared);
        let mut h2v = [0u8; 32];
        let mut v2h = [0u8; 32];
        hk.expand(b"host->viewer", &mut h2v).expect("hkdf");
        hk.expand(b"viewer->host", &mut v2h).expect("hkdf");
        Ok(if is_host {
            SessionKeys {
                send: h2v,
                recv: v2h,
            }
        } else {
            SessionKeys {
                send: v2h,
                recv: h2v,
            }
        })
    }
}

/// AEAD with a 64-bit counter nonce. Keys are per direction, so counters
/// never collide between the two sides.
pub struct Cipher {
    inner: ChaCha20Poly1305,
}

impl Cipher {
    pub fn new(key: &[u8; 32]) -> Self {
        Self {
            inner: ChaCha20Poly1305::new(Key::from_slice(key)),
        }
    }

    fn nonce(counter: u64) -> Nonce {
        let mut n = [0u8; 12];
        n[4..].copy_from_slice(&counter.to_be_bytes());
        Nonce::from(n)
    }

    pub fn seal(&self, counter: u64, plaintext: &[u8]) -> Vec<u8> {
        self.inner
            .encrypt(&Self::nonce(counter), plaintext)
            .expect("chacha20poly1305 encrypt")
    }

    pub fn open(&self, counter: u64, ciphertext: &[u8]) -> Option<Vec<u8>> {
        self.inner.decrypt(&Self::nonce(counter), ciphertext).ok()
    }
}

/// Anti-replay window over nonce counters (RFC 6479 style, 1024 entries).
pub struct ReplayWindow {
    highest: u64,
    bits: [u64; 16],
    seen_any: bool,
}

impl Default for ReplayWindow {
    fn default() -> Self {
        Self {
            highest: 0,
            bits: [0; 16],
            seen_any: false,
        }
    }
}

impl ReplayWindow {
    const SIZE: u64 = 1024;

    /// Returns true if `n` is fresh and records it.
    pub fn check_and_set(&mut self, n: u64) -> bool {
        if !self.seen_any {
            self.seen_any = true;
            self.highest = n;
            self.set(n);
            return true;
        }
        if n > self.highest {
            let advance = n - self.highest;
            if advance >= Self::SIZE {
                self.bits = [0; 16];
            } else {
                for i in 1..=advance {
                    self.clear(self.highest + i);
                }
            }
            self.highest = n;
            self.set(n);
            return true;
        }
        if self.highest - n >= Self::SIZE {
            return false; // too old
        }
        if self.get(n) {
            return false; // replay
        }
        self.set(n);
        true
    }

    fn idx(n: u64) -> (usize, u64) {
        let bit = n % Self::SIZE;
        ((bit / 64) as usize, 1u64 << (bit % 64))
    }
    fn get(&self, n: u64) -> bool {
        let (w, m) = Self::idx(n);
        self.bits[w] & m != 0
    }
    fn set(&mut self, n: u64) {
        let (w, m) = Self::idx(n);
        self.bits[w] |= m;
    }
    fn clear(&mut self, n: u64) {
        let (w, m) = Self::idx(n);
        self.bits[w] &= !m;
    }
}

/// Random 6-digit pairing code, zero-padded.
pub fn random_code() -> String {
    use rand::Rng;
    format!("{:06}", rand::thread_rng().gen_range(0..1_000_000u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spake_both_sides_agree() {
        let a = Handshake::new("123456", 42);
        let b = Handshake::new("123456", 42);
        let (ma, mb) = (a.outbound.clone(), b.outbound.clone());
        let ka = a.finish(&mb, true).unwrap();
        let kb = b.finish(&ma, false).unwrap();
        assert_eq!(ka.send, kb.recv);
        assert_eq!(ka.recv, kb.send);
        assert_ne!(ka.send, ka.recv);
    }

    #[test]
    fn wrong_code_yields_different_keys() {
        let a = Handshake::new("123456", 1);
        let b = Handshake::new("654321", 1);
        let (ma, mb) = (a.outbound.clone(), b.outbound.clone());
        let ka = a.finish(&mb, true).unwrap();
        let kb = b.finish(&ma, false).unwrap();
        assert_ne!(ka.send, kb.recv);
        // and ciphertext from one does not open with the other
        let c = Cipher::new(&ka.send).seal(1, b"hello");
        assert!(Cipher::new(&kb.recv).open(1, &c).is_none());
    }

    #[test]
    fn seal_open_roundtrip_and_tamper() {
        let c = Cipher::new(&[7u8; 32]);
        let ct = c.seal(5, b"payload");
        assert_eq!(c.open(5, &ct).unwrap(), b"payload");
        assert!(c.open(6, &ct).is_none());
        let mut bad = ct.clone();
        bad[0] ^= 1;
        assert!(c.open(5, &bad).is_none());
    }

    #[test]
    fn replay_window() {
        let mut w = ReplayWindow::default();
        assert!(w.check_and_set(10));
        assert!(!w.check_and_set(10));
        assert!(w.check_and_set(8));
        assert!(!w.check_and_set(8));
        assert!(w.check_and_set(5000));
        assert!(!w.check_and_set(10)); // too old now
        assert!(w.check_and_set(4999));
    }
}
