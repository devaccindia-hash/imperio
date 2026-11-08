//! Licence keys.
//!
//! Imperio is free for personal and occasional business use; a Pro licence is
//! a signed key the owner issues (`imperio license-sign --key <seed> --name
//! "Jane" --plan pro [--expires 2027-01-01] [--seats 3]`). The key is
//! verified offline against the embedded public key, so there is no licence
//! server, no phone-home, and no behavioural "commercial use" detection: the
//! user types the key once and it is stored in the config.
//!
//! Format: `IMP-<base64url(json)>.<base64url(signature)>`.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};

pub const KEY_PREFIX: &str = "IMP-";
pub const UPGRADE_URL: &str = "https://devaccindia-hash.github.io/imperio/#pro";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct License {
    pub name: String,
    /// "pro" today; room for more plans later.
    pub plan: String,
    /// `YYYY-MM-DD`, inclusive; `None` = perpetual.
    #[serde(default)]
    pub expires: Option<String>,
    #[serde(default)]
    pub seats: u32,
}

impl License {
    pub fn plan_label(&self) -> String {
        let mut s = match self.plan.as_str() {
            "pro" => "Pro".to_string(),
            other => other.to_string(),
        };
        if self.seats > 1 {
            s.push_str(&format!(" \u{00d7} {}", self.seats));
        }
        s
    }
}

/// Build a key for `lic` with the private seed (owner side).
pub fn encode(lic: &License, seed_b64: &str) -> anyhow::Result<String> {
    let payload = serde_json::to_vec(lic)?;
    let sig = crate::keys::sign(seed_b64, &payload)?;
    Ok(format!("{KEY_PREFIX}{}.{sig}", URL_SAFE_NO_PAD.encode(payload)))
}

/// Verify a key (signature + expiry). Whitespace is ignored.
pub fn verify_key(key: &str) -> Result<License, String> {
    let compact: String = key.chars().filter(|c| !c.is_whitespace()).collect();
    let body = compact
        .strip_prefix(KEY_PREFIX)
        .ok_or_else(|| "a licence key starts with IMP-".to_string())?;
    let (payload_b64, sig) = body
        .split_once('.')
        .ok_or_else(|| "this does not look like a complete licence key".to_string())?;
    let payload = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|_| "this licence key is damaged (payload)".to_string())?;
    if !crate::keys::verify(&payload, sig) {
        return Err("this licence key is not valid for Imperio".into());
    }
    let lic: License =
        serde_json::from_slice(&payload).map_err(|_| "this licence key is damaged (content)".to_string())?;
    if let Some(exp) = &lic.expires {
        if exp.as_str() < today().as_str() {
            return Err(format!("this licence expired on {exp}"));
        }
    }
    Ok(lic)
}

/// Today's date as `YYYY-MM-DD` (UTC), without a calendar crate.
fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn today_is_plausible() {
        let t = today();
        assert_eq!(t.len(), 10);
        assert!(t.starts_with("20"));
    }

    #[test]
    fn malformed_keys_are_rejected() {
        assert!(verify_key("").is_err());
        assert!(verify_key("IMP-abc").is_err());
        assert!(verify_key("IMP-abc.def").is_err());
    }

    #[test]
    fn round_trip_with_fresh_keypair() {
        // Signing works with any seed; verification against the embedded key
        // is covered by the app once a key is embedded.
        let (seed, _) = crate::keys::keygen();
        let lic = License { name: "Test".into(), plan: "pro".into(), expires: None, seats: 1 };
        let key = encode(&lic, &seed).unwrap();
        assert!(key.starts_with(KEY_PREFIX));
        assert!(key.contains('.'));
    }
}
