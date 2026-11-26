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
//! The hook is installed only while the session window has the keyboard, and
//! it is called on the window's own thread (the one that installed it).

use std::sync::{Arc, Mutex};

use imperio_protocol::InputEvent;
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL, VK_ESCAPE, VK_LWIN, VK_RWIN, VK_TAB};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, SetWindowsHookExW, UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT, LLKHF_ALTDOWN, LLKHF_EXTENDED,
    WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN,
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

/// The hook, while it is installed. Create and drop it on the window's thread.
pub struct KeyGrab {
    hook: Option<HHOOK>,
    input: Arc<InputLine>,
}

impl KeyGrab {
    pub fn new(input: Arc<InputLine>) -> Self {
        Self { hook: None, input }
    }

    pub fn active(&self) -> bool {
        self.hook.is_some()
    }

    /// Take the keys (the session window got the keyboard) or give them back
    /// (it lost the keyboard, or the user switched this off).
    pub fn set_active(&mut self, on: bool) {
        if on == self.active() {
            return;
        }
        if on {
            *STATE.lock().unwrap() = Some(State { input: self.input.clone(), taken: Vec::new() });
            // SAFETY: the procedure is a plain function that outlives the hook.
            match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0) } {
                Ok(h) => self.hook = Some(h),
                Err(e) => {
                    tracing::warn!(error = %e, "the Windows key and Alt+Tab stay with this computer (keyboard hook unavailable)");
                    *STATE.lock().unwrap() = None;
                }
            }
        } else if let Some(h) = self.hook.take() {
            // SAFETY: a hook this thread installed.
            let _ = unsafe { UnhookWindowsHookEx(h) };
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
