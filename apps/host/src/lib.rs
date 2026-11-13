//! Host agent.
//!
//! Modes (first matching argument wins):
//!   install-service / uninstall-service  manage the Windows service (admin)
//!   --service                            run under the Service Control Manager
//!   --session-helper                     streaming worker in a user session
//!   (default)                            run interactively in this console
//!
//! Interactive / helper flags:
//!   --port N --server host:7800 --code 123456 --password PW
//!   --bitrate-kbps N --fps N --monitor N --force-relay --no-cursor --no-clipboard

pub mod config;
mod consent;
mod cursor;
mod input;
mod service;
mod ui;

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Context;
use config::Config;
use imperio_capture::{CaptureError, Capturer};
use imperio_clipboard::ClipboardSync;
use imperio_audio::{AudioEncoder, PcmCodec};
use imperio_codec::d3d::{pick_encoder_adapter, Converter, CpuBridge, SharedTexture, VideoDevice, NV12};
use imperio_codec::mf::MfH264Encoder;
use imperio_codec::{EncoderConfig, VideoEncoder};
use imperio_protocol::signal::{Handshake as HandshakeMsg, Kind};
use imperio_protocol::{ControlMessage, MAX_CLIPBOARD_BYTES, OPEN_CODE, PROTOCOL_VERSION};
use imperio_transport::cc::BitrateController;
use imperio_transport::crypto::{random_code, Handshake};
use imperio_transport::identity::{self, DeviceKey};
use imperio_transport::link::{Incoming, SecureLink};
use imperio_transport::pacer::Pacer;
use imperio_transport::rendezvous::{self, Path};
use imperio_transport::{clip, lan, now_us};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFO};

fn monitor_rect(m: imperio_capture::Monitor) -> RECT {
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: valid HMONITOR from the capture crate and a sized MONITORINFO.
    unsafe {
        let _ = GetMonitorInfoW(HMONITOR(m.as_raw_hmonitor()), &mut info);
    }
    info.rcMonitor
}

/// How a captured BGRA frame reaches the encoder's D3D11 device.
enum FrameTransfer {
    /// Encoder on the same adapter as the display: a keyed-mutex shared
    /// texture hands the frame over on the GPU (fast, zero CPU copy).
    Shared {
        cap_tex: SharedTexture,
        enc_tex: SharedTexture,
    },
    /// Encoder on a different adapter (the integrated GPU's low-latency
    /// encoder): the frame travels through system memory.
    Cpu(CpuBridge),
}

impl FrameTransfer {
    /// Publish the just-captured frame to the encoder device. Called while the
    /// capture frame is still alive; `cap_ctx` is its device context.
    fn upload(
        &self,
        cap_ctx: &windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
        src: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
    ) -> anyhow::Result<()> {
        match self {
            FrameTransfer::Shared { cap_tex, .. } => {
                cap_tex.acquire(0, 100)?;
                unsafe {
                    cap_ctx.CopyResource(&cap_tex.texture, src);
                    cap_ctx.Flush();
                }
                cap_tex.release(1)?;
            }
            FrameTransfer::Cpu(bridge) => bridge.upload(cap_ctx, src)?,
        }
        Ok(())
    }

    /// The encoder-device BGRA texture to feed the colour converter. For the
    /// shared path this acquires the keyed mutex; pair it with
    /// [`release_input`](Self::release_input) after the conversion.
    fn acquire_input(
        &self,
    ) -> anyhow::Result<&windows::Win32::Graphics::Direct3D11::ID3D11Texture2D> {
        match self {
            FrameTransfer::Shared { enc_tex, .. } => {
                enc_tex.acquire(1, 100)?;
                Ok(&enc_tex.texture)
            }
            FrameTransfer::Cpu(bridge) => Ok(bridge.input()),
        }
    }

    fn release_input(&self) -> anyhow::Result<()> {
        if let FrameTransfer::Shared { enc_tex, .. } = self {
            enc_tex.release(0)?;
        }
        Ok(())
    }
}

/// Build the codec device and the frame-transfer path for a capturer. Prefers
/// the integrated GPU's low-latency encoder on discrete-primary displays
/// (bridging frames over), and falls back to the same-adapter shared-texture
/// path if that cannot be set up. Returns a human label for logging.
fn build_codec(cap: &Capturer, force: Option<&str>) -> anyhow::Result<(VideoDevice, FrameTransfer, String)> {
    let (cw, ch) = (cap.width(), cap.height());
    match pick_encoder_adapter(cap.device(), force) {
        Ok(Some((adapter, name))) => {
            let attempt = (|| -> anyhow::Result<(VideoDevice, CpuBridge)> {
                let dev = VideoDevice::on_adapter(&adapter)?;
                let bridge = CpuBridge::new(cap.device(), &dev, cw, ch)?;
                Ok((dev, bridge))
            })();
            match attempt {
                Ok((dev, bridge)) => {
                    return Ok((dev, FrameTransfer::Cpu(bridge), format!("cpu-bridge -> {name}")));
                }
                Err(e) => tracing::warn!(error = %e, "cross-adapter encoder setup failed; using same-adapter path"),
            }
        }
        Ok(None) => {}
        Err(e) => tracing::warn!(error = %e, "encoder adapter selection failed; using same-adapter path"),
    }
    let dev = VideoDevice::matching(cap.device())?;
    let cap_tex = SharedTexture::create(cap.device(), cw, ch)?;
    let enc_tex = cap_tex.open_on(&dev.device)?;
    Ok((dev, FrameTransfer::Shared { cap_tex, enc_tex }, "same-adapter".into()))
}

/// What one capture/encode step yields: the encoded frames, the capture
/// timestamp, whether desktop updates were coalesced, and the capture wait.
type Encoded = Option<(Vec<imperio_codec::EncodedFrame>, u64, bool, Duration)>;

