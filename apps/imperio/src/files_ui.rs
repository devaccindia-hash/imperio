//! The Files window: this computer on the left, the other one on the right.
//!
//! Started by the session window as `imperio files-window`, with pipes on
//! standard input and output (see `imperio_files::ipc`). This process only
//! draws and asks; the session window owns the connection and moves the
//! files. Work on this computer's own side (listing, rename, delete…) is
//! done here directly, off the UI thread.
//!
//! Files move by dragging rows from one side to the other, by the Send /
//! Bring buttons, by copy and paste, or by dropping files from Explorer
//! (those go to the folder shown on the other computer's side).

use std::collections::{BTreeSet, HashMap};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use egui::{Align, Align2, Color32, CornerRadius, FontId, Key, Layout, Margin, RichText, Sense, Stroke, StrokeKind, Vec2};
use imperio_files::ipc::{self, UiCmd, UiEvent};
use imperio_files::{clip, ops};
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use imperio_files::{Entry, Event, Op};

use crate::app::{accent, apply_theme, ghost_button, BG, BUTTON, CARD, CARD_HOVER, ERROR, GREEN, MUTED, STROKE, TEXT};

const ROW_H: f32 = 26.0;
const SIZE_COL: f32 = 86.0;
const DATE_COL: f32 = 128.0;

/// What arrives on the UI thread from the reader and worker threads.
enum In {
    Ui(UiEvent),
    /// The session window closed its end: the session is over.
    Closed,
    LocalListing { id: u32, result: Result<ops::Listing, String> },
    LocalOp { id: u32, error: Option<String> },
}

/// Rows being dragged from one side.
struct DragPayload {
    remote: bool,
    paths: Vec<String>,
}

/// Copied or cut rows, waiting for Paste.
struct Clip {
    remote: bool,
    paths: Vec<String>,
    cut: bool,
}

#[derive(Default)]
struct Pane {
    remote: bool,
    title: String,
    /// Folder shown; empty = the starting places (user folders and drives).
    path: String,
    path_edit: String,
    parent: Option<String>,
    entries: Vec<Entry>,
    selected: BTreeSet<usize>,
    anchor: Option<usize>,
    loading: bool,
    /// Shown above the list: why a folder could not be opened, or a note.
    message: Option<String>,
    /// Id of the newest listing asked for; older answers are dropped.
    request: u32,
    /// Listed at least once.
    ready: bool,
}

impl Pane {
    fn selected_paths(&self) -> Vec<String> {
        self.selected.iter().filter_map(|&i| self.entries.get(i)).map(|e| e.path.clone()).collect()
    }

    fn select_only(&mut self, i: usize) {
        self.selected.clear();
        self.selected.insert(i);
        self.anchor = Some(i);
    }
}

enum XferState {
    Running,
    Done,
    Failed(String),
}

struct Transfer {
    xfer: u32,
    label: String,
    /// To the other computer (true) or from it.
    upload: bool,
    dest: String,
    name: String,
    files_done: u32,
    files_total: u32,
    bytes_done: u64,
    bytes_total: u64,
    state: XferState,
    started: Instant,
    took: Option<Duration>,
}

enum Dialog {
    Rename { remote: bool, path: String, name: String, focus: bool },
    NewFolder { remote: bool, parent: String, name: String, focus: bool },
    Delete { remote: bool, paths: Vec<String>, what: String },
}

/// What a pane asks the window to do; applied after drawing.
enum Action {
    Activate(bool),
    Go(bool, String),
    Refresh(bool),
    /// Move `paths` of side `from_remote` to the other side's folder.
    Transfer { from_remote: bool, paths: Vec<String> },
    Copy(bool, bool),
    Paste(bool),
    AskRename(bool),
    AskNewFolder(bool),
    AskDelete(bool),
}

/// What a pane needs to know about the rest of the window.
struct Env {
    other_title: String,
    /// The other computer has answered and allows file access.
    remote_ready: bool,
    can_paste: bool,
    active: bool,
}

