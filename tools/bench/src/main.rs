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
        Some("probe-keys") => {
            let host = args.next().context("host addr")?;
            let code = args.next().context("pairing code shown on the host")?;
            let count: u32 = args.next().unwrap_or_else(|| "200".into()).parse()?;
            let legacy = args.next().as_deref() == Some("--legacy");
            probe_keys(&host, &code, count, legacy)
        }
        Some("bad-link") => {
            let listen: u16 = args.next().context("listen port")?.parse()?;
            let host = args.next().context("host addr")?;
            let mut link = BadLink { rate_kbps: 0, queue_ms: 200, delay_ms: 0, loss: 0.0 };
            while let Some(k) = args.next() {
                let v = args.next().with_context(|| format!("value for {k}"))?;
                match k.as_str() {
                    "--rate-kbps" => link.rate_kbps = v.parse()?,
                    "--queue-ms" => link.queue_ms = v.parse()?,
                    "--delay-ms" => link.delay_ms = v.parse()?,
                    "--loss" => link.loss = v.parse::<f64>()? / 100.0,
                    _ => anyhow::bail!("unknown option {k}"),
                }
            }
            bad_link(listen, &host, link)
        }
        _ => {
            eprintln!(
                "usage: imperio-bench <capture [seconds] | monitors | probe-input [host:port] | lossy-proxy <listen-port> <host:port> [loss%]\n\
                 \x20      | bad-link <listen-port> <host:port> [--rate-kbps N] [--queue-ms N] [--delay-ms N] [--loss %]>"
            );
            Ok(())
        }
    }
}