/// Capture + encode for one display. Rebuilt when the viewer switches display
/// or the desktop duplication is lost (secure desktop, resolution change).
struct Pipeline {
    cap: Capturer,
    width: u32,
    height: u32,
    rect: RECT,
    monitor_index: u8,
    monitor_count: u8,
    transfer: FrameTransfer,
    converter: Converter,
    ring: Vec<windows::Win32::Graphics::Direct3D11::ID3D11Texture2D>,
    ring_i: usize,
    last_slot: Option<usize>,
    encoder: MfH264Encoder,
    cursor: Option<cursor::CursorPainter>,
    start: Instant,
}

impl Pipeline {
    fn for_monitor(cfg: &Config, index: u8) -> anyhow::Result<Self> {
        let monitor_count = Capturer::monitors().len().max(1) as u8;
        let (cap, actual) = Capturer::at_index(index as usize)?;
        let monitor_index = actual as u8;
        let (width, height) = (cap.width() & !1, cap.height() & !1);
        let m = cap.monitor();
        let rect = monitor_rect(m);
        // Choose where to encode. Same-adapter (shared texture) when the
        // display's own GPU has a low-latency encoder; otherwise bridge frames
        // to the integrated GPU's encoder, because a discrete GPU's Media
        // Foundation NVENC transform buffers a deep, high-latency pipeline.
        // IMPERIO_ENCODER_ADAPTER=<substr|same> overrides the choice;
        // IMPERIO_ENCODER=<substr> still forces which MF transform is used.
        let force_adapter = std::env::var("IMPERIO_ENCODER_ADAPTER").ok();
        let (dev, transfer, transfer_path) = build_codec(&cap, force_adapter.as_deref())?;
        let converter = Converter::new(&dev, cap.width(), cap.height(), width, height, true)?;
        let ring = (0..4)
            .map(|_| dev.create_texture(width, height, NV12))
            .collect::<Result<Vec<_>, _>>()?;
        let ecfg = EncoderConfig {
            width,
            height,
            fps: cfg.fps,
            bitrate_kbps: cfg.bitrate_kbps,
            ..Default::default()
        };
        let encoder = MfH264Encoder::new(&dev, ecfg)?;
        let cursor = if cfg.show_cursor {
            match cursor::CursorPainter::new(cap.device(), cap.width(), cap.height()) {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::warn!(error = %e, "cursor painter unavailable");
                    None
                }
            }
        } else {
            None
        };
        tracing::info!(encoder = encoder.name(), transfer = %transfer_path, width, height, monitor = monitor_index, of = monitor_count, "pipeline ready");
        Ok(Self {
            cap,
            width,
            height,
            rect,
            monitor_index,
            monitor_count,
            transfer,
            converter,
            ring,
            ring_i: 0,
            last_slot: None,
            encoder,
            cursor,
            start: Instant::now(),
        })
    }

    fn stream_info(&self) -> ControlMessage {
        ControlMessage::StreamInfo {
            width: self.width,
            height: self.height,
            codec: self.encoder.config().codec,
            monitor: self.monitor_index,
            monitors: self.monitor_count,
        }
    }

    fn encode_last_as_keyframe(&mut self) -> anyhow::Result<Encoded> {
        let Some(slot) = self.last_slot else {
            return Ok(None);
        };
        let pts = self.start.elapsed().as_nanos() as i64 / 100;
        let outputs = self.encoder.encode(&self.ring[slot], pts, true)?;
        Ok(Some((outputs, now_us(), false, Duration::ZERO)))
    }

    fn capture_encode(&mut self, timeout: Duration, force_key: bool) -> anyhow::Result<Encoded> {
        let t_cap = Instant::now();
        let (captured_us, coalesced) = match self.cap.acquire(timeout) {
            Ok(frame) => {
                let us = now_us();
                let coalesced = frame.accumulated_frames() > 1;
                // Optionally draw the cursor, then publish to the encoder device.
                let src = match &self.cursor {
                    Some(painter) => painter.paint(frame.texture(), self.rect),
                    None => frame.texture(),
                };
                self.transfer.upload(frame.device_context(), src)?;
                (us, coalesced)
            }
            Err(CaptureError::Timeout) => {
                return if force_key {
                    self.encode_last_as_keyframe()
                } else {
                    Ok(None)
                };
            }
            Err(CaptureError::AccessLost) => {
                // The duplication is gone for good (secure desktop, display
                // mode change, driver reset). The stream size and even the
                // D3D device may differ afterwards, so the caller rebuilds
                // the whole pipeline rather than patching this one.
                anyhow::bail!("desktop duplication lost (desktop switch or display change)")
            }
            Err(e) => return Err(e.into()),
        };
        let cap_wait = t_cap.elapsed();
        let slot = self.ring_i;
        let nv12 = &self.ring[slot];
        self.ring_i = (self.ring_i + 1) % self.ring.len();
        self.last_slot = Some(slot);
        let input = self.transfer.acquire_input()?;
        let conv = self.converter.convert(input, 0, nv12);
        self.transfer.release_input()?;
        conv?;
        let pts = self.start.elapsed().as_nanos() as i64 / 100;
        let outputs = self.encoder.encode(nv12, pts, force_key)?;
        Ok(Some((outputs, captured_us, coalesced, cap_wait)))
    }
}

fn apply_flags(cfg: &mut Config, args: &[String]) -> anyhow::Result<()> {
    let mut it = args.iter();
    while let Some(k) = it.next() {
        match k.as_str() {
            "--service" | "--session-helper" | "--open" | "--no-ui" => {}
            "--force-relay" => {} // handled at session time via a flag below
            "--no-cursor" => cfg.show_cursor = false,
            "--no-clipboard" => cfg.clipboard = false,
            "--port" => cfg.port = it.next().context("--port")?.parse()?,
            "--server" => cfg.server = Some(it.next().context("--server")?.clone()),
            // --code is a one-off session code handled in main(); skip its value here.
            "--code" => {
                it.next();
            }
            "--password" => cfg.password = Some(it.next().context("--password")?.clone()),
            "--bitrate-kbps" => cfg.bitrate_kbps = it.next().context("--bitrate-kbps")?.parse()?,
            "--fps" => cfg.fps = it.next().context("--fps")?.parse()?,
            "--monitor" => cfg.monitor = it.next().context("--monitor")?.parse()?,
            _ => anyhow::bail!("unknown arg {k}"),
        }
    }
    Ok(())
}

