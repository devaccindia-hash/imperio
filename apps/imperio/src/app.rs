//! The home window.
//!
//! Layout follows the pattern remote-desktop users already know (AnyDesk 9):
//! one address bar at the top with a Connect button, *Your ID* in large
//! accent digits underneath, then tabs — Saved computers (tiles) and Sharing
//! settings. Near-black theme, one red accent, plain words.
//!
//! The host engine (`imperio_host::run_host`) runs on a background thread for
//! the life of the process. Connecting spawns the remote-screen window as a
//! child process (`imperio session …`) because winit allows one event loop per
//! process and eframe owns it. Closing the window hides it to the tray.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use egui::{
    Align, Align2, Color32, CornerRadius, FontId, Layout, Margin, RichText, Sense, Stroke,
    StrokeKind, TextStyle, Vec2,
};
use imperio_host::config::{self, Config};
use imperio_host::{HostControls, HostOptions, HostStatus, StatusSink};

// Palette (near-black with a red accent, after the AnyDesk reference).
pub(crate) const BG: Color32 = Color32::from_rgb(0x1B, 0x1B, 0x1D);
pub(crate) const CARD: Color32 = Color32::from_rgb(0x25, 0x25, 0x28);
pub(crate) const CARD_HOVER: Color32 = Color32::from_rgb(0x2E, 0x2E, 0x32);
pub(crate) const STROKE: Color32 = Color32::from_rgb(0x38, 0x38, 0x3C);
pub(crate) const FIELD: Color32 = Color32::from_rgb(0x2B, 0x2B, 0x2F);
pub(crate) const BUTTON: Color32 = Color32::from_rgb(0x33, 0x33, 0x37);
pub(crate) const TEXT: Color32 = Color32::from_rgb(0xF2, 0xF2, 0xF2);
pub(crate) const MUTED: Color32 = Color32::from_rgb(0x9C, 0x9C, 0xA4);
pub(crate) const GREEN: Color32 = Color32::from_rgb(0x3D, 0xDC, 0x84);
const AMBER: Color32 = Color32::from_rgb(0xFF, 0xB4, 0x54);
pub(crate) const ERROR: Color32 = Color32::from_rgb(0xFF, 0x7B, 0x72);

/// Accent colour as 0xRRGGBB: red on the Free plan, gold once a Pro licence
/// is active, so paying users see the difference everywhere at once.
static ACCENT_RGB: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0xE6453B);
const FREE_ACCENT: u32 = 0xE6453B;
const PRO_ACCENT: u32 = 0xE3B341;

