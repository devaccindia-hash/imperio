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
const BG: Color32 = Color32::from_rgb(0x1B, 0x1B, 0x1D);
const CARD: Color32 = Color32::from_rgb(0x25, 0x25, 0x28);
const CARD_HOVER: Color32 = Color32::from_rgb(0x2E, 0x2E, 0x32);
const STROKE: Color32 = Color32::from_rgb(0x38, 0x38, 0x3C);
const FIELD: Color32 = Color32::from_rgb(0x2B, 0x2B, 0x2F);
const BUTTON: Color32 = Color32::from_rgb(0x33, 0x33, 0x37);
const TEXT: Color32 = Color32::from_rgb(0xF2, 0xF2, 0xF2);
const MUTED: Color32 = Color32::from_rgb(0x9C, 0x9C, 0xA4);
const GREEN: Color32 = Color32::from_rgb(0x3D, 0xDC, 0x84);
const AMBER: Color32 = Color32::from_rgb(0xFF, 0xB4, 0x54);
const ERROR: Color32 = Color32::from_rgb(0xFF, 0x7B, 0x72);

/// Accent colour as 0xRRGGBB: red on the Free plan, gold once a Pro licence
/// is active, so paying users see the difference everywhere at once.
static ACCENT_RGB: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0xE6453B);
const FREE_ACCENT: u32 = 0xE6453B;
const PRO_ACCENT: u32 = 0xE3B341;