/// Live state the host reports (ID, code, addresses, status). Re-exported for
/// the unified `imperio` app, which renders it in the home window.
pub use ui::UiState as HostStatus;

/// Receives every status snapshot the host publishes.
pub type StatusSink = Arc<dyn Fn(&HostStatus) + Send + Sync>;

/// Startup choices for [`run_host`].
pub struct HostOptions {
    pub force_relay: bool,
    pub code_override: Option<String>,
    /// Open mode: no code, but only Tailscale / loopback peers may connect.
    pub open: bool,
}

/// Runtime switches another thread (the home window) can flip while
/// [`run_host`] runs.
#[derive(Clone)]
pub struct HostControls {
    /// Accept new connections (false = paused; current session unaffected).
    pub accepting: Arc<AtomicBool>,
    /// Ask the local user before each session (consent dialog).
    pub ask_first: Arc<AtomicBool>,
    /// Set to request a fresh session code (code mode only); auto-cleared.
    pub regen_code: Arc<AtomicBool>,
    /// Set to make [`run_host`] return.
    pub stop: Arc<AtomicBool>,
}

impl HostControls {
    pub fn new(ask_first: bool) -> Self {
        Self {
            accepting: Arc::new(AtomicBool::new(true)),
            ask_first: Arc::new(AtomicBool::new(ask_first)),
            regen_code: Arc::new(AtomicBool::new(false)),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }
}

fn publish(sink: &Option<StatusSink>, status: &HostStatus) {
    if let Some(s) = sink {
        s(status);
    }
}

/// Command-line entry for the `imperio-host` binary and the `imperio host`
/// subcommand. `raw` is the argument list without the program name.
pub fn cli_main(raw: Vec<String>) -> anyhow::Result<()> {
    // Before any window or screen measurement (see `init_dpi_per_monitor`).
    imperio_ui::init_dpi_per_monitor();

    // Service management subcommands (first positional).
    match raw.first().map(String::as_str) {
        Some("install-service") => {
            init_logging(false);
            return service::install();
        }
        Some("uninstall-service") => {
            init_logging(false);
            return service::uninstall();
        }
        _ => {}
    }
    if service::running_under_scm(&raw) {
        init_logging(true);
        return service::run();
    }

    init_logging(false);
    let force_relay = raw.iter().any(|a| a == "--force-relay");
    let open = raw.iter().any(|a| a == "--open");
    let code_override = raw
        .iter()
        .position(|a| a == "--code")
        .and_then(|i| raw.get(i + 1))
        .cloned();
    if raw.first().map(String::as_str) == Some("--firewall") {
        // Elevated helper launched by the home app's "Allow through firewall".
        ensure_firewall(config::load_or_create().port, true);
        return Ok(());
    }
    let mut cfg = config::load_or_create();
    apply_flags(&mut cfg, &raw)?;
    let open = open || cfg.open_mode;
    let controls = HostControls::new(cfg.require_accept);
    // Status window + tray icon (skip with --no-ui, e.g. for scripted tests).
    let sink: Option<StatusSink> = if raw.iter().any(|a| a == "--no-ui") {
        None
    } else {
        let hui = ui::HostUi::start();
        Some(Arc::new(move |s: &HostStatus| {
            let snapshot = s.clone();
            hui.update(move |t| *t = snapshot);
        }))
    };
    run_host(cfg, HostOptions { force_relay, code_override, open }, controls, sink)
}

fn init_logging(_service: bool) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("info".parse().unwrap()),
        )
        .try_init();
}