pub(crate) fn accent() -> Color32 {
    let v = ACCENT_RGB.load(Ordering::Relaxed);
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

fn accent_hover() -> Color32 {
    let a = accent();
    let lift = |c: u8| (c as u16 + (255 - c as u16) * 22 / 100) as u8;
    Color32::from_rgb(lift(a.r()), lift(a.g()), lift(a.b()))
}

/// Switch the look between Free (red) and Pro (gold) and re-apply the theme.
pub(crate) fn set_pro_look(pro: bool, ctx: &egui::Context) {
    ACCENT_RGB.store(if pro { PRO_ACCENT } else { FREE_ACCENT }, Ordering::Relaxed);
    apply_theme(ctx);
}

#[derive(Clone, PartialEq)]
enum SessionState {
    Idle,
    Connecting,
    Connected,
    Failed(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Saved,
    Settings,
}

#[derive(Clone, PartialEq)]
enum UpdateState {
    Idle,
    Checking,
    UpToDate,
    Available(crate::update::Available),
    /// User chose "Later"; still offered under Settings.
    Dismissed(crate::update::Available),
    Downloading,
    Failed(String),
}

pub struct ImperioApp {
    ctx: egui::Context,
    cfg: Config,
    status: Arc<Mutex<HostStatus>>,
    host_error: Arc<Mutex<Option<String>>>,
    controls: HostControls,
    engine: Option<JoinHandle<()>>,
    // Switches (mirrored into controls / config).
    allow: bool,
    ask_first: bool,
    open_mode: bool,
    // Connect bar.
    target: String,
    code: String,
    trust_new: bool,
    session: Arc<Mutex<SessionState>>,
    /// The running session window's process, and the pipe whose closing
    /// asks it to end the session politely (see `disconnect`).
    session_child: Arc<Mutex<Option<std::process::Child>>>,
    session_stdin: Arc<Mutex<Option<std::process::ChildStdin>>>,
    /// The user ended the current session: its exit is not an error.
    session_cancelled: Arc<AtomicBool>,
    /// Counts sessions, so a late clean-up never touches a newer one.
    session_gen: Arc<std::sync::atomic::AtomicU64>,
    tab: Tab,
    copied: Option<(&'static str, Instant)>,
    first_frame_done: bool,
    update: Arc<Mutex<UpdateState>>,
    license: Option<crate::license::License>,
    license_input: String,
    /// (message, is_success) shown under the licence field.
    license_msg: Option<(String, bool)>,
    started: Instant,
    /// Mirror of `cfg.check_updates` for the periodic checker thread.
    auto_check: Arc<AtomicBool>,
    /// Set when launched by the updater (`--updated`): greet with the version.
    just_updated: bool,
    /// When the pointer started resting on a guarded widget (see `dwell_ok`).
    hover_since: std::collections::HashMap<&'static str, Instant>,
    /// Install button armed by a first click until this instant (two-click install).
    confirm_until: Option<Instant>,
    /// What the local Tailscale app reports (Tailscale mode only).
    tailscale: Arc<Mutex<crate::tailscale::State>>,
    /// When Tailscale was last asked; it is asked again every few seconds.
    ts_asked: Option<Instant>,
}

pub fn run() -> anyhow::Result<()> {
    init_logging();
    // Only one home window: this copy replaces any that is already running.
    crate::single::enforce();
    // Window / taskbar icon follows the plan: red for Free, gold for Pro.
    let pro = config::load_or_create()
        .license_key
        .as_deref()
        .map(|k| crate::license::verify_key(k).is_ok())
        .unwrap_or(false);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Imperio")
            .with_inner_size([960.0, 780.0])
            .with_min_inner_size([760.0, 680.0])
            .with_icon(Arc::new(icon(if pro { PRO_ACCENT } else { FREE_ACCENT }))),
        ..Default::default()
    };
    eframe::run_native(
        "Imperio",
        options,
        Box::new(|cc| Ok(Box::new(ImperioApp::new(cc)))),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

impl ImperioApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut cfg = config::load_or_create();
        // Older versions saved the same ID once per spelling ("425 922 0101"
        // and "4259220101"); show each computer once.
        let mut seen = std::collections::HashSet::new();
        cfg.recent = cfg.recent.iter().map(|r| canonical_target(r)).filter(|r| seen.insert(r.clone())).collect();
        let license = cfg
            .license_key
            .as_deref()
            .and_then(|k| crate::license::verify_key(k).ok());
        set_pro_look(license.is_some(), &cc.egui_ctx);
        let mut app = Self {
            ctx: cc.egui_ctx.clone(),
            allow: true,
            ask_first: cfg.require_accept,
            open_mode: cfg.open_mode,
            target: String::new(),
            code: String::new(),
            trust_new: false,
            status: Arc::new(Mutex::new(HostStatus::default())),
            host_error: Arc::new(Mutex::new(None)),
            controls: HostControls::new(cfg.require_accept),
            engine: None,
            session: Arc::new(Mutex::new(SessionState::Idle)),
            session_child: Default::default(),
            session_stdin: Default::default(),
            session_cancelled: Default::default(),
            session_gen: Default::default(),
            tab: Tab::Saved,
            copied: None,
            first_frame_done: false,
            update: Arc::new(Mutex::new(UpdateState::Idle)),
            license,
            license_input: String::new(),
            license_msg: None,
            started: Instant::now(),
            auto_check: Arc::new(AtomicBool::new(cfg.check_updates)),
            just_updated: std::env::args().any(|a| a == "--updated"),
            hover_since: Default::default(),
            confirm_until: None,
            tailscale: Default::default(),
            ts_asked: None,
            cfg,
        };
        app.start_engine();
        if app.cfg.check_updates {
            app.check_updates();
        }
        app.spawn_periodic_update_checks();
        app
    }

    /// (Re)start the host engine with the current open-mode choice. A helper
    /// thread waits for the previous engine to exit so the port is free.
    fn start_engine(&mut self) {
        self.controls.stop.store(true, Ordering::Relaxed);
        let prev = self.engine.take();
        let controls = HostControls::new(self.ask_first);
        controls.accepting.store(self.allow, Ordering::Relaxed);
        controls.file_access.store(self.cfg.file_access, Ordering::Relaxed);
        self.controls = controls.clone();
        let cfg = self.cfg.clone();
        let open = self.open_mode;
        let status = self.status.clone();
        let err = self.host_error.clone();
        let ctx = self.ctx.clone();
        let ctx2 = ctx.clone();
        let sink: StatusSink = Arc::new(move |s: &HostStatus| {
            if let Ok(mut g) = status.lock() {
                *g = s.clone();
            }
            ctx.request_repaint();
        });
        self.engine = std::thread::Builder::new()
            .name("host-engine".into())
            .spawn(move || {
                if let Some(p) = prev {
                    let _ = p.join();
                }
                if let Ok(mut e) = err.lock() {
                    *e = None;
                }
                let opts = HostOptions { force_relay: false, code_override: None, open };
                if let Err(e) = imperio_host::run_host(cfg, opts, controls, Some(sink)) {
                    tracing::error!(error = format_args!("{e:#}"), "host engine stopped");
                    if let Ok(mut g) = err.lock() {
                        *g = Some(format!("{e:#}"));
                    }
                    ctx2.request_repaint();
                }
            })
            .ok();
    }

    /// Answer to "How do you want to connect?": Tailscale (no code, own
    /// devices only) or the same network (address + code).
    fn set_mode(&mut self, tailscale: bool) {
        let changed = self.open_mode != tailscale;
        self.open_mode = tailscale;
        self.cfg.mode_chosen = true;
        self.save_cfg();
        if changed {
            self.start_engine();
        }
        if tailscale {
            self.refresh_tailscale();
        }
    }

    /// Ask the Tailscale app for its computers, off the UI thread.
    fn refresh_tailscale(&mut self) {
        self.ts_asked = Some(Instant::now());
        let state = self.tailscale.clone();
        let ctx = self.ctx.clone();
        std::thread::Builder::new()
            .name("tailscale".into())
            .spawn(move || {
                let now = crate::tailscale::query();
                if let Ok(mut g) = state.lock() {
                    if *g != now {
                        *g = now;
                        ctx.request_repaint();
                    }
                }
            })
            .ok();
    }

    fn save_cfg(&mut self) {
        self.cfg.require_accept = self.ask_first;
        self.cfg.open_mode = self.open_mode;
        if let Err(e) = config::save(&self.cfg) {
            tracing::warn!(error = %e, "config save failed");
        }
    }

    /// Keep `target` at the top of the saved list (persisted in the config).
    fn remember(&mut self, target: String) {
        let target = canonical_target(&target);
        if target.is_empty() {
            return;
        }
        self.cfg.recent.retain(|r| canonical_target(r) != target);
        self.cfg.recent.insert(0, target);
        self.cfg.recent.truncate(12);
        self.save_cfg();
    }

    fn connect(&mut self) {
        let target = self.target.trim().to_string();
        if target.is_empty() {
            *self.session.lock().unwrap() =
                SessionState::Failed("Type the other computer\u{2019}s address or ID first.".into());
            return;
        }
        // An ID can only be looked up through an Imperio server. Without one,
        // say so in plain words instead of starting a session that cannot work
        // (and do not save the ID as a computer that can be clicked).
        if self.cfg.server.is_none() && imperio_protocol::device_id::parse(&target).is_some() {
            *self.session.lock().unwrap() = SessionState::Failed(
                "Connecting by ID is not available yet. Type the other computer\u{2019}s address instead: \
                 it is shown as \u{201c}Your address\u{201d} on its Imperio window."
                    .into(),
            );
            return;
        }
        self.remember(target.clone());

        let code: String = self.code.chars().filter(|c| !c.is_whitespace()).collect();
        let session = self.session.clone();
        let ctx = self.ctx.clone();
        // The remote-screen window runs in a child process (see module docs).
        // It prints `connected` on stdout once the link is up and, on
        // failure, the reason on stderr with a non-zero exit code.
        let exe = std::env::current_exe().unwrap_or_else(|_| "imperio.exe".into());
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("session").arg(&target).arg("--stdin-control");
        // The code travels in the child's environment, not on its command
        // line (which any process on this PC could read from the task list).
        cmd.env_remove(crate::SESSION_CODE_ENV);
        if !code.is_empty() {
            cmd.env(crate::SESSION_CODE_ENV, &code);
        }
        if self.trust_new {
            cmd.arg("--trust-new");
        }
        if self.cfg.record_sessions {
            cmd.arg("--record");
        }
        if let Some(s) = &self.cfg.server {
            cmd.arg("--server").arg(s);
        }
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        *session.lock().unwrap() = SessionState::Connecting;
        let child_slot = self.session_child.clone();
        let stdin_slot = self.session_stdin.clone();
        let cancelled = self.session_cancelled.clone();
        cancelled.store(false, Ordering::Relaxed);
        self.session_gen.fetch_add(1, Ordering::Relaxed);
        std::thread::Builder::new()
            .name("session".into())
            .spawn(move || {
                use std::io::{BufRead, Read};
                let outcome = (|| -> Result<(), String> {
                    let mut child = cmd
                        .spawn()
                        .map_err(|e| format!("could not start the session window: {e}"))?;
                    if let Some(out) = child.stdout.take() {
                        let session = session.clone();
                        let ctx = ctx.clone();
                        std::thread::spawn(move || {
                            for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                                if line.trim() == "connected" {
                                    *session.lock().unwrap() = SessionState::Connected;
                                    ctx.request_repaint();
                                }
                            }
                        });
                    }
                    let stderr = child.stderr.take();
                    *stdin_slot.lock().unwrap() = child.stdin.take();
                    *child_slot.lock().unwrap() = Some(child);
                    // Reading to the end returns when the child is on its way out.
                    let mut err = String::new();
                    if let Some(mut e) = stderr {
                        let _ = e.read_to_string(&mut err);
                    }
                    // The child stays in its slot (so Disconnect can stop it)
                    // until it has really exited.
                    let status = loop {
                        let mut slot = child_slot.lock().unwrap();
                        let done = match slot.as_mut() {
                            Some(c) => c.try_wait().map_err(|e| e.to_string()),
                            None => Err("the session window was lost".to_string()),
                        };
                        match done {
                            Ok(None) => {}
                            other => {
                                *slot = None;
                                *stdin_slot.lock().unwrap() = None;
                                break other?.expect("exit status");
                            }
                        }
                        drop(slot);
                        std::thread::sleep(Duration::from_millis(50));
                    };
                    if status.success() || cancelled.load(Ordering::Relaxed) {
                        Ok(())
                    } else {
                        // The child explains itself on stderr, sometimes over
                        // a few lines; show all of it, on one wrapped line.
                        let text = err.split_whitespace().collect::<Vec<_>>().join(" ");
                        Err(if text.is_empty() {
                            "The session ended unexpectedly.".to_string()
                        } else {
                            text.chars().take(600).collect()
                        })
                    }
                })();
                *session.lock().unwrap() = match outcome {
                    Ok(()) => SessionState::Idle,
                    Err(e) => {
                        tracing::warn!(error = %e, "session failed");
                        SessionState::Failed(e)
                    }
                };
                ctx.request_repaint();
            })
            .ok();
    }

    /// End the session this computer started (or stop trying to connect).
    /// Closing the pipe lets the session window say goodbye to the other
    /// side and close by itself; if it has not gone shortly after (still
    /// connecting, or stuck), it is stopped.
    fn disconnect(&mut self) {
        self.session_cancelled.store(true, Ordering::Relaxed);
        drop(self.session_stdin.lock().unwrap().take());
        let slot = self.session_child.clone();
        let gen = self.session_gen.clone();
        let mine = gen.load(Ordering::Relaxed);
        std::thread::Builder::new()
            .name("session-stop".into())
            .spawn(move || {
                std::thread::sleep(Duration::from_millis(1500));
                if gen.load(Ordering::Relaxed) == mine {
                    if let Some(c) = slot.lock().unwrap().as_mut() {
                        let _ = c.kill();
                    }
                }
            })
            .ok();
    }

    fn copy(&mut self, what: &'static str, text: String) {
        self.ctx.copy_text(text);
        self.copied = Some((what, Instant::now()));
    }

    fn copied_recently(&self, what: &str) -> bool {
        matches!(self.copied, Some((w, t)) if w == what && t.elapsed() < Duration::from_millis(1500))
    }

    fn plan_text(&self) -> String {
        match &self.license {
            Some(l) if !l.name.trim().is_empty() => format!("{} \u{00b7} {}", l.plan_label(), l.name.trim()),
            Some(l) => format!("{} plan", l.plan_label()),
            None => "Free plan".into(),
        }
    }

    fn update_url(&self) -> String {
        self.cfg
            .update_url
            .clone()
            .unwrap_or_else(|| crate::update::DEFAULT_URL.to_string())
    }

    /// Fetch the signed manifest on a worker thread.
    fn check_updates(&mut self) {
        let (url, state, ctx) = (self.update_url(), self.update.clone(), self.ctx.clone());
        *state.lock().unwrap() = UpdateState::Checking;
        std::thread::Builder::new()
            .name("update-check".into())
            .spawn(move || run_update_check(&url, &state, &ctx))
            .ok();
    }

    /// Re-check every few hours while Imperio sits in the tray, so a new
    /// version is noticed (tray balloon) without a restart.
    fn spawn_periodic_update_checks(&self) {
        let (url, state, ctx) = (self.update_url(), self.update.clone(), self.ctx.clone());
        let enabled = self.auto_check.clone();
        std::thread::Builder::new()
            .name("update-periodic".into())
            .spawn(move || loop {
                std::thread::sleep(Duration::from_secs(6 * 3600));
                if enabled.load(Ordering::Relaxed) {
                    run_update_check(&url, &state, &ctx);
                }
            })
            .ok();
    }

    /// Download, verify, hand over to the new exe, and quit so it can replace us.
    fn install_update(&mut self, a: crate::update::Available, source: &str) {
        tracing::info!(source, since_start_ms = self.started.elapsed().as_millis() as u64, version = %a.version, "install requested");
        let state = self.update.clone();
        let ctx = self.ctx.clone();
        *state.lock().unwrap() = UpdateState::Downloading;
        std::thread::Builder::new()
            .name("update-install".into())
            .spawn(move || {
                let result = crate::update::download_and_stage(&a)
                    .and_then(|staged| crate::update::launch_apply(&staged));
                match result {
                    Ok(()) => {
                        tracing::info!(version = %a.version, "update staged; quitting so it can be applied");
                        crate::tray::request_quit();
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "update failed");
                        *state.lock().unwrap() = UpdateState::Failed(e);
                        ctx.request_repaint();
                    }
                }
            })
            .ok();
    }

    fn activate_license(&mut self) {
        match crate::license::verify_key(&self.license_input) {
            Ok(lic) => {
                self.cfg.license_key = Some(self.license_input.chars().filter(|c| !c.is_whitespace()).collect());
                self.license = Some(lic);
                self.license_input.clear();
                self.license_msg = Some(("Thank you! Pro is active on this computer.".into(), true));
                set_pro_look(true, &self.ctx);
                self.save_cfg();
            }
            Err(e) => self.license_msg = Some((e, false)),
        }
    }

    fn remove_license(&mut self) {
        self.cfg.license_key = None;
        self.license = None;
        self.license_msg = None;
        set_pro_look(false, &self.ctx);
        self.save_cfg();
    }

    /// True once the pointer has rested on a widget for a short moment. Used
    /// before acting on clicks that cost the user something (an update
    /// install): a synthetic "jump and click" fails this, a human never does.
    fn dwell_ok(&mut self, key: &'static str, hovered: bool) -> bool {
        if !hovered {
            self.hover_since.remove(key);
            return false;
        }
        let since = *self.hover_since.entry(key).or_insert_with(Instant::now);
        since.elapsed() >= Duration::from_millis(150)
    }
}

