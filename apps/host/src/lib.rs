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
use imperio_protocol::{ControlMessage, FEATURE_INPUT_BATCH, MAX_CLIPBOARD_BYTES, OPEN_CODE, PROTOCOL_VERSION};
use imperio_transport::cc::BitrateController;
use imperio_transport::crypto::{random_code, Handshake};
use imperio_transport::identity::{self, DeviceKey};
use imperio_transport::link::{Incoming, SecureLink};
use imperio_transport::pacer::Pacer;
use imperio_transport::rendezvous::{self, Path};
use imperio_transport::reliable::Reliable;
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

/// What one capture/encode step yields: the encoded frames, each with the
/// time its own picture was captured, whether desktop updates were
/// coalesced, and the capture wait.
type Encoded = Option<(Vec<(imperio_codec::EncodedFrame, u64)>, bool, Duration, StageTimes)>;

/// Time spent in each step between the screen changing and the encoder
/// being handed the picture (shown in the periodic `stream` log line).
#[derive(Default, Clone, Copy)]
struct StageTimes {
    cursor: Duration,
    upload: Duration,
    convert: Duration,
}

/// Capture + encode for one display. Rebuilt when the viewer switches display
/// or the desktop duplication is lost (secure desktop, resolution change).
struct Pipeline {
    cap: Capturer,
    /// The device the picture is converted and encoded on.
    dev: VideoDevice,
    /// Size of the picture as sent.
    width: u32,
    height: u32,
    /// Size of the screen itself (see [`SIZE_STEPS`]).
    native: (u32, u32),
    /// Index into [`SIZE_STEPS`] of the size being sent.
    size_step: usize,
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
    /// Pictures handed to the encoder and not yet returned: (encoder time
    /// stamp, capture wall-clock time).
    in_encoder: std::collections::VecDeque<(i64, u64)>,
}

/// The picture is sent at the screen's own size, or at three quarters or half
/// of it each way when the connection carries too little for that. A full HD
/// screen at 1 Mbit/s cannot be encoded small enough at full size: every
/// frame is large, so few get through and the picture moves in jerks. A
/// smaller picture is softer but moves.
const SIZE_STEPS: [f64; 3] = [1.0, 0.75, 0.5];

/// A size needs about this many kbit/s per million pixels sent to look
/// reasonable and keep moving (full HD at full size: about 1.9 Mbit/s).
const KBPS_PER_MEGAPIXEL: f64 = 900.0;

/// The size sent at step `step` for a screen of `native` pixels (even numbers,
/// as the encoder requires).
fn stepped_size(native: (u32, u32), step: usize) -> (u32, u32) {
    let f = SIZE_STEPS[step.min(SIZE_STEPS.len() - 1)];
    let side = |v: u32| (((v as f64 * f).round() as u32) & !1).max(2);
    (side(native.0), side(native.1))
}

/// Chooses the size step from the bitrate the connection is carrying.
///
/// Changing size restarts the encoder and sends a full picture, so it must
/// not happen on a passing dip: the size goes down only when the target has
/// stayed too low for a couple of seconds, and comes back up only when it has
/// been comfortably high for longer, and not soon after the last change.
///
/// A high target is not proof that the path carries a larger picture: a
/// small picture of a quiet screen uses a fraction of its target, and the
/// target drifts up unchallenged. Going up is therefore a trial. One that
/// fails (the size has to come down again shortly after) goes back to the
/// size before it and doubles the wait before that larger size is tried
/// again, so a path that is simply slow is tried less and less often
/// instead of every few seconds.
/// A change of picture size decided by [`SizeChooser`].
struct SizeChange {
    step: usize,
    /// The bitrate target to continue from, when the one in force says
    /// little about the new size: a larger picture starts at what it needs
    /// and no more (the target reached with the smaller one was never put
    /// to the test), and after a failed trial the smaller picture gets back
    /// what it had, not what the target collapsed to.
    target_kbps: Option<u32>,
}

struct SizeChooser {
    step: usize,
    low_since: Option<Instant>,
    high_since: Option<Instant>,
    changed: Instant,
    /// When the size last went up, until that has proved to hold.
    went_up: Option<Instant>,
    /// How long after a change the size may go up to each step.
    up_wait: [Duration; SIZE_STEPS.len()],
    /// `IMPERIO_FIXED_SIZE` is set: always the screen's own size (for
    /// comparing with and without).
    fixed: bool,
}

impl SizeChooser {
    const DOWN_AFTER: Duration = Duration::from_secs(2);
    const UP_AFTER: Duration = Duration::from_secs(6);
    const SETTLE: Duration = Duration::from_secs(10);
    /// A larger size that lasted this long was the right call.
    const PROVEN: Duration = Duration::from_secs(45);
    /// Longest wait between trials of a larger size.
    const LONGEST_WAIT: Duration = Duration::from_secs(300);
    /// The target must exceed what the larger size needs by this much.
    const UP_MARGIN: f64 = 1.3;

