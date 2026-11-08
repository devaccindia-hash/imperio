//! Host configuration: `%ProgramData%\Imperio\config.toml`.
//!
//! Shared by the console run and the service so they present the same device
//! identity. Command-line flags override file values.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Nine-digit device id shown to users.
    pub device_id: u32,
    /// Permanent password for unattended access. When unset, a random
    /// 6-digit code is issued per session (shown on the console / log).
    pub password: Option<String>,
    /// Rendezvous server `host:port`; unset = direct (LAN) mode only.
    pub server: Option<String>,
    pub port: u16,
    pub bitrate_kbps: u32,
    pub fps: u32,
    /// Draw the host's mouse cursor into the stream.
    pub show_cursor: bool,
    pub clipboard: bool,
    /// Index of the display streamed first.
    pub monitor: u8,
    /// Persistent Ed25519 identity seed (base64), generated on first run.
    /// Lets reconnecting viewers confirm they reach the same machine.
    pub identity_key: Option<String>,
    /// Optional base32 TOTP secret; when set, unattended access also requires
    /// the current 6-digit code in addition to the password.
    pub totp_secret: Option<String>,
    /// When true, an attended session waits for the local user to approve the
    /// incoming connection before streaming (Enter in the console).
    pub require_accept: bool,
    /// Stream the host's system audio to the viewer (host -> viewer only).
    pub send_audio: bool,
    /// Open mode by default: no code, but only Tailscale / loopback peers.
    pub open_mode: bool,
    /// Recently connected targets, most recent first (home window).
    pub recent: Vec<String>,
    /// Check for a newer version at start (home window).
    pub check_updates: bool,
    /// Override the update manifest URL (default: the project's latest.json).
    pub update_url: Option<String>,
    /// Signed license key (`IMP-…`) entered by the user, if any.
    pub license_key: Option<String>,
    /// Record every session to Videos\Imperio automatically.
    pub record_sessions: bool,
}

impl Default for Config {
    fn default() -> Self {
        use rand::Rng;
        Self {
            device_id: rand::thread_rng().gen_range(100_000_000..1_000_000_000),
            password: None,
            server: None,
            port: imperio_protocol::DEFAULT_PORT,
            bitrate_kbps: 8_000,
            fps: 60,
            show_cursor: true,
            clipboard: true,
            monitor: 0,
            identity_key: None,
            totp_secret: None,
            require_accept: false,
            send_audio: true,
            open_mode: false,
            recent: Vec::new(),
            check_updates: true,
            update_url: None,
            license_key: None,
            record_sessions: false,
        }
    }
}

/// `%ProgramData%\Imperio` (machine-wide, readable by the SYSTEM service),
/// falling back to `%APPDATA%\Imperio` when ProgramData is not writable.
pub fn data_dir() -> PathBuf {
    let candidates = [
        std::env::var_os("ProgramData").map(|p| PathBuf::from(p).join("Imperio")),
        std::env::var_os("APPDATA").map(|p| PathBuf::from(p).join("Imperio")),
        Some(std::env::temp_dir().join("Imperio")),
    ];
    for c in candidates.into_iter().flatten() {
        if std::fs::create_dir_all(&c).is_ok() {
            let probe = c.join(".write-test");
            if std::fs::write(&probe, b"").is_ok() {
                let _ = std::fs::remove_file(&probe);
                return c;
            }
        }
    }
    std::env::temp_dir()
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.toml")
}

/// Load the config, creating it with a fresh device id on first run.
pub fn load_or_create() -> Config {
    let path = config_path();
    if let Ok(text) = std::fs::read_to_string(&path) {
        match toml::from_str::<Config>(&text) {
            Ok(c) => return c,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "config unreadable; using defaults")
            }
        }
    }
    let c = Config::default();
    if let Err(e) = save(&c) {
        tracing::warn!(error = %e, "could not write config");
    }
    c
}

pub fn save(c: &Config) -> anyhow::Result<()> {
    let text = toml::to_string_pretty(c)?;
    std::fs::write(config_path(), text)?;
    Ok(())
}