impl eframe::App for ImperioApp {
    /// Runs before egui digests input. Drops clicks / keys during the first
    /// moments after launch (a stale event can arrive as the window appears
    /// and would press whatever sits there) and logs what arrived so a
    /// phantom click can be traced.
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        let since = self.started.elapsed();
        // Diagnostic trail for phantom clicks (low volume: buttons and keys only).
        for e in &raw.events {
            match e {
                egui::Event::PointerButton { pos, button, pressed, .. } => {
                    tracing::info!(?pos, ?button, pressed, ms = since.as_millis() as u64, "pointer button event")
                }
                egui::Event::Key { key, pressed, .. } => {
                    tracing::info!(?key, pressed, ms = since.as_millis() as u64, "key event")
                }
                egui::Event::Touch { .. } => tracing::info!(ms = since.as_millis() as u64, "touch event"),
                _ => {}
            }
        }
        if since < Duration::from_millis(800) {
            raw.events.retain(|e| {
                !matches!(
                    e,
                    egui::Event::PointerButton { .. } | egui::Event::Key { .. } | egui::Event::Touch { .. }
                )
            });
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.started.elapsed() < Duration::from_millis(900) {
            // Keep frames coming while the start-up input guard is active.
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        if !self.first_frame_done {
            // The native window exists by the first frame.
            self.first_frame_done = true;
            crate::single::started();
            dark_title_bar(frame);
            crate::tray::install(frame);
            if self.just_updated {
                crate::tray::notify(
                    "Imperio updated",
                    &format!("You\u{2019}re now on version {}.", crate::update::CURRENT),
                );
            }
        }
        // "Copied" reverts to "Copy" after a moment; repaint only until then.
        match self.copied {
            Some((_, at)) if at.elapsed() < Duration::from_millis(1500) => {
                ctx.request_repaint_after(Duration::from_millis(200))
            }
            Some(_) => self.copied = None,
            None => {}
        }
        // In Tailscale mode keep the list of computers fresh (they come and go).
        if self.open_mode && self.cfg.mode_chosen {
            const EVERY: Duration = Duration::from_secs(8);
            if self.ts_asked.is_none_or(|t| t.elapsed() >= EVERY) {
                self.refresh_tailscale();
            }
            ctx.request_repaint_after(EVERY);
        }
        let status = self.status.lock().map(|s| s.clone()).unwrap_or_default();
        let host_error = self.host_error.lock().ok().and_then(|e| e.clone());
        let session = self.session.lock().map(|s| s.clone()).unwrap_or(SessionState::Idle);

        // ---- header: brand + status, then the address bar -----------------
        egui::Panel::top("header")
            .frame(egui::Frame::new().fill(BG).inner_margin(Margin::symmetric(24, 14)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    logo(ui, 24.0);
                    ui.add_space(8.0);
                    ui.label(RichText::new("Imperio").size(20.0).strong().color(TEXT));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let (color, text) = if host_error.is_some() {
                            (ERROR, "Sharing unavailable")
                        } else if status.status.starts_with("Viewer connected") {
                            (accent(), "Someone is connected")
                        } else if !self.allow {
                            (AMBER, "Sharing paused")
                        } else if status.device_id.is_empty() {
                            (MUTED, "Starting\u{2026}")
                        } else {
                            (GREEN, "Ready: others can connect to you")
                        };
                        pill(ui, color, text);
                        ui.add_space(8.0);
                        let pro = self.license.is_some();
                        let plan = self.plan_text();
                        pill(ui, if pro { accent() } else { MUTED }, &plan);
                    });
                });
                ui.add_space(12.0);
                self.address_bar(ui, &session);
                self.update_banner(ui);
            });

        // ---- footer -------------------------------------------------------
        egui::Panel::bottom("footer")
            .frame(egui::Frame::new().fill(BG).inner_margin(Margin::symmetric(24, 10)))
            .show(ui, |ui| {
                ui.label(
                    RichText::new("Only allow people you contacted yourself; no bank or tech company will ask you to use this.")
                        .size(12.0)
                        .color(MUTED),
                );
                ui.label(
                    RichText::new("Closing this window keeps sharing on \u{2014} Imperio stays in the tray (bottom right). Quit from there.")
                        .size(12.0)
                        .color(MUTED),
                );
                if self.license.is_some() {
                    ui.label(
                        RichText::new("Pro member \u{2014} thank you for supporting Imperio. Priority help: admin@developerinayat.com")
                            .size(12.0)
                            .color(accent()),
                    );
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    ui.label(RichText::new("Proudly made with").size(12.0).color(MUTED));
                    heart(ui, 12.0, accent());
                    ui.label(RichText::new("in India by").size(12.0).color(MUTED));
                    ui.hyperlink_to(
                        RichText::new("Developer Inayat").size(12.0).color(accent()),
                        "https://developerinayat.com/",
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(RichText::new(format!("v{} \u{00b7} {}", crate::update::CURRENT, self.plan_text())).size(12.0).color(MUTED));
                    });
                });
            });

        // ---- body ---------------------------------------------------------
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG).inner_margin(Margin::symmetric(24, 4)))
            .show(ui, |ui| {
                if !self.cfg.mode_chosen {
                    self.mode_question(ui);
                    return;
                }
                self.mode_row(ui);
                self.your_id(ui, &status, host_error.as_deref());
                ui.add_space(10.0);
                self.tabs(ui);
                ui.add_space(8.0);
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match self.tab {
                    Tab::Saved => self.saved_tab(ui, &session),
                    Tab::Settings => self.settings_tab(ui),
                });
            });
    }
}

