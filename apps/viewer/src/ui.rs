//! Viewer windows: the connect dialog shown before a session, and the menu
//! bar attached to the session window.
//!
//! The session window belongs to winit, which owns its window procedure. To
//! react to menu clicks we subclass that window (`SetWindowSubclass`) and
//! forward `WM_COMMAND` as commands into a sink the main thread installed,
//! which turns them into winit user events.

use std::cell::RefCell;
use std::sync::Mutex;

use imperio_ui as ui;
use windows::core::HSTRING;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CheckMenuRadioItem, CreateMenu, CreatePopupMenu, DeleteMenu, DrawMenuBar, ModifyMenuW,
    SetMenu, BS_AUTOCHECKBOX, BS_DEFPUSHBUTTON, DM_GETDEFID, ES_AUTOHSCROLL, HMENU, IDCANCEL,
    IDOK, MF_BYCOMMAND, MF_BYPOSITION, MF_POPUP, MF_SEPARATOR, MF_STRING, WM_CLOSE, WM_COMMAND,
    WM_DESTROY, WS_BORDER, WS_TABSTOP,
};

// ---------------------------------------------------------------- connect

/// What the user asked for in the connect dialog.
#[derive(Clone, Default)]
pub struct ConnectRequest {
    /// Host address (`ip[:port]`) or a 10-digit Device ID.
    pub target: String,
    pub code: Option<String>,
    pub trust_new: bool,
}

const ID_TARGET: u16 = 101;
const ID_CODE: u16 = 102;
const ID_TRUST: u16 = 103;
const ID_ERROR: u16 = 104;
/// `DM_GETDEFID` reply flag: "this window has a default button".
const DC_HASDEFID: u32 = 0x534B;

struct Dialog {
    target: HWND,
    code: HWND,
    trust: HWND,
    error: HWND,
    result: Option<ConnectRequest>,
}

thread_local! {
    static DIALOG: RefCell<Option<Dialog>> = const { RefCell::new(None) };
}

/// Show the connect dialog (modal, on this thread). `error` is shown in red
/// text when a previous attempt failed. Returns `None` if the user cancelled.
pub fn connect_dialog(prefill: &ConnectRequest, error: Option<&str>) -> Option<ConnectRequest> {
    ui::init_dpi();
    let class = ui::register_class("ImperioConnectDialog", Some(dialog_proc));
    let h = ui::create_window(&class, "Imperio \u{2014} connect", 440, 276).ok()?;

    let font = ui::ui_font(10, false);
    let bold = ui::ui_font(12, true);
    let edit = WS_BORDER.0 | WS_TABSTOP.0 | ES_AUTOHSCROLL as u32;

    ui::create_control(h, "STATIC", "Connect to a computer", 0, 20, 14, 400, 22, 0, bold);
    ui::create_control(
        h,
        "STATIC",
        "Host address (for example 100.101.2.3 or 192.168.1.20), or its 10-digit Device ID",
        0,
        20,
        44,
        400,
        18,
        0,
        font,
    );
    let target = ui::create_control(h, "EDIT", &prefill.target, edit, 20, 64, 400, 26, ID_TARGET, font);
    ui::create_control(
        h,
        "STATIC",
        "Code shown on the host. Leave empty for your own Tailscale devices (open mode).",
        0,
        20,
        100,
        400,
        18,
        0,
        font,
    );
    let code = ui::create_control(
        h,
        "EDIT",
        prefill.code.as_deref().unwrap_or(""),
        edit,
        20,
        120,
        180,
        26,
        ID_CODE,
        font,
    );
    let trust = ui::create_control(
        h,
        "BUTTON",
        "Accept a changed host identity (only if you expected the change)",
        BS_AUTOCHECKBOX as u32 | WS_TABSTOP.0,
        20,
        156,
        400,
        20,
        ID_TRUST,
        font,
    );
    ui::set_checked(trust, prefill.trust_new);
    let err = ui::create_control(h, "STATIC", error.unwrap_or(""), 0, 20, 182, 400, 40, ID_ERROR, font);
    ui::create_control(
        h,
        "BUTTON",
        "Connect",
        BS_DEFPUSHBUTTON as u32 | WS_TABSTOP.0,
        216,
        232,
        104,
        30,
        IDOK.0 as u16,
        font,
    );
    ui::create_control(h, "BUTTON", "Cancel", WS_TABSTOP.0, 330, 232, 90, 30, IDCANCEL.0 as u16, font);

    DIALOG.with(|d| {
        *d.borrow_mut() = Some(Dialog { target, code, trust, error: err, result: None })
    });
    ui::show(h, true);
    ui::focus(if prefill.target.is_empty() { target } else { code });
    ui::message_loop(Some(h));
    ui::destroy(h);
    DIALOG.with(|d| d.borrow_mut().take()).and_then(|d| d.result)
}

