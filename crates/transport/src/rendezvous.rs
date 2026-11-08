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

fn recv_signal(
    socket: &UdpSocket,
    timeout: Duration,
) -> anyhow::Result<Option<(SignalMessage, SocketAddr)>> {
    socket.set_read_timeout(Some(timeout))?;
    let mut buf = [0u8; 1500];
    match socket.recv_from(&mut buf) {
        Ok((n, from)) if n > 1 && buf[0] == Kind::Signal as u8 => {
            Ok(SignalMessage::decode(&buf[1..n]).ok().map(|m| (m, from)))
        }
        Ok(_) => Ok(None),
        Err(e) if is_transient_recv_error(&e) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Host side: register until a viewer asks for us, then punch.
/// Returns when a path to the viewer exists.
pub fn host_wait_for_viewer(
    socket: &UdpSocket,
    server: SocketAddr,
    device_id: DeviceId,
    key: &DeviceKey,
    force_relay: bool,
) -> anyhow::Result<(SessionId, Path)> {
    let locals = local_candidates(socket.local_addr()?.port());
    let public = key.public().to_vec();
    let sig = key
        .sign(&imperio_protocol::signal::register_context(device_id, &public))
        .to_vec();
    let mut last_register = Instant::now() - Duration::from_secs(10);
    let mut registered = false;
    loop {
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
        match recv_signal(socket, Duration::from_millis(500))? {
            Some((SignalMessage::Registered { public }, _)) => {
                if !registered {
                    tracing::info!(%public, "registered with signal server");
                    registered = true;
                }
            }
            Some((
                SignalMessage::Incoming {
                    session,
                    token,
                    peer_public,
                    peer_locals,
                },
                _,
            )) => {
                tracing::info!(session, %peer_public, ?peer_locals, "incoming connection request");
                let mut cands = peer_locals;
                cands.push(peer_public);
                let path = punch(socket, server, session, token, &cands, force_relay)?;
                return Ok((session, path));
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
        match recv_signal(socket, Duration::from_millis(500))? {
            Some((SignalMessage::Assigned { device_id }, _)) => return Ok(device_id),
            Some((SignalMessage::Rejected { reason }, _)) => {
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
    let locals = local_candidates(socket.local_addr()?.port());
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
            match recv_signal(socket, Duration::from_millis(300))? {
                Some((
                    SignalMessage::PeerInfo {
                        session,
                        token,
                        peer_public,
                        peer_locals,
                    },
                    _,
                )) => {
                    tracing::info!(session, %peer_public, ?peer_locals, "peer info received");
                    let mut cands = peer_locals;
                    cands.push(peer_public);
                    let path = punch(socket, server, session, token, &cands, force_relay)?;
                    return Ok((session, path));
                }
                Some((SignalMessage::NotFound { .. }, _)) => bail!("device {target} is not online"),
                Some((SignalMessage::RateLimited, _)) => bail!("signal server rate-limited this client; try again shortly"),
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
        if let Some((SignalMessage::RelayReady { session: s }, _)) =
            recv_signal(socket, Duration::from_millis(600))?
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