struct FilesApp {
    rx: mpsc::Receiver<In>,
    tx: mpsc::Sender<In>,
    ctx: egui::Context,
    local: Pane,
    remote: Pane,
    /// The side that was last clicked; keyboard shortcuts act on it.
    active_remote: bool,
    transfers: Vec<Transfer>,
    dialog: Option<Dialog>,
    clip: Option<Clip>,
    /// The Windows clipboard as it was when `clip` was taken; a newer one
    /// holding files replaces it.
    clip_seq: u32,
    v_down: bool,
    /// Changes awaiting an answer: id -> (side, what was asked).
    pending_ops: HashMap<u32, (bool, &'static str)>,
    next_id: u32,
    next_xfer: u32,
    /// The other computer's answer: `Some(allowed)`.
    welcome: Option<bool>,
    remote_desktop: String,
    asked_at: Instant,
    notice: Option<(String, bool, Instant)>,
    first_frame: bool,
}

pub fn run() -> anyhow::Result<()> {
    crate::app::init_logging();
    let pro = imperio_host::config::load_or_create()
        .license_key
        .as_deref()
        .map(|k| crate::license::verify_key(k).is_ok())
        .unwrap_or(false);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Imperio Files")
            .with_inner_size([1120.0, 740.0])
            .with_min_inner_size([860.0, 520.0])
            .with_icon(std::sync::Arc::new(crate::app::icon_for(pro))),
        ..Default::default()
    };
    eframe::run_native("Imperio Files", options, Box::new(move |cc| Ok(Box::new(FilesApp::new(cc, pro)))))
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// File names are in every script; the bundled fonts are not. Add the
/// system's own fonts behind them.
fn add_system_fonts(ctx: &egui::Context) {
    let dir = std::env::var_os("WINDIR").map(std::path::PathBuf::from).unwrap_or_else(|| "C:\\Windows".into()).join("Fonts");
    for file in ["segoeui.ttf", "Nirmala.ttc", "Nirmala.ttf", "msyh.ttc", "malgun.ttf", "seguisym.ttf"] {
        let Ok(bytes) = std::fs::read(dir.join(file)) else { continue };
        ctx.add_font(egui::epaint::text::FontInsert {
            name: file.to_string(),
            data: egui::FontData::from_owned(bytes),
            families: vec![
                egui::epaint::text::InsertFontFamily {
                    family: egui::FontFamily::Proportional,
                    priority: egui::epaint::text::FontPriority::Lowest,
                },
                egui::epaint::text::InsertFontFamily {
                    family: egui::FontFamily::Monospace,
                    priority: egui::epaint::text::FontPriority::Lowest,
                },
            ],
        });
    }
}

impl FilesApp {
    fn new(cc: &eframe::CreationContext<'_>, pro: bool) -> Self {
        crate::app::set_pro_look(pro, &cc.egui_ctx);
        apply_theme(&cc.egui_ctx);
        add_system_fonts(&cc.egui_ctx);
        let (tx, rx) = mpsc::channel();
        // Events from the session window, until it closes the pipe.
        let reader_tx = tx.clone();
        let ctx = cc.egui_ctx.clone();
        std::thread::Builder::new()
            .name("files-events".into())
            .spawn(move || {
                let mut stdin = std::io::BufReader::new(std::io::stdin());
                while let Ok(ev) = ipc::read_frame::<UiEvent>(&mut stdin) {
                    if reader_tx.send(In::Ui(ev)).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                }
                let _ = reader_tx.send(In::Closed);
                ctx.request_repaint();
            })
            .ok();
        let mut app = Self {
            rx,
            tx,
            ctx: cc.egui_ctx.clone(),
            local: Pane { title: "This computer".into(), ..Default::default() },
            remote: Pane { remote: true, title: "The other computer".into(), loading: true, ..Default::default() },
            active_remote: false,
            transfers: Vec::new(),
            dialog: None,
            clip: None,
            clip_seq: 0,
            v_down: false,
            pending_ops: HashMap::new(),
            next_id: 1,
            next_xfer: 1,
            welcome: None,
            remote_desktop: String::new(),
            asked_at: Instant::now(),
            notice: None,
            first_frame: true,
        };
        app.send(&UiCmd::Hello);
        // Tests point both sides at scratch folders instead of the Desktop.
        let start = std::env::var("IMPERIO_FILES_START_LOCAL").unwrap_or_else(|_| ops::desktop().to_string_lossy().into_owned());
        app.go(false, start);
        app
    }

    fn send(&self, cmd: &UiCmd) {
        // Standard output is the pipe to the session window; nothing else
        // in this process writes to it.
        let _ = ipc::write_frame(&mut std::io::stdout().lock(), cmd);
    }

    fn pane(&mut self, remote: bool) -> &mut Pane {
        if remote {
            &mut self.remote
        } else {
            &mut self.local
        }
    }

    fn id(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id
    }

    fn say(&mut self, text: impl Into<String>, is_error: bool) {
        self.notice = Some((text.into(), is_error, Instant::now()));
    }

