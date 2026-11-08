//! Self-update.
//!
//! Publishing an update = uploading the new `Imperio.exe` plus a small
//! `latest.json` manifest, signed with the project's private key
//! (`imperio release-sign latest.json --key <seed>`):
//!
//! ```json
//! { "version": "0.4.0",
//!   "url": "https://…/Imperio.exe",
//!   "sha256": "<hex of the exe>",
//!   "notes": "What changed",
//!   "sig": "<signature over version, url and sha256>" }
//! ```
//!
//! The app fetches the manifest at start (and on demand), verifies the
//! signature with the embedded public key, compares versions, and offers the
//! update. Installing downloads the exe, checks its SHA-256, then starts the
//! new file with `--apply-update <pid> <path>`: it waits for this process to
//! exit, copies itself over the old exe and relaunches. No service, no
//! installer, works for the portable single exe.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const DEFAULT_URL: &str = "https://devaccindia-hash.github.io/imperio/latest.json";
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub url: String,
    pub sha256: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub sig: String,
}

/// A verified, newer release.
#[derive(Debug, Clone, PartialEq)]
pub struct Available {
    pub version: String,
    pub url: String,
    pub sha256: String,
    pub notes: String,
}

/// Bytes the manifest signature covers.
pub fn signing_message(m: &Manifest) -> Vec<u8> {
    format!("imperio-update\n{}\n{}\n{}", m.version.trim(), m.url.trim(), m.sha256.trim().to_lowercase())
        .into_bytes()
}

fn agent(secs: u64) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(secs)))
        .user_agent(concat!("Imperio/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// Parse "major.minor.patch" (extra parts ignored, missing parts = 0).
fn parse_version(v: &str) -> [u64; 3] {
    let mut out = [0u64; 3];
    for (i, part) in v.trim().trim_start_matches('v').split('.').take(3).enumerate() {
        out[i] = part
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .unwrap_or(0);
    }
    out
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    parse_version(candidate) > parse_version(current)
}

/// Fetch and verify the manifest. `Ok(None)` means up to date.
pub fn check(url: &str) -> Result<Option<Available>, String> {
    let mut res = agent(10).get(url).call().map_err(|e| {
        tracing::info!(error = %e, url, "update manifest fetch failed");
        match e {
            ureq::Error::StatusCode(404) => "No update has been published yet.".to_string(),
            ureq::Error::StatusCode(code) => format!("The update server answered with an error ({code})."),
            _ => "Couldn't check for updates right now. Check your internet connection.".to_string(),
        }
    })?;
    let text = res.body_mut().read_to_string().map_err(|e| {
        tracing::info!(error = %e, "update manifest read failed");
        "Couldn't read the update information.".to_string()
    })?;
    let m: Manifest = serde_json::from_str(&text).map_err(|e| {
        tracing::info!(error = %e, "update manifest malformed");
        "The update information is damaged.".to_string()
    })?;
    if !crate::keys::verify(&signing_message(&m), &m.sig) {
        return Err("The update information is not signed by Imperio\u{2019}s release key, so it was ignored.".into());
    }
    if !is_newer(&m.version, CURRENT) {
        return Ok(None);
    }
    Ok(Some(Available {
        version: m.version.trim().to_string(),
        url: m.url.trim().to_string(),
        sha256: m.sha256.trim().to_lowercase(),
        notes: m.notes,
    }))
}

fn staging_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Imperio")
        .join("update")
}

/// Download the release, verify its SHA-256, and return the staged path.
pub fn download_and_stage(a: &Available) -> Result<PathBuf, String> {
    let mut res = agent(600)
        .get(&a.url)
        .call()
        .map_err(|e| format!("download failed ({e})"))?;
    let bytes = res
        .body_mut()
        .with_config()
        .limit(512 * 1024 * 1024)
        .read_to_vec()
        .map_err(|e| format!("download failed ({e})"))?;
    let digest = Sha256::digest(&bytes);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    if hex != a.sha256 {
        return Err("the downloaded file does not match the published checksum; not installing".into());
    }
    let dir = staging_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = dir.join(format!("Imperio-{}.exe", a.version));
    std::fs::write(&path, &bytes).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}

/// Start the staged exe so it can replace this one once we exit.
pub fn launch_apply(staged: &Path) -> Result<(), String> {
    let target = std::env::current_exe().map_err(|e| e.to_string())?;
    std::process::Command::new(staged)
        .arg("--apply-update")
        .arg(std::process::id().to_string())
        .arg(&target)
        .spawn()
        .map_err(|e| format!("could not start the updater: {e}"))?;
    Ok(())
}

/// Runs inside the *new* exe: wait for the old process to exit, copy
/// ourselves over it, relaunch it.
pub fn apply_update_main(args: &[String]) -> anyhow::Result<()> {
    let pid: u32 = args.first().and_then(|s| s.parse().ok()).unwrap_or(0);
    let target = PathBuf::from(args.get(1).ok_or_else(|| anyhow::anyhow!("missing target path"))?);
    let me = std::env::current_exe()?;
    if pid != 0 {
        wait_for_exit(pid, Duration::from_secs(60));
    }
    // The old exe may stay locked for a moment after the process ends.
    let mut last_err = None;
    for _ in 0..60 {
        match std::fs::copy(&me, &target) {
            Ok(_) => {
                last_err = None;
                break;
            }
            Err(e) => {
                last_err = Some(e);
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
    if let Some(e) = last_err {
        anyhow::bail!("could not replace {}: {e}", target.display());
    }
    // `--updated` lets the new instance greet the user with its version.
    std::process::Command::new(&target).arg("--updated").spawn()?;
    Ok(())
}

fn wait_for_exit(pid: u32, max: Duration) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
    // SAFETY: handle is closed after waiting; a missing process just returns.
    unsafe {
        if let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            let _ = WaitForSingleObject(h, max.as_millis() as u32);
            let _ = CloseHandle(h);
        }
    }
}

/// `imperio release-sign <latest.json> --key <seed>`: fill in `sig` (and the
/// sha256 if an `--exe <path>` is given) and write the file back.
pub fn sign_manifest_file(path: &Path, seed_b64: &str, exe: Option<&Path>) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(path)?;
    // Tolerate a UTF-8 BOM from Windows editors / PowerShell.
    let mut m: Manifest = serde_json::from_str(text.trim_start_matches('\u{feff}'))?;
    if let Some(exe) = exe {
        let bytes = std::fs::read(exe)?;
        m.sha256 = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
    }
    m.sig = crate::keys::sign(seed_b64, &signing_message(&m))?;
    std::fs::write(path, serde_json::to_string_pretty(&m)?)?;
    println!("signed {} (version {}, sha256 {})", path.display(), m.version, m.sha256);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_ordering() {
        assert!(is_newer("0.4.0", "0.3.0"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.3.0", "0.3.0"));
        assert!(!is_newer("0.2.9", "0.3.0"));
        assert!(is_newer("v0.3.1", "0.3.0"));
    }
}