unsafe extern "system" fn dialog_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        DM_GETDEFID => LRESULT(((DC_HASDEFID << 16) | IDOK.0 as u32) as isize),
        WM_COMMAND => {
            let id = ui::loword(w) as i32;
            if id == IDOK.0 {
                DIALOG.with(|d| {
                    if let Some(d) = d.borrow_mut().as_mut() {
                        let target: String = ui::window_text(d.target).trim().to_string();
                        if target.is_empty() {
                            ui::set_text(d.error, "Type the host\u{2019}s address or Device ID first.");
                            ui::focus(d.target);
                            return;
                        }
                        let code: String = ui::window_text(d.code).chars().filter(|c| !c.is_whitespace()).collect();
                        d.result = Some(ConnectRequest {
                            target,
                            code: (!code.is_empty()).then_some(code),
                            trust_new: ui::is_checked(d.trust),
                        });
                        ui::quit();
                    }
                });
            } else if id == IDCANCEL.0 {
                ui::quit();
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            ui::quit();
            LRESULT(0)
        }
        WM_DESTROY => LRESULT(0),
        _ => ui::default_proc(hwnd, msg, w, l),
    }
}

// ------------------------------------------------------------- session menu

/// Commands the session menu can issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiCmd {
    Disconnect,
    ToggleFullscreen,
    SelectDisplay(u8),
    ToggleRecording,
    About,
}

const ID_DISCONNECT: u16 = 1001;
const ID_FULLSCREEN: u16 = 1002;
const ID_RECORD: u16 = 1003;
const ID_ABOUT: u16 = 1200;
const ID_DISPLAY_BASE: u16 = 1100;
const SUBCLASS_ID: usize = 0x1A9E;

type Sink = Box<dyn FnMut(UiCmd) + Send>;
static SINK: Mutex<Option<Sink>> = Mutex::new(None);

/// Install the function that receives menu commands (called once, before the
/// session window exists). The viewer passes a closure that posts a winit
/// user event.
pub fn set_command_sink(f: impl FnMut(UiCmd) + Send + 'static) {
    if let Ok(mut s) = SINK.lock() {
        *s = Some(Box::new(f));
    }
}

fn emit(cmd: UiCmd) {
    if let Ok(mut s) = SINK.lock() {
        if let Some(f) = s.as_mut() {
            f(cmd);
        }
    }
}

/// Menu bar on the session window.
pub struct SessionMenu {
    hwnd: HWND,
    bar: HMENU,
    session: HMENU,
    displays: HMENU,
}

