//! Viewer.
//!
//! Pipeline: encrypted UDP link -> NACK/jitter assembly -> MF H.264 decode
//! (D3D11) -> NV12 texture -> ID3D11VideoProcessor scale/convert straight
//! into a flip-model swap chain back buffer -> Present. Input events go back
//! over the same link.
//!
//! Usage:
//!   imperio-viewer                      (opens the connect dialog)
//!   imperio-viewer <host-ip[:port]> --code 123456 [--no-input]
//!   imperio-viewer --id 123456789 --server host:7800 --code 123456 [--no-input] [--force-relay]

mod known_hosts;
mod render;
mod ui;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::keyboard::KeyCode;

use std::net::{ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use rand::Rng;
use imperio_clipboard::ClipboardSync;
use imperio_codec::mf::MfH264Decoder;
use imperio_codec::VideoDecoder;
use imperio_protocol::signal::{Handshake as HandshakeMsg, Kind};
use imperio_protocol::{
    ControlMessage, InputEvent, MouseButton, VideoCodec, DEFAULT_PORT, DEFAULT_SIGNAL_PORT,
    MAX_CLIPBOARD_BYTES, OPEN_CODE, PROTOCOL_VERSION,
};
use imperio_transport::crypto::Handshake;
use imperio_transport::identity;
use imperio_transport::jitter::FrameAssembler;
use imperio_transport::lan::{self, CompleteFrame};
use imperio_transport::link::{Incoming, SecureLink};
use imperio_transport::{clip, now_us};
use imperio_transport::rendezvous::{self, Path};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::PhysicalKey;
use winit::platform::scancode::PhysicalKeyExtScancode;
use winit::window::{Window, WindowId};

/// Messages from the network thread to the UI thread.
enum Net {
    StreamInfo {
        width: u32,
        height: u32,
        codec: VideoCodec,
        monitor: u8,
        monitors: u8,
    },
    Clipboard(String),
    Frame(CompleteFrame),
    /// A menu command from the session window.
    Ui(ui::UiCmd),
    Bye,
}

struct Args {
    direct: Option<String>,
    id: Option<u32>,
    server: Option<String>,
    code: Option<String>,
    totp: Option<String>,
    trust_new: bool,
    send_input: bool,
    force_relay: bool,
}

/// Drop all whitespace (spaces, tabs, and stray CR/LF from `set /p` or a
/// pasted code) so "063 460" and "063460\r" both work.
fn strip_ws(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

fn parse_args(raw: Vec<String>) -> anyhow::Result<Args> {
    let mut a = Args {
        direct: None,
        id: None,
        server: None,
        code: None,
        totp: None,
        trust_new: false,
        send_input: true,
        force_relay: false,
    };
    let mut it = raw.into_iter();
    while let Some(k) = it.next() {
        match k.as_str() {
            "--no-input" => a.send_input = false,
            "--force-relay" => a.force_relay = true,
            "--trust-new" => a.trust_new = true,
            "--id" => a.id = Some(strip_ws(&it.next().context("--id value")?).parse()?),
            "--server" => a.server = Some(it.next().context("--server value")?.trim().to_string()),
            "--code" => a.code = Some(strip_ws(&it.next().context("--code value")?)),
            "--totp" => a.totp = Some(strip_ws(&it.next().context("--totp value")?)),
            s if !s.starts_with("--") => a.direct = Some(s.trim().to_string()),
            _ => anyhow::bail!("unknown arg {k}"),
        }
    }
    // No target on the command line is fine: the connect dialog asks for it.
    Ok(a)
}

struct App {
    link: Arc<SecureLink>,
    send_input: bool,
    totp: Option<String>,
    clipboard: Option<Arc<ClipboardSync>>,
    menu: Option<ui::SessionMenu>,
    fullscreen: bool,
    /// Recording wanted; the recorder is created at the first keyframe once
    /// the stream size is known.
    record_on: bool,
    recorder: Option<imperio_codec::record::Mp4Recorder>,
    window: Option<Arc<Window>>,
    renderer: Option<render::Renderer>,
    decoder: Option<Box<dyn VideoDecoder>>,
    stream: Option<(u32, u32)>,
    monitor: u8,
    monitors: u8,
    ctrl: bool,
    shift: bool,
    cursor: Option<(f64, f64)>,
    frames: u64,
    latency_sum_us: u64,
    last_title: Instant,
}

impl App {
    /// Feed one received access unit to the recorder, creating it lazily
    /// once the stream size is known (it waits for the first keyframe).
    fn record_frame(&mut self, f: &CompleteFrame, pts: i64) {
        if !self.record_on {
            return;
        }
        if self.recorder.is_none() {
            let Some((w, h)) = self.stream else { return };
            let path = imperio_codec::record::default_recording_path();
            match imperio_codec::record::Mp4Recorder::create(&path, w, h, 60) {
                Ok(rec) => {
                    tracing::info!(path = %rec.path.display(), "recording started");
                    self.recorder = Some(rec);
                    self.send(ControlMessage::RequestKeyframe);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "recording unavailable");
                    self.record_on = false;
                    if let Some(m) = &self.menu {
                        m.set_recording(false);
                    }
                    imperio_ui::info_box("Recording", &format!("Could not start recording:\n{e:#}"));
                    return;
                }
            }
        }
        if let Some(rec) = self.recorder.as_mut() {
            if let Err(e) = rec.write(&f.data, f.keyframe, pts) {
                tracing::warn!(error = %e, "recording write failed");
            }
        }
    }

    /// Session > Start / Stop recording.
    fn toggle_recording(&mut self) {
        if self.record_on {
            self.stop_recording(true);
        } else {
            self.record_on = true;
            self.send(ControlMessage::RequestKeyframe);
            if let Some(m) = &self.menu {
                m.set_recording(true);
            }
        }
    }

    fn stop_recording(&mut self, notify: bool) {
        self.record_on = false;
        if let Some(m) = &self.menu {
            m.set_recording(false);
        }
        if let Some(rec) = self.recorder.take() {
            match rec.finish() {
                Ok((path, frames)) => {
                    tracing::info!(path = %path.display(), frames, "recording saved");
                    if notify {
                        imperio_ui::info_box(
                            "Recording saved",
                            &format!("Saved {frames} frames to\n{}", path.display()),
                        );
                    }
                }
                Err(e) => tracing::warn!(error = %e, "recording finalize failed"),
            }
        }
    }

    /// F11 / Session > Full screen. Hides the menu bar while full screen.
    fn toggle_fullscreen(&mut self) {
        let Some(w) = &self.window else { return };
        self.fullscreen = !self.fullscreen;
        w.set_fullscreen(
            self.fullscreen
                .then_some(winit::window::Fullscreen::Borderless(None)),
        );
        if let Some(m) = &self.menu {
            m.set_fullscreen(self.fullscreen);
        }
    }

    fn send(&self, msg: ControlMessage) {
        if !self.send_input && matches!(msg, ControlMessage::Input(_)) {
            return;
        }
        if let Err(e) = self.link.send_control(&msg) {
            tracing::debug!(error = %e, "send failed");
        }
    }

    fn update_title(&self) {
        if let Some(w) = &self.window {
            let hint = if self.monitors > 1 {
                format!(" - display {}/{} (Ctrl+Shift+1..{})", self.monitor + 1, self.monitors, self.monitors)
            } else {
                String::new()
            };
            w.set_title(&format!("Imperio viewer{hint}"));
        }
    }

    fn normalised_cursor(&self) -> Option<(u16, u16)> {
        let (x, y) = self.cursor?;
        let r = self.renderer.as_ref()?;
        let dest = r.dest_rect();
        let w = (dest.right - dest.left).max(1) as f64;
        let h = (dest.bottom - dest.top).max(1) as f64;
        let nx = ((x - dest.left as f64) / w).clamp(0.0, 1.0);
        let ny = ((y - dest.top as f64) / h).clamp(0.0, 1.0);
        Some(((nx * 65535.0) as u16, (ny * 65535.0) as u16))
    }

    fn handle_frame(&mut self, f: CompleteFrame) -> anyhow::Result<()> {
        let pts = f.timestamp_us as i64 * 10;
        self.record_frame(&f, pts);
        let (Some(dec), Some(r)) = (self.decoder.as_mut(), self.renderer.as_mut()) else {
            return Ok(());
        };
        let t0 = Instant::now();
        let decoded = dec.decode(&f.data, pts)?;
        let dec_ms = t0.elapsed();
        for d in decoded {
            r.present(&d.texture, d.subresource)?;
            self.frames += 1;
            // Only meaningful when host and viewer share a clock.
            self.latency_sum_us += now_us().saturating_sub(f.timestamp_us);
        }
        if self.last_title.elapsed() > Duration::from_secs(1) {
            let secs = self.last_title.elapsed().as_secs_f64();
            if let (Some(w), true) = (&self.window, self.frames > 0) {
                let fps = self.frames as f64 / secs;
                let dec = dec_ms.as_secs_f64() * 1e3;
                let g2g = self.latency_sum_us as f64 / self.frames as f64 / 1e3;
                let rtt = self.link.rtt().as_secs_f64() * 1e3;
                let disp = if self.monitors > 1 { format!(" [display {}/{}]", self.monitor + 1, self.monitors) } else { String::new() };
                w.set_title(&format!("Imperio viewer{disp} - {fps:.0} fps, rtt {rtt:.1} ms, decode {dec:.1} ms, present {g2g:.1} ms"));
                tracing::info!(
                    fps = format_args!("{fps:.1}"),
                    rtt_ms = format_args!("{rtt:.1}"),
                    decode_ms = format_args!("{dec:.2}"),
                    capture_to_present_ms = format_args!("{g2g:.1}"),
                    "viewer"
                );
            }
            self.frames = 0;
            self.latency_sum_us = 0;
            self.last_title = Instant::now();
        }
        Ok(())
    }
}

impl ApplicationHandler<Net> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("imperio viewer - connecting...")
            .with_inner_size(PhysicalSize::new(1280u32, 720u32));
        let window = Arc::new(event_loop.create_window(attrs).expect("window"));
        match render::Renderer::new(&window) {
            Ok(r) => self.renderer = Some(r),
            Err(e) => {
                tracing::error!(error = %e, "renderer init failed");
                event_loop.exit();
                return;
            }
        }
        // Native menu bar on the winit window (Session / Display / Help).
        if let Ok(h) = window.window_handle() {
            if let RawWindowHandle::Win32(w32) = h.as_raw() {
                let hwnd = windows::Win32::Foundation::HWND(w32.hwnd.get() as *mut _);
                self.menu = ui::SessionMenu::attach(hwnd);
            }
        }
        self.window = Some(window);
        self.send(ControlMessage::Hello {
            version: PROTOCOL_VERSION,
        });
        // Second factor, if the host requires one.
        if let Some(code) = &self.totp {
            let _ = self.link.send_control(&ControlMessage::Totp(code.clone()));
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, ev: Net) {
        match ev {
            Net::StreamInfo {
                width,
                height,
                codec,
                monitor,
                monitors,
            } => {
                self.monitor = monitor;
                self.monitors = monitors;
                self.update_title();
                if let Some(m) = &self.menu {
                    m.set_displays(monitors, monitor);
                }
                if self.stream != Some((width, height)) {
                    tracing::info!(width, height, ?codec, monitor, monitors, "stream info");
                    self.stream = Some((width, height));
                    if let Some(r) = self.renderer.as_mut() {
                        r.set_source_size(width, height);
                    }
                    match MfH264Decoder::new(
                        self.renderer.as_ref().unwrap().device(),
                        width,
                        height,
                        60,
                    ) {
                        Ok(d) => self.decoder = Some(Box::new(d)),
                        Err(e) => tracing::error!(error = %e, "decoder init failed"),
                    }
                }
            }
            Net::Clipboard(text) => {
                if let Some(cb) = &self.clipboard {
                    cb.apply_remote(text);
                }
            }
            Net::Frame(f) => {
                if let Err(e) = self.handle_frame(f) {
                    tracing::warn!(error = %e, "frame failed; requesting keyframe");
                    self.send(ControlMessage::RequestKeyframe);
                }
            }
            Net::Ui(cmd) => match cmd {
                ui::UiCmd::Disconnect => {
                    let _ = self.link.send_control(&ControlMessage::Bye);
                    event_loop.exit();
                }
                ui::UiCmd::ToggleFullscreen => self.toggle_fullscreen(),
                ui::UiCmd::SelectDisplay(n) => {
                    if n < self.monitors {
                        self.send(ControlMessage::SelectMonitor(n));
                    }
                }
                ui::UiCmd::ToggleRecording => self.toggle_recording(),
                ui::UiCmd::About => imperio_ui::info_box("Imperio viewer", &ui::about_text()),
            },
            Net::Bye => {
                tracing::info!("host closed the session");
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                let _ = self.link.send_control(&ControlMessage::Bye);
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(r) = self.renderer.as_mut() {
                    if let Err(e) = r.resize(size.width, size.height) {
                        tracing::warn!(error = %e, "resize failed");
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = Some((position.x, position.y));
                if let Some((nx, ny)) = self.normalised_cursor() {
                    self.send(ControlMessage::Input(InputEvent::MouseMove { nx, ny }));
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let button = match button {
                    winit::event::MouseButton::Left => MouseButton::Left,
                    winit::event::MouseButton::Right => MouseButton::Right,
                    winit::event::MouseButton::Middle => MouseButton::Middle,
                    winit::event::MouseButton::Back => MouseButton::X1,
                    winit::event::MouseButton::Forward => MouseButton::X2,
                    winit::event::MouseButton::Other(_) => return,
                };
                self.send(ControlMessage::Input(InputEvent::MouseButton {
                    button,
                    down: state == ElementState::Pressed,
                }));
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => ((x * 120.0) as i32, (y * 120.0) as i32),
                    MouseScrollDelta::PixelDelta(p) => {
                        ((p.x * 120.0 / 50.0) as i32, (p.y * 120.0 / 50.0) as i32)
                    }
                };
                if dx != 0 || dy != 0 {
                    self.send(ControlMessage::Input(InputEvent::MouseWheel { dx, dy }));
                }
            }
            WindowEvent::ModifiersChanged(m) => {
                self.ctrl = m.state().control_key();
                self.shift = m.state().shift_key();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let down = event.state == ElementState::Pressed;
                // Local shortcut: F11 toggles full screen (not forwarded).
                if down && !event.repeat && event.physical_key == PhysicalKey::Code(KeyCode::F11) {
                    self.toggle_fullscreen();
                    return;
                }
                // Local shortcut: Ctrl+Shift+<digit> selects a host display.
                // Not forwarded to the host.
                if down && self.ctrl && self.shift {
                    if let winit::keyboard::Key::Character(s) = &event.logical_key {
                        if let Some(d) = s.chars().next().and_then(|c| c.to_digit(10)) {
                            if d >= 1 && (d as u8) <= self.monitors {
                                self.send(ControlMessage::SelectMonitor(d as u8 - 1));
                                return;
                            }
                        }
                    }
                }
                // winit's Windows scancode is a 16-bit extended scancode:
                // high byte 0xE0 marks extended keys.
                let (scancode, extended) = match event.physical_key {
                    PhysicalKey::Code(_) | PhysicalKey::Unidentified(_) => {
                        match event.physical_key.to_scancode() {
                            Some(sc) => ((sc & 0xFF) as u16, (sc >> 8) & 0xFF == 0xE0),
                            None => (0, false),
                        }
                    }
                };
                let unicode = event.text.as_ref().and_then(|t| t.chars().next());
                self.send(ControlMessage::Input(InputEvent::Key {
                    scancode,
                    extended,
                    down,
                    unicode,
                }));
            }
            _ => {}
        }
    }
}

/// Why the session ended, when it was not the user closing the window. Set by
/// the network thread, reported by [`run_session`] as its error.
type EndReason = Arc<Mutex<Option<String>>>;

/// No authenticated datagram for this long means the other side is gone.
const LINK_LOST_AFTER: Duration = Duration::from_secs(10);

/// What the network thread needs to know about this session.
struct RxOptions {
    trust_new: bool,
    /// The Device ID the user asked for (ID mode); the host must present it.
    expect_id: Option<u32>,
    /// Second-factor code to send along with Hello, if the user gave one.
    totp: Option<String>,
    end_reason: EndReason,
}

/// Network thread: receive, NACK/report housekeeping, frame assembly, RTT.
fn spawn_rx(socket: Arc<UdpSocket>, link: Arc<SecureLink>, proxy: EventLoopProxy<Net>, opts: RxOptions) {
    let RxOptions { trust_new, expect_id, totp, end_reason } = opts;
    // End the session with an explanation the user will see.
    let fail = move |proxy: &EventLoopProxy<Net>, why: String| {
        tracing::error!(reason = %why, "session ended");
        if let Ok(mut r) = end_reason.lock() {
            r.get_or_insert(why);
        }
        let _ = proxy.send_event(Net::Bye);
    };
    std::thread::Builder::new()
        .name("net-rx".into())
        .spawn(move || {
            // Non-blocking + short sleep rather than a read timeout: Winsock
            // rounds SO_RCVTIMEO below 500 ms up to 500 ms, which would stall
            // NACK and deadline housekeeping whenever the stream is idle.
            socket.set_nonblocking(true).ok();
            let mut buf = vec![0u8; 2048];
            let mut assembler = FrameAssembler::default();
            // Play the host's audio. Failure (no device, etc.) just means a
            // silent session, never a dead video stream.
            let audio_out = match imperio_audio::playback::AudioPlayback::start() {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!(error = %e, "audio playback unavailable; video only");
                    None
                }
            };
            let mut audio_dec = imperio_audio::PcmCodec;
            let mut audio_buf = vec![0i16; imperio_audio::INTERLEAVED_PER_FRAME];
            let mut last_ping = Instant::now() - Duration::from_secs(1);
            let mut last_key_req = Instant::now() - Duration::from_secs(1);
            let mut last_stats = Instant::now();
            let mut clip_in = clip::Reassembler::default();
            // Liveness and start-up: Hello is repeated until the host answers
            // (control datagrams are not retransmitted by the link).
            let started = Instant::now();
            let mut last_heard = Instant::now();
            let mut last_hello = Instant::now();
            let mut stream_started = false;
            loop {
                let now = Instant::now();
                match socket.recv_from(&mut buf) {
                    Ok((n, from)) => {
                        if from == link.peer() {
                            let incoming = link.handle_datagram(&buf[..n], now);
                            if incoming.is_some() {
                                last_heard = now;
                            }
                            match incoming {
                                Some(Incoming::Media { payload, .. }) => {
                                    if let Some((h, p)) = lan::parse(payload) {
                                        assembler.push(h, p, now);
                                    }
                                }
                                Some(Incoming::Control(msg)) => match msg {
                                    ControlMessage::StreamInfo {
                                        width,
                                        height,
                                        codec,
                                        monitor,
                                        monitors,
                                    } => {
                                        stream_started = true;
                                        let _ = proxy.send_event(Net::StreamInfo {
                                            width,
                                            height,
                                            codec,
                                            monitor,
                                            monitors,
                                        });
                                    }
                                    ControlMessage::Clipboard(text) => {
                                        let _ = proxy.send_event(Net::Clipboard(text));
                                    }
                                    ControlMessage::ClipboardChunk { id, index, count, data } => {
                                        if let Some(text) = clip_in.push(id, index, count, data) {
                                            let _ = proxy.send_event(Net::Clipboard(text));
                                        }
                                    }
                                    ControlMessage::Denied(reason) => {
                                        // Shown to the user; keep it to one sane line.
                                        let reason: String = reason.chars().filter(|c| !c.is_control()).take(300).collect();
                                        fail(&proxy, reason);
                                        return;
                                    }
                                    ControlMessage::Identity { device_id, public, sig } => {
                                        let sig_ok = sig.len() == 64
                                            && public.len() == 32
                                            && identity::verify_session(
                                                public.as_slice().try_into().unwrap(),
                                                link.session_keys(),
                                                sig.as_slice().try_into().unwrap(),
                                            );
                                        if !sig_ok {
                                            fail(&proxy, "The other computer could not prove its identity, so the session was closed.".into());
                                            return;
                                        }
                                        if expect_id.is_some_and(|want| want != device_id) {
                                            fail(
                                                &proxy,
                                                format!(
                                                    "A different computer answered (ID {}) than the one you asked for. The session was closed.",
                                                    imperio_protocol::device_id::format(device_id)
                                                ),
                                            );
                                            return;
                                        }
                                        let fp = identity::fingerprint(public.as_slice().try_into().unwrap());
                                        match known_hosts::check(device_id, &public, trust_new) {
                                            known_hosts::Trust::New => tracing::info!(device_id, fingerprint = %fp, "host identity pinned (first use)"),
                                            known_hosts::Trust::Known => tracing::info!(device_id, fingerprint = %fp, "host identity verified"),
                                            known_hosts::Trust::Mismatch => {
                                                tracing::error!(device_id, fingerprint = %fp, "HOST IDENTITY CHANGED — possible impersonation. Disconnecting. Re-run with --trust-new only if you expected this.");
                                                fail(
                                                    &proxy,
                                                    "This is not the same computer you connected to before under this ID (its identity key changed). \
                                                     If it was reinstalled or replaced, tick \u{201c}accept a changed identity\u{201d} under Sharing settings \u{203a} Advanced and connect again. \
                                                     Otherwise, do not connect."
                                                        .into(),
                                                );
                                                return;
                                            }
                                        }
                                    }
                                    ControlMessage::Pong { t_us } => {
                                        let rtt =
                                            Duration::from_micros(now_us().saturating_sub(t_us));
                                        link.set_rtt(rtt);
                                        // Two NACK round trips plus slack.
                                        assembler.set_deadline(
                                            link.rtt() * 2 + Duration::from_millis(30),
                                        );
                                    }
                                    ControlMessage::Bye => {
                                        let _ = proxy.send_event(Net::Bye);
                                        return;
                                    }
                                    _ => {}
                                },
                                Some(Incoming::Audio { codec, payload, .. }) => {
                                    use imperio_audio::AudioDecoder;
                                    if let (Some(pb), Some(tag)) =
                                        (&audio_out, imperio_audio::AudioCodec::from_u8(codec))
                                    {
                                        if tag == audio_dec.codec() {
                                            if let Ok(per_ch) =
                                                audio_dec.decode(&payload, &mut audio_buf)
                                            {
                                                let n = per_ch * imperio_audio::CHANNELS as usize;
                                                pb.submit(audio_buf[..n].to_vec());
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1))
                    }
                    Err(_) => {}
                }
                if now.duration_since(last_heard) > LINK_LOST_AFTER {
                    fail(&proxy, "The connection to the other computer was lost.".into());
                    return;
                }
                if !stream_started
                    && now.duration_since(started) < Duration::from_secs(6)
                    && now.duration_since(last_hello) >= Duration::from_millis(500)
                {
                    let _ = link.send_control(&ControlMessage::Hello { version: PROTOCOL_VERSION });
                    if let Some(code) = &totp {
                        let _ = link.send_control(&ControlMessage::Totp(code.clone()));
                    }
                    last_hello = now;
                }
                link.tick(now);
                let (frames, need_key) = assembler.poll(now);
                for f in frames {
                    let _ = proxy.send_event(Net::Frame(f));
                }
                if need_key && now.duration_since(last_key_req) > Duration::from_millis(150) {
                    let _ = link.send_control(&ControlMessage::RequestKeyframe);
                    last_key_req = now;
                }
                if now.duration_since(last_ping) >= Duration::from_millis(500) {
                    let _ = link.send_control(&ControlMessage::Ping { t_us: now_us() });
                    last_ping = now;
                }
                if now.duration_since(last_stats) >= Duration::from_secs(2) {
                    let s = link.stats();
                    let a = assembler.stats;
                    tracing::info!(
                        received = s.received,
                        lost = s.lost,
                        recovered = s.recovered,
                        nacks = s.nacks_sent,
                        dup = s.duplicates,
                        queue_ms = format_args!("{:.1}", s.queue_delay_us as f64 / 1e3),
                        rtt_ms = format_args!("{:.1}", s.rtt_us as f64 / 1e3),
                        frames_dropped = a.dropped_incomplete,
                        frames_discarded = a.discarded_waiting_key,
                        frames_late_ok = a.completed_late,
                        deadline_ms = assembler.deadline().as_millis(),
                        "link"
                    );
                    last_stats = now;
                }
            }
        })
        .expect("rx thread");
}

/// After SPAKE2 both sides hold *a* key, but only the same key if the code
/// matched. Prove it before opening a window: ping until one datagram from
/// the host opens under our key. A wrong code never produces one.
fn confirm_keys(socket: &UdpSocket, link: &SecureLink) -> anyhow::Result<bool> {
    socket.set_read_timeout(Some(Duration::from_millis(200)))?;
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut last_ping = Instant::now() - Duration::from_secs(1);
    let mut buf = vec![0u8; 2048];
    while Instant::now() < deadline {
        if last_ping.elapsed() >= Duration::from_millis(300) {
            let _ = link.send_control(&ControlMessage::Ping { t_us: now_us() });
            last_ping = Instant::now();
        }
        match socket.recv_from(&mut buf) {
            Ok((n, from)) if from == link.peer() => {
                if link.handle_datagram(&buf[..n], Instant::now()).is_some() {
                    return Ok(true);
                }
            }
            Ok(_) => {}
            Err(e) if rendezvous::is_transient_recv_error(&e) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(false)
}

/// Resolve a path to the host, complete the SPAKE2 handshake and confirm the
/// keys match. Returns the encrypted link, ready to use.
fn connect(args: &Args, socket: &Arc<UdpSocket>) -> anyhow::Result<(Path, Arc<SecureLink>)> {
    // No --code given: assume the host is in open mode (no code). If the host
    // actually requires a code, authentication will simply fail.
    let code = args.code.clone().unwrap_or_else(|| OPEN_CODE.to_string());
    let (session, path) = match (&args.direct, args.id) {
        (Some(target), _) => {
            let target = if target.contains(':') {
                target.clone()
            } else {
                format!("{target}:{DEFAULT_PORT}")
            };
            let addr = target.to_socket_addrs()?.next().context("resolve host")?;
            (rand::thread_rng().gen::<u64>(), Path::Direct(addr))
        }
        (None, Some(id)) => {
            let server = args
                .server
                .clone()
                .context("--server is required with --id")?;
            let server = if server.contains(':') {
                server
            } else {
                format!("{server}:{DEFAULT_SIGNAL_PORT}")
            };
            let server = server.to_socket_addrs()?.next().context("resolve server")?;
            rendezvous::viewer_connect(socket, server, id, args.force_relay)?
        }
        _ => unreachable!(),
    };
    println!("Connecting to {} ...", path.addr());
    for attempt in 0..6 {
        let ours = Handshake::new(&code, session);
        socket.send_to(
            &HandshakeMsg {
                session,
                spake_msg: ours.outbound.clone(),
            }
            .encode(),
            path.addr(),
        )?;
        if let Some((body, from)) =
            rendezvous::recv_kind(socket, Kind::Handshake, Duration::from_secs(2))?
        {
            if from == path.addr() {
                if let Ok(msg) = HandshakeMsg::decode(&body) {
                    if msg.session == session {
                        let keys = ours.finish(&msg.spake_msg, false)?;
                        let link = Arc::new(SecureLink::new(socket.clone(), path.addr(), keys));
                        if confirm_keys(socket, &link)? {
                            return Ok((path, link));
                        }
                        if args.code.is_some() {
                            anyhow::bail!(
                                "That code was not accepted. Check the code shown on the other computer right now (it changes after each session) and try again."
                            );
                        }
                        anyhow::bail!(
                            "That computer asks for a code. Type the code shown on its Imperio window, then connect again."
                        );
                    }
                }
            }
        }
        if attempt == 0 {
            println!("  no reply yet, still trying...");
        }
    }
    anyhow::bail!("{}", diagnose(path.addr()))
}

/// Plain-language explanation when the host never replies, so the user does
/// not have to run network commands by hand.
fn diagnose(host: std::net::SocketAddr) -> String {
    use std::net::IpAddr;
    // Our own LAN address, via the route-to-internet trick.
    let my_ip = UdpSocket::bind("0.0.0.0:0")
        .ok()
        .and_then(|s| s.connect("8.8.8.8:80").ok().map(|_| s))
        .and_then(|s| s.local_addr().ok())
        .map(|a| a.ip());

    let is_vpn = |v4: std::net::Ipv4Addr| {
        let o = v4.octets();
        o[0] == 100 && (64..=127).contains(&o[1]) // Tailscale / CGNAT
    };
    let same_subnet = match (my_ip, host.ip()) {
        // This very computer: the network is not the problem.
        (_, h) if h.is_loopback() => true,
        (Some(IpAddr::V4(me)), IpAddr::V4(h)) => {
            // Two VPN addresses (Tailscale) count as "same network" even
            // though their octets differ; otherwise compare the /24.
            (is_vpn(me) && is_vpn(h)) || {
                let (m, hh) = (me.octets(), h.octets());
                m[0] == hh[0] && m[1] == hh[1] && m[2] == hh[2]
            }
        }
        _ => true, // can't tell; don't blame the network
    };

    let mut msg = match my_ip {
        Some(ip) => format!("Could not reach {} (this computer is {ip}). ", host.ip()),
        None => format!("Could not reach {}. ", host.ip()),
    };
    if !same_subnet {
        msg.push_str(
            "The two computers are on different networks (the first three numbers of the addresses differ). \
             Put both on the same Wi-Fi or router, or use the Tailscale address on both. Guest Wi-Fi often blocks this.\n",
        );
    } else {
        msg.push_str(
            "The other computer is not answering. On that computer: make sure Imperio is open and sharing is on, \
             click Sharing settings \u{203a} Advanced \u{203a} \u{201c}Allow through firewall\u{201d}, and check the address you typed matches the one it shows.\n",
        );
    }
    msg
}

/// Everything needed to open one remote session.
pub struct SessionRequest {
    /// Host address (`ip[:port]`) or a 10-digit Device ID.
    pub target: String,
    pub code: Option<String>,
    pub trust_new: bool,
    /// Signal server, required when `target` is a Device ID.
    pub server: Option<String>,
    pub send_input: bool,
    pub force_relay: bool,
    pub totp: Option<String>,
    /// Called once the encrypted link is up, just before the window opens.
    pub on_connected: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Start recording to Videos\Imperio as soon as the stream arrives.
    pub record: bool,
}

/// Command-line entry: parse flags, ask through the connect dialog when no
/// target was given (or an attempt failed), then run the session.
pub fn cli_main(raw: Vec<String>) -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env().add_directive("info".parse()?),
        )
        .try_init();
    let args = parse_args(raw)?;
    let mut request = ui::ConnectRequest {
        target: args
            .direct
            .clone()
            .or(args.id.map(imperio_protocol::device_id::format))
            .unwrap_or_default(),
        code: args.code.clone(),
        trust_new: args.trust_new,
    };
    let mut error: Option<String> = None;
    let mut ask = request.target.is_empty();
    loop {
        if ask {
            let Some(r) = ui::connect_dialog(&request, error.as_deref()) else {
                return Ok(());
            };
            request = r;
        }
        ask = true;
        let req = SessionRequest {
            target: request.target.clone(),
            code: request.code.clone(),
            trust_new: request.trust_new,
            server: args.server.clone(),
            send_input: args.send_input,
            force_relay: args.force_relay,
            totp: args.totp.clone(),
            on_connected: None,
            record: false,
        };
        match run_session(req) {
            Ok(()) => return Ok(()),
            Err(e) => {
                let text = format!("{e:#}");
                tracing::warn!(error = %text, "session failed");
                // The dialog has room for about two lines; the log has it all.
                error = Some(text.lines().take(2).collect::<Vec<_>>().join(" "));
            }
        }
    }
}

/// Connect to `req.target` and run the session window until it closes.
/// Works from any thread: the home app calls it from a worker thread.
pub fn run_session(req: SessionRequest) -> anyhow::Result<()> {
    use winit::platform::windows::EventLoopBuilderExtWindows;
    imperio_codec::mf::startup()?;
    let mut args = Args {
        direct: None,
        id: None,
        server: req.server.clone(),
        code: req.code.clone(),
        totp: req.totp.clone(),
        trust_new: req.trust_new,
        send_input: req.send_input,
        force_relay: req.force_relay,
    };
    let target = req.target.trim().to_string();
    if target.is_empty() {
        anyhow::bail!("Type the host\u{2019}s address or Device ID first.");
    }
    match imperio_protocol::device_id::parse(&target) {
        Some(id) => {
            if args.server.is_none() {
                anyhow::bail!(
                    "A Device ID needs a signal server (set `server` in the config or start with \
                     --server host:7800). Type the host\u{2019}s address instead."
                );
            }
            args.id = Some(id);
        }
        None => args.direct = Some(target),
    }
    let socket = Arc::new(imperio_transport::bind_udp("0.0.0.0:0")?);
    let (path, link) = connect(&args, &socket)?;
    tracing::info!(peer = %path.addr(), relay = matches!(path, Path::Relay(_)), "handshake complete; link encrypted");
    if let Some(f) = &req.on_connected {
        f();
    }

    let end_reason: EndReason = Arc::new(Mutex::new(None));
    let event_loop = EventLoop::<Net>::with_user_event().with_any_thread(true).build()?;
    spawn_rx(
        socket.clone(),
        link.clone(),
        event_loop.create_proxy(),
        RxOptions {
            trust_new: args.trust_new,
            expect_id: args.id,
            totp: args.totp.clone(),
            end_reason: end_reason.clone(),
        },
    );
    // Menu clicks on the session window arrive as winit user events.
    let menu_proxy = event_loop.create_proxy();
    ui::set_command_sink(move |cmd| {
        let _ = menu_proxy.send_event(Net::Ui(cmd));
    });

    // Clipboard: viewer -> host on local change (only when input is enabled).
    let clipboard = if args.send_input {
        let link = link.clone();
        let next_id = AtomicU32::new(1);
        Some(Arc::new(ClipboardSync::spawn(MAX_CLIPBOARD_BYTES, move |text| {
            clip::send(&link, next_id.fetch_add(1, Ordering::Relaxed), text);
        })))
    } else {
        None
    };

    let mut app = App {
        link,
        send_input: args.send_input,
        totp: args.totp.clone(),
        clipboard,
        window: None,
        renderer: None,
        decoder: None,
        stream: None,
        monitor: 0,
        monitors: 1,
        ctrl: false,
        shift: false,
        cursor: None,
        frames: 0,
        latency_sum_us: 0,
        last_title: Instant::now(),
        menu: None,
        fullscreen: false,
        record_on: req.record,
        recorder: None,
    };
    event_loop.run_app(&mut app)?;
    app.stop_recording(false);
    // Ended by the network side (refused, identity problem, link lost)?
    if let Some(why) = end_reason.lock().ok().and_then(|mut r| r.take()) {
        anyhow::bail!("{why}");
    }
    Ok(())
}
