//! The user's own Tailscale computers, read from the Tailscale app installed
//! on this PC (`tailscale status --json`). Nothing is sent anywhere: this only
//! asks the local Tailscale client what it already knows, so the home window
//! can offer "click a computer" instead of "type an address".

use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    /// Computer name as Tailscale shows it, lower-case.
    pub name: String,
    /// Its Tailscale IPv4 address (100.x.y.z).
    pub ip: String,
    pub online: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum State {
    /// Not asked yet.
    #[default]
    Unknown,
    /// No Tailscale app on this PC.
    NotInstalled,
    /// Installed, but switched off or not signed in.
    Stopped,
    /// Connected. `peers` are the account's other Windows computers,
    /// reachable ones first.
    Running { peers: Vec<Peer> },
}

fn exe() -> PathBuf {
    if let Some(pf) = std::env::var_os("ProgramFiles") {
        let p = PathBuf::from(pf).join("Tailscale").join("tailscale.exe");
        if p.is_file() {
            return p;
        }
    }
    PathBuf::from("tailscale.exe")
}

/// Ask the local Tailscale client. Takes a moment; call it off the UI thread.
pub fn query() -> State {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new(exe())
        .args(["status", "--json"])
        .stdin(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match out {
        Err(_) => State::NotInstalled,
        Ok(o) => parse(&String::from_utf8_lossy(&o.stdout)),
    }
}

/// Read `tailscale status --json`. Anything unreadable counts as "stopped":
/// the app is there but cannot tell us about a working connection.
pub fn parse(json: &str) -> State {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return State::Stopped;
    };
    if v["BackendState"].as_str() != Some("Running") {
        return State::Stopped;
    }
    let mut peers: Vec<Peer> = v["Peer"]
        .as_object()
        .map(|m| m.values().filter_map(peer).collect())
        .unwrap_or_default();
    peers.sort_by(|a, b| b.online.cmp(&a.online).then_with(|| a.name.cmp(&b.name)));
    State::Running { peers }
}

fn peer(p: &serde_json::Value) -> Option<Peer> {
    // Imperio runs on Windows only; phones and servers would just be clutter.
    if !p["OS"].as_str()?.eq_ignore_ascii_case("windows") {
        return None;
    }
    let ip = p["TailscaleIPs"]
        .as_array()?
        .iter()
        .filter_map(|x| x.as_str())
        .find(|s| s.parse::<std::net::Ipv4Addr>().is_ok())?
        .to_string();
    let name = p["HostName"].as_str().filter(|s| !s.trim().is_empty())?.trim().to_lowercase();
    Some(Peer { name, ip, online: p["Online"].as_bool().unwrap_or(false) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_windows_peers_online_first() {
        let json = r#"{
            "BackendState": "Running",
            "Peer": {
                "a": {"HostName": "Zed-PC", "OS": "windows", "Online": false,
                      "TailscaleIPs": ["fd7a::1", "100.64.0.9"]},
                "b": {"HostName": "Office", "OS": "windows", "Online": true,
                      "TailscaleIPs": ["100.64.0.7", "fd7a::2"]},
                "c": {"HostName": "phone", "OS": "android", "Online": true,
                      "TailscaleIPs": ["100.64.0.8"]}
            }
        }"#;
        assert_eq!(
            parse(json),
            State::Running {
                peers: vec![
                    Peer { name: "office".into(), ip: "100.64.0.7".into(), online: true },
                    Peer { name: "zed-pc".into(), ip: "100.64.0.9".into(), online: false },
                ]
            }
        );
    }

    #[test]
    fn signed_out_or_garbage_is_stopped() {
        assert_eq!(parse(r#"{"BackendState": "NeedsLogin"}"#), State::Stopped);
        assert_eq!(parse("not json"), State::Stopped);
    }

    #[test]
    fn running_alone_has_no_peers() {
        assert_eq!(parse(r#"{"BackendState": "Running"}"#), State::Running { peers: vec![] });
    }
}