impl SessionMenu {
    /// Build the menu bar on `hwnd` (the winit window) and subclass it so menu
    /// clicks reach [`set_command_sink`].
    pub fn attach(hwnd: HWND) -> Option<Self> {
        // SAFETY: hwnd is the live winit window on this thread; menus created
        // here are owned by the window once SetMenu succeeds.
        let (bar, displays, session) = unsafe {
            let bar = CreateMenu().ok()?;
            let session = CreatePopupMenu().ok()?;
            AppendMenuW(session, MF_STRING, ID_FULLSCREEN as usize, &HSTRING::from("Full screen\tF11")).ok()?;
            AppendMenuW(session, MF_STRING, ID_RECORD as usize, &HSTRING::from("Start recording")).ok()?;
            AppendMenuW(session, MF_SEPARATOR, 0, None).ok()?;
            AppendMenuW(session, MF_STRING, ID_DISCONNECT as usize, &HSTRING::from("Disconnect")).ok()?;
            let displays = CreatePopupMenu().ok()?;
            AppendMenuW(displays, MF_STRING, ID_DISPLAY_BASE as usize, &HSTRING::from("Display 1\tCtrl+Shift+1")).ok()?;
            let help = CreatePopupMenu().ok()?;
            AppendMenuW(help, MF_STRING, ID_ABOUT as usize, &HSTRING::from("Shortcuts && about")).ok()?;
            AppendMenuW(bar, MF_POPUP, session.0 as usize, &HSTRING::from("Session")).ok()?;
            AppendMenuW(bar, MF_POPUP, displays.0 as usize, &HSTRING::from("Display")).ok()?;
            AppendMenuW(bar, MF_POPUP, help.0 as usize, &HSTRING::from("Help")).ok()?;
            SetMenu(hwnd, Some(bar)).ok()?;
            let _ = DrawMenuBar(hwnd);
            SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, 0).ok().ok()?;
            (bar, displays, session)
        };
        Some(Self { hwnd, bar, session, displays })
    }

    /// Rebuild the Display submenu for `count` displays with `current` checked.
    pub fn set_displays(&self, count: u8, current: u8) {
        // SAFETY: menu handles are owned by our window and used on its thread.
        unsafe {
            while DeleteMenu(self.displays, 0, MF_BYPOSITION).is_ok() {}
            for i in 0..count.max(1) {
                let label = format!("Display {}\tCtrl+Shift+{}", i + 1, i + 1);
                let _ = AppendMenuW(self.displays, MF_STRING, (ID_DISPLAY_BASE + i as u16) as usize, &HSTRING::from(label));
            }
            let first = ID_DISPLAY_BASE as u32;
            let last = first + count.max(1) as u32 - 1;
            let _ = CheckMenuRadioItem(self.displays, first, last, first + current as u32, MF_BYCOMMAND.0);
            let _ = DrawMenuBar(self.hwnd);
        }
    }

    /// Flip the Session menu item between "Start recording" / "Stop recording".
    pub fn set_recording(&self, on: bool) {
        let label = if on { "Stop recording" } else { "Start recording" };
        // SAFETY: live menu owned by our window, used on its thread.
        unsafe {
            let _ = ModifyMenuW(
                self.session,
                ID_RECORD as u32,
                MF_BYCOMMAND | MF_STRING,
                ID_RECORD as usize,
                &HSTRING::from(label),
            );
            let _ = DrawMenuBar(self.hwnd);
        }
    }

    /// Hide the bar in full screen, restore it otherwise.
    pub fn set_fullscreen(&self, fullscreen: bool) {
        // SAFETY: live window and menu on this thread.
        unsafe {
            let _ = SetMenu(self.hwnd, if fullscreen { None } else { Some(self.bar) });
            let _ = DrawMenuBar(self.hwnd);
        }
    }
}

unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    msg: u32,
    w: WPARAM,
    l: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    // Menu commands arrive with the high word of WPARAM == 0.
    if msg == WM_COMMAND && (w.0 >> 16) & 0xFFFF == 0 {
        let id = ui::loword(w);
        let cmd = match id {
            ID_DISCONNECT => Some(UiCmd::Disconnect),
            ID_FULLSCREEN => Some(UiCmd::ToggleFullscreen),
            ID_RECORD => Some(UiCmd::ToggleRecording),
            ID_ABOUT => Some(UiCmd::About),
            n if (ID_DISPLAY_BASE..ID_DISPLAY_BASE + 16).contains(&n) => {
                Some(UiCmd::SelectDisplay((n - ID_DISPLAY_BASE) as u8))
            }
            _ => None,
        };
        if let Some(cmd) = cmd {
            emit(cmd);
            return LRESULT(0);
        }
    }
    DefSubclassProc(hwnd, msg, w, l)
}

/// Text for Help > Shortcuts & about.
pub fn about_text() -> String {
    format!(
        "Imperio viewer {}\n\n\
         F11 \u{2014} toggle full screen\n\
         Ctrl+Shift+1..9 \u{2014} switch the host display\n\
         Session > Start recording saves the session as an MP4 in Videos\\Imperio.\n\
         Close the window or use Session > Disconnect to end the session.\n\n\
         All traffic is end-to-end encrypted; the host identity is pinned on first use.\n\n\
         Proudly made with \u{2764} in India by Developer Inayat\n\
         https://developerinayat.com/",
        env!("CARGO_PKG_VERSION")
    )
}