/// Run the host until `controls.stop` is set (or an unrecoverable error).
/// Publishes status snapshots to `sink`; blocks the calling thread.
pub fn run_host(
    mut cfg: Config,
    opts: HostOptions,
    controls: HostControls,
    sink: Option<StatusSink>,
) -> anyhow::Result<()> {
    let HostOptions { force_relay, code_override, open } = opts;
    imperio_ui::init_dpi_per_monitor();
    tracing::info!("imperio-host {} on udp/{}", env!("CARGO_PKG_VERSION"), cfg.port);
    ensure_firewall(cfg.port, false);
    imperio_codec::mf::startup()?;
    let mut status = HostStatus::default();
    let mut last_banner = String::new();

    // Persistent device identity: generate and save on first run.
    let device_key = match cfg.identity_key.as_deref().and_then(DeviceKey::from_seed_b64) {
        Some(k) => k,
        None => {
            let k = DeviceKey::generate();
            cfg.identity_key = Some(k.seed_b64());
            let _ = config::save(&cfg);
            k
        }
    };
    let device_key = Arc::new(device_key);
    tracing::info!(fingerprint = %identity::fingerprint(&device_key.public()), "device identity");

    // Decode the optional TOTP secret once.
    let totp_secret = cfg
        .totp_secret
        .as_deref()
        .and_then(imperio_transport::totp::decode_base32);
    if cfg.totp_secret.is_some() && totp_secret.is_none() {
        tracing::warn!("totp_secret is not valid base32; ignoring");
    }
    let totp_secret = Arc::new(totp_secret);

    let server: Option<SocketAddr> = match &cfg.server {
        Some(s) => {
            let s = if s.contains(':') { s.clone() } else { format!("{s}:{}", imperio_protocol::DEFAULT_SIGNAL_PORT) };
            Some(std::net::ToSocketAddrs::to_socket_addrs(&s)?.next().context("resolve server")?)
        }
        None => None,
    };
    let device_id = cfg.device_id;
    let socket = Arc::new(imperio_transport::bind_udp(("0.0.0.0", cfg.port)).context("bind")?);

    let mut failed_handshakes = 0u32;
    // Precedence: open mode (fixed public secret, no code), else configured
    // unattended password, else a fixed --code for this run, else a rotating
    // per-session code.
    let unattended = if open { Some(OPEN_CODE.to_string()) } else { cfg.password.clone() };
    let fixed = unattended.clone().or(code_override);
    let mut code = fixed.clone().unwrap_or_else(random_code);

    loop {
        if controls.stop.load(Ordering::Relaxed) {
            status.status = "Stopped".into();
            publish(&sink, &status);
            return Ok(());
        }
        if controls.regen_code.swap(false, Ordering::Relaxed) && fixed.is_none() {
            code = random_code();
        }
        // Everything a person needs to read out, for the console and the window.
        let security = if open {
            "Open: only your own Tailscale devices can connect (no code); LAN and internet are refused".to_string()
        } else if unattended.is_some() {
            "Unattended password configured".to_string()
        } else {
            format!("Code {}", spaced_code(&code))
        };
        let shown_code = (!open && unattended.is_none()).then(|| code.clone());
        let addresses: Vec<String> = match &server {
            Some(s) => vec![format!("via signal server {s}")],
            None => {
                let mut ips = lan_ips(cfg.port);
                // In open mode only the Tailscale address is usable.
                if open {
                    ips.retain(|(_, label)| label.starts_with("VPN"));
                }
                if ips.is_empty() {
                    if open {
                        vec!["(!) No Tailscale address found. Open mode needs Tailscale running on this PC; start it, then re-run.".into()]
                    } else {
                        vec![format!("This PC's network IP, port {} (run ipconfig)", cfg.port)]
                    }
                } else {
                    ips.iter().map(|(addr, label)| format!("{addr}   ({label})")).collect()
                }
            }
        };
        let accepting = controls.accepting.load(Ordering::Relaxed);
        let mut banner = format!(
            "  Device ID : {}\n  Security  : {security}\n",
            imperio_protocol::device_id::format(device_id)
        );
        for (i, a) in addresses.iter().enumerate() {
            let tag = if i == 0 { "Address   :" } else { "            " };
            banner.push_str(&format!("  {tag} {a}\n"));
        }
        if banner != last_banner {
            println!("\n{banner}");
            last_banner = banner;
        }
        status.device_id = imperio_protocol::device_id::format(device_id);
        status.security = security;
        status.code = shown_code;
        status.addresses = addresses;
        status.status = if accepting {
            "Waiting for a viewer to connect\u{2026}".into()
        } else {
            "Connections paused".into()
        };
        publish(&sink, &status);
        if !accepting {
            std::thread::sleep(Duration::from_millis(250));
            continue;
        }

        let (session, path, first_handshake) = match server {
            Some(server) => {
                // Let the outer loop act on pause / stop / new-code requests.
                let cancel = || {
                    !controls.accepting.load(Ordering::Relaxed)
                        || controls.regen_code.load(Ordering::Relaxed)
                        || controls.stop.load(Ordering::Relaxed)
                };
                let found = rendezvous::host_wait_for_viewer(&socket, server, device_id, &device_key, force_relay, &cancel);
                let (session, path) = match found {
                    Ok(Some(x)) => x,
                    Ok(None) => continue,
                    Err(e) => {
                        // One viewer's failed hole punch (or a server hiccup)
                        // must not end sharing for everyone after it.
                        tracing::warn!(error = format_args!("{e:#}"), "rendezvous failed; retrying");
                        status.status = format!("Signal server problem: {e:#}");
                        publish(&sink, &status);
                        std::thread::sleep(Duration::from_secs(3));
                        continue;
                    }
                };
                let hs = wait_handshake(&socket, Some(path.addr()), Duration::from_secs(5))?;
                (session, path, hs)
            }
            None => {
                let found = loop {
                    // Let the outer loop act on pause / stop / new-code requests.
                    if !controls.accepting.load(Ordering::Relaxed)
                        || controls.regen_code.load(Ordering::Relaxed)
                        || controls.stop.load(Ordering::Relaxed)
                    {
                        break None;
                    }
                    if let Some((hs, from)) = recv_handshake(&socket, Duration::from_secs(1))? {
                        // In open mode, only accept tailnet / loopback peers so
                        // a LAN or internet host cannot connect without a code.
                        if open && !is_tailnet_or_local(from.ip()) {
                            tracing::warn!(%from, "refused non-Tailscale connection (open mode)");
                            continue;
                        }
                        break Some((hs, from));
                    }
                };
                let Some((hs, from)) = found else { continue };
                (hs.session, Path::Direct(from), Some(hs))
            }
        };
        let Some(first_handshake) = first_handshake else {
            tracing::warn!("peer found but no handshake arrived");
            continue;
        };
        if first_handshake.session != session {
            continue;
        }
        // Open mode has no secret, so the network is the only gate: whatever
        // way the peer was found (direct, signal server, relay), it must be a
        // tailnet or loopback address. A relayed or internet peer is refused.
        if open && !is_tailnet_or_local(path.addr().ip()) {
            tracing::warn!(peer = %path.addr(), "refused non-Tailscale connection (open mode)");
            continue;
        }

        let ours = Handshake::new(&code, session);
        socket.send_to(&HandshakeMsg { session, spake_msg: ours.outbound.clone() }.encode(), path.addr())?;
        let keys = match ours.finish(&first_handshake.spake_msg, true) {
            Ok(k) => k,
            Err(e) => {
                tracing::warn!(error = %e, "handshake failed");
                continue;
            }
        };
        let link = Arc::new(SecureLink::new(socket.clone(), path.addr(), keys));
        tracing::info!(peer = %path.addr(), relay = matches!(path, Path::Relay(_)), "link up; awaiting authenticated Hello");
        status.status = format!("Viewer connected from {}", path.addr());
        publish(&sink, &status);

        let _ = session;
        let ctx = SessionCtx {
            cfg: &cfg,
            device_key: device_key.clone(),
            totp_secret: totp_secret.clone(),
            ask_first: controls.ask_first.load(Ordering::Relaxed),
        };
        match run_session(ctx, link, &socket) {
            Ok(SessionEnd::Bye) => failed_handshakes = 0,
            Ok(SessionEnd::NoHello) | Ok(SessionEnd::Rejected) => {
                failed_handshakes += 1;
                tracing::warn!(failed_handshakes, "authentication failed (wrong code / 2FA?)");
                if fixed.is_none() && failed_handshakes >= 3 {
                    code = random_code();
                    tracing::warn!("too many failed attempts; new code issued");
                }
                // Escalating backoff throttles online brute force.
                let backoff = Duration::from_secs((1u64 << failed_handshakes.min(5)).min(30));
                std::thread::sleep(backoff);
            }
            Ok(SessionEnd::Timeout) => tracing::warn!("viewer stopped reporting; session closed"),
            Err(e) => tracing::error!(error = format_args!("{e:#}"), "session error"),
        }
    }
}

