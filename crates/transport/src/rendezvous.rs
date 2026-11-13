//! Finding the peer: device IDs, the signal server, hole punching, relay.
//!
//! Flow (both sides use one UDP socket for everything, so the NAT mapping
//! the server observes is the one media will use):
//!
//! 1. Host sends `Register` every few seconds; server learns its public
//!    address. Viewer sends `Connect{target}`; server tells each side the
//!    other's public and LAN addresses plus a session id.
//! 2. Both spray `Punch` probes at every candidate for up to ~3 s. The first
//!    probe or ack that arrives fixes the peer address (direct path).
//! 3. Otherwise both `RelayBind` and the server forwards between them.
//!
//! No STUN dependency: the server itself observes the reflexive address.

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use imperio_protocol::signal::{DeviceId, Kind, Punch, SessionId, SignalMessage};

use crate::identity::DeviceKey;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Path {
    /// Peer reachable directly at this address.
    Direct(SocketAddr),
    /// Peer reachable only via the signal server's relay.
    Relay(SocketAddr),
}

impl Path {
    pub fn addr(&self) -> SocketAddr {
        match self {
            Path::Direct(a) | Path::Relay(a) => *a,
        }
    }
}

/// Best-effort LAN addresses for this socket's port: the interface that
/// routes to the internet, plus loopback for same-machine testing.
pub fn local_candidates(port: u16) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    if let Ok(probe) = UdpSocket::bind("0.0.0.0:0") {
        if probe.connect("8.8.8.8:80").is_ok() {
            if let Ok(a) = probe.local_addr() {
                out.push(SocketAddr::new(a.ip(), port));
            }
        }
    }
    out.push(SocketAddr::new([127, 0, 0, 1].into(), port));
    out
}

/// Errors a UDP receive loop should treat as "nothing arrived": timeouts,
/// and Windows' WSAECONNRESET, which `recv` reports after an earlier send got
/// an ICMP port-unreachable (common right after a peer exits).
pub fn is_transient_recv_error(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::ConnectionReset
    )
}

/// Most LAN candidates accepted from (or sent to) the signal server.
const MAX_LOCAL_CANDIDATES: usize = 4;

/// A LAN candidate must actually be a LAN address. The list comes from the
/// other peer by way of the server, so without this check anyone could make a
/// host spray hole-punch probes at arbitrary internet addresses.
fn is_lan_candidate(a: &SocketAddr) -> bool {
    match a.ip() {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_private() || v4.is_loopback() || v4.is_link_local()
                // Carrier-grade NAT / Tailscale range, 100.64.0.0/10.
                || (o[0] == 100 && (64..=127).contains(&o[1]))
        }
        std::net::IpAddr::V6(v6) => {
            let s0 = v6.segments()[0];
            v6.is_loopback() || (s0 & 0xfe00) == 0xfc00 || (s0 & 0xffc0) == 0xfe80
        }
    }
}

/// The addresses worth probing for a peer: its (few, genuinely local) LAN
/// candidates plus the public address the server observed.
fn punch_candidates(peer_public: SocketAddr, peer_locals: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let mut cands: Vec<SocketAddr> = peer_locals
        .into_iter()
        .filter(|a| a.port() != 0 && is_lan_candidate(a))
        .take(MAX_LOCAL_CANDIDATES)
        .collect();
    if !cands.contains(&peer_public) {
        cands.push(peer_public);
    }
    cands
}