fn accent() -> Color32 {
    let v = ACCENT_RGB.load(Ordering::Relaxed);
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

fn accent_hover() -> Color32 {
    let a = accent();
    let lift = |c: u8| (c as u16 + (255 - c as u16) * 22 / 100) as u8;
    Color32::from_rgb(lift(a.r()), lift(a.g()), lift(a.b()))
}

/// Switch the look between Free (red) and Pro (gold) and re-apply the theme.
fn set_pro_look(pro: bool, ctx: &egui::Context) {
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
}

pub fn run() -> anyhow::Result<()> {
    init_logging();
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
        let cfg = config::load_or_create();
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

    fn save_cfg(&mut self) {
        self.cfg.require_accept = self.ask_first;
        self.cfg.open_mode = self.open_mode;
        if let Err(e) = config::save(&self.cfg) {
            tracing::warn!(error = %e, "config save failed");
        }
    }

    /// Keep `target` at the top of the saved list (persisted in the config).
    fn remember(&mut self, target: String) {
        let target = target.trim().to_string();
        if target.is_empty() {
            return;
        }
        self.cfg.recent.retain(|r| r != &target);
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
        self.remember(target.clone());

        let code: String = self.code.chars().filter(|c| !c.is_whitespace()).collect();
        let session = self.session.clone();
        let ctx = self.ctx.clone();
        // The remote-screen window runs in a child process (see module docs).
        // It prints `connected` on stdout once the link is up and, on
        // failure, the reason on stderr with a non-zero exit code.
        let exe = std::env::current_exe().unwrap_or_else(|_| "imperio.exe".into());
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("session").arg(&target);
        if !code.is_empty() {
            cmd.arg("--code").arg(&code);
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
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        *session.lock().unwrap() = SessionState::Connecting;
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
                    let mut err = String::new();
                    if let Some(mut e) = child.stderr.take() {
                        let _ = e.read_to_string(&mut err);
                    }
                    let status = child.wait().map_err(|e| e.to_string())?;
                    if status.success() {
                        Ok(())
                    } else {
                        Err(err
                            .lines()
                            .rev()
                            .find(|l| !l.trim().is_empty())
                            .unwrap_or("the session ended with an error")
                            .to_string())
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
            dark_title_bar(frame);
            crate::tray::install(frame);
            if self.just_updated {
                crate::tray::notify(
                    "Imperio updated",
                    &format!("You\u{2019}re now on version {}.", crate::update::CURRENT),
                );
            }
        }
        if self.copied.is_some() {
            ctx.request_repaint_after(Duration::from_millis(200));
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
            let enter = field(ui, &mut self.target, "Enter the other computer\u{2019}s address or ID", field_w, 17.0);
            let enter2 = field(ui, &mut self.code, "Code (if any)", code_w, 16.0);
            let label = match session {
                SessionState::Connecting => "Connecting\u{2026}",
                SessionState::Connected => "Session open",
                _ => "Connect",
            };
            let button = egui::Button::new(RichText::new(label).size(15.0).strong().color(Color32::WHITE))
                .fill(if busy { BUTTON } else { accent() })
                .corner_radius(CornerRadius::same(10))
                .min_size(Vec2::new(connect_w, 46.0));
            let clicked = ui.add_enabled(!busy, button).clicked();
            if !busy && (clicked || enter || enter2) {
                self.connect();
            }
        });
        let line = match session {
            SessionState::Failed(e) => Some((format!("\u{26A0} {e}"), ERROR)),
            SessionState::Connecting => Some(("Reaching the other computer\u{2026}".to_string(), MUTED)),
            SessionState::Connected => Some(("The remote screen is open in its own window. F11 for full screen.".to_string(), MUTED)),
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
                                let install = egui::Button::new(
                                    RichText::new("Download and install").size(13.5).strong().color(Color32::WHITE),
                                )
                                .fill(accent())
                                .corner_radius(CornerRadius::same(8))
                                .min_size(Vec2::new(0.0, 32.0));
                                // `hovered()` guards against a phantom click
                                // reported on the button's first frame.
                                let r = ui.add(install);
                                let rested = self.dwell_ok("install_banner", r.hovered());
                                if r.clicked() && rested {
                                    self.install_update(a.clone(), "banner");
                                } else if r.clicked() {
                                    tracing::warn!("ignored install click: pointer had not rested on the button");
                                }
                            });
                        });
                    });
            }
            UpdateState::Downloading => {
                ui.add_space(8.0);
                ui.label(
                    RichText::new("Downloading the update\u{2026} Imperio will restart by itself when it is ready.")
                        .size(12.5)
                        .color(AMBER),
                );
            }
            _ => {}
        }
    }

    /// Centered "Your ID 425 922 0101" with code and address underneath.
    fn your_id(&mut self, ui: &mut egui::Ui, status: &HostStatus, host_error: Option<&str>) {
        let centered = Layout::left_to_right(Align::Center).with_main_align(Align::Center);
        let w = ui.available_width();
        ui.add_space(2.0);
        ui.allocate_ui_with_layout(Vec2::new(w, 60.0), centered, |ui| {
            ui.label(RichText::new("Your ID").size(20.0).color(MUTED));
            ui.add_space(14.0);
            let id = if status.device_id.is_empty() { "\u{2014}".to_string() } else { status.device_id.clone() };
            ui.label(RichText::new(id).size(46.0).strong().color(accent()));
            ui.add_space(12.0);
            let label = if self.copied_recently("id") { "Copied" } else { "Copy" };
            if ghost_button(ui, label).clicked() && !status.device_id.is_empty() {
                let digits: String = status.device_id.chars().filter(|c| !c.is_whitespace()).collect();
                self.copy("id", digits);
            }
        });
        ui.allocate_ui_with_layout(Vec2::new(w, 36.0), centered, |ui| {
            if let Some(code) = &status.code {
                let shown = format!("{} {}", &code[..code.len().min(3)], &code[code.len().min(3)..]);
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
                ui.label(RichText::new("No code needed for your own Tailscale devices").size(15.0).color(TEXT));
            } else if !status.security.is_empty() {
                ui.label(RichText::new(&status.security).size(15.0).color(TEXT));
            }
        });
        ui.vertical_centered(|ui| {
            if status.code.is_some() {
                ui.label(
                    RichText::new("Tell the person who will connect your ID or address and this code. The code changes after each session.")
                        .size(12.5)
                        .color(MUTED),
                );
            }
            if status.addresses.is_empty() {
                ui.label(RichText::new("Looking up this computer\u{2019}s address\u{2026}").size(13.0).color(MUTED));
            } else {
                let joined = status.addresses.join("   \u{2022}   ");
                ui.label(RichText::new(format!("Your address:  {joined}")).size(13.0).color(TEXT));
            }
            ui.label(RichText::new(&status.status).size(12.5).color(MUTED));
            if let Some(e) = host_error {
                ui.label(RichText::new(format!("\u{26A0} {e}")).size(12.5).color(ERROR));
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
        if self.cfg.recent.is_empty() {
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
            return;
        }
        let mut connect_to: Option<String> = None;
        let mut remove: Option<String> = None;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(14.0, 14.0);
            for r in self.cfg.recent.clone() {
                let resp = tile(ui, &r, busy);
                if resp.clicked() && !busy {
                    connect_to = Some(r.clone());
                }
                if resp.secondary_clicked() {
                    remove = Some(r);
                }
            }
        });
        ui.add_space(10.0);
        ui.label(RichText::new("Click a computer to connect (uses the code typed above, if any). Right-click to remove.").size(12.0).color(MUTED));
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
                if toggle_row(
                    ui,
                    "No code for my own Tailscale devices",
                    "Open mode: only Tailscale devices and this PC may connect",
                    &mut self.open_mode,
                ) {
                    self.save_cfg();
                    self.start_engine();
                }
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
            UpdateState::Downloading => "Downloading the update\u{2026} Imperio will restart by itself.".to_string(),
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
                let install = egui::Button::new(RichText::new(format!("Install {}", a.version)).size(13.0).strong().color(Color32::WHITE))
                    .fill(accent())
                    .corner_radius(CornerRadius::same(8));
                let r = ui.add(install);
                let rested = self.dwell_ok("install_settings", r.hovered());
                if r.clicked() && rested {
                    self.install_update(a.clone(), "settings");
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

fn ghost_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(text).size(13.0).color(TEXT))
            .fill(BUTTON)
            .stroke(Stroke::new(1.0, STROKE))
            .corner_radius(CornerRadius::same(8))
            .min_size(Vec2::new(0.0, 30.0)),
    )
}