impl ImperioApp {
    /// One line: [ other computer's address or ID ] [ code ] [ Connect ].
    fn address_bar(&mut self, ui: &mut egui::Ui, session: &SessionState) {
        let busy = matches!(session, SessionState::Connecting | SessionState::Connected);
        ui.horizontal(|ui| {
            let connect_w = 140.0;
            let code_w = 150.0;
            let gap = ui.spacing().item_spacing.x;
            let field_w = (ui.available_width() - connect_w - code_w - 2.0 * gap).max(220.0);
            let hint = if self.cfg.server.is_some() {
                "Enter the other computer\u{2019}s address or ID"
            } else if self.open_mode && self.cfg.mode_chosen {
                "Click a computer below, or type its address"
            } else {
                "Enter the other computer\u{2019}s address"
            };
            let enter = field(ui, &mut self.target, hint, field_w, 17.0);
            let enter2 = field(ui, &mut self.code, "Code (if any)", code_w, 16.0);
            // While a session is open (or being set up) the same button ends it.
            let label = match session {
                SessionState::Connecting => "Cancel",
                SessionState::Connected => "Disconnect",
                _ => "Connect",
            };
            let button = egui::Button::new(RichText::new(label).size(15.0).strong().color(Color32::WHITE))
                .fill(if busy { BUTTON } else { accent() })
                .stroke(Stroke::new(1.0, if busy { accent() } else { Color32::TRANSPARENT }))
                .corner_radius(CornerRadius::same(10))
                .min_size(Vec2::new(connect_w, 46.0));
            let clicked = ui.add(button).clicked();
            if busy && clicked {
                self.disconnect();
            } else if !busy && (clicked || enter || enter2) {
                self.connect();
            }
        });
        let line = match session {
            SessionState::Failed(e) => Some((e.clone(), ERROR)),
            SessionState::Connecting => Some(("Reaching the other computer\u{2026}".to_string(), MUTED)),
            SessionState::Connected => Some((
                "The remote screen is open in its own window. F11 for full screen. Close that window or press Disconnect to end."
                    .to_string(),
                MUTED,
            )),
            SessionState::Idle => Some((
                "Type the address shown on the other computer\u{2019}s Imperio window, then Connect. Encrypted end to end."
                    .to_string(),
                MUTED,
            )),
        };
        if let Some((text, color)) = line {
            ui.add_space(6.0);
            ui.label(RichText::new(text).size(12.5).color(color));
        }
    }

