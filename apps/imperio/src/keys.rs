//! Offline Ed25519 signing shared by the updater and the licence system.
//!
//! The matching private seed lives only with the project owner (see
//! `imperio keygen`). Update manifests and licence keys are signed with it;
//! the app verifies them against the embedded public key, so neither a
//! tampered download nor a forged licence key is accepted.

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

/// The project's release / licensing public key (base64). Generated once with
/// `imperio keygen`; replace only if the private key is lost.
pub const PUBLIC_KEY_B64: &str = "2CcJzbAMI2/xPfmbS8z/6Okqyl/7Rr74zP4e6KuuW6c=";

fn public_key() -> Option<VerifyingKey> {
    let bytes = STANDARD.decode(PUBLIC_KEY_B64).ok()?;
    let arr: [u8; 32] = bytes.try_into().ok()?;
    VerifyingKey::from_bytes(&arr).ok()
}

/// Verify `sig` (base64, standard or URL-safe) over `msg` with the embedded key.
pub fn verify(msg: &[u8], sig_b64: &str) -> bool {
    let Some(vk) = public_key() else {
        return false;
    };
    let bytes = STANDARD
        .decode(sig_b64)
        .or_else(|_| URL_SAFE_NO_PAD.decode(sig_b64));
    let Ok(bytes) = bytes else {
        return false;
    };
    let Ok(arr): Result<[u8; 64], _> = bytes.try_into() else {
        return false;
    };
    vk.verify(msg, &Signature::from_bytes(&arr)).is_ok()
}

/// Generate a fresh keypair: (private seed base64, public key base64).
pub fn keygen() -> (String, String) {
    use rand::RngCore;
    let mut seed = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut seed);
    let sk = SigningKey::from_bytes(&seed);
    (STANDARD.encode(seed), STANDARD.encode(sk.verifying_key().to_bytes()))
}

/// Sign `msg` with a private seed (base64). Returns the signature as
/// URL-safe base64 without padding (safe inside licence keys and JSON).
pub fn sign(seed_b64: &str, msg: &[u8]) -> anyhow::Result<String> {
    let bytes = STANDARD.decode(seed_b64.trim())?;
    let seed: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("private key must be 32 bytes"))?;
    let sk = SigningKey::from_bytes(&seed);
    Ok(URL_SAFE_NO_PAD.encode(sk.sign(msg).to_bytes()))
}
