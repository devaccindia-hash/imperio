//! RFC 6238 TOTP, for an optional second factor on unattended access.
//!
//! When a host configures a TOTP secret, the pairing material becomes
//! `password + current_code`, so knowing the stored password alone is not
//! enough to connect. The host accepts the current 30-second window and its
//! two neighbours to tolerate clock skew.

use hmac::{Hmac, Mac};
use sha1::Sha1;

const STEP: u64 = 30;
const DIGITS: u32 = 6;

/// Decode a base32 (RFC 4648, no padding required) secret as used by
/// authenticator apps.
pub fn decode_base32(s: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut bits = 0u32;
    let mut nbits = 0;
    let mut out = Vec::new();
    for c in s.trim().bytes().filter(|&c| c != b'=' && c != b' ') {
        let up = c.to_ascii_uppercase();
        let v = ALPHABET.iter().position(|&a| a == up)? as u32;
        bits = (bits << 5) | v;
        nbits += 5;
        if nbits >= 8 {
            nbits -= 8;
            out.push((bits >> nbits) as u8);
        }
    }
    Some(out)
}

fn hotp(secret: &[u8], counter: u64) -> u32 {
    let mut mac = <Hmac<Sha1>>::new_from_slice(secret).expect("hmac key");
    mac.update(&counter.to_be_bytes());
    let hs = mac.finalize().into_bytes();
    let offset = (hs[hs.len() - 1] & 0x0f) as usize;
    let bin = ((hs[offset] as u32 & 0x7f) << 24)
        | ((hs[offset + 1] as u32) << 16)
        | ((hs[offset + 2] as u32) << 8)
        | (hs[offset + 3] as u32);
    bin % 10u32.pow(DIGITS)
}

/// The code for a given UNIX time.
pub fn totp_at(secret: &[u8], unix_secs: u64) -> String {
    format!("{:0width$}", hotp(secret, unix_secs / STEP), width = DIGITS as usize)
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Accept the current window and ±1 (constant-time compare per candidate).
pub fn check(secret: &[u8], code: &str, unix_secs: u64) -> bool {
    let code = code.trim();
    let counter = unix_secs / STEP;
    for c in [counter.wrapping_sub(1), counter, counter + 1] {
        let expect = format!("{:0width$}", hotp(secret, c), width = DIGITS as usize);
        if ct_eq(expect.as_bytes(), code.as_bytes()) {
            return true;
        }
    }
    false
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc6238_sha1_vector() {
        // RFC 6238 test secret "12345678901234567890" at T=59 -> 94287082.
        let secret = b"12345678901234567890";
        assert_eq!(totp_at(secret, 59), "287082");
        assert_eq!(totp_at(secret, 1111111109), "081804");
    }

    #[test]
    fn window_tolerance() {
        let secret = b"12345678901234567890";
        let t = 37_037_037u64 * 30; // start of a 30s step
        let code = totp_at(secret, t);
        assert!(check(secret, &code, t)); // same step
        assert!(check(secret, &code, t + 20)); // same step
        assert!(check(secret, &code, t + 40)); // next step, within +1
        assert!(check(secret, &code, t.wrapping_sub(5))); // previous step, within -1
        assert!(!check(secret, &code, t + 95)); // +3 steps: too far
        assert!(!check(secret, "000000", t) || code == "000000");
    }

    #[test]
    fn base32_decodes() {
        // "JBSWY3DPEHPK3PXP" = "Hello!\xde\xad\xbe\xef"-ish; just check length/bytes.
        assert_eq!(decode_base32("JBSWY3DP").unwrap(), b"Hello");
    }
}