    /// "A new version is available" strip under the address bar.
    fn update_banner(&mut self, ui: &mut egui::Ui) {
        if self.just_updated && self.started.elapsed() < Duration::from_secs(15) {
            ui.add_space(6.0);
            ui.label(
                RichText::new(format!("Updated to version {}. Thank you for keeping Imperio current.", crate::update::CURRENT))
                    .size(12.5)
                    .color(GREEN),
            );
            ui.ctx().request_repaint_after(Duration::from_secs(1));
        }
        let upd = self.update.lock().map(|u| u.clone()).unwrap_or(UpdateState::Idle);
        match &upd {
            UpdateState::Available(a) => {
                ui.add_space(10.0);
                egui::Frame::new()
                    .fill(Color32::from_rgb(0x3A, 0x24, 0x23))
                    .stroke(Stroke::new(1.0, accent()))
                    .corner_radius(CornerRadius::same(10))
                    .inner_margin(Margin::symmetric(14, 10))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.label(
                                    RichText::new(format!(
                                        "Imperio {} is available (you have {}).",
                                        a.version,
                                        crate::update::CURRENT
                                    ))
                                    .size(14.0)
                                    .strong()
                                    .color(TEXT),
                                );
                                if !a.notes.trim().is_empty() {
                                    ui.label(RichText::new(a.notes.trim()).size(12.5).color(MUTED));
                                }
                            });
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ghost_button(ui, "Later").clicked() {
                                    *self.update.lock().unwrap() = UpdateState::Dismissed(a.clone());
                                }
                                // Two deliberate clicks: the first arms the button
                                // for a few seconds, the second installs. A single
                                // stray press can never start an update.
                                let armed = self.confirm_until.is_some_and(|t| t > Instant::now());
                                let label = if armed { "Click again to confirm" } else { "Download and install" };
                                let install = egui::Button::new(RichText::new(label).size(13.5).strong().color(Color32::WHITE))
                                    .fill(if armed { accent_hover() } else { accent() })
                                    .corner_radius(CornerRadius::same(8))
                                    .min_size(Vec2::new(0.0, 32.0));
                                let r = ui.add(install);
                                let rested = self.dwell_ok("install_banner", r.hovered());
                                if r.clicked() && rested && armed {
                                    self.confirm_until = None;
                                    self.install_update(a.clone(), "banner");
                                } else if r.clicked() && rested {
                                    self.confirm_until = Some(Instant::now() + Duration::from_secs(6));
                                } else if r.clicked() {
                                    tracing::warn!("ignored install click: pointer had not rested on the button");
                                }
                                if armed {
                                    ui.ctx().request_repaint_after(Duration::from_millis(500));
                                }
                            });
                        });
                    });
            }
            UpdateState::Downloading => {
                ui.add_space(8.0);
                ui.label(RichText::new(crate::update::downloading_text()).size(12.5).color(AMBER));
                // Keep the figures moving.
                ui.ctx().request_repaint_after(Duration::from_millis(500));
            }
            _ => {}
        }
    }

    /// First run: one question, two answers. Nothing else on the page.
    fn mode_question(&mut self, ui: &mut egui::Ui) {
        ui.add_space(26.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("How do you want to connect?").size(26.0).strong().color(TEXT));
            ui.add_space(4.0);
            ui.label(RichText::new("Pick one. You can change it any time.").size(13.5).color(MUTED));
        });
        ui.add_space(20.0);
        let mut pick: Option<bool> = None;
        ui.columns(2, |cols| {
            if choice_card(
                &mut cols[0],
                "With Tailscale",
                "Works from anywhere: home, office, another city.",
                &[
                    "No code to type. Your computers appear as a list; click one.",
                    "Needs the free Tailscale app on both computers, signed in to the same account.",
                ],
            ) {
                pick = Some(true);
            }
            if choice_card(
                &mut cols[1],
                "On the same Wi-Fi or network",
                "Both computers are in the same home or office.",
                &[
                    "Nothing else to install.",
                    "You type the other computer\u{2019}s address and a 6-digit code it shows.",
                ],
            ) {
                pick = Some(false);
            }
        });
        if let Some(tailscale) = pick {
            self.set_mode(tailscale);
        }
    }

    /// "Connect: [With Tailscale] [Same network]" above everything else.
    fn mode_row(&mut self, ui: &mut egui::Ui) {
        let mut pick: Option<bool> = None;
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Connect").size(13.5).color(MUTED));
            ui.add_space(4.0);
            for (tailscale, label) in [(true, "With Tailscale"), (false, "On the same network")] {
                let on = self.open_mode == tailscale;
                let b = egui::Button::new(RichText::new(label).size(13.0).color(if on { Color32::WHITE } else { TEXT }))
                    .fill(if on { accent() } else { BUTTON })
                    .stroke(Stroke::new(1.0, if on { accent() } else { STROKE }))
                    .corner_radius(CornerRadius::same(255))
                    .min_size(Vec2::new(0.0, 30.0));
                if ui.add(b).clicked() && !on {
                    pick = Some(tailscale);
                }
            }
        });
        if let Some(tailscale) = pick {
            self.set_mode(tailscale);
        }
    }

    /// The one thing to tell the other person, in large type: this computer's
    /// address (or its ID when an Imperio server is configured), with the
    /// code or the Tailscale state underneath.
    fn your_id(&mut self, ui: &mut egui::Ui, status: &HostStatus, host_error: Option<&str>) {
        let centered = Layout::left_to_right(Align::Center).with_main_align(Align::Center);
        let w = ui.available_width();
        let by_id = self.cfg.server.is_some();
        let ts = self.tailscale.lock().map(|t| t.clone()).unwrap_or_default();
        let address = own_address(&status.addresses, self.open_mode);
        // (label, shown, copied)
        let (label, shown, copy) = if by_id {
            let digits: String = status.device_id.chars().filter(|c| !c.is_whitespace()).collect();
            ("Your ID", status.device_id.clone(), digits)
        } else {
            let a = address.clone().unwrap_or_default();
            ("Your address", a.clone(), a)
        };
        ui.add_space(2.0);
        ui.allocate_ui_with_layout(Vec2::new(w, 60.0), centered, |ui| {
            ui.label(RichText::new(label).size(20.0).color(MUTED));
            ui.add_space(14.0);
            let big = if shown.is_empty() { "\u{2014}".to_string() } else { shown };
            ui.label(RichText::new(big).size(46.0).strong().color(accent()));
            ui.add_space(12.0);
            let label = if self.copied_recently("id") { "Copied" } else { "Copy" };
            if ghost_button(ui, label).clicked() && !copy.is_empty() {
                self.copy("id", copy);
            }
        });
        let mut recheck = false;
        ui.allocate_ui_with_layout(Vec2::new(w, 36.0), centered, |ui| {
            if let Some(code) = &status.code {
                // "123 456" for the usual six digits; anything else as it is.
                let shown = if code.len() == 6 && code.is_ascii() {
                    format!("{} {}", &code[..3], &code[3..])
                } else {
                    code.clone()
                };
                ui.label(RichText::new("Code").size(16.0).color(MUTED));
                ui.add_space(10.0);
                ui.label(RichText::new(shown).size(26.0).strong().color(TEXT));
                ui.add_space(10.0);
                let label = if self.copied_recently("code") { "Copied" } else { "Copy" };
                if ghost_button(ui, label).clicked() {
                    self.copy("code", code.clone());
                }
                if ghost_button(ui, "New code").on_hover_text("Issue a fresh one-time code").clicked() {
                    self.controls.regen_code.store(true, Ordering::Relaxed);
                }
            } else if self.open_mode {
                use crate::tailscale::State;
                match ts {
                    State::Unknown => {
                        ui.label(RichText::new("Checking Tailscale\u{2026}").size(15.0).color(MUTED));
                    }
                    State::Running { .. } => {
                        ui.label(
                            RichText::new("No code needed. Only computers on your Tailscale account can connect.")
                                .size(15.0)
                                .color(TEXT),
                        );
                    }
                    State::NotInstalled => {
                        ui.label(RichText::new("Tailscale is not installed on this computer.").size(15.0).color(AMBER));
                        ui.add_space(8.0);
                        ui.hyperlink_to(
                            RichText::new("Get Tailscale (free)").size(15.0).color(accent()),
                            "https://tailscale.com/download",
                        );
                        ui.add_space(8.0);
                        recheck = ghost_button(ui, "Check again").clicked();
                    }
                    State::Stopped => {
                        ui.label(
                            RichText::new("Tailscale is switched off or signed out. Open Tailscale and connect it.")
                                .size(15.0)
                                .color(AMBER),
                        );
                        ui.add_space(8.0);
                        recheck = ghost_button(ui, "Check again").clicked();
                    }
                }
            } else if !status.security.is_empty() {
                ui.label(RichText::new(&status.security).size(15.0).color(TEXT));
            }
        });
        if recheck {
            self.refresh_tailscale();
        }
        ui.vertical_centered(|ui| {
            if status.code.is_some() {
                ui.label(
                    RichText::new(if by_id {
                        "Tell the person who will connect your ID or address and this code. The code changes after each session."
                    } else {
                        "Tell the person who will connect this address and code. The code changes after each session."
                    })
                    .size(12.5)
                    .color(MUTED),
                );
            }
            if status.addresses.is_empty() {
                ui.label(RichText::new("Looking up this computer\u{2019}s address\u{2026}").size(13.0).color(MUTED));
            } else if by_id || (address.is_none() && !self.open_mode) {
                let joined = status.addresses.join("   \u{2022}   ");
                ui.label(RichText::new(format!("Your address:  {joined}")).size(13.0).color(TEXT));
            }
            ui.label(RichText::new(&status.status).size(12.5).color(MUTED));
            // Someone is looking at this computer: one button to stop them.
            if status.status.starts_with("Viewer connected") {
                ui.add_space(4.0);
                let stop = egui::Button::new(RichText::new("Disconnect them").size(13.5).strong().color(Color32::WHITE))
                    .fill(accent())
                    .corner_radius(CornerRadius::same(8))
                    .min_size(Vec2::new(0.0, 32.0));
                if ui.add(stop).on_hover_text("Ends their session now. Sharing stays on for next time.").clicked() {
                    self.controls.end_session.store(true, Ordering::Relaxed);
                }
            }
            if let Some(e) = host_error {
                ui.label(RichText::new(e).size(12.5).color(ERROR));
                ui.label(RichText::new("Is another copy of Imperio already running?").size(12.0).color(MUTED));
            }
        });
    }

    fn tabs(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            for (tab, label) in [(Tab::Saved, "Saved computers"), (Tab::Settings, "Sharing settings")] {
                let selected = self.tab == tab;
                let text = RichText::new(label)
                    .size(15.0)
                    .strong()
                    .color(if selected { TEXT } else { MUTED });
                let resp = ui
                    .add(egui::Label::new(text).sense(Sense::click()))
                    .on_hover_cursor(egui::CursorIcon::PointingHand);
                if resp.clicked() {
                    self.tab = tab;
                }
                if selected {
                    let r = resp.rect;
                    ui.painter().line_segment(
                        [egui::pos2(r.left(), r.bottom() + 7.0), egui::pos2(r.right(), r.bottom() + 7.0)],
                        Stroke::new(2.5, accent()),
                    );
                }
                ui.add_space(22.0);
            }
        });
        ui.add_space(7.0);
        let r = ui.available_rect_before_wrap();
        ui.painter().line_segment([egui::pos2(r.left(), r.top()), egui::pos2(r.right(), r.top())], Stroke::new(1.0, STROKE));
    }

    fn saved_tab(&mut self, ui: &mut egui::Ui, session: &SessionState) {
        let busy = matches!(session, SessionState::Connecting | SessionState::Connected);
        let mut connect_to: Option<String> = None;
        let mut remove: Option<String> = None;

        // Tailscale mode: the account's computers, one click each.
        let mut peer_ips: Vec<String> = Vec::new();
        if self.open_mode {
            use crate::tailscale::State;
            let ts = self.tailscale.lock().map(|t| t.clone()).unwrap_or_default();
            if let State::Running { peers } = ts {
                ui.add_space(2.0);
                ui.label(RichText::new("Your Tailscale computers").size(13.5).strong().color(TEXT));
                ui.add_space(6.0);
                if peers.is_empty() {
                    ui.label(
                        RichText::new(
                            "No other computers yet. On the other computer, install Tailscale and Imperio, \
                             and sign in to Tailscale with the same account. It will appear here by itself.",
                        )
                        .size(13.0)
                        .color(MUTED),
                    );
                } else {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = Vec2::new(14.0, 14.0);
                        for p in &peers {
                            let can = p.online && !busy;
                            let (sub, color) = if !p.online {
                                ("Offline \u{00b7} turn it on and open Tailscale", MUTED)
                            } else if busy {
                                ("session in progress", MUTED)
                            } else {
                                ("Online \u{00b7} click to connect", GREEN)
                            };
                            if tile(ui, &p.name, sub, color, can).clicked() && can {
                                connect_to = Some(p.ip.clone());
                            }
                        }
                    });
                }
                ui.add_space(14.0);
                peer_ips = peers.into_iter().map(|p| p.ip).collect();
            }
        }

        // Saved computers that are not already shown above.
        let saved: Vec<String> = self
            .cfg
            .recent
            .iter()
            .filter(|r| !peer_ips.iter().any(|ip| r.as_str() == ip || r.starts_with(&format!("{ip}:"))))
            .cloned()
            .collect();
        if saved.is_empty() {
            if !self.open_mode {
                ui.add_space(24.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("No saved computers yet").size(17.0).strong().color(TEXT));
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("Computers you connect to are saved here automatically, so next time it\u{2019}s one click.")
                            .size(13.0)
                            .color(MUTED),
                    );
                });
            }
        } else {
            if self.open_mode {
                ui.label(RichText::new("Saved computers").size(13.5).strong().color(TEXT));
                ui.add_space(6.0);
            }
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(14.0, 14.0);
                for r in saved {
                    let (sub, color) = if busy { ("session in progress", MUTED) } else { ("Click to connect", accent()) };
                    let resp = tile(ui, &r, sub, color, !busy);
                    if resp.clicked() && !busy {
                        connect_to = Some(r.clone());
                    }
                    if resp.secondary_clicked() {
                        remove = Some(r);
                    }
                }
            });
            ui.add_space(10.0);
            ui.label(
                RichText::new("Click a computer to connect (uses the code typed above, if any). Right-click to remove.")
                    .size(12.0)
                    .color(MUTED),
            );
        }
        if let Some(r) = remove {
            self.cfg.recent.retain(|x| x != &r);
            self.save_cfg();
        }
        if let Some(t) = connect_to {
            self.target = t;
            self.connect();
        }
    }

    fn settings_tab(&mut self, ui: &mut egui::Ui) {
        ui.columns(2, |cols| {
            self.settings_left(&mut cols[0]);
            self.settings_right(&mut cols[1]);
        });
    }

    /// Sharing switches, recording, Advanced.
    fn settings_left(&mut self, ui: &mut egui::Ui) {
        if toggle_row(ui, "Allow connections", "Turn off to pause sharing without closing Imperio", &mut self.allow) {
            self.controls.accepting.store(self.allow, Ordering::Relaxed);
        }
        if toggle_row(ui, "Ask me before someone connects", "A Yes / No window appears when someone wants in", &mut self.ask_first) {
            self.controls.ask_first.store(self.ask_first, Ordering::Relaxed);
            self.save_cfg();
        }
        if toggle_row(
            ui,
            "Allow file access",
            "People who connect can browse, copy, rename and delete files on this PC",
            &mut self.cfg.file_access,
        ) {
            self.controls.file_access.store(self.cfg.file_access, Ordering::Relaxed);
            self.save_cfg();
        }
        if toggle_row(
            ui,
            "Record my sessions",
            "Saved as MP4 in Videos \u{203a} Imperio (also in the session menu)",
            &mut self.cfg.record_sessions,
        ) {
            self.save_cfg();
        }

        ui.add_space(8.0);
        egui::CollapsingHeader::new(RichText::new("Advanced").size(13.5).color(MUTED))
            .id_salt("advanced")
            .show(ui, |ui| {
                ui.add_space(4.0);
                ui.checkbox(
                    &mut self.trust_new,
                    RichText::new("When connecting, accept a changed identity for that computer (only if you expected the change)")
                        .size(12.5)
                        .color(MUTED),
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ghost_button(ui, "Allow through firewall\u{2026}")
                        .on_hover_text("Opens UDP 7700 for Imperio (asks for administrator rights)")
                        .clicked()
                    {
                        run_firewall_helper();
                    }
                    ui.label(RichText::new("If other computers can\u{2019}t reach this one.").size(12.0).color(MUTED));
                });
            });
    }

    /// Plan (licence) and Updates.
    fn settings_right(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Plan").size(15.0).strong().color(TEXT));
        ui.add_space(6.0);
        let mut remove = false;
        let mut activate = false;
        match &self.license {
            Some(l) => {
                ui.label(RichText::new(format!("{} \u{2014} licensed to {}", l.plan_label(), l.name)).size(14.0).color(TEXT));
                if let Some(e) = &l.expires {
                    ui.label(RichText::new(format!("Valid until {e}")).size(12.0).color(MUTED));
                }
                ui.horizontal(|ui| {
                    if ghost_button(ui, "Remove key").clicked() {
                        remove = true;
                    }
                });
            }
            None => {
                ui.label(RichText::new("Free plan \u{2014} for personal and occasional business use.").size(14.0).color(TEXT));
                ui.horizontal(|ui| {
                    ui.hyperlink_to(RichText::new("Upgrade to Pro").size(13.5).color(accent()), crate::license::UPGRADE_URL);
                    ui.label(RichText::new("Already have a key? Paste it below.").size(12.5).color(MUTED));
                });
                ui.horizontal(|ui| {
                    let w = (ui.available_width() - 120.0).max(200.0);
                    let enter = field(ui, &mut self.license_input, "IMP-\u{2026}", w, 14.0);
                    if ghost_button(ui, "Activate").clicked() || enter {
                        activate = true;
                    }
                });
            }
        }
        if let Some((msg, ok)) = &self.license_msg {
            ui.label(RichText::new(msg).size(12.5).color(if *ok { GREEN } else { ERROR }));
        }
        if remove {
            self.remove_license();
        }
        if activate {
            self.activate_license();
        }

        ui.add_space(16.0);
        ui.label(RichText::new("Updates").size(15.0).strong().color(TEXT));
        ui.add_space(6.0);
        let upd = self.update.lock().map(|u| u.clone()).unwrap_or(UpdateState::Idle);
        let cur = crate::update::CURRENT;
        let status = match &upd {
            UpdateState::Idle => format!("Version {cur}"),
            UpdateState::Checking => format!("Version {cur} \u{2014} checking\u{2026}"),
            UpdateState::UpToDate => format!("Version {cur} \u{2014} you\u{2019}re up to date"),
            UpdateState::Available(a) | UpdateState::Dismissed(a) => {
                format!("Version {cur} \u{2014} {} is available", a.version)
            }
            UpdateState::Downloading => crate::update::downloading_text(),
            UpdateState::Failed(e) => format!("Version {cur} \u{2014} {e}"),
        };
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(status).size(13.5).color(TEXT));
            let busy = matches!(upd, UpdateState::Checking | UpdateState::Downloading);
            let check = egui::Button::new(RichText::new("Check for updates").size(13.0).color(TEXT))
                .fill(BUTTON)
                .stroke(Stroke::new(1.0, STROKE))
                .corner_radius(CornerRadius::same(8));
            if ui.add_enabled(!busy, check).clicked() {
                self.check_updates();
            }
            if let UpdateState::Available(a) | UpdateState::Dismissed(a) = &upd {
                let armed = self.confirm_until.is_some_and(|t| t > Instant::now());
                let label = if armed { "Click again to confirm".to_string() } else { format!("Install {}", a.version) };
                let install = egui::Button::new(RichText::new(label).size(13.0).strong().color(Color32::WHITE))
                    .fill(if armed { accent_hover() } else { accent() })
                    .corner_radius(CornerRadius::same(8));
                let r = ui.add(install);
                let rested = self.dwell_ok("install_settings", r.hovered());
                if r.clicked() && rested && armed {
                    self.confirm_until = None;
                    self.install_update(a.clone(), "settings");
                } else if r.clicked() && rested {
                    self.confirm_until = Some(Instant::now() + Duration::from_secs(6));
                }
                if armed {
                    ui.ctx().request_repaint_after(Duration::from_millis(500));
                }
            }
        });
        if toggle_row(
            ui,
            "Check for updates when Imperio starts",
            "Only a small version file is fetched; nothing about you is sent",
            &mut self.cfg.check_updates,
        ) {
            self.auto_check.store(self.cfg.check_updates, Ordering::Relaxed);
            self.save_cfg();
        }
    }
}

