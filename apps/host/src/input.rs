//! Input injection via `SendInput`.
//!
//! `SendInput` delivers to windows on the calling thread's current desktop.
//! When Windows shows an elevation prompt or the lock screen it switches the
//! active input desktop; a process running as the machine account may follow
//! it with `SetThreadDesktop`, while an ordinary user process may not and
//! injection simply pauses until the interactive desktop returns. `SendInput`
//! also delivers nothing to higher-integrity windows and reports no error, so
//! there is no failure to surface here.

use std::sync::Mutex;

use imperio_protocol::{InputEvent, MouseButton};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop,
    DESKTOP_ACCESS_FLAGS, USER_OBJECT_INFORMATION_INDEX,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, MOUSE_EVENT_FLAGS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

/// winuser.h XBUTTON1 / XBUTTON2 for MOUSEINPUT::mouseData.
const XBUTTON1: u32 = 0x0001;
const XBUTTON2: u32 = 0x0002;
const UOI_NAME: USER_OBJECT_INFORMATION_INDEX = USER_OBJECT_INFORMATION_INDEX(2);

struct State {
    /// Streamed display rectangle in virtual-desktop pixels (l, t, w, h).
    monitor: (i32, i32, i32, i32),
    /// Name of the desktop this thread last attached to.
    desktop: String,
}

/// Maps normalised stream coordinates onto the streamed display and injects.
pub struct Injector {
    state: Mutex<State>,
}

impl Injector {
    pub fn new(left: i32, top: i32, width: i32, height: i32) -> Self {
        Self {
            state: Mutex::new(State { monitor: (left, top, width, height), desktop: String::new() }),
        }
    }

    pub fn set_monitor(&self, left: i32, top: i32, width: i32, height: i32) {
        self.state.lock().unwrap().monitor = (left, top, width, height);
    }

    /// Attach this thread to the active input desktop if it changed since the
    /// last call. Returns false when the desktop could not be opened (e.g. an
    /// unprivileged process while the secure desktop is up), in which case
    /// injection is skipped for now.
    pub fn follow_input_desktop(&self) -> bool {
        // SAFETY: OpenInputDesktop returns a desktop handle we close below.
        let handle = unsafe { OpenInputDesktop(Default::default(), true, DESKTOP_ACCESS_FLAGS(0x0001_FFFF)) };
        let Ok(desktop) = handle else {
            return false;
        };
        let name = desktop_name(desktop.0);
        let mut st = self.state.lock().unwrap();
        if name.is_empty() || name == st.desktop {
            // No change (or unknown): keep the current attachment.
            unsafe {
                let _ = CloseDesktop(desktop);
            }
            return !st.desktop.is_empty() || name.is_empty();
        }
        // SAFETY: valid desktop handle; on success this thread is attached and
        // we must not close the handle it is now using until we switch away.
        let ok = unsafe { SetThreadDesktop(desktop) }.is_ok();
        if ok {
            tracing::info!(desktop = %name, "attached to input desktop");
            st.desktop = name;
        } else {
            unsafe {
                let _ = CloseDesktop(desktop);
            }
        }
        ok
    }

    pub fn inject(&self, ev: InputEvent) {
        let monitor = self.state.lock().unwrap().monitor;
        let input = match ev {
            InputEvent::MouseMove { nx, ny } => {
                let (ml, mt, mw, mh) = monitor;
                let (vl, vt, vw, vh) = virtual_desktop();
                // Stream-normalised -> display pixel -> virtual-desktop normalised.
                let px = ml as f64 + nx as f64 / 65535.0 * mw as f64;
                let py = mt as f64 + ny as f64 / 65535.0 * mh as f64;
                let ax = ((px - vl as f64) / vw.max(1) as f64 * 65535.0).round() as i32;
                let ay = ((py - vt as f64) / vh.max(1) as f64 * 65535.0).round() as i32;
                mouse(ax, ay, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK)
            }
            InputEvent::MouseButton { button, down } => {
                let (flags, data) = match (button, down) {
                    (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                    (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
                    (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                    (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
                    (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                    (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                    (MouseButton::X1, true) => (MOUSEEVENTF_XDOWN, XBUTTON1 as i32),
                    (MouseButton::X1, false) => (MOUSEEVENTF_XUP, XBUTTON1 as i32),
                    (MouseButton::X2, true) => (MOUSEEVENTF_XDOWN, XBUTTON2 as i32),
                    (MouseButton::X2, false) => (MOUSEEVENTF_XUP, XBUTTON2 as i32),
                };
                mouse(0, 0, data, flags)
            }
            InputEvent::MouseWheel { dx, dy } => {
                if dy != 0 {
                    send(&[mouse(0, 0, dy, MOUSEEVENTF_WHEEL)]);
                }
                if dx != 0 {
                    send(&[mouse(0, 0, dx, MOUSEEVENTF_HWHEEL)]);
                }
                return;
            }
            InputEvent::Key { scancode, extended, down, unicode } => {
                if scancode != 0 {
                    let mut flags = KEYEVENTF_SCANCODE;
                    if extended {
                        flags |= KEYEVENTF_EXTENDEDKEY;
                    }
                    if !down {
                        flags |= KEYEVENTF_KEYUP;
                    }
                    key(scancode, flags)
                } else if let Some(c) = unicode {
                    let mut buf = [0u16; 2];
                    for u in c.encode_utf16(&mut buf).iter() {
                        let mut flags = KEYEVENTF_UNICODE;
                        if !down {
                            flags |= KEYEVENTF_KEYUP;
                        }
                        send(&[key(*u, flags)]);
                    }
                    return;
                } else {
                    return;
                }
            }
        };
        send(&[input]);
    }
}

fn virtual_desktop() -> (i32, i32, i32, i32) {
    // SAFETY: GetSystemMetrics has no preconditions.
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

fn desktop_name(handle: *mut core::ffi::c_void) -> String {
    let mut buf = [0u16; 256];
    let mut needed = 0u32;
    // SAFETY: valid handle; buffer sized in bytes.
    let ok = unsafe {
        GetUserObjectInformationW(
            windows::Win32::Foundation::HANDLE(handle),
            UOI_NAME,
            Some(buf.as_mut_ptr() as *mut _),
            (buf.len() * 2) as u32,
            Some(&mut needed),
        )
    };
    if ok.is_err() {
        return String::new();
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

fn send(inputs: &[INPUT]) {
    // SAFETY: inputs is a valid slice of initialised INPUT structs.
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        tracing::debug!(sent, wanted = inputs.len(), "SendInput delivered fewer events than requested");
    }
}

fn mouse(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT { dx, dy, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

fn key(scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: Default::default(), wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}