/// Receive one signaling message. Only datagrams that come from `server` are
/// accepted: anything else claiming to be signaling is a third party trying to
/// steer this client (for example towards a host it controls).
fn recv_signal(
    socket: &UdpSocket,
    server: SocketAddr,
    timeout: Duration,
) -> anyhow::Result<Option<SignalMessage>> {
    socket.set_read_timeout(Some(timeout))?;
    let mut buf = [0u8; 1500];
    match socket.recv_from(&mut buf) {
        Ok((n, from)) if from == server && n > 1 && buf[0] == Kind::Signal as u8 => {
            Ok(SignalMessage::decode(&buf[1..n]).ok())
        }
        Ok(_) => Ok(None),
        Err(e) if is_transient_recv_error(&e) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Host side: register until a viewer asks for us, then punch.
/// Returns `Some` when a path to the viewer exists, or `None` as soon as
/// `cancel` reports true (the user paused sharing, asked for a new code, or
/// is quitting).
pub fn host_wait_for_viewer(
    socket: &UdpSocket,
    server: SocketAddr,
    device_id: DeviceId,
    key: &DeviceKey,
    force_relay: bool,
    cancel: &dyn Fn() -> bool,
) -> anyhow::Result<Option<(SessionId, Path)>> {
    let locals = local_candidates(socket.local_addr()?.port());
    let public = key.public().to_vec();
    let sig = key
        .sign(&imperio_protocol::signal::register_context(device_id, &public))
        .to_vec();
    let mut last_register = Instant::now() - Duration::from_secs(10);
    let mut registered = false;
    loop {
        if cancel() {
            return Ok(None);
        }
        if last_register.elapsed() > Duration::from_secs(3) {
            socket.send_to(
                &SignalMessage::Register {
                    device_id,
                    public_key: public.clone(),
                    sig: sig.clone(),
                    locals: locals.clone(),
                }
                .encode(),
                server,
            )?;
            last_register = Instant::now();
        }
        match recv_signal(socket, server, Duration::from_millis(500))? {
            Some(SignalMessage::Registered { public }) => {
                if !registered {
                    tracing::info!(%public, "registered with signal server");
                    registered = true;
                }
            }
            Some(SignalMessage::Rejected { reason }) => {
                bail!("the signal server refused this device: {reason}")
            }
            Some(SignalMessage::Incoming {
                session,
                token,
                peer_public,
                peer_locals,
            }) => {
                tracing::info!(session, %peer_public, ?peer_locals, "incoming connection request");
                let cands = punch_candidates(peer_public, peer_locals);
                let path = punch(socket, server, session, token, &cands, force_relay)?;
                return Ok(Some((session, path)));
            }
            _ => {}
        }
    }
}

/// Obtain a fresh, server-assigned, non-enumerable device ID, proving
/// ownership of `key` so the server can bind the ID to it. Used once on first
/// run when a signal server is configured; the ID is then persisted.
pub fn register_new(
    socket: &UdpSocket,
    server: SocketAddr,
    key: &DeviceKey,
) -> anyhow::Result<DeviceId> {
    let public = key.public().to_vec();
    let sig = key
        .sign(&imperio_protocol::signal::new_device_context(&public))
        .to_vec();
    let msg = SignalMessage::RegisterNew {
        public_key: public,
        sig,
    }
    .encode();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last = Instant::now() - Duration::from_secs(10);
    loop {
        if last.elapsed() > Duration::from_secs(2) {
            socket.send_to(&msg, server)?;
            last = Instant::now();
        }
        if Instant::now() > deadline {
            anyhow::bail!("signal server did not assign a device id in time");
        }
        match recv_signal(socket, server, Duration::from_millis(500))? {
            Some(SignalMessage::Assigned { device_id }) => return Ok(device_id),
            Some(SignalMessage::Rejected { reason }) => {
                anyhow::bail!("registration rejected: {reason}")
            }
            _ => {}
        }
    }
}

/// Viewer side: ask the server for `target`, then punch.
pub fn viewer_connect(
    socket: &UdpSocket,
    server: SocketAddr,
    target: DeviceId,
    force_relay: bool,
) -> anyhow::Result<(SessionId, Path)> {
    let mut locals = local_candidates(socket.local_addr()?.port());
    locals.truncate(MAX_LOCAL_CANDIDATES);
    for attempt in 0..3 {
        socket.send_to(
            &SignalMessage::Connect {
                target,
                locals: locals.clone(),
            }
            .encode(),
            server,
        )?;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            match recv_signal(socket, server, Duration::from_millis(300))? {
                Some(SignalMessage::PeerInfo {
                    session,
                    token,
                    peer_public,
                    peer_locals,
                }) => {
                    tracing::info!(session, %peer_public, ?peer_locals, "peer info received");
                    let cands = punch_candidates(peer_public, peer_locals);
                    let path = punch(socket, server, session, token, &cands, force_relay)?;
                    return Ok((session, path));
                }
                Some(SignalMessage::NotFound { .. }) => bail!(
                    "That computer is not online right now (ID {}). Ask them to open Imperio and check the ID.",
                    imperio_protocol::device_id::format(target)
                ),
                Some(SignalMessage::RateLimited) => {
                    bail!("Too many attempts in a short time. Wait a few seconds and try again.")
                }
                _ => {}
            }
        }
        tracing::warn!(attempt, "no answer from signal server, retrying");
    }
    bail!("signal server did not answer")
}

/// Spray probes at all candidates; fall back to relay.
fn punch(
    socket: &UdpSocket,
    server: SocketAddr,
    session: SessionId,
    token: u64,
    candidates: &[SocketAddr],
    force_relay: bool,
) -> anyhow::Result<Path> {
    if !force_relay {
        let probe = Punch {
            session,
            ack: false,
        }
        .encode();
        let ack = Punch { session, ack: true }.encode();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut last_spray = Instant::now() - Duration::from_secs(1);
        let mut buf = [0u8; 1500];
        socket.set_read_timeout(Some(Duration::from_millis(50)))?;
        while Instant::now() < deadline {
            if last_spray.elapsed() >= Duration::from_millis(100) {
                for c in candidates {
                    let _ = socket.send_to(&probe, c);
                }
                last_spray = Instant::now();
            }
            if let Ok((n, from)) = socket.recv_from(&mut buf) {
                if n > 1 && buf[0] == Kind::Punch as u8 {
                    if let Ok(p) = Punch::decode(&buf[1..n]) {
                        if p.session == session {
                            // Tell the peer we heard it, a few times for luck.
                            for _ in 0..3 {
                                let _ = socket.send_to(&ack, from);
                            }
                            tracing::info!(%from, "direct path established");
                            return Ok(Path::Direct(from));
                        }
                    }
                }
            }
        }
        tracing::warn!("hole punching failed; falling back to relay");
    }
    // Relay (token-authorised).
    for _ in 0..5 {
        socket.send_to(&SignalMessage::RelayBind { session, token }.encode(), server)?;
        if let Some(SignalMessage::RelayReady { session: s }) =
            recv_signal(socket, server, Duration::from_millis(600))?
        {
            if s == session {
                tracing::info!(%server, "relay path established");
                return Ok(Path::Relay(server));
            }
        }
    }
    bail!("relay binding failed")
}

/// Send an abuse report to the signal server (fire and forget).
pub fn report_abuse(
    socket: &UdpSocket,
    server: SocketAddr,
    reported_device: Option<DeviceId>,
    session: Option<SessionId>,
    reason: &str,
) {
    let msg = SignalMessage::Report {
        reported_device,
        session,
        reason: reason.chars().take(500).collect(),
    };
    let _ = socket.send_to(&msg.encode(), server);
}

/// Persistent nine-digit device id in %APPDATA%\imperio\device_id.
pub fn load_or_create_device_id() -> DeviceId {
    use rand::Rng;
    let dir = std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("imperio");
    let path = dir.join("device_id");
    if let Ok(s) = std::fs::read_to_string(&path) {
        if let Ok(id) = s.trim().parse::<u32>() {
            if (100_000_000..1_000_000_000).contains(&id) {
                return id;
            }
        }
    }
    let id: u32 = rand::thread_rng().gen_range(100_000_000..1_000_000_000);
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(&path, id.to_string());
    id
}

/// Drain and parse the first datagram of `kind` within `timeout`.
pub fn recv_kind(
    socket: &UdpSocket,
    kind: Kind,
    timeout: Duration,
) -> anyhow::Result<Option<(Vec<u8>, SocketAddr)>> {
    socket
        .set_read_timeout(Some(timeout))
        .context("set_read_timeout")?;
    let deadline = Instant::now() + timeout;
    let mut buf = vec![0u8; 2048];
    while Instant::now() < deadline {
        match socket.recv_from(&mut buf) {
            Ok((n, from)) if n > 1 && buf[0] == kind as u8 => {
                return Ok(Some((buf[1..n].to_vec(), from)))
            }
            Ok(_) => continue,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Ok(None)
            }
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn punch_candidates_keep_only_real_lan_addresses() {
        let public: SocketAddr = "203.0.113.9:40000".parse().unwrap();
        let locals: Vec<SocketAddr> = [
            "192.168.1.20:7700",
            "8.8.8.8:53", // not a LAN address: would be a reflection target
            "10.0.0.5:7700",
            "100.100.1.2:7700", // Tailscale
            "192.168.1.21:0",   // port 0 is never valid
            "172.16.0.1:7700",
            "192.168.1.22:7700", // over the cap
        ]
        .iter()
        .map(|s| s.parse().unwrap())
        .collect();
        let c = punch_candidates(public, locals);
        assert_eq!(c.len(), MAX_LOCAL_CANDIDATES + 1);
        assert!(c.contains(&public));
        assert!(!c.iter().any(|a| a.ip().to_string() == "8.8.8.8"));
        assert!(!c.iter().any(|a| a.port() == 0));
    }

    #[test]
    fn signaling_from_a_stranger_is_ignored() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let stranger = UdpSocket::bind("127.0.0.1:0").unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        let to = client.local_addr().unwrap();
        let server_addr = server.local_addr().unwrap();
        let msg = SignalMessage::RelayReady { session: 1 }.encode();
        stranger.send_to(&msg, to).unwrap();
        let got = recv_signal(&client, server_addr, Duration::from_millis(600)).unwrap();
        assert!(got.is_none(), "a forged signal message must be dropped");
        server.send_to(&msg, to).unwrap();
        let got = recv_signal(&client, server_addr, Duration::from_millis(600)).unwrap();
        assert_eq!(got, Some(SignalMessage::RelayReady { session: 1 }));
    }
}