// ------------------------------------------------------------------ widgets

/// One spelling per computer: a Device ID in its display form ("425 922 0101"
/// however it was typed), anything else trimmed.
fn canonical_target(target: &str) -> String {
    let target = target.trim();
    match imperio_protocol::device_id::parse(target) {
        Some(id) => imperio_protocol::device_id::format(id),
        None => target.to_string(),
    }
}

/// Text field of a given width. Returns true when Enter was pressed in it.
fn field(ui: &mut egui::Ui, value: &mut String, hint: &str, width: f32, size: f32) -> bool {
    let resp = ui
        .allocate_ui(Vec2::new(width, 46.0), |ui| {
            egui::Frame::new()
                .fill(FIELD)
                .stroke(Stroke::new(1.0, STROKE))
                .corner_radius(CornerRadius::same(10))
                .inner_margin(Margin::symmetric(12, 11))
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::singleline(value)
                            .hint_text(RichText::new(hint).color(MUTED))
                            .font(FontId::proportional(size))
                            .text_color(TEXT)
                            .desired_width(f32::INFINITY)
                            .frame(egui::Frame::NONE),
                    )
                })
                .inner
        })
        .inner;
    resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
}

pub(crate) fn ghost_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(text).size(13.0).color(TEXT))
            .fill(BUTTON)
            .stroke(Stroke::new(1.0, STROKE))
            .corner_radius(CornerRadius::same(8))
            .min_size(Vec2::new(0.0, 30.0)),
    )
}