/// Types `count` key presses (each a press and a release, 8 ms apart) into a
/// running host through the real encrypted session, the way the viewer does,
/// and reports how many the host confirmed. Run the host with
/// `IMPERIO_INPUT_LOG_ONLY=1`: it then writes each event to its log instead
/// of pressing keys on this computer, and the log shows what arrived, in
/// which order, and whether any key was left held down.
///
/// `--legacy` sends the way versions up to 0.3.17 did: one datagram per
/// event, never repeated.
fn probe_keys(host: &str, code: &str, count: u32, legacy: bool) -> anyhow::Result<()> {
    use imperio_protocol::signal::{Handshake as HandshakeMsg, Kind};
    use imperio_protocol::{ControlMessage, InputEvent, FEATURE_INPUT_BATCH, PROTOCOL_VERSION};
    use imperio_transport::crypto::Handshake;
    use imperio_transport::input::InputSender;
    use imperio_transport::link::{Incoming, SecureLink};
    use imperio_transport::rendezvous::recv_kind;
    use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let host: SocketAddr = host.to_socket_addrs()?.next().context("resolve host")?;
    let socket = Arc::new(UdpSocket::bind("0.0.0.0:0")?);
    let session: u64 = 0x5EED_0000_0000_0002;
    // The handshake itself may be lost on the way: try a few times.
    let mut link = None;
    for _ in 0..8 {
        let ours = Handshake::new(code, session);
        socket.send_to(&HandshakeMsg { session, spake_msg: ours.outbound.clone() }.encode(), host)?;
        if let Some((body, _)) = recv_kind(&socket, Kind::Handshake, Duration::from_secs(1))? {
            let reply = HandshakeMsg::decode(&body)?;
            if reply.session == session {
                link = Some(Arc::new(SecureLink::new(socket.clone(), host, ours.finish(&reply.spake_msg, false)?)));
                break;
            }
        }
    }
    let link = link.context("no handshake reply from host")?;
    let sender = Arc::new(InputSender::new());
    let started = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));

    // What the viewer's network thread does: hear the host, confirm, repeat.
    let reader = {
        let (socket, link, sender, started, done) = (socket.clone(), link.clone(), sender.clone(), started.clone(), done.clone());
        std::thread::spawn(move || {
            socket.set_nonblocking(true).ok();
            let mut buf = vec![0u8; 2048];
            let mut last_ping = Instant::now();
            let mut last_hello = Instant::now() - Duration::from_secs(1);
            while !done.load(Ordering::Relaxed) {
                let now = Instant::now();
                match socket.recv_from(&mut buf) {
                    Ok((n, from)) if from == link.peer() => {
                        if let Some(Incoming::Control(msg)) = link.handle_datagram(&buf[..n], now) {
                            match msg {
                                ControlMessage::StreamInfo { .. } => started.store(true, Ordering::Relaxed),
                                ControlMessage::Features(bits) if !legacy && bits & FEATURE_INPUT_BATCH != 0 => sender.enable(),
                                ControlMessage::InputAck { seq } => sender.on_ack(seq),
                                ControlMessage::Pong { t_us } => {
                                    link.set_rtt(Duration::from_micros(imperio_transport::now_us().saturating_sub(t_us)))
                                }
                                _ => {}
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(_) => std::thread::sleep(Duration::from_millis(1)),
                }
                if !started.load(Ordering::Relaxed) && last_hello.elapsed() > Duration::from_millis(300) {
                    let _ = link.send_control(&ControlMessage::Hello { version: PROTOCOL_VERSION });
                    last_hello = now;
                }
                if last_ping.elapsed() > Duration::from_millis(300) {
                    let _ = link.send_control(&ControlMessage::Ping { t_us: imperio_transport::now_us() });
                    last_ping = now;
                }
                if let Some(again) = sender.tick(now, link.rtt()) {
                    let _ = link.send_control(&again);
                }
            }
        })
    };

    let wait_for = |what: &dyn Fn() -> bool, limit: Duration| {
        let t = Instant::now();
        while !what() && t.elapsed() < limit {
            std::thread::sleep(Duration::from_millis(10));
        }
        what()
    };
    if !wait_for(&|| started.load(Ordering::Relaxed), Duration::from_secs(10)) {
        anyhow::bail!("the host never started the stream");
    }
    if !legacy && !wait_for(&|| sender.batching(), Duration::from_secs(3)) {
        println!("the host did not announce confirmed input; sending the old way");
    }
    let mode = if sender.batching() { "confirmed" } else { "unconfirmed (old way)" };
    println!("typing {count} key presses, input is {mode}");
    for i in 0..count {
        for down in [true, false] {
            // Scancodes 2..=11 are the digit keys: the host's log shows the order.
            let ev = InputEvent::Key { scancode: 2 + (i % 10) as u16, extended: false, down, unicode: None };
            let _ = link.send_control(&sender.push(ev, Instant::now()));
            std::thread::sleep(Duration::from_millis(8));
        }
    }
    let all = wait_for(&|| sender.unconfirmed() == 0, Duration::from_secs(8));
    if sender.batching() {
        println!(
            "host confirmed {}: {} events still unconfirmed",
            if all { "everything" } else { "NOT everything" },
            sender.unconfirmed()
        );
    }
    done.store(true, Ordering::Relaxed);
    let _ = reader.join();
    for _ in 0..5 {
        let _ = link.send_control(&ControlMessage::Bye);
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

/// A slow, delayed, lossy link (see [`bad_link`]). Each direction has its own.
#[derive(Clone, Copy)]
struct BadLink {
    /// Capacity in kbit/s; 0 = unlimited.
    rate_kbps: u32,
    /// How much may wait in front of the bottleneck before the tail is dropped.
    queue_ms: u64,
    /// Fixed one-way delay added to everything.
    delay_ms: u64,
    /// Random loss, 0..1.
    loss: f64,
}

/// UDP proxy that behaves like a weak connection: a bottleneck of a given
/// capacity with a queue in front of it (datagrams wait, so delay grows, and
/// are dropped once the queue is full), a fixed delay, and random loss. The
/// same in both directions. Point the viewer at the proxy.
///
/// This is what a far Wi-Fi access point, a phone hotspot or a relayed VPN
/// path does to a stream that sends more than the path carries; the plain
/// `lossy-proxy` only drops and never delays.
fn bad_link(listen: u16, host: &str, link: BadLink) -> anyhow::Result<()> {
    use std::collections::VecDeque;
    use std::net::{SocketAddr, ToSocketAddrs};
    use std::sync::{Arc, Mutex};

    struct Direction {
        /// (when it leaves, where to, the datagram), in leaving order.
        queue: VecDeque<(Instant, SocketAddr, Vec<u8>)>,
        /// When the bottleneck has finished sending what it already holds.
        free_at: Instant,
        forwarded: u64,
        dropped_full: u64,
        dropped_random: u64,
        wait_max: Duration,
    }
    impl Direction {
        fn new() -> Self {
            Self {
                queue: VecDeque::new(),
                free_at: Instant::now(),
                forwarded: 0,
                dropped_full: 0,
                dropped_random: 0,
                wait_max: Duration::ZERO,
            }
        }
    }

    let host: SocketAddr = host.to_socket_addrs()?.next().context("resolve host")?;
    let sock = Arc::new(imperio_transport::bind_udp(("0.0.0.0", listen))?);
    println!(
        "bad link on udp/{listen} -> {host}: {} kbit/s, queue {} ms, delay {} ms, loss {:.1}% (each way)",
        link.rate_kbps,
        link.queue_ms,
        link.delay_ms,
        link.loss * 100.0
    );
    // [0] towards the host, [1] towards the viewer.
    let dirs = Arc::new(Mutex::new([Direction::new(), Direction::new()]));

    // Sender: releases each datagram when its time has come.
    {
        let (sock, dirs) = (sock.clone(), dirs.clone());
        std::thread::spawn(move || {
            let mut last = Instant::now();
            loop {
                let now = Instant::now();
                let mut due = Vec::new();
                {
                    let mut d = dirs.lock().unwrap();
                    for dir in d.iter_mut() {
                        while dir.queue.front().is_some_and(|(at, _, _)| *at <= now) {
                            let (_, to, data) = dir.queue.pop_front().unwrap();
                            dir.forwarded += 1;
                            due.push((to, data));
                        }
                    }
                    if last.elapsed() > Duration::from_secs(2) {
                        for (name, dir) in ["to-host", "to-viewer"].iter().zip(d.iter_mut()) {
                            println!(
                                "{name}: forwarded {} dropped_queue_full {} dropped_random {} longest_wait_ms {}",
                                dir.forwarded,
                                dir.dropped_full,
                                dir.dropped_random,
                                dir.wait_max.as_millis()
                            );
                            dir.wait_max = Duration::ZERO;
                        }
                        last = Instant::now();
                    }
                }
                for (to, data) in due {
                    let _ = sock.send_to(&data, to);
                }
                std::thread::sleep(Duration::from_micros(300));
            }
        });
    }

    let mut client: Option<SocketAddr> = None;
    let mut buf = vec![0u8; 2048];
    let mut rng: u64 = 0x9E3779B97F4A7C15;
    let mut next = move || {
        rng ^= rng >> 12;
        rng ^= rng << 25;
        rng ^= rng >> 27;
        (rng.wrapping_mul(0x2545F4914F6CDD1D) >> 11) as f64 / (1u64 << 53) as f64
    };
    loop {
        let (n, from) = match sock.recv_from(&mut buf) {
            Ok(x) => x,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
            Err(e) => return Err(e.into()),
        };
        let (index, to) = if from == host {
            match client {
                Some(c) => (1, c),
                None => continue,
            }
        } else {
            client = Some(from);
            (0, host)
        };
        let now = Instant::now();
        let mut d = dirs.lock().unwrap();
        let dir = &mut d[index];
        if next() < link.loss {
            dir.dropped_random += 1;
            continue;
        }
        let mut leaves = now;
        if link.rate_kbps > 0 {
            // 28 bytes of IP and UDP header travel with every datagram.
            let cost = Duration::from_secs_f64((n + 28) as f64 * 8.0 / (link.rate_kbps as f64 * 1000.0));
            let start = dir.free_at.max(now);
            if start.duration_since(now) > Duration::from_millis(link.queue_ms) {
                dir.dropped_full += 1;
                continue;
            }
            dir.free_at = start + cost;
            leaves = dir.free_at;
        }
        dir.wait_max = dir.wait_max.max(leaves.duration_since(now));
        dir.queue.push_back((leaves + Duration::from_millis(link.delay_ms), to, buf[..n].to_vec()));
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
