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
/// Probed once per process: the answer cannot change while we run, and the
/// probe touches the disk.
pub fn data_dir() -> PathBuf {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        // `IMPERIO_DATA_DIR` keeps everything in a folder of your choice: a
        // portable copy on a USB stick, or a throwaway profile for tests.
        if let Some(dir) = std::env::var_os("IMPERIO_DATA_DIR").map(PathBuf::from) {
            if !dir.as_os_str().is_empty() && std::fs::create_dir_all(&dir).is_ok() {
                return dir;
            }
        }
        let candidates = [
            std::env::var_os("ProgramData").map(|p| PathBuf::from(p).join("Imperio")),
            std::env::var_os("APPDATA").map(|p| PathBuf::from(p).join("Imperio")),
            Some(std::env::temp_dir().join("Imperio")),
        ];
        for c in candidates.into_iter().flatten() {
            if std::fs::create_dir_all(&c).is_ok() {
                let probe = c.join(format!(".write-test-{}", std::process::id()));
                if std::fs::write(&probe, b"").is_ok() {
                    let _ = std::fs::remove_file(&probe);
                    return c;
                }
            }
        }
        std::env::temp_dir()
    })
    .clone()
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.toml")
}

/// Serialises config writes within the process (home window and host engine
/// both save).
static SAVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Load the config, creating it with a fresh device id on first run.
///
/// The returned config always carries a device identity key: it is generated
/// here, once, rather than lazily by whichever component needs it first.
/// (Generating it later, on a copy, let a second copy of the config overwrite
/// the file without the key, so the computer came back with a new identity
/// after every restart and viewers were told it had been swapped.)
pub fn load_or_create() -> Config {
    let path = config_path();
    let mut loaded = None;
    match std::fs::read_to_string(&path) {
        Ok(text) => match toml::from_str::<Config>(&text) {
            Ok(c) => loaded = Some(c),
            Err(e) => {
                // Never silently replace a file that may hold the identity
                // key, password and licence: keep it next to the new one.
                let backup = path.with_extension("toml.unreadable");
                let kept = std::fs::copy(&path, &backup).is_ok();
                tracing::warn!(path = %path.display(), error = %e, kept_copy = kept, "config unreadable; starting from defaults")
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(path = %path.display(), error = %e, "config could not be read; using defaults"),
    }
    let fresh = loaded.is_none();
    let mut c = loaded.unwrap_or_default();
    let has_identity = c
        .identity_key
        .as_deref()
        .and_then(imperio_transport::identity::DeviceKey::from_seed_b64)
        .is_some();
    if !has_identity {
        c.identity_key = Some(imperio_transport::identity::DeviceKey::generate().seed_b64());
    }
    if fresh || !has_identity {
        if let Err(e) = save(&c) {
            tracing::warn!(error = %e, "could not write config");
        }
    }
    c
}

/// Write the config atomically: a crash or power cut mid-write must not leave
/// a truncated file (which would cost the device identity).
pub fn save(c: &Config) -> anyhow::Result<()> {
    let text = toml::to_string_pretty(c)?;
    let path = config_path();
    let tmp = path.with_extension(format!("toml.{}.tmp", std::process::id()));
    let _guard = SAVE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    // `rename` replaces the destination on Windows (MOVEFILE_REPLACE_EXISTING).
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}