/// Per-session security context threaded into `run_session`.
struct SessionCtx<'a> {
    cfg: &'a Config,
    /// Ask the local user (consent dialog) before streaming this session.
    ask_first: bool,
    device_key: Arc<DeviceKey>,
    totp_secret: Arc<Option<Vec<u8>>>,
}

/// Tailscale / CGNAT address range (100.64.0.0/10). In open mode only peers
/// from here (or loopback, for local testing) may connect, so an open host is
/// reachable by your own tailnet devices but not by your LAN or the internet.
fn is_tailnet_or_local(ip: std::net::IpAddr) -> bool {
    if ip.is_loopback() {
        return true;
    }
    matches!(ip, std::net::IpAddr::V4(v4) if {
        let o = v4.octets();
        o[0] == 100 && (64..=127).contains(&o[1])
    })
}

/// "123 456" for the usual six-digit code; anything else (a fixed `--code`)
/// is shown as it is.
fn spaced_code(code: &str) -> String {
    if code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit()) {
        format!("{} {}", &code[..3], &code[3..])
    } else {
        code.to_string()
    }
}

/// Run `netsh` from the Windows system directory (never from the current
/// directory or PATH, where a planted `netsh.exe` could be picked up — this
/// also runs elevated) and without flashing a console window.
fn netsh(args: &[&str]) -> std::io::Result<std::process::Output> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let root = std::env::var_os("SystemRoot")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"));
    std::process::Command::new(root.join("System32").join("netsh.exe"))
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
}

/// Open an inbound Windows Firewall rule for the host's UDP port, so viewers
/// on the network can reach it. Adding a rule needs administrator rights; an
/// existing rule for this port is left alone unless `force` is set (the
/// elevated "Allow through firewall" helper).
fn ensure_firewall(port: u16, force: bool) {
    const NAME: &str = "name=Imperio Host";
    let port_arg = format!("localport={port}");
    if !force {
        if let Ok(o) = netsh(&["advfirewall", "firewall", "show", "rule", NAME]) {
            // The port number is the one part of the listing that is not
            // translated, so this works on every display language.
            if o.status.success() && String::from_utf8_lossy(&o.stdout).contains(&port.to_string()) {
                return;
            }
        }
    }
    // Remove any previous rule so re-runs don't stack duplicates.
    let _ = netsh(&["advfirewall", "firewall", "delete", "rule", NAME]);
    let out = netsh(&[
        "advfirewall",
        "firewall",
        "add",
        "rule",
        NAME,
        "dir=in",
        "action=allow",
        "protocol=UDP",
        &port_arg,
    ]);
    match out {
        Ok(o) if o.status.success() => tracing::info!(port, "firewall opened for inbound UDP"),
        _ => tracing::info!(
            port,
            "no firewall rule yet (needs administrator rights): use \"Allow through firewall\" if other computers cannot connect"
        ),
    }
}

/// This machine's usable LAN IPv4 address(es) with the host port, for the
/// viewer to connect to. Uses the "which interface routes to the internet"
/// trick, then adds any other private IPv4s as fallbacks.
fn lan_ips(port: u16) -> Vec<(String, &'static str)> {
    use std::net::{IpAddr, Ipv4Addr, ToSocketAddrs};
    // A Tailscale / CGNAT address (100.64.0.0/10) reaches the host from any
    // network, so it is the most useful one and is listed first.
    fn is_vpn(v4: Ipv4Addr) -> bool {
        let o = v4.octets();
        o[0] == 100 && (64..=127).contains(&o[1])
    }
    fn keep(ip: IpAddr) -> Option<(Ipv4Addr, &'static str)> {
        match ip {
            IpAddr::V4(v4) if is_vpn(v4) => Some((v4, "VPN / Tailscale - works from other networks")),
            IpAddr::V4(v4) if v4.is_private() => Some((v4, "same Wi-Fi / network only")),
            _ => None,
        }
    }
    let mut out: Vec<(Ipv4Addr, &'static str)> = Vec::new();
    let mut add = |v4: Ipv4Addr, label| {
        if !out.iter().any(|(e, _)| *e == v4) {
            out.push((v4, label));
        }
    };
    // Primary: the source address the OS picks to reach the internet.
    if let Ok(probe) = UdpSocket::bind("0.0.0.0:0") {
        if probe.connect("8.8.8.8:80").is_ok() {
            if let Ok(local) = probe.local_addr() {
                if let Some((v4, l)) = keep(local.ip()) {
                    add(v4, l);
                }
            }
        }
    }
    // The rest: resolve this machine's hostname to its other IPv4s (Ethernet,
    // Wi-Fi, and the Tailscale adapter all show up here).
    if let Ok(host) = std::env::var("COMPUTERNAME") {
        if let Ok(addrs) = (host.as_str(), port).to_socket_addrs() {
            for a in addrs {
                if let Some((v4, l)) = keep(a.ip()) {
                    add(v4, l);
                }
            }
        }
    }
    // VPN addresses first (they are the ones that work across networks).
    out.sort_by_key(|(_, label)| !label.starts_with("VPN"));
    out.into_iter().map(|(ip, label)| (format!("{ip}:{port}"), label)).collect()
}

fn recv_handshake(socket: &UdpSocket, timeout: Duration) -> anyhow::Result<Option<(HandshakeMsg, SocketAddr)>> {
    Ok(rendezvous::recv_kind(socket, Kind::Handshake, timeout)?
        .and_then(|(body, from)| HandshakeMsg::decode(&body).ok().map(|m| (m, from))))
}

fn wait_handshake(socket: &UdpSocket, expect_from: Option<SocketAddr>, timeout: Duration) -> anyhow::Result<Option<HandshakeMsg>> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some((m, from)) = recv_handshake(socket, Duration::from_millis(500))? {
            if expect_from.is_none_or(|e| e == from) {
                return Ok(Some(m));
            }
        }
    }
    Ok(None)
}