    fn new(now: Instant) -> Self {
        Self {
            step: 0,
            low_since: None,
            high_since: None,
            changed: now,
            went_up: None,
            up_wait: [Self::SETTLE; SIZE_STEPS.len()],
            fixed: std::env::var_os("IMPERIO_FIXED_SIZE").is_some_and(|v| !v.is_empty()),
        }
    }

    /// kbit/s that sending `native` at `step` needs.
    fn needs(native: (u32, u32), step: usize) -> f64 {
        let (w, h) = stepped_size(native, step);
        w as f64 * h as f64 / 1e6 * KBPS_PER_MEGAPIXEL
    }

    /// What a size is started at: a little more than it needs.
    fn starting_kbps(native: (u32, u32), step: usize) -> f64 {
        Self::needs(native, step) * 1.1
    }

    /// Feed the current target; returns the change to make, if any.
    fn update(&mut self, target_kbps: u32, native: (u32, u32), now: Instant) -> Option<SizeChange> {
        if self.fixed {
            return None;
        }
        let kbps = target_kbps as f64;
        let too_low = self.step + 1 < SIZE_STEPS.len() && kbps < Self::needs(native, self.step);
        let roomy = self.step > 0 && kbps > Self::needs(native, self.step - 1) * Self::UP_MARGIN;
        self.low_since = if too_low { self.low_since.or(Some(now)) } else { None };
        self.high_since = if roomy { self.high_since.or(Some(now)) } else { None };
        let held = |since: Option<Instant>, long: Duration| since.is_some_and(|t| now.duration_since(t) >= long);
        if held(self.went_up, Self::PROVEN) {
            self.went_up = None;
            self.up_wait[self.step] = Self::SETTLE;
        }
        let mut restart = None;
        // A size on trial is given up quickly: every moment it stays, the
        // picture stutters.
        let down_after = if self.went_up.is_some() { Duration::from_millis(700) } else { Self::DOWN_AFTER };
        let next = if held(self.low_since, down_after) {
            let mut step = self.step + 1;
            if self.went_up.take().is_some() {
                // The trial of this size failed: back to the one before it.
                // (The target has collapsed with it and says little.)
                self.up_wait[self.step] = (self.up_wait[self.step] * 2).min(Self::LONGEST_WAIT);
                restart = Some(kbps.max(Self::starting_kbps(native, step)));
            } else {
                // Straight to a size this target is enough for, not one
                // step (and one full picture) at a time.
                while step + 1 < SIZE_STEPS.len() && kbps < Self::needs(native, step) {
                    step += 1;
                }
            }
            step
        } else if held(self.high_since, Self::UP_AFTER) && now.duration_since(self.changed) >= self.up_wait[self.step - 1] {
            self.went_up = Some(now);
            restart = Some(kbps.min(Self::starting_kbps(native, self.step - 1)));
            self.step - 1
        } else {
            return None;
        };
        self.step = next;
        self.low_since = None;
        self.high_since = None;
        self.changed = now;
        Some(SizeChange { step: next, target_kbps: restart.map(|k| k as u32) })
    }
}