/// A saved computer as a clickable tile.
fn tile(ui: &mut egui::Ui, addr: &str, busy: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(228.0, 104.0), Sense::click());
    let hovered = resp.hovered() && !busy;
    let p = ui.painter();
    p.rect(
        rect,
        CornerRadius::same(12),
        if hovered { CARD_HOVER } else { CARD },
        Stroke::new(1.0, if hovered { accent() } else { STROKE }),
        StrokeKind::Inside,
    );
    // Little monitor glyph.
    let m = egui::Rect::from_min_size(rect.min + Vec2::new(16.0, 16.0), Vec2::new(26.0, 17.0));
    p.rect_stroke(m, CornerRadius::same(3), Stroke::new(1.8, accent()), StrokeKind::Inside);
    p.line_segment([m.center_bottom() + Vec2::new(-6.0, 5.0), m.center_bottom() + Vec2::new(6.0, 5.0)], Stroke::new(1.8, accent()));
    p.text(
        rect.min + Vec2::new(16.0, 46.0),
        Align2::LEFT_TOP,
        addr,
        FontId::proportional(16.0),
        TEXT,
    );
    p.text(
        rect.min + Vec2::new(16.0, 76.0),
        Align2::LEFT_TOP,
        if busy { "session in progress" } else { "Click to connect" },
        FontId::proportional(12.5),
        if busy { MUTED } else { accent() },
    );
    if busy {
        resp
    } else {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
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
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    let t = ui.input(|i| i.time);
    let phase = (t % 1.6) / 1.6;
    let beat = |center: f64| (1.0 - ((phase - center).abs() / 0.09)).max(0.0);
    let scale = 1.0 + 0.16 * (beat(0.10) + 0.7 * beat(0.30)) as f32;
    ui.ctx().request_repaint_after(Duration::from_millis(33));
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

fn logo(ui: &mut egui::Ui, size: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(7), accent());
    p.circle_filled(rect.center(), size * 0.18, Color32::WHITE);
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

fn apply_theme(ctx: &egui::Context) {
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
fn dark_title_bar(frame: &eframe::Frame) {
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

/// Window / taskbar / tray icon: rounded square in `rgb` with a white dot.
fn icon(rgb: u32) -> egui::IconData {
    let (r, g, b) = ((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8);
    let n = 64usize;
    let mut rgba = vec![0u8; n * n * 4];
    for y in 0..n {
        for x in 0..n {
            let (fx, fy) = (x as f32 - 31.5, y as f32 - 31.5);
            let inside = fx.abs() < 28.0
                && fy.abs() < 28.0
                && (fx.abs() < 20.0 || fy.abs() < 20.0 || (fx.abs() - 20.0).hypot(fy.abs() - 20.0) < 8.0);
            let i = (y * n + x) * 4;
            if inside {
                rgba[i..i + 4].copy_from_slice(&[r, g, b, 0xFF]);
            }
            if (fx * fx + fy * fy).sqrt() < 9.0 {
                rgba[i..i + 4].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
            }
        }
    }
    egui::IconData { rgba, width: n as u32, height: n as u32 }
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
