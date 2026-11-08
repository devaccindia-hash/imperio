//! Host status window and tray icon.
//!
//! A small native window shows what the person at the host needs to read out
//! or check: the Device ID (with its check digit), the security mode or
//! current code, the address(es) a viewer should type, and the live status.
//! Text sits in read-only edit boxes so it can be selected, plus Copy buttons.
//! Closing the window hides it to the notification area; the tray menu brings
//! it back or quits. All Win32 work happens on one dedicated thread; the host
//! loop publishes state through [`HostUi::update`].

use std::cell::RefCell;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use imperio_ui as ui;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    ES_AUTOHSCROLL, ES_MULTILINE, ES_READONLY, WM_CLOSE, WM_COMMAND, WM_CONTEXTMENU, WM_DESTROY,
    WM_LBUTTONDBLCLK, WM_RBUTTONUP, WS_BORDER, WS_TABSTOP,
};

/// What the window displays. The host fills this in as it learns things.
#[derive(Default, Clone)]
pub struct UiState {
    pub device_id: String,
    /// "Code 063 460", "Open: your Tailscale devices only", "Unattended password".
    pub security: String,
    /// The raw code, if there is one, for the Copy button.
    pub code: Option<String>,
    pub addresses: Vec<String>,
    pub status: String,
}

const WM_REFRESH: u32 = ui::WM_APP_BASE;
const WM_TRAY: u32 = ui::WM_APP_BASE + 1;

const ID_COPY_ID: u16 = 101;
const ID_COPY_CODE: u16 = 102;
const ID_HIDE: u16 = 103;
const ID_TRAY_SHOW: u16 = 201;
const ID_TRAY_QUIT: u16 = 202;

struct Controls {
    id: HWND,
    security: HWND,
    copy_code: HWND,
    addresses: HWND,
    status: HWND,
}

static STATE: OnceLock<Arc<Mutex<UiState>>> = OnceLock::new();
thread_local! {
    static CONTROLS: RefCell<Option<Controls>> = const { RefCell::new(None) };
    static TRAY: RefCell<Option<ui::TrayIcon>> = const { RefCell::new(None) };
}

/// Handle to the UI thread. Cheap to clone-by-field; `update` is thread-safe.
pub struct HostUi {
    state: Arc<Mutex<UiState>>,
    hwnd: Arc<AtomicIsize>,
}

impl HostUi {
    /// Spawn the UI thread and show the window.
    pub fn start() -> Self {
        let state = STATE.get_or_init(|| Arc::new(Mutex::new(UiState::default()))).clone();
        let hwnd = Arc::new(AtomicIsize::new(0));
        let hwnd_thread = hwnd.clone();
        let spawned = std::thread::Builder::new()
            .name("host-ui".into())
            .spawn(move || {
                ui::init_dpi();
                let class = ui::register_class("ImperioHostWindow", Some(wndproc));
                let Ok(h) = ui::create_window(&class, "Imperio Host", 420, 330) else {
                    tracing::warn!("host window could not be created; console only");
                    return;
                };
                build_controls(h);
                hwnd_thread.store(ui::hwnd_to_isize(h), Ordering::Release);
                TRAY.with(|t| *t.borrow_mut() = Some(ui::TrayIcon::add(h, "Imperio Host", WM_TRAY)));
                refresh(h);
                ui::show(h, true);
                ui::message_loop(None);
                TRAY.with(|t| t.borrow_mut().take());
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "host UI thread failed to start");
        }
        Self { state, hwnd }
    }

    /// Change the displayed state and repaint. Safe from any thread.
    pub fn update(&self, f: impl FnOnce(&mut UiState)) {
        if let Ok(mut s) = self.state.lock() {
            f(&mut s);
        }
        let h = self.hwnd.load(Ordering::Acquire);
        if h != 0 {
            ui::post(ui::isize_to_hwnd(h), WM_REFRESH);
        }
    }
}