/// The address to read out to the other person, taken from the host engine's
/// list ("192.168.1.7:7700   (same Wi-Fi / network only)"). On the same
/// network the Wi-Fi address is the right one even when a VPN address exists;
/// the usual port is left off because the other side assumes it.
fn own_address(addresses: &[String], tailscale: bool) -> Option<String> {
    let parsed = |a: &String| -> Option<std::net::SocketAddr> { a.split_whitespace().next()?.parse().ok() };
    let pick = if tailscale {
        None
    } else {
        addresses.iter().filter(|a| a.contains("same Wi-Fi")).find_map(parsed)
    }
    .or_else(|| addresses.iter().find_map(parsed))?;
    Some(if pick.port() == imperio_protocol::DEFAULT_PORT { pick.ip().to_string() } else { pick.to_string() })
}

/// One of the two answers to "How do you want to connect?". True when clicked.
fn choice_card(ui: &mut egui::Ui, title: &str, lead: &str, points: &[&str]) -> bool {
    let plain = |text: RichText| egui::Label::new(text).selectable(false);
    let inner = egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, STROKE))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(Margin::symmetric(20, 18))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(190.0);
            ui.add(plain(RichText::new(title).size(19.0).strong().color(TEXT)));
            ui.add_space(4.0);
            ui.add(plain(RichText::new(lead).size(14.0).color(TEXT)));
            ui.add_space(10.0);
            for p in points {
                ui.add(plain(RichText::new(*p).size(13.0).color(MUTED)));
                ui.add_space(4.0);
            }
            ui.add_space(8.0);
            ui.add(plain(RichText::new("Choose this").size(13.5).strong().color(accent())));
        });
    let resp = ui
        .interact(inner.response.rect, ui.scope_id().with(title), Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    if resp.hovered() {
        ui.painter().rect_stroke(
            inner.response.rect,
            CornerRadius::same(14),
            Stroke::new(1.5, accent()),
            StrokeKind::Inside,
        );
    }
    resp.clicked()
}

/// A computer as a clickable tile: its name or address, and a line under it.
fn tile(ui: &mut egui::Ui, title: &str, sub: &str, sub_color: Color32, enabled: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(228.0, 104.0), Sense::click());
    let hovered = resp.hovered() && enabled;
    let p = ui.painter();
    p.rect(
        rect,
        CornerRadius::same(12),
        if hovered { CARD_HOVER } else { CARD },
        Stroke::new(1.0, if hovered { accent() } else { STROKE }),
        StrokeKind::Inside,
    );
    // Little monitor glyph.
    let glyph = if enabled { accent() } else { MUTED };
    let m = egui::Rect::from_min_size(rect.min + Vec2::new(16.0, 16.0), Vec2::new(26.0, 17.0));
    p.rect_stroke(m, CornerRadius::same(3), Stroke::new(1.8, glyph), StrokeKind::Inside);
    p.line_segment([m.center_bottom() + Vec2::new(-6.0, 5.0), m.center_bottom() + Vec2::new(6.0, 5.0)], Stroke::new(1.8, glyph));
    // Long computer names are cut rather than drawn over the next tile.
    let shown: String = if title.chars().count() > 22 {
        title.chars().take(21).chain(std::iter::once('\u{2026}')).collect()
    } else {
        title.to_string()
    };
    p.text(rect.min + Vec2::new(16.0, 46.0), Align2::LEFT_TOP, shown, FontId::proportional(16.0), TEXT);
    p.text(rect.min + Vec2::new(16.0, 76.0), Align2::LEFT_TOP, sub, FontId::proportional(12.5), sub_color);
    if enabled {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    }
}

fn pill(ui: &mut egui::Ui, color: Color32, text: &str) {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, STROKE))
        .corner_radius(CornerRadius::same(255))
        .inner_margin(Margin::symmetric(12, 6))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                ui.painter().circle_filled(rect.center(), 5.0, color);
                ui.label(RichText::new(text).size(12.5).color(TEXT));
            });
        });
}

