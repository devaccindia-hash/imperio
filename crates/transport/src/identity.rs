//! Persistent device identity and trust-on-first-use pinning.
//!
//! The pairing code authenticates *a* session, but a viewer that reconnects
//! wants to know it is reaching the *same* host as last time, not a different
//! machine that has since learned the code (or a relay impersonating the
//! host). Each host holds a long-term Ed25519 identity key. After the
//! encrypted link is up, the host signs a value derived from the session keys
//! (so the signature proves both key possession and liveness in *this*
//! session) and sends its public key. The viewer verifies the signature and
//! pins `device_id -> public key`; a later mismatch is surfaced loudly.
//!
//! This is the SSH `known_hosts` model.

use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey, Signature};
use sha2::{Digest, Sha256};

use crate::crypto::SessionKeys;

/// A host's long-term signing key.
pub struct DeviceKey(SigningKey);

impl DeviceKey {
    pub fn generate() -> Self {
        let mut seed = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut seed);
        Self(SigningKey::from_bytes(&seed))
    }

    pub fn from_seed_b64(s: &str) -> Option<Self> {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD.decode(s.trim()).ok()?;
        let seed: [u8; 32] = bytes.try_into().ok()?;
        Some(Self(SigningKey::from_bytes(&seed)))
    }

    pub fn seed_b64(&self) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(self.0.to_bytes())
    }

    pub fn public(&self) -> [u8; 32] {
        self.0.verifying_key().to_bytes()
    }

    /// Sign the session's channel binding; proves key possession in-session.
    pub fn sign_session(&self, keys: &SessionKeys) -> [u8; 64] {
        self.0.sign(&channel_binding(keys)).to_bytes()
    }

    /// Sign arbitrary bytes (e.g. a signal-server registration context).
    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.0.sign(msg).to_bytes()
    }
}

/// A value both peers derive identically from their (mirrored) session keys,
/// used as the message the host signs to prove liveness in this session.
pub fn channel_binding(keys: &SessionKeys) -> [u8; 32] {
    // send/recv are swapped between the two sides; sort so both agree.
    let (a, b) = if keys.send <= keys.recv { (keys.send, keys.recv) } else { (keys.recv, keys.send) };
    let mut h = Sha256::new();
    h.update(b"imperio-channel-binding-v1");
    h.update(a);
    h.update(b);
    h.finalize().into()
}

/// Verify a host's session signature against its claimed public key.
pub fn verify_session(public: &[u8; 32], keys: &SessionKeys, sig: &[u8; 64]) -> bool {
    let Ok(vk) = VerifyingKey::from_bytes(public) else { return false };
    let signature = Signature::from_bytes(sig);
    vk.verify(&channel_binding(keys), &signature).is_ok()
}

/// Short human-readable fingerprint of a public key (for logs / UX).
pub fn fingerprint(public: &[u8; 32]) -> String {
    let h = Sha256::digest(public);
    h[..8].iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::Handshake;

    fn session_pair() -> (SessionKeys, SessionKeys) {
        let a = Handshake::new("123456", 1);
        let b = Handshake::new("123456", 1);
        let (ma, mb) = (a.outbound.clone(), b.outbound.clone());
        (a.finish(&mb, true).unwrap(), b.finish(&ma, false).unwrap())
    }

    #[test]
    fn roundtrip_seed() {
        let k = DeviceKey::generate();
        let k2 = DeviceKey::from_seed_b64(&k.seed_b64()).unwrap();
        assert_eq!(k.public(), k2.public());
    }

    #[test]
    fn host_signature_verifies_for_viewer() {
        let (host_keys, viewer_keys) = session_pair();
        let dev = DeviceKey::generate();
        let sig = dev.sign_session(&host_keys);
        // Viewer verifies against its (mirrored) keys and the host's pubkey.
        assert!(verify_session(&dev.public(), &viewer_keys, &sig));
    }

    #[test]
    fn wrong_key_or_session_fails() {
        let (host_keys, viewer_keys) = session_pair();
        let dev = DeviceKey::generate();
        let other = DeviceKey::generate();
        let sig = dev.sign_session(&host_keys);
        assert!(!verify_session(&other.public(), &viewer_keys, &sig));
        // A signature from a different session must not verify here.
        let (other_session, _) = session_pair();
        let sig2 = dev.sign_session(&other_session);
        assert!(!verify_session(&dev.public(), &viewer_keys, &sig2));
    }
}
