//! Notification-area icon for the home window.
//!
//! Closing the window hides it instead of quitting, so sharing keeps running
//! in the background; the tray icon brings it back (click) or quits (menu).
//! The eframe/winit window owns its window procedure, so we subclass it to
//! receive the tray callback and to intercept `WM_CLOSE`.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};

use imperio_ui as ui;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    PostMessageW, SendMessageW, HICON, ICON_SMALL, WM_CLOSE, WM_CONTEXTMENU, WM_GETICON,
    WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_RBUTTONUP,
};

const WM_TRAY: u32 = ui::WM_APP_BASE + 7;
/// Posted (from any thread) to show a balloon; text waits in `PENDING`.
const WM_NOTIFY_USER: u32 = ui::WM_APP_BASE + 8;
/// Tray callback low word when the user clicks a balloon.
const NIN_BALLOONUSERCLICK: u32 = 0x0405;
const ID_OPEN: u16 = 1;
const ID_QUIT: u16 = 2;
const SUBCLASS_ID: usize = 0x1A9F;

static PENDING: std::sync::Mutex<Option<(String, String)>> = std::sync::Mutex::new(None);

/// Show a tray balloon (title, text) — e.g. "a new version is available" —
/// visible even when the window is hidden. Safe from any thread; clicking
/// the balloon opens the window.
pub fn notify(title: &str, text: &str) {
    if let Ok(mut p) = PENDING.lock() {
        *p = Some((title.to_string(), text.to_string()));
    }
    let h = HWND_VALUE.load(Ordering::Relaxed);
    if h != 0 {
        // SAFETY: PostMessageW is thread-safe; a dead hwnd just fails.
        unsafe {
            let _ = PostMessageW(Some(HWND(h as *mut _)), WM_NOTIFY_USER, WPARAM(0), LPARAM(0));
        }
    }
}

/// Set once the user chose Quit; lets the next `WM_CLOSE` through.
static QUITTING: AtomicBool = AtomicBool::new(false);
/// The home window, once installed (0 = none).
static HWND_VALUE: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
/// When the tray was attached (first frame); used to ignore very early closes.
static INSTALLED_AT: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Quit for real (e.g. to apply an update): remove the tray icon and close
/// the window without the hide-to-tray interception. Safe from any thread.
pub fn request_quit() {
    QUITTING.store(true, Ordering::Relaxed);
    let h = HWND_VALUE.load(Ordering::Relaxed);
    if h != 0 {
        // SAFETY: PostMessageW is thread-safe; a dead hwnd just fails.
        unsafe {
            let _ = PostMessageW(Some(HWND(h as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
}

thread_local! {
    static TRAY: RefCell<Option<ui::TrayIcon>> = const { RefCell::new(None) };
}

/// Attach the tray icon and close-to-tray behaviour to the eframe window.
/// Call once, from the first frame (the native window exists by then).
pub fn install(frame: &eframe::Frame) {
    let Ok(h) = frame.window_handle() else { return };
    let RawWindowHandle::Win32(w) = h.as_raw() else { return };
    let hwnd = HWND(w.hwnd.get() as *mut _);
    HWND_VALUE.store(hwnd.0 as isize, Ordering::Relaxed);
    let _ = INSTALLED_AT.set(std::time::Instant::now());
    // SAFETY: hwnd is the live window on this thread; the subclass stays
    // installed for the window's lifetime.
    unsafe {
        let icon = SendMessageW(hwnd, WM_GETICON, Some(WPARAM(ICON_SMALL as usize)), None);
        let hicon = (icon.0 != 0).then_some(HICON(icon.0 as *mut _));
        if SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, 0).as_bool() {
            TRAY.with(|t| {
                *t.borrow_mut() = Some(ui::TrayIcon::add_with_icon(
                    hwnd,
                    "Imperio \u{2014} sharing is on (click to open)",
                    WM_TRAY,
                    hicon,
                ))
            });
        }
    }
    // When the updater relaunches us the window can inherit a hidden start
    // state; make sure it is on screen.
    ui::show(hwnd, true);
}

unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    msg: u32,
    w: WPARAM,
    l: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    match msg {
        WM_NOTIFY_USER => {
            if let Some((title, text)) = PENDING.lock().ok().and_then(|mut p| p.take()) {
                TRAY.with(|t| {
                    if let Some(t) = t.borrow().as_ref() {
                        t.notify(&title, &text);
                    }
                });
            }
            LRESULT(0)
        }
        WM_TRAY => {
            match ui::lparam_loword(l) {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK | NIN_BALLOONUSERCLICK => ui::show(hwnd, true),
                WM_RBUTTONUP | WM_CONTEXTMENU => {
                    let choice = ui::popup_menu(
                        hwnd,
                        &[(ID_OPEN, "Open Imperio"), (0, ""), (ID_QUIT, "Quit and stop sharing")],
                    );
                    match choice {
                        ID_OPEN => ui::show(hwnd, true),
                        ID_QUIT => {
                            QUITTING.store(true, Ordering::Relaxed);
                            TRAY.with(|t| t.borrow_mut().take());
                            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        // Close button / Alt+F4: hide to the tray and keep sharing. A close
        // arriving within the first moments after launch is not something a
        // person did (seen right after an update relaunch); ignore it.
        WM_CLOSE if !QUITTING.load(Ordering::Relaxed) => {
            let age = INSTALLED_AT.get().map(|t| t.elapsed()).unwrap_or_default();
            if age < std::time::Duration::from_millis(2500) {
                tracing::warn!(ms = age.as_millis() as u64, "ignored close request right after launch");
            } else {
                ui::show(hwnd, false);
            }
            LRESULT(0)
        }
        _ => DefSubclassProc(hwnd, msg, w, l),
    }
}
