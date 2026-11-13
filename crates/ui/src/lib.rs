//! Small native Win32 UI toolkit shared by the host and viewer.
//!
//! Imperio's windows are deliberately plain Win32 (no GUI framework): zero
//! extra dependencies, a few hundred kilobytes, works in the static portable
//! build, and looks native. This crate holds the boring plumbing (wide
//! strings, window classes, controls, fonts, DPI scaling, message loops, the
//! tray icon) so `apps/host/src/ui.rs` and `apps/viewer/src/ui.rs` can read
//! like layout code.

use windows::core::{Result, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateFontW, GetStockObject, HBRUSH, HFONT, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS,
    DEFAULT_CHARSET, DEFAULT_PITCH, FF_DONTCARE, FW_NORMAL, FW_SEMIBOLD, OUT_DEFAULT_PRECIS,
    WHITE_BRUSH,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    GetDpiForSystem, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    DPI_AWARENESS_CONTEXT_SYSTEM_AWARE,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFYICONDATAW,
};
use windows::Win32::UI::Controls::{BST_CHECKED, BST_UNCHECKED};
use windows::Win32::UI::WindowsAndMessaging::*;

pub use windows::Win32::Foundation::{HWND as Hwnd, LPARAM as LParam, LRESULT as LResult, WPARAM as WParam};

/// First free application message; callers add small offsets.
pub const WM_APP_BASE: u32 = WM_APP + 0x100;

/// Null-terminated UTF-16 for a Win32 string argument.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Our module handle (for window classes and resources).
pub fn hinstance() -> HINSTANCE {
    // SAFETY: GetModuleHandleW(None) never fails for the current process.
    unsafe { GetModuleHandleW(None).map(|h| HINSTANCE(h.0)).unwrap_or_default() }
}

/// Opt the process into system DPI awareness so text is crisp. Must run before
/// any window is created; harmless if the manifest already set it.
pub fn init_dpi() {
    // SAFETY: plain API call; failure just means a manifest already decided.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_SYSTEM_AWARE) };
}

/// Opt the process into per-monitor DPI awareness, so every coordinate the
/// process sees (monitor rectangles, the virtual desktop, the cursor) is in
/// real pixels. The host needs this before it measures anything: screen
/// capture switches the process to real pixels anyway, and a rectangle read
/// before that moment would be in scaled units and send the remote mouse to
/// the wrong place on a display scaled above 100%. Harmless if already set.
pub fn init_dpi_per_monitor() {
    // SAFETY: plain API call; failure means awareness was already decided.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

/// Scale factor from the 96-dpi design grid to the system DPI.
pub fn dpi_scale() -> f32 {
    // SAFETY: plain API call.
    unsafe { GetDpiForSystem() as f32 / 96.0 }
}

/// Scale a design-grid length to device pixels.
pub fn px(v: i32) -> i32 {
    (v as f32 * dpi_scale()).round() as i32
}

/// The standard UI font (Segoe UI), optionally semibold, sized for the DPI.
pub fn ui_font(points: i32, bold: bool) -> HFONT {
    let height = -((points as f32 * dpi_scale() * 96.0 / 72.0).round() as i32);
    let weight = if bold { FW_SEMIBOLD.0 } else { FW_NORMAL.0 } as i32;
    let face = wide("Segoe UI");
    // SAFETY: valid font parameters and a null-terminated face name.
    unsafe {
        CreateFontW(
            height,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            DEFAULT_PITCH.0 as u32 | FF_DONTCARE.0 as u32,
            PCWSTR(face.as_ptr()),
        )
    }
}

/// Register a top-level window class with `proc`. Idempotent per name.
pub fn register_class(name: &str, proc: WNDPROC) -> Vec<u16> {
    let class = wide(name);
    // SAFETY: all fields are valid for the lifetime of the call; the class
    // name buffer is returned to the caller, who keeps it alive while using it.
    unsafe {
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: proc,
            hInstance: hinstance(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(GetStockObject(WHITE_BRUSH).0),
            lpszClassName: PCWSTR(class.as_ptr()),
            ..Default::default()
        };
        RegisterClassW(&wc); // a second call for the same name is a no-op
    }
    class
}

/// Create a fixed-size, non-resizable top-level window whose *client* area is
/// `width` x `height` on the design grid, placed in the upper third of the
/// screen.
pub fn create_window(class: &[u16], title: &str, width: i32, height: i32) -> Result<HWND> {
    let (w, h) = (px(width), px(height));
    // SAFETY: valid class and title strings; sizes are positive.
    unsafe {
        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let mut rc = RECT { left: 0, top: 0, right: w, bottom: h };
        let _ = AdjustWindowRectEx(&mut rc, style, false, WINDOW_EX_STYLE(0));
        let (ow, oh) = (rc.right - rc.left, rc.bottom - rc.top);
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            &HSTRING::from(title),
            style,
            (sw - ow) / 2,
            (sh - oh) / 3,
            ow,
            oh,
            None,
            None,
            Some(hinstance()),
            None,
        )
    }
}