fn build_controls(h: HWND) {
    let font = ui::ui_font(10, false);
    let bold = ui::ui_font(10, true);
    let big = ui::ui_font(18, true);
    let edit = WS_BORDER.0 | ES_READONLY as u32 | ES_AUTOHSCROLL as u32;
    let button = WS_TABSTOP.0;

    ui::create_control(h, "STATIC", "Your Device ID", 0, 20, 14, 260, 18, 0, bold);
    let id = ui::create_control(h, "EDIT", "", edit, 20, 34, 264, 36, 301, big);
    ui::create_control(h, "BUTTON", "Copy ID", button, 300, 37, 100, 30, ID_COPY_ID, font);

    ui::create_control(h, "STATIC", "Security", 0, 20, 84, 260, 18, 0, bold);
    let security = ui::create_control(h, "EDIT", "", edit, 20, 104, 264, 26, 302, font);
    let copy_code =
        ui::create_control(h, "BUTTON", "Copy code", button, 300, 102, 100, 30, ID_COPY_CODE, font);

    ui::create_control(h, "STATIC", "Address to type on the viewer", 0, 20, 144, 300, 18, 0, bold);
    let addresses = ui::create_control(
        h,
        "EDIT",
        "",
        WS_BORDER.0 | ES_READONLY as u32 | ES_MULTILINE as u32,
        20,
        164,
        380,
        58,
        303,
        font,
    );

    ui::create_control(h, "STATIC", "Status", 0, 20, 232, 200, 18, 0, bold);
    let status = ui::create_control(h, "EDIT", "", edit, 20, 252, 380, 26, 304, font);

    ui::create_control(h, "BUTTON", "Hide to tray", button, 300, 290, 100, 28, ID_HIDE, font);

    CONTROLS.with(|c| {
        *c.borrow_mut() = Some(Controls { id, security, copy_code, addresses, status })
    });
}

fn refresh(_h: HWND) {
    let Some(state) = STATE.get().and_then(|s| s.lock().ok().map(|g| g.clone())) else {
        return;
    };
    CONTROLS.with(|c| {
        if let Some(c) = c.borrow().as_ref() {
            ui::set_text(c.id, &state.device_id);
            ui::set_text(c.security, &state.security);
            ui::set_text(c.addresses, &state.addresses.join("\r\n"));
            ui::set_text(c.status, &state.status);
            // SAFETY: live control on this thread.
            let _ = unsafe { EnableWindow(c.copy_code, state.code.is_some()) };
        }
    });
}

fn copy_to_clipboard(text: &str) {
    match arboard::Clipboard::new().and_then(|mut c| c.set_text(text.to_string())) {
        Ok(()) => {}
        Err(e) => tracing::warn!(error = %e, "clipboard copy failed"),
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_REFRESH => {
            refresh(hwnd);
            LRESULT(0)
        }
        WM_COMMAND => {
            match ui::loword(w) {
                ID_COPY_ID => {
                    if let Some(s) = STATE.get().and_then(|s| s.lock().ok()) {
                        let digits: String = s.device_id.chars().filter(|c| !c.is_whitespace()).collect();
                        copy_to_clipboard(&digits);
                    }
                }
                ID_COPY_CODE => {
                    if let Some(code) = STATE.get().and_then(|s| s.lock().ok()).and_then(|s| s.code.clone()) {
                        copy_to_clipboard(&code);
                    }
                }
                ID_HIDE => ui::show(hwnd, false),
                _ => {}
            }
            LRESULT(0)
        }
        WM_TRAY => {
            match ui::lparam_loword(l) {
                WM_LBUTTONDBLCLK => ui::show(hwnd, true),
                WM_RBUTTONUP | WM_CONTEXTMENU => {
                    let choice = ui::popup_menu(
                        hwnd,
                        &[(ID_TRAY_SHOW, "Show Imperio Host"), (0, ""), (ID_TRAY_QUIT, "Quit (stop sharing)")],
                    );
                    match choice {
                        ID_TRAY_SHOW => ui::show(hwnd, true),
                        ID_TRAY_QUIT => {
                            tracing::info!("quit from tray menu");
                            TRAY.with(|t| t.borrow_mut().take());
                            std::process::exit(0);
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        // Closing hides to the tray; sharing keeps running until Quit.
        WM_CLOSE => {
            ui::show(hwnd, false);
            LRESULT(0)
        }
        WM_DESTROY => {
            ui::quit();
            LRESULT(0)
        }
        _ => ui::default_proc(hwnd, msg, w, l),
    }
}