enum SessionEnd {
    Bye,
    NoHello,
    /// Authenticated the code but failed a later check (TOTP, consent).
    Rejected,
    Timeout,
}

enum Event {
    Hello,
    /// The viewer speaks a different protocol version.
    Incompatible(u16),
    Totp(bool),
    KeyframeRequest,
    SelectMonitor(u8),
    Bye,
}

/// Tell the viewer why it is being turned away, then close. Best effort: the
/// viewer shows the reason instead of a window that never fills in.
fn deny(link: &SecureLink, reason: &str) {
    let _ = link.send_control(&ControlMessage::Denied(reason.to_string()));
    let _ = link.send_control(&ControlMessage::Bye);
}

fn run_session(ctx: SessionCtx, link: Arc<SecureLink>, socket: &Arc<UdpSocket>) -> anyhow::Result<SessionEnd> {
    let cfg = ctx.cfg;
    // Create the injector from a cheap monitor-rect lookup and start the
    // receive thread *before* the slow encoder/pipeline build, so Ping/Pong
    // (hence the RTT estimate) stays responsive from the first moment instead
    // of being answered only after the GPU encoder is ready.
    let mons = Capturer::monitors();
    let rect0 = mons
        .get(cfg.monitor as usize)
        .or_else(|| mons.first())
        .map(|m| monitor_rect(*m))
        .unwrap_or(RECT { left: 0, top: 0, right: 1920, bottom: 1080 });
    let injector = Arc::new(input::Injector::new(
        rect0.left,
        rect0.top,
        rect0.right - rect0.left,
        rect0.bottom - rect0.top,
    ));

    let running = Arc::new(AtomicBool::new(true));
    // The gate. Holding the session keys only proves the peer knew the code.
    // Until the second factor and the local "Yes" (when configured) have also
    // passed, nothing the peer sends may act on this computer: otherwise it
    // could type into the consent dialog that is asking about it.
    let authorized = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<Event>();
    let target_kbps = Arc::new(AtomicU32::new(cfg.bitrate_kbps));
    let last_feedback_us = Arc::new(AtomicU64::new(now_us()));
    // Clipboard sync starts only once the session is authorized (see below).
    let clipboard: Arc<OnceLock<ClipboardSync>> = Arc::new(OnceLock::new());

    let rx_thread = {
        let link = link.clone();
        let socket = socket.clone();
        let running = running.clone();
        let authorized = authorized.clone();
        let injector = injector.clone();
        let target_kbps = target_kbps.clone();
        let last_feedback_us = last_feedback_us.clone();
        let clipboard = clipboard.clone();
        let totp_secret = ctx.totp_secret.clone();
        let mut cc = BitrateController::new(cfg.bitrate_kbps, 500, cfg.bitrate_kbps);
        std::thread::Builder::new().name("session-rx".into()).spawn(move || {
            socket.set_read_timeout(Some(Duration::from_millis(50))).ok();
            let mut buf = vec![0u8; 2048];
            let mut clip_in = clip::Reassembler::default();
            let mut last_desktop_check = Instant::now() - Duration::from_secs(1);
            // Attach to the active input desktop up front.
            injector.follow_input_desktop();
            while running.load(Ordering::Relaxed) {
                // Keep input going to the right desktop (UAC / lock screen).
                if last_desktop_check.elapsed() > Duration::from_millis(200) {
                    injector.follow_input_desktop();
                    last_desktop_check = Instant::now();
                }
                let Ok((n, from)) = socket.recv_from(&mut buf) else { continue };
                if from != link.peer() {
                    continue;
                }
                let now = Instant::now();
                match link.handle_datagram(&buf[..n], now) {
                    Some(Incoming::Control(msg)) => {
                        last_feedback_us.store(now_us(), Ordering::Relaxed);
                        let authed = authorized.load(Ordering::Acquire);
                        match msg {
                            // --- allowed before authorization: the handshake
                            // itself and liveness. None of these touch the PC.
                            ControlMessage::Hello { version } => {
                                if version == PROTOCOL_VERSION {
                                    let _ = tx.send(Event::Hello);
                                } else {
                                    tracing::warn!(version, "viewer protocol mismatch");
                                    let _ = tx.send(Event::Incompatible(version));
                                }
                            }
                            ControlMessage::Totp(code) => {
                                let ok = totp_secret
                                    .as_ref()
                                    .as_ref()
                                    .map(|sec| imperio_transport::totp::check(sec, &code, imperio_transport::totp::now_secs()))
                                    .unwrap_or(true);
                                let _ = tx.send(Event::Totp(ok));
                            }
                            ControlMessage::Ping { t_us } => {
                                let _ = link.send_control(&ControlMessage::Pong { t_us });
                            }
                            ControlMessage::Bye => {
                                let _ = tx.send(Event::Bye);
                            }
                            // --- everything below acts on this computer.
                            _ if !authed => {
                                tracing::debug!("ignored a control message sent before authorization")
                            }
                            ControlMessage::RequestKeyframe => {
                                let _ = tx.send(Event::KeyframeRequest);
                            }
                            ControlMessage::SelectMonitor(n) => {
                                let _ = tx.send(Event::SelectMonitor(n));
                            }
                            ControlMessage::Clipboard(text) => {
                                if let Some(cb) = clipboard.get() {
                                    cb.apply_remote(text);
                                }
                            }
                            ControlMessage::ClipboardChunk { id, index, count, data } => {
                                if let (Some(cb), Some(text)) = (clipboard.get(), clip_in.push(id, index, count, data)) {
                                    cb.apply_remote(text);
                                }
                            }
                            ControlMessage::Input(ev) => injector.inject(ev),
                            other => tracing::debug!(?other, "unexpected control message"),
                        }
                    }
                    Some(Incoming::Report(r)) => {
                        last_feedback_us.store(now_us(), Ordering::Relaxed);
                        if let Some(kbps) = cc.on_report(&r, now) {
                            target_kbps.store(kbps, Ordering::Relaxed);
                        }
                    }
                    _ => {}
                }
            }
            // Never leave a key or mouse button held down on this computer
            // because the viewer vanished mid-press.
            injector.release_all();
        })?
    };

    let result = (|| -> anyhow::Result<SessionEnd> {
        // Gate: authenticated Hello, then optional TOTP, then optional consent.
        if let Err(end) = authenticate(&rx, &link, ctx.totp_secret.is_some(), ctx.ask_first) {
            return Ok(end);
        }
        authorized.store(true, Ordering::Release);
        // Prove device identity so the viewer can pin it (TOFU).
        let sig = ctx.device_key.sign_session(link.session_keys());
        link.send_control(&ControlMessage::Identity {
            device_id: cfg.device_id,
            public: ctx.device_key.public().to_vec(),
            sig: sig.to_vec(),
        })?;

        // Clipboard: host -> viewer on local change. Started only now, so
        // nothing copied on this computer reaches a peer that was refused.
        if cfg.clipboard {
            let link = link.clone();
            let next_id = AtomicU32::new(1);
            let _ = clipboard.set(ClipboardSync::spawn(MAX_CLIPBOARD_BYTES, move |text| {
                clip::send(&link, next_id.fetch_add(1, Ordering::Relaxed), text);
            }));
        }

        // Build the capture + encode pipeline now that the peer is trusted.
        let pipeline = Pipeline::for_monitor(cfg, cfg.monitor)?;
        injector.set_monitor(
            pipeline.rect.left,
            pipeline.rect.top,
            pipeline.rect.right - pipeline.rect.left,
            pipeline.rect.bottom - pipeline.rect.top,
        );
        // Stream system audio alongside video (host -> viewer). Runs on its own
        // thread and stops when `running` clears at session end. Audio failure
        // never blocks the video session.
        if cfg.send_audio {
            let link_a = link.clone();
            let running_a = running.clone();
            std::thread::Builder::new()
                .name("audio-send".into())
                .spawn(move || match imperio_audio::capture::AudioCapture::start() {
                    Ok(cap) => {
                        let mut enc = PcmCodec;
                        let mut seq: u32 = 0;
                        tracing::info!(codec = ?enc.codec(), "audio capture started");
                        while running_a.load(Ordering::Relaxed) {
                            if let Some(frame) = cap.try_recv() {
                                if let Ok(payload) = enc.encode(&frame) {
                                    // Keep each datagram under the MTU budget.
                                    // Raw PCM (> MTU) splits on 4-byte sample
                                    // bounds; compressed frames fit in one.
                                    let codec = enc.codec() as u8;
                                    let ts = now_us();
                                    for chunk in payload.chunks(imperio_audio::MAX_AUDIO_PAYLOAD) {
                                        let _ = link_a.send_audio(codec, seq, ts, chunk);
                                        seq = seq.wrapping_add(1);
                                    }
                                }
                            } else {
                                std::thread::sleep(Duration::from_millis(2));
                            }
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "audio capture unavailable; continuing without audio"),
                })
                .ok();
        }
        stream_loop(cfg, pipeline, &injector, &link, &rx, &target_kbps, &last_feedback_us)
    })();

    authorized.store(false, Ordering::Release);
    // Let the viewer close (with a reason, if there is one) rather than sit
    // in front of a frozen picture.
    match &result {
        Ok(SessionEnd::Bye) => {}
        Ok(_) => {
            let _ = link.send_control(&ControlMessage::Bye);
        }
        Err(e) => deny(&link, &format!("The other computer could not share its screen ({e:#}).")),
    }
    running.store(false, Ordering::Relaxed);
    let _ = rx_thread.join();
    result
}

/// Wait for the authenticated Hello, then (if required) a valid TOTP, then (if
/// configured) local consent. Returns `Err(end)` to abort the session; the
/// viewer has been told why by then.
fn authenticate(
    rx: &mpsc::Receiver<Event>,
    link: &SecureLink,
    totp_required: bool,
    require_accept: bool,
) -> Result<(), SessionEnd> {
    let peer = link.peer();
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut hello = false;
    let mut totp_ok = !totp_required;
    while !(hello && totp_ok) {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Hello) => hello = true,
            Ok(Event::Incompatible(version)) => {
                deny(
                    link,
                    &format!(
                        "The two computers run different Imperio versions (protocol {version} here, {PROTOCOL_VERSION} there). Update Imperio on both."
                    ),
                );
                // Not a guessing attempt: the peer did know the code.
                return Err(SessionEnd::Bye);
            }
            Ok(Event::Totp(ok)) => {
                if !ok {
                    tracing::warn!(%peer, "second factor (TOTP) rejected");
                    deny(link, "The two-step code was wrong or has expired.");
                    return Err(SessionEnd::Rejected);
                }
                totp_ok = true;
            }
            Ok(Event::Bye) => return Err(SessionEnd::Bye),
            Ok(_) => {}
            Err(_) if Instant::now() > deadline => {
                if hello {
                    deny(link, "That computer also asks for a two-step (authenticator) code.");
                }
                return Err(SessionEnd::NoHello);
            }
            Err(_) => {}
        }
    }
    // A prominent notice on every accepted connection (scam awareness).
    println!();
    println!("  ⚠  Incoming remote session from {peer}.");
    println!("     Only allow people you trust and contacted yourself. No");
    println!("     legitimate company (bank, Microsoft, tax office) will ask");
    println!("     you to install this or read out your code.");
    tracing::warn!(%peer, "remote session starting");
    if require_accept && !consent::ask(peer) {
        tracing::warn!(%peer, "remote session refused by local user");
        deny(link, "The person at the other computer declined the connection.");
        return Err(SessionEnd::Rejected);
    }
    Ok(())
}