/// A child control. `class` is a predefined class ("STATIC", "EDIT",
/// "BUTTON"); coordinates are on the 96-dpi design grid.
#[allow(clippy::too_many_arguments)]
pub fn create_control(
    parent: HWND,
    class: &str,
    text: &str,
    style: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    id: u16,
    font: HFONT,
) -> HWND {
    let cls = wide(class);
    // SAFETY: parent is a live window; strings are null-terminated.
    unsafe {
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(cls.as_ptr()),
            &HSTRING::from(text),
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(style),
            px(x),
            px(y),
            px(w),
            px(h),
            Some(parent),
            Some(HMENU(id as usize as *mut _)),
            Some(hinstance()),
            None,
        )
        .unwrap_or_default();
        SendMessageW(hwnd, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
        hwnd
    }
}

pub fn set_text(hwnd: HWND, text: &str) {
    // SAFETY: hwnd is a live window owned by this process.
    let _ = unsafe { SetWindowTextW(hwnd, &HSTRING::from(text)) };
}

pub fn window_text(hwnd: HWND) -> String {
    // SAFETY: buffer sized from GetWindowTextLengthW plus the terminator.
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }
}

pub fn is_checked(button: HWND) -> bool {
    // SAFETY: plain message send to a live control.
    unsafe { SendMessageW(button, BM_GETCHECK, None, None).0 as u32 == BST_CHECKED.0 }
}

pub fn set_checked(button: HWND, checked: bool) {
    let v = if checked { BST_CHECKED.0 } else { BST_UNCHECKED.0 } as usize;
    // SAFETY: plain message send to a live control.
    unsafe {
        SendMessageW(button, BM_SETCHECK, Some(WPARAM(v)), None);
    }
}

pub fn focus(hwnd: HWND) {
    // SAFETY: hwnd is a live control on this thread.
    let _ = unsafe { windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(Some(hwnd)) };
}

pub fn show(hwnd: HWND, visible: bool) {
    // SAFETY: hwnd is a live window.
    unsafe {
        let _ = ShowWindow(hwnd, if visible { SW_SHOW } else { SW_HIDE });
        if visible {
            let _ = SetForegroundWindow(hwnd);
        }
    }
}

