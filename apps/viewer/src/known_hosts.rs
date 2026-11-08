//! Trust-on-first-use pinning of host identity keys.
//!
//! `%APPDATA%\Imperio\known_hosts`, one `device_id base64(pubkey)` per line.
//! First time we see a device we record its key; later we require a match.

use std::collections::HashMap;
use std::path::PathBuf;

use base64::Engine;

pub enum Trust {
    /// First time seeing this device; key recorded.
    New,
    /// Key matches the stored one.
    Known,
    /// Key differs from the stored one — possible impersonation.
    Mismatch,
}

fn path() -> PathBuf {
    let dir = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Imperio");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("known_hosts")
}

fn load() -> HashMap<u32, String> {
    let mut map = HashMap::new();
    if let Ok(text) = std::fs::read_to_string(path()) {
        for line in text.lines() {
            let mut it = line.split_whitespace();
            if let (Some(id), Some(key)) = (it.next(), it.next()) {
                if let Ok(id) = id.parse::<u32>() {
                    map.insert(id, key.to_string());
                }
            }
        }
    }
    map
}

fn store(map: &HashMap<u32, String>) {
    let mut text = String::new();
    for (id, key) in map {
        text.push_str(&format!("{id} {key}\n"));
    }
    let _ = std::fs::write(path(), text);
}

/// Check `public` against the pinned key for `device_id`. On `New`, pin it.
/// On `Mismatch`, pin only when `trust_new` is set (user override).
pub fn check(device_id: u32, public: &[u8], trust_new: bool) -> Trust {
    let enc = base64::engine::general_purpose::STANDARD.encode(public);
    let mut map = load();
    match map.get(&device_id) {
        None => {
            map.insert(device_id, enc);
            store(&map);
            Trust::New
        }
        Some(stored) if *stored == enc => Trust::Known,
        Some(_) => {
            if trust_new {
                map.insert(device_id, enc);
                store(&map);
                Trust::New
            } else {
                Trust::Mismatch
            }
        }
    }
}
