//! Imperio: the single executable. With no arguments it opens the home
//! window (share this computer / connect to another). Subcommands keep the
//! scriptable command-line modes and the Windows-service plumbing:
//!
//!   imperio                      home window (host engine runs inside it)
//!   imperio host [flags]         console host (old `imperio-host`)
//!   imperio view [target] [...]  console viewer (old `imperio-viewer`)
//!   imperio install-service | uninstall-service | --service | --session-helper
//!   imperio --firewall           open the firewall rule (run elevated)
#![windows_subsystem = "windows"]

mod app;
mod keys;
mod license;
mod tray;
mod update;

use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};

fn main() -> anyhow::Result<()> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    match raw.first().map(String::as_str) {
        // --- owner tools: signing key, release manifests, licence keys ---
        Some("keygen") => {
            attach_console();
            let (seed, public) = keys::keygen();
            println!("PRIVATE seed (keep secret, back it up):\n  {seed}\n");
            println!("PUBLIC key (paste into apps/imperio/src/keys.rs PUBLIC_KEY_B64):\n  {public}");
            Ok(())
        }
        Some("release-sign") => {
            attach_console();
            let path = raw.get(1).ok_or_else(|| anyhow::anyhow!("usage: imperio release-sign <latest.json> --key <seed> [--exe <Imperio.exe>]"))?;
            let seed = flag(&raw, "--key").ok_or_else(|| anyhow::anyhow!("--key <seed> is required"))?;
            let exe = flag(&raw, "--exe").map(std::path::PathBuf::from);
            update::sign_manifest_file(std::path::Path::new(path), &seed, exe.as_deref())
        }
        Some("license-sign") => {
            attach_console();
            let seed = flag(&raw, "--key").ok_or_else(|| anyhow::anyhow!("--key <seed> is required"))?;
            let lic = license::License {
                name: flag(&raw, "--name").unwrap_or_default(),
                plan: flag(&raw, "--plan").unwrap_or_else(|| "pro".into()),
                expires: flag(&raw, "--expires"),
                seats: flag(&raw, "--seats").and_then(|s| s.parse().ok()).unwrap_or(1),
            };
            println!("{}", license::encode(&lic, &seed)?);
            Ok(())
        }
        Some("--apply-update") => update::apply_update_main(&raw[1..]),
        Some("host") => {
            attach_console();
            imperio_host::cli_main(raw[1..].to_vec())
        }
        Some("view") => {
            attach_console();
            imperio_viewer::cli_main(raw[1..].to_vec())
        }
        Some("install-service" | "uninstall-service" | "--service" | "--session-helper" | "--firewall") => {
            attach_console();
            imperio_host::cli_main(raw)
        }
        Some("session") => session_child(raw[1..].to_vec()),
        _ => app::run(),
    }
}

/// Internal: one remote session in its own process. The home window spawns
/// this because winit permits a single event loop per process and eframe owns
/// it. Prints `connected` on stdout once the encrypted link is up; on failure
/// prints the reason on stderr and exits 1.
fn session_child(args: Vec<String>) -> anyhow::Result<()> {
    app::init_logging();
    let mut req = imperio_viewer::SessionRequest {
        target: String::new(),
        code: None,
        trust_new: false,
        server: None,
        send_input: true,
        force_relay: false,
        totp: None,
        record: false,
        on_connected: Some(std::sync::Arc::new(|| {
            use std::io::Write;
            println!("connected");
            let _ = std::io::stdout().flush();
        })),
    };
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--code" => req.code = it.next(),
            "--server" => req.server = it.next(),
            "--trust-new" => req.trust_new = true,
            "--no-input" => req.send_input = false,
            "--record" => req.record = true,
            s if !s.starts_with("--") => req.target = s.to_string(),
            _ => {}
        }
    }
    if let Err(e) = imperio_viewer::run_session(req) {
        eprintln!("{e:#}");
        std::process::exit(1);
    }
    Ok(())
}

/// Value of `--name <value>` in `args`, if present.
fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// A GUI-subsystem process has no console; when launched from one, attach to
/// it so the console subcommands can print.
fn attach_console() {
    // SAFETY: plain API call; failure just means there is no parent console.
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}