/// A small heart drawn with the painter (the bundled fonts lack the glyph),
/// with a gentle heartbeat.
fn heart(ui: &mut egui::Ui, size: f32, color: Color32) {
    const PERIOD: f64 = 1.6;
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    let (t, focused) = ui.input(|i| (i.time, i.focused));
    // The heartbeat only runs while the window is in front, and only redraws
    // during the two beats: an idle or background Imperio must not keep the
    // GPU and CPU awake 30 times a second for a decoration.
    let phase = if focused { (t % PERIOD) / PERIOD } else { 0.5 };
    let beat = |center: f64| (1.0 - ((phase - center).abs() / 0.09)).max(0.0);
    let scale = 1.0 + 0.16 * (beat(0.10) + 0.7 * beat(0.30)) as f32;
    if focused {
        let wait = if phase < 0.40 { 0.033 } else { (1.0 - phase) * PERIOD };
        ui.ctx().request_repaint_after(Duration::from_secs_f64(wait));
    }
    let p = ui.painter();
    let c = rect.center();
    let r = size * 0.27 * scale;
    p.circle_filled(egui::pos2(c.x - r * 0.95, c.y - r * 0.55), r, color);
    p.circle_filled(egui::pos2(c.x + r * 0.95, c.y - r * 0.55), r, color);
    p.add(egui::Shape::convex_polygon(
        vec![
            egui::pos2(c.x - r * 1.9, c.y - r * 0.35),
            egui::pos2(c.x + r * 1.9, c.y - r * 0.35),
            egui::pos2(c.x, c.y + r * 1.75),
        ],
        color,
        Stroke::NONE,
    ));
}

/// The logo beside the name, in the plan's colour.
fn logo(ui: &mut egui::Ui, size: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    let rgb = ACCENT_RGB.load(Ordering::Relaxed);
    let id = egui::Id::unique(("imperio-logo", rgb));
    let cached = ui.ctx().data_mut(|d| d.get_temp::<egui::TextureHandle>(id));
    let texture = cached.unwrap_or_else(|| {
        const N: usize = 128;
        let image = egui::ColorImage::from_rgba_unmultiplied([N, N], &imperio_ui::logo::rgba(N as u32, rgb));
        let texture = ui.ctx().load_texture("imperio-logo", image, egui::TextureOptions::LINEAR);
        ui.ctx().data_mut(|d| d.insert_temp(id, texture.clone()));
        texture
    });
    // The drawing keeps a margin inside its picture; show it a little larger
    // so the coloured square itself is `size` across.
    let shown = rect.expand(size * 4.0 / 56.0);
    let whole = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
    ui.painter().image(texture.id(), shown, whole, Color32::WHITE);
}

/// Switch with a label and a one-line explanation. Returns true when toggled.
fn toggle_row(ui: &mut egui::Ui, label: &str, sub: &str, on: &mut bool) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(RichText::new(label).size(14.5).color(TEXT));
            ui.label(RichText::new(sub).size(12.0).color(MUTED));
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            changed = toggle(ui, on).changed();
        });
    });
    ui.add_space(8.0);
    changed
}

fn toggle(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let (rect, mut response) = ui.allocate_exact_size(Vec2::new(44.0, 24.0), Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    if ui.is_rect_visible(rect) {
        let how_on = ui.ctx().animate_bool_responsive(response.id, *on);
        let fill = if *on { accent() } else { BUTTON };
        let radius = 0.5 * rect.height();
        ui.painter().rect(rect, radius, fill, Stroke::new(1.0, STROKE), StrokeKind::Inside);
        let x = egui::lerp((rect.left() + radius)..=(rect.right() - radius), how_on);
        ui.painter().circle_filled(egui::pos2(x, rect.center().y), radius - 4.0, Color32::WHITE);
    }
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

// -------------------------------------------------------------------- theme

pub(crate) fn apply_theme(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = CARD;
    v.extreme_bg_color = FIELD;
    v.faint_bg_color = CARD;
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = accent().linear_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0, accent());
    v.hyperlink_color = accent();
    v.window_corner_radius = CornerRadius::same(12);
    v.window_stroke = Stroke::new(1.0, STROKE);
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = CornerRadius::same(8);
        w.bg_stroke = Stroke::new(1.0, STROKE);
        w.fg_stroke = Stroke::new(1.0, TEXT);
    }
    v.widgets.noninteractive.bg_fill = CARD;
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, MUTED);
    v.widgets.inactive.bg_fill = BUTTON;
    v.widgets.inactive.weak_bg_fill = BUTTON;
    v.widgets.hovered.bg_fill = CARD_HOVER;
    v.widgets.hovered.weak_bg_fill = CARD_HOVER;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, accent_hover());
    v.widgets.active.bg_fill = accent();
    v.widgets.active.weak_bg_fill = accent();
    ctx.set_visuals(v);
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = Vec2::new(10.0, 8.0);
        s.spacing.button_padding = Vec2::new(14.0, 8.0);
        s.spacing.interact_size = Vec2::new(40.0, 28.0);
        s.text_styles.insert(TextStyle::Heading, FontId::proportional(22.0));
        s.text_styles.insert(TextStyle::Body, FontId::proportional(14.5));
        s.text_styles.insert(TextStyle::Button, FontId::proportional(14.5));
        s.text_styles.insert(TextStyle::Small, FontId::proportional(12.0));
        s.text_styles.insert(TextStyle::Monospace, FontId::monospace(14.0));
    });
}

/// Ask DWM for a dark title bar so the frame matches the content.
pub(crate) fn dark_title_bar(frame: &eframe::Frame) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
    if let Ok(h) = frame.window_handle() {
        if let RawWindowHandle::Win32(w) = h.as_raw() {
            let hwnd = windows::Win32::Foundation::HWND(w.hwnd.get() as *mut _);
            let on: u32 = 1;
            // SAFETY: valid HWND; attribute value is a 4-byte BOOL.
            unsafe {
                let _ = DwmSetWindowAttribute(
                    hwnd,
                    DWMWA_USE_IMMERSIVE_DARK_MODE,
                    &on as *const u32 as *const _,
                    std::mem::size_of::<u32>() as u32,
                );
            }
        }
    }
}

/// The icon for the plan: red for Free, gold for Pro.
pub(crate) fn icon_for(pro: bool) -> egui::IconData {
    icon(if pro { PRO_ACCENT } else { FREE_ACCENT })
}

/// Window / taskbar / tray icon: the logo on a square of colour `rgb`.
fn icon(rgb: u32) -> egui::IconData {
    const N: u32 = 64;
    egui::IconData { rgba: imperio_ui::logo::rgba(N, rgb), width: N, height: N }
}

// ------------------------------------------------------------------ helpers

/// One manifest check: updates `state`, repaints, and raises a tray balloon
/// the first time a given version is seen (so a hidden Imperio still tells
/// the user). A version the user dismissed stays dismissed.
fn run_update_check(url: &str, state: &Arc<Mutex<UpdateState>>, ctx: &egui::Context) {
    let result = crate::update::check(url);
    let mut s = state.lock().unwrap();
    let known = match &*s {
        UpdateState::Available(a) | UpdateState::Dismissed(a) => Some(a.version.clone()),
        _ => None,
    };
    let dismissed = matches!(&*s, UpdateState::Dismissed(_));
    *s = match result {
        Ok(Some(a)) => {
            let same = known.as_deref() == Some(a.version.as_str());
            if !same {
                crate::tray::notify(
                    "Imperio update available",
                    &format!("Version {} is ready. Open Imperio and click \u{201c}Download and install\u{201d}.", a.version),
                );
            }
            if same && dismissed {
                UpdateState::Dismissed(a)
            } else {
                UpdateState::Available(a)
            }
        }
        Ok(None) => UpdateState::UpToDate,
        Err(e) => {
            tracing::info!(error = %e, "update check");
            UpdateState::Failed(e)
        }
    };
    drop(s);
    ctx.request_repaint();
}

fn run_firewall_helper() {
    use windows::core::HSTRING;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    let Ok(exe) = std::env::current_exe() else { return };
    // SAFETY: valid strings; "runas" triggers the UAC prompt.
    unsafe {
        let _ = ShellExecuteW(
            None,
            &HSTRING::from("runas"),
            &HSTRING::from(exe.to_string_lossy().as_ref()),
            &HSTRING::from("--firewall"),
            None,
            SW_HIDE,
        );
    }
}

/// Log to `%LOCALAPPDATA%\Imperio\imperio.log` (a GUI process has no console).
pub fn init_logging() {
    let dir = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Imperio");
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("imperio.log")) {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::from_default_env().add_directive("info".parse().unwrap()),
            )
            .with_ansi(false)
            .with_writer(Mutex::new(file))
            .try_init();
    }
}