/// Run a message loop until `WM_QUIT`. With `dialog` set, Tab / Enter / Esc
/// navigate that window's controls like a real dialog.
pub fn message_loop(dialog: Option<HWND>) {
    let mut msg = MSG::default();
    // SAFETY: standard message pump.
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if let Some(d) = dialog {
                if IsDialogMessageW(d, &msg).as_bool() {
                    continue;
                }
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

pub fn quit() {
    // SAFETY: plain API call.
    unsafe { PostQuitMessage(0) };
}

/// Post an application message to a window from any thread.
pub fn post(hwnd: HWND, msg: u32) {
    // SAFETY: PostMessageW is thread-safe; a dead hwnd just fails.
    let _ = unsafe { PostMessageW(Some(hwnd), msg, WPARAM(0), LPARAM(0)) };
}

/// Low 16 bits of a WPARAM (control / menu id in WM_COMMAND).
pub fn loword(w: WPARAM) -> u16 {
    (w.0 & 0xFFFF) as u16
}

pub fn lparam_loword(l: LPARAM) -> u32 {
    (l.0 as u32) & 0xFFFF
}

/// Notification-area (tray) icon bound to `hwnd`; clicks arrive as
/// `callback` messages whose LPARAM low word is the mouse message.
pub struct TrayIcon {
    hwnd: HWND,
}

impl TrayIcon {
    pub fn add(hwnd: HWND, tip: &str, callback: u32) -> Self {
        Self::add_with_icon(hwnd, tip, callback, None)
    }

    /// Like [`add`](Self::add) but with a specific icon (e.g. the window's
    /// own, fetched with `WM_GETICON`); `None` uses the stock application icon.
    pub fn add_with_icon(hwnd: HWND, tip: &str, callback: u32, icon: Option<HICON>) -> Self {
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: callback,
            // SAFETY: stock icon load.
            hIcon: icon.unwrap_or_else(|| unsafe { LoadIconW(None, IDI_APPLICATION).unwrap_or_default() }),
            ..Default::default()
        };
        for (i, c) in tip.encode_utf16().take(127).enumerate() {
            nid.szTip[i] = c;
        }
        // SAFETY: fully initialised structure.
        let _ = unsafe { Shell_NotifyIconW(NIM_ADD, &nid) };
        Self { hwnd }
    }
}

impl TrayIcon {
    /// Show a balloon notification from the tray icon (title ≤ 63 chars,
    /// text ≤ 255 chars). Clicking it sends `NIN_BALLOONUSERCLICK` (0x405) to
    /// the callback message.
    pub fn notify(&self, title: &str, text: &str) {
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: 1,
            uFlags: NIF_INFO,
            ..Default::default()
        };
        nid.Anonymous.uVersion = 0;
        nid.dwInfoFlags = NIIF_INFO;
        for (i, c) in title.encode_utf16().take(63).enumerate() {
            nid.szInfoTitle[i] = c;
        }
        for (i, c) in text.encode_utf16().take(255).enumerate() {
            nid.szInfo[i] = c;
        }
        // SAFETY: fully initialised structure for an icon we added.
        let _ = unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };
    }
}

impl Drop for TrayIcon {
    fn drop(&mut self) {
        let nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: 1,
            ..Default::default()
        };
        // SAFETY: fully initialised structure.
        let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &nid) };
    }
}

/// Show a popup menu at the cursor and return the chosen command id (0 = none).
/// An empty label inserts a separator.
pub fn popup_menu(hwnd: HWND, items: &[(u16, &str)]) -> u16 {
    // SAFETY: menu handle is created and destroyed within this function.
    unsafe {
        let Ok(menu) = CreatePopupMenu() else {
            return 0;
        };
        for (id, text) in items {
            if text.is_empty() {
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            } else {
                let _ = AppendMenuW(menu, MF_STRING, *id as usize, &HSTRING::from(*text));
            }
        }
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let chosen = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_RETURNCMD | TPM_NONOTIFY,
            pt.x,
            pt.y,
            Some(0),
            hwnd,
            None,
        );
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        chosen.0 as u16
    }
}

/// Simple modal information box.
pub fn info_box(title: &str, text: &str) {
    // SAFETY: valid strings.
    unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(text),
            &HSTRING::from(title),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

/// `DefWindowProcW` passthrough for window procedures in other crates.
pub fn default_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    // SAFETY: plain passthrough.
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}

pub fn destroy(hwnd: HWND) {
    // SAFETY: hwnd is a live window owned by this thread.
    let _ = unsafe { DestroyWindow(hwnd) };
}

/// Convert an `HWND` to/from an integer so it can live in an atomic.
pub fn hwnd_to_isize(h: HWND) -> isize {
    h.0 as isize
}
pub fn isize_to_hwnd(v: isize) -> HWND {
    HWND(v as *mut _)
}
