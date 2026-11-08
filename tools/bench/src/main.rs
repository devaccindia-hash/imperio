//! Measurement harness. Every number we publish comes from here.
//!
//! Phase 1 subcommand: `capture` - run Desktop Duplication for N seconds and
//! report achieved fps, acquire wait, dirty-rect counts and coalesced frames.

use std::time::{Duration, Instant};

use anyhow::Context;
use imperio_capture::{CaptureError, Capturer};

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env().add_directive("info".parse()?),
        )
        .init();

    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("capture") => {
            let secs: u64 = args
                .next()
                .as_deref()
                .unwrap_or("5")
                .parse()
                .context("seconds")?;
            bench_capture(Duration::from_secs(secs))
        }
        Some("monitors") => list_monitors(),
        Some("probe-input") => {
            let host = args
                .next()
                .unwrap_or_else(|| format!("127.0.0.1:{}", imperio_protocol::DEFAULT_PORT));
            let code = args.next().context("pairing code shown on the host")?;
            probe_input(&host, &code)
        }
        Some("totp") => {
            let secret = args.next().context("base32 TOTP secret")?;
            let bytes = imperio_transport::totp::decode_base32(&secret).context("bad base32")?;
            println!("{}", imperio_transport::totp::totp_at(&bytes, imperio_transport::totp::now_secs()));
            Ok(())
        }
        Some("lossy-proxy") => {
            let listen: u16 = args.next().context("listen port")?.parse()?;
            let host = args.next().context("host addr")?;
            let loss: f64 = args.next().unwrap_or_else(|| "5".into()).parse()?;
            lossy_proxy(listen, &host, loss / 100.0)
        }
        _ => {
            eprintln!("usage: imperio-bench <capture [seconds] | monitors | probe-input [host:port] | lossy-proxy <listen-port> <host:port> [loss%]>");
            Ok(())
        }
    }
}

/// UDP proxy that drops a fraction of datagrams in both directions, for
/// exercising NACK recovery on one machine. Point the viewer at the proxy.
fn lossy_proxy(listen: u16, host: &str, loss: f64) -> anyhow::Result<()> {
    use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
    let host: SocketAddr = host.to_socket_addrs()?.next().context("resolve host")?;
    let sock: UdpSocket = imperio_transport::bind_udp(("0.0.0.0", listen))?;
    println!(
        "lossy proxy on udp/{listen} -> {host}, dropping {:.1}% each way",
        loss * 100.0
    );
    let mut client: Option<SocketAddr> = None;
    let mut buf = vec![0u8; 2048];
    // xorshift64*: deterministic, dependency-free.
    let mut rng: u64 = 0x9E3779B97F4A7C15;
    let mut next = move || {
        rng ^= rng >> 12;
        rng ^= rng << 25;
        rng ^= rng >> 27;
        (rng.wrapping_mul(0x2545F4914F6CDD1D) >> 11) as f64 / (1u64 << 53) as f64
    };
    let (mut fwd, mut dropped) = (0u64, 0u64);
    let mut last = Instant::now();
    loop {
        let (n, from) = match sock.recv_from(&mut buf) {
            Ok(x) => x,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
            Err(e) => return Err(e.into()),
        };
        let to = if from == host {
            match client {
                Some(c) => c,
                None => continue,
            }
        } else {
            client = Some(from);
            host
        };
        if next() < loss {
            dropped += 1;
        } else {
            sock.send_to(&buf[..n], to)?;
            fwd += 1;
        }
        if last.elapsed() > Duration::from_secs(5) {
            println!(
                "forwarded {fwd}, dropped {dropped} ({:.1}%)",
                dropped as f64 * 100.0 / (fwd + dropped).max(1) as f64
            );
            last = Instant::now();
        }
    }
}