    /// Show `path` on one side (empty = the starting places).
    fn go(&mut self, remote: bool, path: String) {
        let id = self.id();
        let p = self.pane(remote);
        p.request = id;
        p.loading = true;
        if remote {
            self.send(&UiCmd::List { id, path });
        } else {
            let tx = self.tx.clone();
            let ctx = self.ctx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(In::LocalListing { id, result: ops::list(&path) });
                ctx.request_repaint();
            });
        }
    }

    fn refresh(&mut self, remote: bool) {
        let path = self.pane(remote).path.clone();
        self.go(remote, path);
    }

    /// Ask for a change on one side; the answer refreshes that side.
    fn run_op(&mut self, remote: bool, op: Op, what: &'static str) {
        let id = self.id();
        self.pending_ops.insert(id, (remote, what));
        if remote {
            self.send(&UiCmd::Op { id, op });
        } else {
            let tx = self.tx.clone();
            let ctx = self.ctx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(In::LocalOp { id, error: ops::run(&op).err() });
                ctx.request_repaint();
            });
        }
    }

    /// Send `paths` of one side into the folder shown on the other.
    fn transfer(&mut self, from_remote: bool, paths: Vec<String>) {
        if paths.is_empty() {
            return;
        }
        if self.welcome != Some(true) {
            self.say("The other computer is not sharing its files.", true);
            return;
        }
        let dest = self.pane(!from_remote).path.clone();
        if dest.is_empty() {
            let side = if from_remote { "this computer\u{2019}s" } else { "the other computer\u{2019}s" };
            self.say(format!("Open a folder on {side} side first; that is where the files will go."), true);
            return;
        }
        self.next_xfer += 1;
        let xfer = self.next_xfer;
        let label = match paths.as_slice() {
            [one] => file_name(one),
            many => format!("{} items", many.len()),
        };
        self.transfers.push(Transfer::new(xfer, label, !from_remote, dest.clone()));
        if from_remote {
            self.send(&UiCmd::Download { xfer, paths, dest });
        } else {
            self.send(&UiCmd::Upload { xfer, paths, dest });
        }
    }

    fn handle(&mut self, msg: In) {
        match msg {
            In::Closed => self.ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            In::LocalListing { id, result } => {
                let (path, parent, entries, error) = match result {
                    Ok(l) => (l.path, l.parent, l.entries, l.note),
                    Err(e) => (String::new(), None, Vec::new(), Some(e)),
                };
                self.listing(false, id, path, parent, entries, error);
            }
            In::LocalOp { id, error } => self.op_done(id, error),
            In::Ui(UiEvent::Focus) => self.ctx.send_viewport_cmd(egui::ViewportCommand::Focus),
            In::Ui(UiEvent::Started { xfer, label, dest, upload }) => {
                self.transfers.push(Transfer::new(xfer, label, upload, dest));
            }
            In::Ui(UiEvent::Engine(ev)) => match ev {
                Event::Welcome { allowed, computer, desktop, .. } => {
                    let first = self.welcome.is_none();
                    let was = self.welcome.replace(allowed);
                    self.remote.title = computer;
                    self.remote_desktop = desktop.clone();
                    if allowed && (first || was == Some(false)) {
                        self.go(true, std::env::var("IMPERIO_FILES_START_REMOTE").unwrap_or(desktop));
                    } else if !allowed {
                        self.remote.loading = false;
                    }
                }
                Event::Listing { id, path, parent, entries, error } => self.listing(true, id, path, parent, entries, error),
                Event::OpDone { id, error } => self.op_done(id, error),
                Event::RemoteClip { .. } | Event::DragItems { .. } => {}
                Event::Progress { xfer, name, files_done, files_total, bytes_done, bytes_total } => {
                    if let Some(t) = self.transfers.iter_mut().find(|t| t.xfer == xfer) {
                        t.name = name;
                        t.files_done = files_done;
                        t.files_total = files_total;
                        t.bytes_done = bytes_done;
                        t.bytes_total = bytes_total;
                    }
                }
                Event::Finished { xfer, error, files, bytes } => {
                    let mut refresh = None;
                    if let Some(t) = self.transfers.iter_mut().find(|t| t.xfer == xfer) {
                        t.files_done = files;
                        t.bytes_done = bytes;
                        t.took = Some(t.started.elapsed());
                        // A note about files left out still means the rest arrived.
                        t.state = match error {
                            Some(e) => XferState::Failed(e),
                            None => XferState::Done,
                        };
                        refresh = Some((t.upload, t.dest.clone()));
                    }
                    if let Some((remote, dest)) = refresh {
                        if self.pane(remote).path.eq_ignore_ascii_case(&dest) {
                            self.refresh(remote);
                        }
                    }
                }
            },
        }
    }

    fn listing(&mut self, remote: bool, id: u32, path: String, parent: Option<String>, entries: Vec<Entry>, error: Option<String>) {
        let p = self.pane(remote);
        if id != p.request {
            return; // an answer to an older question
        }
        p.loading = false;
        if entries.is_empty() && error.is_some() && (p.ready || !path.is_empty()) && parent.is_none() {
            // Could not be opened: stay where we were and say why.
            p.message = error;
            p.path_edit = p.path.clone();
            if !p.ready {
                // Nothing shown yet: fall back to the starting places.
                p.ready = true;
                self.go(remote, String::new());
            }
            return;
        }
        let moved = p.path != path;
        p.ready = true;
        p.path = path.clone();
        p.path_edit = path;
        p.parent = parent;
        p.entries = entries;
        p.message = error;
        if moved {
            p.selected.clear();
            p.anchor = None;
        } else {
            let n = p.entries.len();
            p.selected.retain(|&i| i < n);
        }
    }

    fn op_done(&mut self, id: u32, error: Option<String>) {
        let Some((remote, what)) = self.pending_ops.remove(&id) else { return };
        match error {
            Some(e) => self.say(format!("{what} did not work. {e}"), true),
            None => self.say(format!("{what}: done."), false),
        }
        let p = self.pane(remote);
        p.selected.clear();
        self.refresh(remote);
    }

    fn apply(&mut self, action: Action) {
        match action {
            Action::Activate(remote) => self.active_remote = remote,
            Action::Go(remote, path) => self.go(remote, path),
            Action::Refresh(remote) => self.refresh(remote),
            Action::Transfer { from_remote, paths } => self.transfer(from_remote, paths),
            Action::Copy(remote, cut) => {
                let paths = self.pane(remote).selected_paths();
                if paths.is_empty() {
                    return;
                }
                // Rows of this computer also go on the Windows clipboard, so
                // they can be pasted in Explorer or any other program.
                self.clip_seq = match remote {
                    false => clip::write_files(&paths.iter().map(std::path::PathBuf::from).collect::<Vec<_>>()),
                    true => None,
                }
                .unwrap_or_else(clip::sequence);
                let n = paths.len();
                self.clip = Some(Clip { remote, paths, cut });
                self.say(format!("{} ready to paste. Open the folder you want and choose Paste.", count(n, "item")), false);
            }
            Action::Paste(remote) => {
                // Files copied in Explorer (or anywhere else) since the last
                // copy made in this window are what "paste" means now.
                let os = clip::sequence();
                if self.clip.is_none() || os != self.clip_seq {
                    let files = clip::read_files();
                    if !files.is_empty() {
                        let paths = files.iter().map(|f| f.to_string_lossy().into_owned()).collect();
                        self.clip = Some(Clip { remote: false, paths, cut: false });
                        self.clip_seq = os;
                    }
                }
                let Some(clip) = self.clip.take() else { return };
                if clip.remote != remote {
                    // Onto the other computer: that is a transfer.
                    let from = clip.remote;
                    let paths = clip.paths.clone();
                    self.clip = Some(Clip { cut: false, ..clip });
                    self.transfer(from, paths);
                    return;
                }
                let to_dir = self.pane(remote).path.clone();
                if to_dir.is_empty() {
                    self.say("Open a folder first, then paste into it.", true);
                    self.clip = Some(clip);
                    return;
                }
                if clip.cut {
                    self.run_op(remote, Op::Move { from: clip.paths, to_dir }, "Move");
                } else {
                    self.run_op(remote, Op::Copy { from: clip.paths.clone(), to_dir }, "Copy");
                    self.clip = Some(clip);
                }
            }
            Action::AskRename(remote) => {
                let p = self.pane(remote);
                if let (1, Some(e)) = (p.selected.len(), p.selected.iter().next().and_then(|&i| p.entries.get(i))) {
                    self.dialog = Some(Dialog::Rename { remote, path: e.path.clone(), name: e.name.clone(), focus: true });
                }
            }
            Action::AskNewFolder(remote) => {
                let parent = self.pane(remote).path.clone();
                if !parent.is_empty() {
                    self.dialog = Some(Dialog::NewFolder { remote, parent, name: "New folder".into(), focus: true });
                }
            }
            Action::AskDelete(remote) => {
                let p = self.pane(remote);
                let paths = p.selected_paths();
                let what = match paths.as_slice() {
                    [] => return,
                    [one] => format!("\u{201c}{}\u{201d}", file_name(one)),
                    many => format!("these {} items", many.len()),
                };
                self.dialog = Some(Dialog::Delete { remote, paths, what });
            }
        }
    }

    /// Keys for the side that was last clicked.
    fn shortcuts(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        if self.dialog.is_some() || ctx.text_edit_focused() {
            return;
        }
        let remote = self.active_remote;
        let in_folder = !self.pane(remote).path.is_empty();
        let (mut copy, mut cut) = (false, false);
        // Ctrl+V is read from the keyboard itself: the toolkit only reports a
        // paste when the clipboard holds text, and here it holds files.
        let (v, ctrl) = unsafe { (GetAsyncKeyState(0x56) < 0, GetAsyncKeyState(0x11) < 0) };
        let paste = v && ctrl && !self.v_down && ctx.input(|i| i.focused);
        self.v_down = v;
        let (f5, del, f2, back, enter, all) = ctx.input(|i| {
            for e in &i.events {
                match e {
                    egui::Event::Copy => copy = true,
                    egui::Event::Cut => cut = true,

                    _ => {}
                }
            }
            (
                i.key_pressed(Key::F5),
                i.key_pressed(Key::Delete),
                i.key_pressed(Key::F2),
                i.key_pressed(Key::Backspace),
                i.key_pressed(Key::Enter),
                i.modifiers.command && i.key_pressed(Key::A),
            )
        });
        if f5 {
            actions.push(Action::Refresh(remote));
        }
        if copy {
            actions.push(Action::Copy(remote, false));
        }
        if cut && in_folder {
            actions.push(Action::Copy(remote, true));
        }
        if paste {
            actions.push(Action::Paste(remote));
        }
        if del && in_folder {
            actions.push(Action::AskDelete(remote));
        }
        if f2 && in_folder {
            actions.push(Action::AskRename(remote));
        }
        let p = self.pane(remote);
        if back {
            if let Some(parent) = p.parent.clone() {
                actions.push(Action::Go(remote, parent));
            }
        }
        if enter {
            if let (1, Some(e)) = (p.selected.len(), p.selected.iter().next().and_then(|&i| p.entries.get(i))) {
                if e.dir {
                    actions.push(Action::Go(remote, e.path.clone()));
                }
            }
        }
        if all {
            p.selected = (0..p.entries.len()).collect();
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.dialog.take() else { return };
        let mut done: Option<bool> = None; // Some(true) = confirmed
        let modal = egui::Modal::new(egui::Id::unique("files-dialog")).show(ctx, |ui| {
            ui.set_width(380.0);
            let (enter, escape) = ui.input(|i| (i.key_pressed(Key::Enter), i.key_pressed(Key::Escape)));
            let confirm = match &mut dialog {
                Dialog::Rename { name, focus, .. } => name_field(ui, "Rename", name, focus),
                Dialog::NewFolder { name, focus, .. } => name_field(ui, "New folder", name, focus),
                Dialog::Delete { remote, what, paths } => {
                    let place = if *remote { self.remote.title.clone() } else { "this computer".to_string() };
                    ui.label(RichText::new(format!("Delete {what}?")).size(17.0).strong().color(TEXT));
                    ui.add_space(4.0);
                    let (it, them) = if paths.len() == 1 { ("It goes", "it") } else { ("They go", "they") };
                    ui.label(
                        RichText::new(format!("{it} to the Recycle Bin on {place}, where {them} can be restored."))
                            .size(13.0)
                            .color(MUTED),
                    );
                    false
                }
            };
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let label = match &dialog {
                        Dialog::Rename { .. } => "Rename",
                        Dialog::NewFolder { .. } => "Create",
                        Dialog::Delete { .. } => "Delete",
                    };
                    let go = ui
                        .add(
                            egui::Button::new(RichText::new(label).size(13.5).strong().color(Color32::WHITE))
                                .fill(accent())
                                .corner_radius(CornerRadius::same(8))
                                .min_size(Vec2::new(96.0, 32.0)),
                        )
                        .clicked();
                    if go || confirm || (enter && matches!(dialog, Dialog::Delete { .. })) {
                        done = Some(true);
                    }
                    if ghost_button(ui, "Cancel").clicked() || escape {
                        done.get_or_insert(false);
                    }
                });
            });
        });
        if modal.should_close() {
            done.get_or_insert(false);
        }
        match done {
            None => self.dialog = Some(dialog),
            Some(false) => {}
            Some(true) => match dialog {
                Dialog::Rename { remote, path, name, .. } => self.run_op(remote, Op::Rename { path, name }, "Rename"),
                Dialog::NewFolder { remote, parent, name, .. } => self.run_op(remote, Op::NewFolder { parent, name }, "New folder"),
                Dialog::Delete { remote, paths, .. } => self.run_op(remote, Op::Delete { paths }, "Delete"),
            },
        }
    }

    fn transfers_ui(&mut self, ui: &mut egui::Ui) {
        let mut cancel = None;
        let any_finished = self.transfers.iter().any(|t| !matches!(t.state, XferState::Running));
        ui.horizontal(|ui| {
            ui.label(RichText::new("Transfers").size(14.0).strong().color(TEXT));
            if any_finished {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.link(RichText::new("Clear finished").size(12.5)).clicked() {
                        self.transfers.retain(|t| matches!(t.state, XferState::Running));
                    }
                });
            }
        });
        let other = self.remote.title.clone();
        egui::ScrollArea::vertical().id_salt("transfers").max_height(132.0).show(ui, |ui| {
            for t in self.transfers.iter().rev() {
                ui.horizontal(|ui| {
                    let direction = if t.upload { format!("To {other}") } else { "To this computer".to_string() };
                    ui.allocate_ui_with_layout(Vec2::new(250.0, 20.0), Layout::left_to_right(Align::Center), |ui| {
                        ui.set_min_width(250.0);
                        ui.add(
                            egui::Label::new(RichText::new(format!("{} \u{2014} {}", t.label, direction)).size(13.0).color(TEXT))
                                .truncate(),
                        );
                    });
                    match &t.state {
                        XferState::Running => {
                            let frac = if t.bytes_total > 0 { t.bytes_done as f32 / t.bytes_total as f32 } else { 0.0 };
                            ui.add(egui::ProgressBar::new(frac.clamp(0.0, 1.0)).desired_width(220.0).desired_height(10.0).fill(accent()));
                            let secs = t.started.elapsed().as_secs_f64().max(0.2);
                            let mut detail = format!("{} of {}", size_text(t.bytes_done), size_text(t.bytes_total));
                            if t.files_total > 1 {
                                detail.push_str(&format!(" \u{00b7} file {} of {}", (t.files_done + 1).min(t.files_total), t.files_total));
                            }
                            detail.push_str(&format!(" \u{00b7} {}/s", size_text((t.bytes_done as f64 / secs) as u64)));
                            ui.label(RichText::new(detail).size(12.0).color(MUTED));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui.link(RichText::new("Cancel").size(12.5)).clicked() {
                                    cancel = Some(t.xfer);
                                }
                            });
                        }
                        XferState::Done => {
                            let took = t.took.unwrap_or_default().as_secs_f64();
                            ui.label(
                                RichText::new(format!("Done \u{00b7} {}, {} in {:.1} s", count(t.files_done as usize, "file"), size_text(t.bytes_done), took))
                                    .size(12.5)
                                    .color(GREEN),
                            );
                        }
                        XferState::Failed(e) => {
                            let arrived = if t.files_done > 0 { format!(" ({} arrived)", count(t.files_done as usize, "file")) } else { String::new() };
                            ui.add(egui::Label::new(RichText::new(format!("{e}{arrived}")).size(12.5).color(ERROR)).truncate())
                                .on_hover_text(e);
                        }
                    }
                });
            }
        });
        if let Some(xfer) = cancel {
            self.send(&UiCmd::Cancel { xfer });
        }
    }
}

