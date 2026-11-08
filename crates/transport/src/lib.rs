//! Transport.
//!
//! Layers, bottom up:
//!
//! * [`rendezvous`]: find the peer (device ID -> addresses via the signal
//!   server), UDP hole punching, relay fallback. Produces a socket + address.
//! * [`crypto`]: SPAKE2 password-authenticated key exchange from the 6-digit
//!   session code, then ChaCha20-Poly1305 session keys per direction.
//! * [`link`]: the encrypted datagram link: sequence numbers, NACK
//!   retransmission from a send history, receiver reports, RTT.
//! * [`jitter`]: in-order frame assembly with a retransmit deadline and
//!   keyframe resync.
//! * [`cc`]: adaptive bitrate from receiver reports.
//! * [`lan`]: frame <-> chunk framing.
//!
//! Every market leader runs its own UDP protocol rather than WebRTC; this is
//! ours. ICE-style connectivity is intentionally decoupled so a browser
//! client can be added later.

pub mod cc;
pub mod crypto;
pub mod identity;
pub mod jitter;
pub mod lan;
pub mod link;
pub mod pacer;
pub mod rendezvous;
pub mod totp;

/// Microseconds since the UNIX epoch on the local clock.
pub fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// Bind a UDP socket with large kernel buffers. A 1080p60 stream arrives
/// in bursts of a dozen datagrams per frame; Windows' default receive buffer
/// drops some of them even on loopback.
pub fn bind_udp(addr: impl std::net::ToSocketAddrs) -> std::io::Result<std::net::UdpSocket> {
    let addr = addr
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::other("no address"))?;
    let domain = if addr.is_ipv4() {
        socket2::Domain::IPV4
    } else {
        socket2::Domain::IPV6
    };
    let sock = socket2::Socket::new(domain, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))?;
    let _ = sock.set_recv_buffer_size(4 << 20);
    let _ = sock.set_send_buffer_size(4 << 20);
    sock.bind(&addr.into())?;
    Ok(sock.into())
}