/// Verifies the viewer -> host input path against a running host on this
/// machine, through the real encrypted session: pairs with the code, sends a
/// MouseMove to the stream centre, checks the real cursor moved there, then
/// puts the cursor back where it was.
fn probe_input(host: &str, code: &str) -> anyhow::Result<()> {
    use imperio_protocol::signal::{Handshake as HandshakeMsg, Kind};
    use imperio_protocol::{ControlMessage, InputEvent, PROTOCOL_VERSION};
    use imperio_transport::crypto::Handshake;
    use imperio_transport::link::SecureLink;
    use imperio_transport::rendezvous::recv_kind;
    use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
    use std::sync::Arc;
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN,
    };

    fn cursor() -> (i32, i32) {
        let mut p = POINT::default();
        // SAFETY: valid out-pointer.
        unsafe {
            let _ = GetCursorPos(&mut p);
        }
        (p.x, p.y)
    }

    let host: SocketAddr = host.to_socket_addrs()?.next().context("resolve host")?;
    let socket = Arc::new(UdpSocket::bind("0.0.0.0:0")?);
    let session: u64 = 0x5EED_0000_0000_0001;
    let ours = Handshake::new(code, session);
    socket.send_to(
        &HandshakeMsg {
            session,
            spake_msg: ours.outbound.clone(),
        }
        .encode(),
        host,
    )?;
    let (body, _) = recv_kind(&socket, Kind::Handshake, Duration::from_secs(3))?
        .context("no handshake reply from host")?;
    let reply = HandshakeMsg::decode(&body)?;
    let keys = ours.finish(&reply.spake_msg, false)?;
    let link = SecureLink::new(socket.clone(), host, keys);
    link.send_control(&ControlMessage::Hello {
        version: PROTOCOL_VERSION,
    })?;
    println!("paired and authenticated with {host}");
    std::thread::sleep(Duration::from_millis(300));

    let (sw, sh) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    let before = cursor();
    link.send_control(&ControlMessage::Input(InputEvent::MouseMove {
        nx: 32768,
        ny: 32768,
    }))?;
    std::thread::sleep(Duration::from_millis(200));
    let after = cursor();

    // Restore (primary-monitor assumption is fine for a probe).
    let nx = (before.0 as f64 / sw.max(1) as f64 * 65535.0).clamp(0.0, 65535.0) as u16;
    let ny = (before.1 as f64 / sh.max(1) as f64 * 65535.0).clamp(0.0, 65535.0) as u16;
    link.send_control(&ControlMessage::Input(InputEvent::MouseMove { nx, ny }))?;
    link.send_control(&ControlMessage::Bye)?;

    let (cx, cy) = (sw / 2, sh / 2);
    let ok = (after.0 - cx).abs() <= 2 && (after.1 - cy).abs() <= 2;
    println!(
        "cursor before {:?}, after injected centre move {:?}, expected ({cx}, {cy}) -> {}",
        before,
        after,
        if ok { "PASS" } else { "FAIL" }
    );
    if !ok {
        anyhow::bail!("input probe failed");
    }
    Ok(())
}

fn list_monitors() -> anyhow::Result<()> {
    for m in imperio_capture::Monitor::enumerate().map_err(|e| anyhow::anyhow!(e.to_string()))? {
        println!(
            "[{}] {} {}x{} @ {} Hz ({})",
            m.index().unwrap_or(0),
            m.name().unwrap_or_default(),
            m.width().unwrap_or(0),
            m.height().unwrap_or(0),
            m.refresh_rate().unwrap_or(0),
            m.device_name().unwrap_or_default(),
        );
    }
    Ok(())
}

fn bench_capture(duration: Duration) -> anyhow::Result<()> {
    let mut cap = Capturer::primary()?;
    println!(
        "capturing {}x{} for {:?} (move the mouse / play a video for activity)",
        cap.width(),
        cap.height(),
        duration
    );

    let start = Instant::now();
    let mut frames = 0u64;
    let mut timeouts = 0u64;
    let mut coalesced = 0u64;
    let mut dirty_total = 0u64;
    let mut wait_total = Duration::ZERO;
    let mut wait_max = Duration::ZERO;
    let mut recreates = 0u32;

    while start.elapsed() < duration {
        let t0 = Instant::now();
        match cap.acquire(Duration::from_millis(100)) {
            Ok(frame) => {
                let wait = t0.elapsed();
                wait_total += wait;
                wait_max = wait_max.max(wait);
                frames += 1;
                dirty_total += frame.dirty_rects().len() as u64;
                if frame.accumulated_frames() > 1 {
                    coalesced += 1;
                }
            }
            Err(CaptureError::Timeout) => timeouts += 1,
            Err(CaptureError::AccessLost) => {
                recreates += 1;
                cap = cap.recreate()?;
            }
            Err(e) => return Err(e.into()),
        }
    }

    let elapsed = start.elapsed().as_secs_f64();
    println!(
        "frames        {frames}  ({:.1} fps over wall clock)",
        frames as f64 / elapsed
    );
    println!("timeouts      {timeouts}  (100 ms windows with no desktop change)");
    println!("coalesced     {coalesced}  (frames where DXGI merged >1 update)");
    if frames > 0 {
        println!(
            "dirty rects   {:.1} avg per frame",
            dirty_total as f64 / frames as f64
        );
        println!(
            "acquire wait  {:.2} ms avg, {:.2} ms max",
            wait_total.as_secs_f64() * 1e3 / frames as f64,
            wait_max.as_secs_f64() * 1e3
        );
    }
    println!("recreates     {recreates}");
    Ok(())
}