/// How long a session waits for the screen to become capturable again (an
/// elevation prompt or lock screen that this process may not capture, a
/// display being reconfigured) before giving up.
const RECAPTURE_GIVE_UP: Duration = Duration::from_secs(120);

fn stream_loop(
    cfg: &Config,
    first: Pipeline,
    injector: &Arc<input::Injector>,
    link: &Arc<SecureLink>,
    rx: &mpsc::Receiver<Event>,
    target_kbps: &Arc<AtomicU32>,
    last_feedback_us: &Arc<AtomicU64>,
) -> anyhow::Result<SessionEnd> {
    tracing::info!("viewer authenticated; streaming");
    link.send_control(&first.stream_info())?;

    // `None` while the pipeline is being rebuilt (display switch, or the
    // desktop duplication was lost). The old one is dropped *before* a new
    // one is created so the two never hold the same display at once.
    let mut monitor = first.monitor_index;
    let mut slot = Some(first);
    let mut lost_since: Option<Instant> = None;

    let mut force_key = true;
    let mut frame_id = 0u32;
    let mut stats = Stats::default();
    let mut current_kbps = cfg.bitrate_kbps;
    let mut pacer = Pacer::new(current_kbps, 25_000);
    let frame_timeout = Duration::from_millis(1000 / cfg.fps.max(1) as u64 + 1);

    loop {
        while let Ok(ev) = rx.try_recv() {
            match ev {
                Event::KeyframeRequest => force_key = true,
                Event::Bye => return Ok(SessionEnd::Bye),
                Event::Totp(_) | Event::Incompatible(_) => {}
                Event::Hello => {
                    force_key = true;
                    if let Some(p) = &slot {
                        link.send_control(&p.stream_info())?;
                    }
                }
                Event::SelectMonitor(n) => {
                    if n != monitor {
                        monitor = n;
                        slot = None;
                    }
                }
            }
        }
        if now_us().saturating_sub(last_feedback_us.load(Ordering::Relaxed)) > 5_000_000 {
            return Ok(SessionEnd::Timeout);
        }

        let Some(pipeline) = slot.as_mut() else {
            match Pipeline::for_monitor(cfg, monitor) {
                Ok(p) => {
                    monitor = p.monitor_index;
                    injector.set_monitor(
                        p.rect.left,
                        p.rect.top,
                        p.rect.right - p.rect.left,
                        p.rect.bottom - p.rect.top,
                    );
                    link.send_control(&p.stream_info())?;
                    tracing::info!(monitor, "capture pipeline (re)built");
                    // A fresh encoder starts at the configured bitrate; the
                    // adaptive target is re-applied on the next pass.
                    current_kbps = cfg.bitrate_kbps;
                    pacer.set_target(current_kbps, 25_000);
                    force_key = true;
                    slot = Some(p);
                }
                Err(e) => {
                    let since = *lost_since.get_or_insert_with(Instant::now);
                    if since.elapsed() > RECAPTURE_GIVE_UP {
                        return Err(e.context("the screen could not be captured"));
                    }
                    tracing::debug!(error = format_args!("{e:#}"), "screen not capturable yet; retrying");
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
            continue;
        };

        let want = target_kbps.load(Ordering::Relaxed);
        if want != current_kbps && p_set_bitrate(pipeline, want) {
            current_kbps = want;
            pacer.set_target(want, 25_000);
        }

        let step = match pipeline.capture_encode(frame_timeout, force_key) {
            Ok(step) => step,
            Err(e) => {
                tracing::warn!(error = format_args!("{e:#}"), "capture pipeline failed; rebuilding");
                lost_since.get_or_insert_with(Instant::now);
                slot = None;
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        lost_since = None;
        let Some((outputs, captured_us, coalesced, cap_wait)) = step else {
            continue;
        };
        force_key = false;
        stats.coalesced += coalesced as u64;
        for out in outputs {
            frame_id = frame_id.wrapping_add(1);
            for c in lan::split(frame_id, out.keyframe, captured_us, &out.data) {
                pacer.wait_for(c.len() + 40);
                link.send_media(&c, now_us())?;
            }
            link.send_media(&[], now_us())?;
            link.send_media(&[], now_us())?;
            stats.record(cap_wait, out.encode_time, out.data.len(), out.keyframe);
        }
        stats.maybe_report(link, current_kbps);
    }
}

fn p_set_bitrate(p: &mut Pipeline, kbps: u32) -> bool {
    match p.encoder.set_bitrate(kbps) {
        Ok(()) => {
            tracing::info!(kbps, "bitrate adapted");
            true
        }
        Err(e) => {
            tracing::debug!(error = %e, "set_bitrate");
            false
        }
    }
}

#[derive(Default)]
struct Stats {
    frames: u64,
    bytes: u64,
    keyframes: u64,
    coalesced: u64,
    cap_total: Duration,
    enc_total: Duration,
    enc_max: Duration,
    last: Option<Instant>,
}

impl Stats {
    fn record(&mut self, cap: Duration, enc: Duration, bytes: usize, key: bool) {
        self.frames += 1;
        self.bytes += bytes as u64;
        self.keyframes += key as u64;
        self.cap_total += cap;
        self.enc_total += enc;
        self.enc_max = self.enc_max.max(enc);
    }

    fn maybe_report(&mut self, link: &SecureLink, kbps: u32) {
        let now = Instant::now();
        let last = *self.last.get_or_insert(now);
        if now.duration_since(last) < Duration::from_secs(2) {
            return;
        }
        let secs = now.duration_since(last).as_secs_f64();
        if self.frames > 0 {
            let s = link.stats();
            tracing::info!(
                fps = format_args!("{:.1}", self.frames as f64 / secs),
                video_mbps = format_args!("{:.2}", self.bytes as f64 * 8.0 / 1e6 / secs),
                target_kbps = kbps,
                keyframes = self.keyframes,
                enc_ms = format_args!("{:.2}", self.enc_total.as_secs_f64() * 1e3 / self.frames as f64),
                retransmits = s.retransmits,
                "stream"
            );
        }
        *self = Stats { last: Some(now), ..Default::default() };
    }
}