/// The text field of the Rename / New folder dialogs. True when Enter was
/// pressed in it.
fn name_field(ui: &mut egui::Ui, title: &str, name: &mut String, focus: &mut bool) -> bool {
    ui.label(RichText::new(title).size(17.0).strong().color(TEXT));
    ui.add_space(6.0);
    let resp = ui.add(egui::TextEdit::singleline(name).desired_width(f32::INFINITY).font(FontId::proportional(15.0)));
    if *focus {
        *focus = false;
        resp.request_focus();
    }
    resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) && !name.trim().is_empty()
}

impl Transfer {
    fn new(xfer: u32, label: String, upload: bool, dest: String) -> Self {
        Self {
            xfer,
            label,
            upload,
            dest,
            name: String::new(),
            files_done: 0,
            files_total: 0,
            bytes_done: 0,
            bytes_total: 0,
            state: XferState::Running,
            started: Instant::now(),
            took: None,
        }
    }
}

impl eframe::App for FilesApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.first_frame {
            self.first_frame = false;
            crate::app::dark_title_bar(frame);
        }
        while let Ok(msg) = self.rx.try_recv() {
            self.handle(msg);
        }
        // An older Imperio on the other computer never answers.
        let silent = self.welcome.is_none() && self.asked_at.elapsed() > Duration::from_secs(5);
        if self.welcome.is_none() {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
        if self.transfers.iter().any(|t| matches!(t.state, XferState::Running)) {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
        if let Some((_, _, at)) = &self.notice {
            if at.elapsed() > Duration::from_secs(8) {
                self.notice = None;
            } else {
                ctx.request_repaint_after(Duration::from_secs(1));
            }
        }

        // Files dropped from Explorer go to the folder shown on the other side.
        let dropped: Vec<String> = ctx.input(|i| {
            i.raw.dropped_files.iter().map(|f| f.path().to_string_lossy().into_owned()).filter(|p| !p.is_empty()).collect()
        });
        if !dropped.is_empty() {
            if self.remote.path.is_empty() && self.welcome == Some(true) && !self.remote_desktop.is_empty() {
                self.remote.path = self.remote_desktop.clone();
                self.refresh(true);
            }
            self.transfer(false, dropped);
        }

        let mut actions = Vec::new();
        self.shortcuts(&ctx, &mut actions);

        egui::Panel::bottom("transfers")
            .frame(egui::Frame::new().fill(BG).inner_margin(Margin::symmetric(20, 10)))
            .show(ui, |ui| {
                if let Some((text, is_error, _)) = &self.notice {
                    ui.label(RichText::new(text).size(13.0).color(if *is_error { ERROR } else { GREEN }));
                }
                if !self.transfers.is_empty() {
                    self.transfers_ui(ui);
                    ui.add_space(4.0);
                }
                ui.label(
                    RichText::new(
                        "Drag files from one side to the other, or drop them here from Explorer. Right-click for rename, copy, move and delete.",
                    )
                    .size(12.0)
                    .color(MUTED),
                );
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG).inner_margin(Margin::symmetric(20, 14)))
            .show(ui, |ui| {
                let can_paste = self.clip.is_some() || clip::has_files();
                let remote_ready = self.welcome == Some(true);
                let local_env = Env {
                    other_title: if self.welcome.is_some() { self.remote.title.clone() } else { "the other computer".into() },
                    remote_ready,
                    can_paste,
                    active: !self.active_remote,
                };
                let remote_env =
                    Env { other_title: "this computer".into(), remote_ready, can_paste, active: self.active_remote };
                let off = self.welcome == Some(false);
                ui.spacing_mut().item_spacing.x = 28.0;
                ui.columns(2, |cols| {
                    cols[0].spacing_mut().item_spacing.x = 8.0;
                    cols[1].spacing_mut().item_spacing.x = 8.0;
                    pane_ui(&mut self.local, &mut cols[0], &local_env, &mut actions);
                    if remote_ready {
                        pane_ui(&mut self.remote, &mut cols[1], &remote_env, &mut actions);
                    } else {
                        let ui = &mut cols[1];
                        ui.label(RichText::new(&self.remote.title).size(16.0).strong().color(TEXT));
                        ui.add_space(30.0);
                        let text = if off {
                            format!(
                                "File access is turned off on {}.\n\nIt can be turned on there: Imperio \u{203a} Sharing settings \u{203a} Allow file access.",
                                self.remote.title
                            )
                        } else if silent {
                            "The other computer has not answered about files.\n\nIt needs Imperio 0.3.16 or newer. Update Imperio there and connect again.".to_string()
                        } else {
                            "Asking the other computer\u{2026}".to_string()
                        };
                        ui.label(RichText::new(text).size(14.0).color(MUTED));
                        if off || silent {
                            ui.add_space(10.0);
                            if ui.horizontal(|ui| ghost_button(ui, "Try again").clicked()).inner {
                                self.asked_at = Instant::now();
                                if silent {
                                    self.welcome = None;
                                }
                                self.send(&UiCmd::Hello);
                            }
                        }
                    }
                });
            });

        // What is being dragged follows the pointer.
        if let Some(payload) = egui::DragAndDrop::payload::<DragPayload>(&ctx) {
            if let Some(pos) = ctx.input(|i| i.pointer.latest_pos()) {
                let text = match payload.paths.as_slice() {
                    [one] => file_name(one),
                    many => format!("{} items", many.len()),
                };
                let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::unique("files-drag")));
                let galley = painter.layout_no_wrap(text, FontId::proportional(13.0), TEXT);
                let rect = egui::Rect::from_min_size(pos + Vec2::new(14.0, 10.0), galley.size() + Vec2::new(16.0, 10.0));
                painter.rect(rect, CornerRadius::same(6), CARD_HOVER, Stroke::new(1.0, accent()), StrokeKind::Inside);
                painter.galley(rect.min + Vec2::new(8.0, 5.0), galley, TEXT);
            }
        }

        self.dialogs(&ctx);
        for a in actions {
            self.apply(a);
        }
    }
}