impl Pipeline {
    /// `size_step` and `kbps` are where the session stands: a pipeline
    /// rebuilt in the middle of one (display switch, lock screen) carries on
    /// at the size and rate the connection was carrying.
    fn for_monitor(cfg: &Config, index: u8, size_step: usize, kbps: u32) -> anyhow::Result<Self> {
        let monitor_count = Capturer::monitors().len().max(1) as u8;
        let (cap, actual) = Capturer::at_index(index as usize)?;
        let monitor_index = actual as u8;
        let native = (cap.width() & !1, cap.height() & !1);
        let (width, height) = stepped_size(native, size_step);
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
        let ecfg = EncoderConfig { width, height, fps: cfg.fps, bitrate_kbps: kbps, ..Default::default() };
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
            dev,
            width,
            height,
            native,
            size_step,
            rect,
            monitor_index,
            monitor_count,
            transfer,
            converter,
            ring,
            ring_i: 0,
            last_slot: None,
            in_encoder: Default::default(),
            encoder,
            cursor,
            start: Instant::now(),
        })
    }

    /// Send the same screen at another size: a new converter and encoder on
    /// the device already in use. The capture itself is left alone, so the
    /// change costs one full picture and no pause. On failure nothing has
    /// changed.
    fn resize(&mut self, cfg: &Config, size_step: usize, kbps: u32) -> anyhow::Result<()> {
        let (width, height) = stepped_size(self.native, size_step);
        let converter = Converter::new(&self.dev, self.cap.width(), self.cap.height(), width, height, true)?;
        let ring = (0..4)
            .map(|_| self.dev.create_texture(width, height, NV12))
            .collect::<Result<Vec<_>, _>>()?;
        let ecfg = EncoderConfig { width, height, fps: cfg.fps, bitrate_kbps: kbps, ..Default::default() };
        let encoder = MfH264Encoder::new(&self.dev, ecfg)?;
        self.converter = converter;
        self.ring = ring;
        self.ring_i = 0;
        self.last_slot = None;
        self.in_encoder.clear();
        self.encoder = encoder;
        self.width = width;
        self.height = height;
        self.size_step = size_step;
        Ok(())
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

    /// Pair each encoded frame with the capture time of its own picture.
    fn stamp(&mut self, outputs: Vec<imperio_codec::EncodedFrame>) -> Vec<(imperio_codec::EncodedFrame, u64)> {
        outputs
            .into_iter()
            .map(|out| {
                let mut captured = now_us();
                while let Some(&(pts, us)) = self.in_encoder.front() {
                    if pts > out.pts_100ns {
                        break;
                    }
                    captured = us;
                    self.in_encoder.pop_front();
                }
                (out, captured)
            })
            .collect()
    }

    fn encode_last_as_keyframe(&mut self) -> anyhow::Result<Encoded> {
        let Some(slot) = self.last_slot else {
            return Ok(None);
        };
        let pts = self.start.elapsed().as_nanos() as i64 / 100;
        self.in_encoder.push_back((pts, now_us()));
        let outputs = self.encoder.encode(&self.ring[slot], pts, true)?;
        Ok(Some((self.stamp(outputs), false, Duration::ZERO, StageTimes::default())))
    }

    fn capture_encode(&mut self, timeout: Duration, force_key: bool) -> anyhow::Result<Encoded> {
        let t_cap = Instant::now();
        let mut stages = StageTimes::default();
        let (captured_us, coalesced) = match self.cap.acquire(timeout) {
            Ok(frame) => {
                let us = now_us();
                let coalesced = frame.accumulated_frames() > 1;
                // Optionally draw the cursor, then publish to the encoder device.
                let t = Instant::now();
                let src = match &self.cursor {
                    Some(painter) => painter.paint(frame.texture(), self.rect),
                    None => frame.texture(),
                };
                stages.cursor = t.elapsed();
                let t = Instant::now();
                self.transfer.upload(frame.device_context(), src)?;
                stages.upload = t.elapsed();
                (us, coalesced)
            }
            Err(CaptureError::Timeout) => {
                if force_key {
                    return self.encode_last_as_keyframe();
                }
                // Nothing changed on screen: hand over anything the encoder
                // finished late, so no picture waits for the next change.
                let late = self.encoder.poll()?;
                return Ok(if late.is_empty() {
                    None
                } else {
                    Some((self.stamp(late), false, Duration::ZERO, StageTimes::default()))
                });
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
        let cap_wait = t_cap.elapsed().saturating_sub(stages.cursor + stages.upload);
        let slot = self.ring_i;
        let nv12 = &self.ring[slot];
        self.ring_i = (self.ring_i + 1) % self.ring.len();
        self.last_slot = Some(slot);
        let t = Instant::now();
        let input = self.transfer.acquire_input()?;
        let conv = self.converter.convert(input, 0, nv12);
        self.transfer.release_input()?;
        conv?;
        stages.convert = t.elapsed();
        let pts = self.start.elapsed().as_nanos() as i64 / 100;
        self.in_encoder.push_back((pts, captured_us));
        // The queue only grows if an encoder swallows pictures; keep it small.
        while self.in_encoder.len() > 64 {
            self.in_encoder.pop_front();
        }
        let outputs = self.encoder.encode(nv12, pts, force_key)?;
        Ok(Some((self.stamp(outputs), coalesced, cap_wait, stages)))
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
            "--no-files" => cfg.file_access = false,
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
    /// Set to end the session in progress (the person at this computer
    /// pressed "Disconnect"); auto-cleared. Sharing stays on for the next one.
    pub end_session: Arc<AtomicBool>,
    /// People who connect may use the files of this computer. Read on every
    /// request, so switching it off takes effect in a running session.
    pub file_access: Arc<AtomicBool>,
}

impl HostControls {
    pub fn new(ask_first: bool) -> Self {
        Self {
            accepting: Arc::new(AtomicBool::new(true)),
            ask_first: Arc::new(AtomicBool::new(ask_first)),
            regen_code: Arc::new(AtomicBool::new(false)),
            stop: Arc::new(AtomicBool::new(false)),
            end_session: Arc::new(AtomicBool::new(false)),
            file_access: Arc::new(AtomicBool::new(true)),
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
    controls.file_access.store(cfg.file_access, Ordering::Relaxed);
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
                        vec!["No Tailscale address found. Start Tailscale on this PC, or turn open mode off to share with a code.".into()]
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
            end_session: controls.end_session.clone(),
            file_access: controls.file_access.clone(),
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
    /// See [`HostControls::end_session`].
    end_session: Arc<AtomicBool>,
    /// See [`HostControls::file_access`].
    file_access: Arc<AtomicBool>,
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
    // A "Disconnect" pressed before this session began is not about it.
    ctx.end_session.store(false, Ordering::Relaxed);
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
    // Start at half the configured rate and climb from there: the first
    // seconds must not flood a path nothing is known about yet.
    let start_kbps = (cfg.bitrate_kbps / 2).max(MIN_KBPS).min(cfg.bitrate_kbps);
    let target_kbps = Arc::new(AtomicU32::new(start_kbps));
    // Set by the stream loop when the picture changes size: the target the
    // bitrate control (which lives on the receive thread) continues from.
    let target_reset = Arc::new(AtomicU32::new(0));
    let last_feedback_us = Arc::new(AtomicU64::new(now_us()));
    // Clipboard sync starts only once the session is authorized (see below).
    let clipboard: Arc<OnceLock<ClipboardSync>> = Arc::new(OnceLock::new());
    // The file service, likewise: it exists only for an authorized session.
    let files: Arc<OnceLock<Reliable>> = Arc::new(OnceLock::new());

    let rx_thread = {
        let link = link.clone();
        let socket = socket.clone();
        let running = running.clone();
        let authorized = authorized.clone();
        let injector = injector.clone();
        let target_kbps = target_kbps.clone();
        let target_reset = target_reset.clone();
        let last_feedback_us = last_feedback_us.clone();
        let clipboard = clipboard.clone();
        let files = files.clone();
        let totp_secret = ctx.totp_secret.clone();
        let mut cc = BitrateController::new(start_kbps, MIN_KBPS.min(cfg.bitrate_kbps), cfg.bitrate_kbps);
        std::thread::Builder::new().name("session-rx".into()).spawn(move || {
            socket.set_read_timeout(Some(Duration::from_millis(50))).ok();
            let mut buf = vec![0u8; 2048];
            let mut clip_in = clip::Reassembler::default();
            let mut input_in = imperio_transport::input::InputReceiver::default();
            let mut last_feature_hint = Instant::now();
            let mut shares = SendRates::default();
            let mut last_share_log = Instant::now();
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
                            ControlMessage::InputBatch { events } => {
                                // Numbered events: only the new ones are
                                // applied, and the viewer is told how far
                                // this computer has got (see
                                // `imperio_transport::input`).
                                let (fresh, seq) = input_in.accept(events);
                                for ev in fresh {
                                    injector.inject(ev);
                                }
                                let _ = link.send_control(&ControlMessage::InputAck { seq });
                            }
                            ControlMessage::Input(ev) => {
                                injector.inject(ev);
                                // An unnumbered event from a viewer that could
                                // number them means it missed the announcement.
                                if last_feature_hint.elapsed() > Duration::from_secs(1) {
                                    last_feature_hint = Instant::now();
                                    let _ = link.send_control(&ControlMessage::Features(FEATURE_INPUT_BATCH));
                                }
                            }
                            other => tracing::debug!(?other, "unexpected control message"),
                        }
                    }
                    Some(Incoming::Stream { ack, body }) => {
                        // Like every message that acts on this computer:
                        // nothing before authorization.
                        if authorized.load(Ordering::Acquire) {
                            if let Some(f) = files.get() {
                                f.handle(ack, body);
                            }
                        }
                    }
                    Some(Incoming::Report(r)) => {
                        last_feedback_us.store(now_us(), Ordering::Relaxed);
                        // The picture and a file copy travel the same path.
                        // The bitrate control is told what the copy sends,
                        // and the copy is held to the part of the target the
                        // picture is not using (see `file_ceiling`).
                        let reset = target_reset.swap(0, Ordering::Relaxed);
                        if reset != 0 {
                            cc.set_target(reset, now);
                        }
                        let media = link.media_sent();
                        let chan = files.get();
                        let (video_kbps, file_kbps) = shares.rates(now, media.bytes, chan.map_or(0, |f| f.sent_bytes()));
                        cc.set_other_load(file_kbps);
                        if let Some(kbps) = cc.on_report(&r, media, now) {
                            target_kbps.store(kbps, Ordering::Relaxed);
                        }
                        if let Some(f) = chan {
                            let ceiling = file_ceiling(cc.target_kbps(), cc.limited(), video_kbps);
                            f.set_rate_limit(ceiling);
                            if f.busy() && last_share_log.elapsed() > Duration::from_secs(2) {
                                last_share_log = now;
                                tracing::info!(
                                    files_kbps = file_kbps as u32,
                                    picture_kbps = video_kbps as u32,
                                    file_ceiling_kbps = ceiling,
                                    target_kbps = cc.target_kbps(),
                                    "sharing the connection between the picture and a file copy"
                                );
                            }
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

        // File service: answers the viewer's requests while "Allow file
        // access" is on. Always started, so a viewer is told when it is off.
        {
            let wire: Arc<dyn imperio_transport::reliable::Wire> = link.clone();
            let (chan, inbox) = Reliable::start(wire);
            imperio_files::Peer::start(
                chan.clone(),
                inbox,
                imperio_files::Role::Host { allowed: ctx.file_access.clone() },
                |_| {},
            );
            let _ = files.set(chan);
        }

        // Build the capture + encode pipeline now that the peer is trusted.
        let pipeline = Pipeline::for_monitor(cfg, cfg.monitor, 0, start_kbps)?;
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
        stream_loop(cfg, pipeline, &injector, &link, &rx, &target_kbps, &target_reset, &last_feedback_us, &ctx.end_session)
    })();

    authorized.store(false, Ordering::Release);
    if let Some(f) = files.get() {
        f.close();
    }
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

/// The stream never asks the encoder for less than this.
const MIN_KBPS: u32 = 300;

/// How fast the picture and the file stream have been leaving, measured over
/// about half a second from the running totals of bytes sent.
#[derive(Default)]
struct SendRates {
    /// (when, picture bytes, file bytes), oldest first.
    samples: std::collections::VecDeque<(Instant, u64, u64)>,
}

impl SendRates {
    const WINDOW: Duration = Duration::from_millis(500);

    /// Add the totals as they stand; returns (picture, files) in kbit/s.
    fn rates(&mut self, now: Instant, picture_bytes: u64, file_bytes: u64) -> (f64, f64) {
        self.samples.push_back((now, picture_bytes, file_bytes));
        while self.samples.len() > 2 && now.duration_since(self.samples[1].0) >= Self::WINDOW {
            self.samples.pop_front();
        }
        let (then, picture0, files0) = self.samples[0];
        let secs = now.duration_since(then).as_secs_f64();
        if secs < 0.05 {
            return (0.0, 0.0);
        }
        let kbps = |from: u64, to: u64| to.saturating_sub(from) as f64 * 8.0 / 1000.0 / secs;
        (kbps(picture0, picture_bytes), kbps(files0, file_bytes))
    }
}

/// The most a file copy may send, in kbit/s, while the picture is being sent
/// down the same path; 0 = no ceiling.
///
/// While the bitrate target sits at its maximum the path has shown no limit,
/// and the copy runs as fast as its own flow control allows (a copy on a
/// local network must not be held to the picture's few megabits). Once the
/// path has shown a limit, the target is about what it carries: the copy gets
/// what the picture is not using of it, and never less than half, so a busy
/// picture slows a copy down but cannot stop it. The picture then gives up
/// its part of the room in the usual way, through the bitrate control.
fn file_ceiling(target_kbps: u32, path_limited: bool, picture_kbps: f64) -> u32 {
    if !path_limited {
        return 0;
    }
    let target = target_kbps as f64;
    (target - picture_kbps).max(target * 0.5).max(150.0) as u32
}

/// Slowest pacing of a frame's datagrams, whatever the bitrate target: none.
/// Pacing follows the target (see `Pacer::set_target`) all the way down, so
/// that a full picture, a hundred or more datagrams, is never fired into a
/// slow path faster than the path empties; the tail would be dropped every
/// time and the picture would never arrive whole. (This was 25 Mbit/s once,
/// which no weak connection carries.) When the encoder cannot get a busy
/// screen down to the target, pacing is also what holds the rate: frames
/// leave more slowly, so fewer are captured.
const PACE_FLOOR_KBPS: u32 = 0;

/// Full pictures (keyframes) on request, at a rate the path can carry.
///
/// A viewer that lost part of a frame asks for a full picture, and keeps
/// asking until one arrives. A full picture is 20 to 50 times the size of an
/// ordinary frame; answering every request sends them back to back, which on
/// a slow path is exactly what keeps the next one from arriving. One is sent
/// at once; the next waits until the last has had time to cross.
struct KeyframeGate {
    wanted: bool,
    last_sent: Option<Instant>,
    last_bytes: usize,
}

impl KeyframeGate {
    fn new() -> Self {
        Self { wanted: true, last_sent: None, last_bytes: 0 }
    }

    fn request(&mut self) {
        self.wanted = true;
    }

    /// Time the last full picture needs to cross at the current rate, with
    /// room for the frames around it.
    fn spacing(&self, kbps: u32) -> Duration {
        let crossing = Duration::from_secs_f64(self.last_bytes as f64 * 8.0 / (kbps.max(1) as f64 * 1250.0));
        (crossing * 2).clamp(Duration::from_millis(300), Duration::from_secs(4))
    }

    /// Whether the next frame should be a full picture.
    fn due(&self, kbps: u32) -> bool {
        self.wanted && self.last_sent.is_none_or(|t| t.elapsed() >= self.spacing(kbps))
    }

    /// The encoder has been asked for one; it may come out a frame later.
    fn asked(&mut self) {
        self.wanted = false;
        self.last_sent = Some(Instant::now());
    }

    /// A full picture went out (asked for or the encoder's own).
    fn sent(&mut self, bytes: usize) {
        self.wanted = false;
        self.last_sent = Some(Instant::now());
        self.last_bytes = bytes;
    }
}

/// How long a session waits for the screen to become capturable again (an
/// elevation prompt or lock screen that this process may not capture, a
/// display being reconfigured) before giving up.
const RECAPTURE_GIVE_UP: Duration = Duration::from_secs(120);

#[allow(clippy::too_many_arguments)]
fn stream_loop(
    cfg: &Config,
    first: Pipeline,
    injector: &Arc<input::Injector>,
    link: &Arc<SecureLink>,
    rx: &mpsc::Receiver<Event>,
    target_kbps: &Arc<AtomicU32>,
    target_reset: &AtomicU32,
    last_feedback_us: &Arc<AtomicU64>,
    end_session: &AtomicBool,
) -> anyhow::Result<SessionEnd> {
    tracing::info!("viewer authenticated; streaming");
    announce(link, &first)?;
    let announced = Instant::now();
    // The stream description is not retransmitted by the link. After a
    // change in the middle of a session it is said again a couple of times,
    // so a viewer that missed it is not left decoding the wrong size.
    let mut last_announce = Instant::now();
    let mut announce_again = 0u8;
    let mut sizes = SizeChooser::new(Instant::now());

    // `None` while the pipeline is being rebuilt (display switch, or the
    // desktop duplication was lost). The old one is dropped *before* a new
    // one is created so the two never hold the same display at once.
    let mut monitor = first.monitor_index;
    let mut slot = Some(first);
    let mut lost_since: Option<Instant> = None;

    let mut keys = KeyframeGate::new();
    let mut frame_id = 0u32;
    let mut stats = Stats::default();
    let mut current_kbps = slot.as_ref().map_or(cfg.bitrate_kbps, |p| p.encoder.config().bitrate_kbps);
    let mut pacer = Pacer::new(current_kbps, PACE_FLOOR_KBPS);
    let frame_timeout = Duration::from_millis(1000 / cfg.fps.max(1) as u64 + 1);

    loop {
        if end_session.swap(false, Ordering::Relaxed) {
            tracing::info!("session ended from this computer");
            deny(link, "The other computer ended the session.");
            return Ok(SessionEnd::Bye);
        }
        while let Ok(ev) = rx.try_recv() {
            match ev {
                Event::KeyframeRequest => keys.request(),
                Event::Bye => return Ok(SessionEnd::Bye),
                Event::Totp(_) | Event::Incompatible(_) => {}
                Event::Hello => {
                    // The viewer repeats Hello until the stream description
                    // reaches it. One that was already on its way when the
                    // description left needs no second full picture.
                    if announced.elapsed() > Duration::from_millis(250) {
                        keys.request();
                    }
                    if let Some(p) = &slot {
                        announce(link, p)?;
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
            let kbps = target_kbps.load(Ordering::Relaxed).max(MIN_KBPS);
            match Pipeline::for_monitor(cfg, monitor, sizes.step, kbps) {
                Ok(p) => {
                    monitor = p.monitor_index;
                    injector.set_monitor(
                        p.rect.left,
                        p.rect.top,
                        p.rect.right - p.rect.left,
                        p.rect.bottom - p.rect.top,
                    );
                    announce(link, &p)?;
                    last_announce = Instant::now();
                    announce_again = 2;
                    tracing::info!(monitor, "capture pipeline (re)built");
                    current_kbps = kbps;
                    pacer.set_target(current_kbps, PACE_FLOOR_KBPS);
                    // A new encoder starts with a full picture by itself.
                    keys = KeyframeGate::new();
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
        // A smaller picture when the connection carries too little for this
        // size, a larger one again when it has room.
        if let Some(change) = sizes.update(want, pipeline.native, Instant::now()) {
            // Where the bitrate control carries on from (see `SizeChange`).
            let want = match change.target_kbps {
                Some(kbps) => {
                    let kbps = kbps.clamp(MIN_KBPS.min(cfg.bitrate_kbps), cfg.bitrate_kbps);
                    target_kbps.store(kbps, Ordering::Relaxed);
                    target_reset.store(kbps, Ordering::Relaxed);
                    kbps
                }
                None => want,
            };
            match pipeline.resize(cfg, change.step, want.max(MIN_KBPS)) {
                Ok(()) => {
                    tracing::info!(
                        width = pipeline.width,
                        height = pipeline.height,
                        screen_width = pipeline.native.0,
                        screen_height = pipeline.native.1,
                        target_kbps = want,
                        "picture size changed to suit the connection"
                    );
                    current_kbps = want.max(MIN_KBPS);
                    pacer.set_target(current_kbps, PACE_FLOOR_KBPS);
                    // A new encoder starts with a full picture by itself.
                    keys = KeyframeGate::new();
                    announce(link, pipeline)?;
                    last_announce = Instant::now();
                    announce_again = 2;
                    continue;
                }
                Err(e) => {
                    tracing::warn!(error = format_args!("{e:#}"), "the picture size could not be changed; keeping it");
                    sizes.step = pipeline.size_step;
                }
            }
        }
        if announce_again > 0 && last_announce.elapsed() > Duration::from_millis(400) {
            announce(link, pipeline)?;
            last_announce = Instant::now();
            announce_again -= 1;
        }
        if want != current_kbps && p_set_bitrate(pipeline, want) {
            current_kbps = want;
            pacer.set_target(want, PACE_FLOOR_KBPS);
        }

        let force_key = keys.due(current_kbps);
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
        let Some((outputs, coalesced, cap_wait, stages)) = step else {
            continue;
        };
        if force_key {
            keys.asked();
        }
        stats.coalesced += coalesced as u64;
        for (out, captured_us) in outputs {
            frame_id = frame_id.wrapping_add(1);
            let t_send = Instant::now();
            pacer.begin_frame(out.data.len());
            for c in lan::split(frame_id, out.keyframe, captured_us, &out.data) {
                pacer.wait_for(c.len() + 40);
                link.send_media(&c, now_us())?;
            }
            link.send_media(&[], now_us())?;
            link.send_media(&[], now_us())?;
            if out.keyframe {
                keys.sent(out.data.len());
            }
            stats.record(cap_wait, out.encode_time, out.data.len(), out.keyframe);
            stats.stage(stages, t_send.elapsed());
        }
        stats.maybe_report(link, current_kbps);
    }
}

/// Tell the viewer what is being streamed and what this version understands.
fn announce(link: &SecureLink, p: &Pipeline) -> std::io::Result<()> {
    link.send_control(&p.stream_info())?;
    link.send_control(&ControlMessage::Features(FEATURE_INPUT_BATCH))?;
    link.send_control(&ControlMessage::NativeSize { width: p.native.0, height: p.native.1 })
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
    /// Largest full picture sent, in bytes.
    key_bytes_max: usize,
    coalesced: u64,
    cap_total: Duration,
    enc_total: Duration,
    enc_max: Duration,
    cursor_total: Duration,
    upload_total: Duration,
    convert_total: Duration,
    send_total: Duration,
    send_max: Duration,
    /// Longest pause between two frames leaving, and when the last one left.
    gap_max: Duration,
    last_frame: Option<Instant>,
    last: Option<Instant>,
}

impl Stats {
    fn record(&mut self, cap: Duration, enc: Duration, bytes: usize, key: bool) {
        self.frames += 1;
        self.bytes += bytes as u64;
        self.keyframes += key as u64;
        if key {
            self.key_bytes_max = self.key_bytes_max.max(bytes);
        }
        self.cap_total += cap;
        self.enc_total += enc;
        self.enc_max = self.enc_max.max(enc);
    }

    fn stage(&mut self, s: StageTimes, send: Duration) {
        self.cursor_total += s.cursor;
        self.upload_total += s.upload;
        self.convert_total += s.convert;
        self.send_total += send;
        self.send_max = self.send_max.max(send);
        let now = Instant::now();
        if let Some(prev) = self.last_frame {
            self.gap_max = self.gap_max.max(now.duration_since(prev));
        }
        self.last_frame = Some(now);
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
                keyframe_kb = self.key_bytes_max / 1000,
                enc_ms = format_args!("{:.2}", self.enc_total.as_secs_f64() * 1e3 / self.frames as f64),
                enc_max_ms = format_args!("{:.1}", self.enc_max.as_secs_f64() * 1e3),
                cursor_ms = format_args!("{:.2}", self.cursor_total.as_secs_f64() * 1e3 / self.frames as f64),
                upload_ms = format_args!("{:.2}", self.upload_total.as_secs_f64() * 1e3 / self.frames as f64),
                convert_ms = format_args!("{:.2}", self.convert_total.as_secs_f64() * 1e3 / self.frames as f64),
                send_ms = format_args!("{:.2}", self.send_total.as_secs_f64() * 1e3 / self.frames as f64),
                send_max_ms = format_args!("{:.1}", self.send_max.as_secs_f64() * 1e3),
                longest_gap_ms = format_args!("{:.0}", self.gap_max.as_secs_f64() * 1e3),
                retransmits = s.retransmits,
                "stream"
            );
        }
        *self = Stats { last: Some(now), last_frame: self.last_frame, ..Default::default() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_HD: (u32, u32) = (1920, 1080);

    /// Feed the same target every 100 ms for `secs`; the step changes seen.
    fn hold(c: &mut SizeChooser, t: &mut Instant, kbps: u32, secs: f64) -> Vec<usize> {
        let mut seen = Vec::new();
        for _ in 0..(secs * 10.0) as u32 {
            *t += Duration::from_millis(100);
            seen.extend(c.update(kbps, FULL_HD, *t).map(|change| change.step));
        }
        seen
    }

    #[test]
    fn a_larger_size_that_does_not_hold_is_tried_less_and_less_often() {
        let mut t = Instant::now();
        let mut c = SizeChooser::new(t);
        assert_eq!(hold(&mut c, &mut t, 800, 3.0), vec![2]);
        // The target drifts up (a quiet screen), the size follows, and the
        // path turns out not to carry it: down again. How long each trial
        // takes to come round:
        let mut waits = Vec::new();
        for _ in 0..4 {
            let mut waited = 0.0;
            while hold(&mut c, &mut t, 2000, 1.0).is_empty() {
                waited += 1.0;
                assert!(waited < 400.0);
            }
            waits.push(waited);
            assert_eq!(hold(&mut c, &mut t, 500, 3.0), vec![2]);
        }
        assert!(waits.windows(2).all(|w| w[1] > w[0] * 1.5), "{waits:?}");
        // A larger size that holds puts the wait back to normal.
        while hold(&mut c, &mut t, 2000, 1.0).is_empty() {}
        assert!(hold(&mut c, &mut t, 1500, 60.0).is_empty());
        assert_eq!(c.up_wait[1], SizeChooser::SETTLE);
        // From there, full size is tried, fails, and costs one step only;
        // three quarters, which has held, is not made to wait for it.
        let mut waited = 0.0;
        while hold(&mut c, &mut t, 4000, 1.0).is_empty() {
            waited += 1.0;
        }
        assert_eq!(c.step, 0);
        assert_eq!(hold(&mut c, &mut t, 400, 1.5), vec![1], "back to the size before the trial, and quickly");
        assert!(c.up_wait[0] > SizeChooser::SETTLE && c.up_wait[1] == SizeChooser::SETTLE);
        assert!(waited < 12.0);
    }

    #[test]
    fn a_file_copy_gets_what_the_picture_leaves() {
        // No limit seen on the path: no ceiling.
        assert_eq!(file_ceiling(8000, false, 6000.0), 0);
        // A limited path and a still screen: the whole target.
        assert_eq!(file_ceiling(2000, true, 0.0), 2000);
        // A busy picture: half, not nothing.
        assert_eq!(file_ceiling(2000, true, 1900.0), 1000);
        assert_eq!(file_ceiling(2000, true, 600.0), 1400);
        assert_eq!(file_ceiling(200, true, 200.0), 150);
    }

    #[test]
    fn send_rates_follow_the_totals() {
        let mut r = SendRates::default();
        let t = Instant::now();
        assert_eq!(r.rates(t, 0, 0), (0.0, 0.0));
        // 125 kB and 25 kB in each 100 ms: 10 and 2 Mbit/s.
        let mut last = (0.0, 0.0);
        for i in 1..=20u64 {
            last = r.rates(t + Duration::from_millis(100 * i), 125_000 * i, 25_000 * i);
        }
        assert!((last.0 - 10_000.0).abs() < 1.0 && (last.1 - 2_000.0).abs() < 1.0, "{last:?}");
        // The copy ends: within the window its rate is back to nothing.
        for i in 21..=30u64 {
            last = r.rates(t + Duration::from_millis(100 * i), 125_000 * i, 25_000 * 20);
        }
        assert!(last.1 < 1.0, "{last:?}");
    }

    #[test]
    fn sizes_are_even_and_in_proportion() {
        assert_eq!(stepped_size(FULL_HD, 0), (1920, 1080));
        assert_eq!(stepped_size(FULL_HD, 1), (1440, 810));
        assert_eq!(stepped_size(FULL_HD, 2), (960, 540));
        assert_eq!(stepped_size((1366, 768), 1), (1024, 576));
        let (w, h) = stepped_size((1365, 767), 2);
        assert!(w % 2 == 0 && h % 2 == 0);
    }

    #[test]
    fn a_fast_connection_keeps_the_full_size() {
        let mut t = Instant::now();
        let mut c = SizeChooser::new(t);
        assert!(hold(&mut c, &mut t, 4000, 30.0).is_empty());
        // About what a 3 Mbit/s connection settles at: still full size.
        assert!(hold(&mut c, &mut t, 2300, 30.0).is_empty());
    }

    #[test]
    fn a_slow_connection_gets_a_smaller_picture_and_a_passing_dip_does_not() {
        let mut t = Instant::now();
        let mut c = SizeChooser::new(t);
        // One second at a low target, then back up: nothing changes.
        assert!(hold(&mut c, &mut t, 800, 1.0).is_empty());
        assert!(hold(&mut c, &mut t, 4000, 1.0).is_empty());
        // 1.2 Mbit/s that stays: three quarters. 0.8 Mbit/s: half.
        assert_eq!(hold(&mut c, &mut t, 1200, 5.0), vec![1]);
        assert_eq!(hold(&mut c, &mut t, 800, 5.0), vec![2]);
        // Nothing smaller than half, however slow.
        assert!(hold(&mut c, &mut t, 300, 20.0).is_empty());
    }

    #[test]
    fn the_size_comes_back_slowly_and_only_with_room_to_spare() {
        let mut t = Instant::now();
        let mut c = SizeChooser::new(t);
        // 0.8 Mbit/s is too little even for three quarters: straight to half.
        assert_eq!(hold(&mut c, &mut t, 800, 3.0), vec![2]);
        assert!(hold(&mut c, &mut t, 800, 3.0).is_empty());
        // Just above what three quarters needs is not enough to go back.
        assert!(hold(&mut c, &mut t, 1150, 30.0).is_empty());
        // Comfortably above: back one step, not at once, then the next.
        assert!(hold(&mut c, &mut t, 6000, 5.0).is_empty());
        assert_eq!(hold(&mut c, &mut t, 6000, 4.0), vec![1]);
        assert!(hold(&mut c, &mut t, 6000, 6.0).is_empty());
        assert_eq!(hold(&mut c, &mut t, 6000, 4.0), vec![0]);
    }
}
