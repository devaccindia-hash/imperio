//! Keys Windows acts on before a window ever sees them.
//!
//! The Windows key, Alt+Tab, Alt+Esc and Ctrl+Esc are handled by Windows
//! itself: pressed in the session window they opened the Start menu or
//! switched windows on *this* computer, and the other computer never heard of
//! them. While the session window is in front, a low-level keyboard hook
//! takes those keys first, keeps them from Windows here and sends them to the
//! other computer instead.
//!
//! Two combinations cannot be taken by any program and stay with this
//! computer: Ctrl+Alt+Del and Windows+L.
//!
//! The hook is installed only while the session window has the keyboard, on a
//! thread of its own (see [`KeyGrab`]).

use std::sync::{Arc, Mutex};

use imperio_protocol::InputEvent;
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL, VK_ESCAPE, VK_LWIN, VK_RWIN, VK_TAB};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT,
    LLKHF_ALTDOWN, LLKHF_EXTENDED, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_QUIT, WM_SYSKEYDOWN,
};

use crate::input::InputLine;

struct State {
    input: Arc<InputLine>,
    /// Virtual keys whose press was taken: their release is taken too, even
    /// if Alt or Ctrl has been let go in between.
    taken: Vec<u32>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

/// Should this key event go to the other computer instead of Windows here?
fn wanted(vk: u32, down: bool, alt: bool, ctrl: bool, taken: &[u32]) -> bool {
    if !down {
        return taken.contains(&vk);
    }
    match vk {
        v if v == VK_LWIN.0 as u32 || v == VK_RWIN.0 as u32 => true,
        // Alt+Tab (and with Shift: backwards).
        v if v == VK_TAB.0 as u32 => alt,
        // Alt+Esc (next window) and Ctrl+Esc (Start menu).
        v if v == VK_ESCAPE.0 as u32 => alt || ctrl,
        _ => false,
    }
}

unsafe extern "system" fn hook_proc(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    // Zero is HC_ACTION: there is a key event to look at.
    if code == 0 {
        // SAFETY: for this hook, LPARAM points at a KBDLLHOOKSTRUCT.
        let k = &*(l.0 as *const KBDLLHOOKSTRUCT);
        let down = matches!(w.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
        let alt = k.flags.0 & LLKHF_ALTDOWN.0 != 0;
        let ctrl = GetAsyncKeyState(VK_CONTROL.0 as i32) < 0;
        let mut taken = false;
        if let Ok(mut state) = STATE.lock() {
            if let Some(st) = state.as_mut() {
                if wanted(k.vkCode, down, alt, ctrl, &st.taken) {
                    tracing::debug!(vk = k.vkCode, scan = k.scanCode, down, "key taken for the other computer");
                    st.taken.retain(|v| *v != k.vkCode);
                    if down {
                        st.taken.push(k.vkCode);
                    }
                    st.input.send(InputEvent::Key {
                        scancode: (k.scanCode & 0xFF) as u16,
                        extended: k.flags.0 & LLKHF_EXTENDED.0 != 0,
                        down,
                        unicode: None,
                    });
                    taken = true;
                }
            }
        }
        if taken {
            return LRESULT(1);
        }
    }
    CallNextHookEx(None, code, w, l)
}

/// The hook, while it is installed.
///
/// It lives on a thread of its own that does nothing but wait for Windows
/// to call it. Installed on the session window's thread it was never called
/// at all (seen on Windows 11 with a window that also receives raw input, as
/// this one does), and a hook that answers late is dropped by Windows
/// without a word, so it must not share a thread with decoding and drawing.
pub struct KeyGrab {
    thread: Option<(u32, std::thread::JoinHandle<()>)>,
    input: Arc<InputLine>,
}

impl KeyGrab {
    pub fn new(input: Arc<InputLine>) -> Self {
        Self { thread: None, input }
    }

    pub fn active(&self) -> bool {
        self.thread.is_some()
    }

    /// Take the keys (the session window got the keyboard) or give them back
    /// (it lost the keyboard, or the user switched this off).
    pub fn set_active(&mut self, on: bool) {
        if on == self.active() {
            return;
        }
        if on {
            *STATE.lock().unwrap() = Some(State { input: self.input.clone(), taken: Vec::new() });
            let (ready, started) = std::sync::mpsc::channel::<Result<u32, String>>();
            let spawned = std::thread::Builder::new().name("key-hook".into()).spawn(move || {
                // SAFETY: the procedure is a plain function; the hook is
                // removed below, on the thread that installed it.
                let hook: HHOOK = match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), Some(imperio_ui::hinstance()), 0) } {
                    Ok(h) => h,
                    Err(e) => {
                        let _ = ready.send(Err(e.to_string()));
                        return;
                    }
                };
                // SAFETY: plain call.
                let _ = ready.send(Ok(unsafe { GetCurrentThreadId() }));
                let mut msg = MSG::default();
                // Windows calls the hook from inside GetMessageW; the loop
                // ends on the WM_QUIT posted by `set_active(false)`.
                // SAFETY: standard message loop on this thread.
                unsafe {
                    while GetMessageW(&mut msg, None, 0, 0).as_bool() {}
                    let _ = UnhookWindowsHookEx(hook);
                }
            });
            match spawned.ok().zip(started.recv().ok()) {
                Some((handle, Ok(id))) => {
                    tracing::info!("keys: the Windows key and Alt+Tab now go to the other computer");
                    self.thread = Some((id, handle));
                }
                other => {
                    let why = match other {
                        Some((_, Err(e))) => e,
                        _ => "the hook thread did not start".to_string(),
                    };
                    tracing::warn!(error = %why, "the Windows key and Alt+Tab stay with this computer (keyboard hook unavailable)");
                    *STATE.lock().unwrap() = None;
                }
            }
        } else if let Some((id, handle)) = self.thread.take() {
            // SAFETY: plain call; a thread that has gone just makes it fail.
            let _ = unsafe { PostThreadMessageW(id, WM_QUIT, WPARAM(0), LPARAM(0)) };
            let _ = handle.join();
            tracing::info!("keys: the Windows key and Alt+Tab are back with this computer");
            // Whatever was held is let go by the caller (`release_held`).
            *STATE.lock().unwrap() = None;
        }
    }
}

impl Drop for KeyGrab {
    fn drop(&mut self) {
        self.set_active(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIN: u32 = 0x5B;
    const TAB: u32 = 0x09;
    const ESC: u32 = 0x1B;
    const A: u32 = 0x41;

    #[test]
    fn which_keys_are_taken() {
        // The Windows key always; Tab and Esc only as part of a combination.
        assert!(wanted(WIN, true, false, false, &[]));
        assert!(wanted(0x5C, true, false, false, &[]));
        assert!(!wanted(TAB, true, false, false, &[]));
        assert!(wanted(TAB, true, true, false, &[]));
        assert!(!wanted(ESC, true, false, false, &[]));
        assert!(wanted(ESC, true, true, false, &[]));
        assert!(wanted(ESC, true, false, true, &[]));
        // Ordinary keys never, whatever is held.
        assert!(!wanted(A, true, true, true, &[]));
    }

    #[test]
    fn a_release_follows_its_press() {
        // Tab taken with Alt held, Alt let go first: the release of Tab must
        // still go where the press went.
        assert!(wanted(TAB, false, false, false, &[TAB]));
        // Tab pressed without Alt went to the window; so does its release,
        // even if Alt is down by then.
        assert!(!wanted(TAB, false, true, false, &[]));
        assert!(!wanted(WIN, false, false, false, &[]));
    }
}