fn accent_button(ui: &mut egui::Ui, enabled: bool, text: &str) -> egui::Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(text).size(13.0).strong().color(Color32::WHITE))
            .fill(if enabled { accent() } else { BUTTON })
            .corner_radius(CornerRadius::same(8))
            .min_size(Vec2::new(0.0, 30.0)),
    )
}

/// One side: title, path bar, buttons, and the list.
fn pane_ui(p: &mut Pane, ui: &mut egui::Ui, env: &Env, actions: &mut Vec<Action>) {
    let pane_rect = ui.max_rect();
    let remote = p.remote;
    let in_folder = !p.path.is_empty();

    ui.horizontal(|ui| {
        ui.label(RichText::new(&p.title).size(16.0).strong().color(if env.active { TEXT } else { MUTED }));
        if p.loading {
            ui.add(egui::Spinner::new().size(14.0));
        }
    });
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        if ui.add_enabled(p.parent.is_some(), egui::Button::new(RichText::new("Up").size(13.0)).min_size(Vec2::new(0.0, 30.0))).clicked() {
            if let Some(parent) = p.parent.clone() {
                actions.push(Action::Go(remote, parent));
            }
        }
        let width = (ui.available_width() - 204.0).max(80.0);
        let field = ui.add(
            egui::TextEdit::singleline(&mut p.path_edit)
                .hint_text(RichText::new("Folders and drives").color(MUTED))
                .desired_width(width)
                .margin(Margin::symmetric(8, 6))
                .align(Align2::LEFT_CENTER)
                .min_size(Vec2::new(0.0, 30.0)),
        );
        if field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
            actions.push(Action::Go(remote, p.path_edit.trim().to_string()));
        }
        if ghost_button(ui, "Refresh").clicked() {
            actions.push(Action::Refresh(remote));
        }
        if ui.add_enabled(in_folder, egui::Button::new(RichText::new("New folder").size(13.0)).min_size(Vec2::new(0.0, 30.0))).clicked() {
            actions.push(Action::AskNewFolder(remote));
        }
    });
    ui.horizontal(|ui| {
        let n = p.selected.len();
        let label = if remote { "Bring to this computer".to_string() } else { format!("Send to {}", env.other_title) };
        if accent_button(ui, n > 0 && env.remote_ready, &label).clicked() {
            actions.push(Action::Transfer { from_remote: remote, paths: p.selected_paths() });
        }
        let count = if n > 0 { format!("{n} selected") } else { count(p.entries.len(), "item") };
        ui.label(RichText::new(count).size(12.5).color(MUTED));
    });
    if let Some(m) = &p.message {
        ui.label(RichText::new(m).size(13.0).color(ERROR));
    }

    // Column titles.
    ui.add_space(2.0);
    let (head, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 20.0), Sense::hover());
    let hp = ui.painter();
    hp.text(head.left_center() + Vec2::new(34.0, 0.0), Align2::LEFT_CENTER, "Name", FontId::proportional(12.0), MUTED);
    hp.text(head.right_center() - Vec2::new(DATE_COL + 10.0, 0.0), Align2::RIGHT_CENTER, "Size", FontId::proportional(12.0), MUTED);
    hp.text(head.right_center() - Vec2::new(DATE_COL, 0.0), Align2::LEFT_CENTER, "Changed", FontId::proportional(12.0), MUTED);
    hp.line_segment([head.left_bottom(), head.right_bottom()], Stroke::new(1.0, STROKE));

    let (ctrl, shift) = ui.input(|i| (i.modifiers.command, i.modifiers.shift));
    let total = p.entries.len();
    let list_h = ui.available_height();
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        egui::ScrollArea::vertical().id_salt(("file-list", remote)).auto_shrink([false, false]).show_rows(ui, ROW_H, total, |ui, range| {
            let range_end = range.end;
            for i in range {
                let e = p.entries[i].clone();
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::click_and_drag());
                let selected = p.selected.contains(&i);

                if resp.clicked() || resp.double_clicked() {
                    actions.push(Action::Activate(remote));
                    if shift && p.anchor.is_some() {
                        let a = p.anchor.unwrap_or(i);
                        p.selected = (a.min(i)..=a.max(i)).collect();
                    } else if ctrl {
                        if !p.selected.remove(&i) {
                            p.selected.insert(i);
                        }
                        p.anchor = Some(i);
                    } else {
                        p.select_only(i);
                    }
                }
                if resp.double_clicked() && e.dir {
                    actions.push(Action::Go(remote, e.path.clone()));
                }
                if (resp.secondary_clicked() || resp.drag_started()) && !p.selected.contains(&i) {
                    p.select_only(i);
                    actions.push(Action::Activate(remote));
                }
                if resp.drag_started() {
                    resp.dnd_set_drag_payload(DragPayload { remote, paths: p.selected_paths() });
                }

                // Paint the row.
                let painter = ui.painter_at(rect);
                if selected {
                    painter.rect_filled(rect, CornerRadius::same(5), accent().linear_multiply(0.30));
                } else if resp.hovered() {
                    painter.rect_filled(rect, CornerRadius::same(5), CARD);
                }
                let icon = egui::Rect::from_center_size(rect.left_center() + Vec2::new(17.0, 0.0), Vec2::new(16.0, 13.0));
                if e.dir {
                    painter.rect_filled(icon, CornerRadius::same(2), accent());
                    let tab = egui::Rect::from_min_size(icon.left_top() - Vec2::new(0.0, 3.0), Vec2::new(7.0, 4.0));
                    painter.rect_filled(tab, CornerRadius::same(1), accent());
                } else {
                    let sheet = egui::Rect::from_center_size(icon.center(), Vec2::new(11.0, 14.0));
                    painter.rect_stroke(sheet, CornerRadius::same(2), Stroke::new(1.3, MUTED), StrokeKind::Inside);
                }
                let right = rect.right() - DATE_COL - SIZE_COL - 10.0;
                let name_clip = egui::Rect::from_min_max(rect.left_top() + Vec2::new(34.0, 0.0), egui::pos2(right.max(rect.left() + 60.0), rect.bottom()));
                ui.painter_at(name_clip).text(name_clip.left_center(), Align2::LEFT_CENTER, &e.name, FontId::proportional(13.5), TEXT);
                if !e.dir {
                    painter.text(
                        rect.right_center() - Vec2::new(DATE_COL + 10.0, 0.0),
                        Align2::RIGHT_CENTER,
                        size_text(e.size),
                        FontId::proportional(12.5),
                        MUTED,
                    );
                }
                if e.modified > 0 {
                    painter.text(
                        rect.right_center() - Vec2::new(DATE_COL, 0.0),
                        Align2::LEFT_CENTER,
                        date_text(e.modified),
                        FontId::proportional(12.5),
                        MUTED,
                    );
                }

                resp.context_menu(|ui| {
                    let n = p.selected.len();
                    if e.dir && n == 1 && ui.button("Open").clicked() {
                        actions.push(Action::Go(remote, e.path.clone()));
                        ui.close();
                    }
                    let send = if remote { "Bring to this computer".to_string() } else { format!("Send to {}", env.other_title) };
                    if ui.add_enabled(env.remote_ready, egui::Button::new(send)).clicked() {
                        actions.push(Action::Transfer { from_remote: remote, paths: p.selected_paths() });
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Copy").clicked() {
                        actions.push(Action::Copy(remote, false));
                        ui.close();
                    }
                    if ui.add_enabled(in_folder, egui::Button::new("Cut (to move)")).clicked() {
                        actions.push(Action::Copy(remote, true));
                        ui.close();
                    }
                    if ui.add_enabled(env.can_paste, egui::Button::new("Paste here")).clicked() {
                        actions.push(Action::Paste(remote));
                        ui.close();
                    }
                    ui.separator();
                    if ui.add_enabled(in_folder && n == 1, egui::Button::new("Rename")).clicked() {
                        actions.push(Action::AskRename(remote));
                        ui.close();
                    }
                    if ui.add_enabled(in_folder, egui::Button::new("Delete")).clicked() {
                        actions.push(Action::AskDelete(remote));
                        ui.close();
                    }
                });
            }
            // The empty space under the rows: click to clear the selection,
            // right-click for what applies to the folder itself.
            if range_end == total {
                let rest = Vec2::new(ui.available_width(), (list_h - total as f32 * ROW_H).max(48.0));
                let (_, bg) = ui.allocate_exact_size(rest, Sense::click());
                if bg.clicked() {
                    p.selected.clear();
                    p.anchor = None;
                    actions.push(Action::Activate(remote));
                }
                bg.context_menu(|ui| {
                    if ui.add_enabled(env.can_paste, egui::Button::new("Paste here")).clicked() {
                        actions.push(Action::Paste(remote));
                        ui.close();
                    }
                    if ui.add_enabled(in_folder, egui::Button::new("New folder")).clicked() {
                        actions.push(Action::AskNewFolder(remote));
                        ui.close();
                    }
                    if ui.button("Refresh").clicked() {
                        actions.push(Action::Refresh(remote));
                        ui.close();
                    }
                });
            }
        });
    });

    // Rows dragged from the other side and let go over this one.
    if let Some(payload) = egui::DragAndDrop::payload::<DragPayload>(ui.ctx()) {
        let over = ui.input(|i| i.pointer.latest_pos()).is_some_and(|pos| pane_rect.contains(pos));
        if payload.remote != remote && over {
            ui.painter().rect_stroke(pane_rect.expand(4.0), CornerRadius::same(8), Stroke::new(1.5, accent()), StrokeKind::Inside);
            if ui.input(|i| i.pointer.any_released()) {
                egui::DragAndDrop::clear_payload(ui.ctx());
                actions.push(Action::Transfer { from_remote: payload.remote, paths: payload.paths.clone() });
            }
        }
    }
}

/// `1 file`, `3 files`.
fn count(n: usize, what: &str) -> String {
    if n == 1 {
        format!("1 {what}")
    } else {
        format!("{n} {what}s")
    }
}

fn file_name(path: &str) -> String {
    let trimmed = path.trim_end_matches(['\\', '/']);
    trimmed.rsplit(['\\', '/']).next().filter(|s| !s.is_empty()).unwrap_or(path).to_string()
}

fn size_text(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    match unit {
        0 => format!("{bytes} bytes"),
        _ if value >= 100.0 => format!("{value:.0} {}", UNITS[unit]),
        _ => format!("{value:.1} {}", UNITS[unit]),
    }
}

/// Seconds since 1970 as local date and time: `2026-11-21 18:04`.
fn date_text(secs: u64) -> String {
    use windows::Win32::Foundation::SYSTEMTIME;
    use windows::Win32::System::Time::SystemTimeToTzSpecificLocalTime;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Days since 1970 to a calendar date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + (month <= 2) as i64;
    let utc = SYSTEMTIME {
        wYear: year as u16,
        wMonth: month as u16,
        wDay: day as u16,
        wHour: (rem / 3600) as u16,
        wMinute: (rem % 3600 / 60) as u16,
        wSecond: (rem % 60) as u16,
        ..Default::default()
    };
    let mut local = SYSTEMTIME::default();
    // SAFETY: both structures are valid for the call.
    let t = if unsafe { SystemTimeToTzSpecificLocalTime(None, &utc, &mut local) }.is_ok() { local } else { utc };
    format!("{:04}-{:02}-{:02} {:02}:{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_helpers() {
        assert_eq!(file_name("C:\\a\\b.txt"), "b.txt");
        assert_eq!(file_name("C:\\a\\folder\\"), "folder");
        assert_eq!(file_name("C:\\"), "C:");
        assert_eq!(size_text(0), "0 bytes");
        assert_eq!(size_text(1023), "1023 bytes");
        assert_eq!(size_text(1536), "1.5 KB");
        assert_eq!(size_text(250 * 1024 * 1024), "250 MB");
        // 2026-11-21 12:34:56 UTC; the local date is that day or a neighbour.
        let text = date_text(1_795_264_496);
        assert!(text.starts_with("2026-11-2"), "{text}");
    }
}
