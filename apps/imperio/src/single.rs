//! One Imperio home window at a time.
//!
//! Starting Imperio while another copy is already running (often hidden in
//! the tray, sometimes an older download) used to leave two: the second could
//! not share ("Is another copy of Imperio already running?") and the first
//! kept the port. Now the copy that was just started wins: it closes every
//! other home window, and the remote-screen windows those had opened, before
//! it begins.
//!
//! Other copies are recognised by their window (title "Imperio", the window
//! class this program creates), not by file name, so a download saved as
//! "Imperio (1).exe" and versions that predate this module are found too.
//! Only the current desktop session is looked at, and only home windows:
//! `imperio host`, the service, the updater and owner tools are left alone.
//!
//! Setting `IMPERIO_DATA_DIR` (a deliberately separate profile: portable
//! copy, tests) switches this off.

use std::sync::atomic::{AtomicIsize, Ordering};

use windows::core::{w, BOOL};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    CreateMutexW, OpenProcess, ReleaseMutex, TerminateProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
    PROCESS_TERMINATE,
};
use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetClassNameW, GetWindowTextW, GetWindowThreadProcessId};

const HOME_TITLE: &str = "Imperio";

/// Startup lock, held from `enforce` until `started` so two copies launched
/// in the same instant take turns instead of each missing the other.
static LOCK: AtomicIsize = AtomicIsize::new(0);

struct Proc {
    pid: u32,
    parent: u32,
    exe: String,
}

/// Close every other home window (and its session windows). Call before this
/// copy opens its own window or binds its port.
pub fn enforce() {
    if std::env::var_os("IMPERIO_DATA_DIR").is_some_and(|d| !d.is_empty()) {
        return;
    }
    // SAFETY: plain Win32 calls with valid arguments; handles are closed below.
    unsafe {
        if let Ok(m) = CreateMutexW(None, false, w!("Local\\ImperioHomeStartup")) {
            let _ = WaitForSingleObject(m, 10_000);
            LOCK.store(m.0 as isize, Ordering::Relaxed);
        }
    }
    let me = std::process::id();
    let class = own_window_class();
    let mut homes: Vec<u32> = Vec::new();
    for (pid, title, cls) in top_level_windows() {
        if pid != me && title == HOME_TITLE && cls == class && !homes.contains(&pid) {
            homes.push(pid);
        }
    }
    if homes.is_empty() {
        return;
    }
    let procs = processes();
    let mut doomed = homes.clone();
    for home in &homes {
        let exe = procs.iter().find(|p| p.pid == *home).map(|p| p.exe.to_lowercase());
        // Remote-screen windows are the same program started by that copy.
        for p in &procs {
            if p.parent == *home && p.pid != me && Some(p.exe.to_lowercase()) == exe && !doomed.contains(&p.pid) {
                doomed.push(p.pid);
            }
        }
    }
    for pid in doomed {
        let closed = stop(pid);
        tracing::info!(pid, closed, "another copy of Imperio was running");
    }
}

/// This copy's window is up: let the next one in.
pub fn started() {
    let raw = LOCK.swap(0, Ordering::Relaxed);
    if raw != 0 {
        let m = HANDLE(raw as *mut core::ffi::c_void);
        // SAFETY: the mutex handle created in `enforce`, owned by this thread
        // or abandoned on exit; both are fine to release and close.
        unsafe {
            let _ = ReleaseMutex(m);
            let _ = CloseHandle(m);
        }
    }
}

/// End a process and wait (briefly) until it is gone, so its port is free.
fn stop(pid: u32) -> bool {
    // SAFETY: the handle is valid while held and closed before returning.
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, false, pid) else {
            return false; // gone already, or started "as administrator"
        };
        let ok = TerminateProcess(h, 0).is_ok();
        let _ = WaitForSingleObject(h, 3_000);
        let _ = CloseHandle(h);
        ok
    }
}

/// The class name winit gives the windows it creates (the home window is one).
fn own_window_class() -> String {
    "Window Class".to_string()
}

fn top_level_windows() -> Vec<(u32, String, String)> {
    unsafe extern "system" fn each(hwnd: HWND, out: LPARAM) -> BOOL {
        // SAFETY: `out` is the Vec passed to EnumWindows below, alive for the call.
        let list = unsafe { &mut *(out.0 as *mut Vec<(u32, String, String)>) };
        let mut pid = 0u32;
        let mut title = [0u16; 128];
        let mut class = [0u16; 128];
        // SAFETY: valid window handle from the enumeration; buffers are sized.
        let (t, c) = unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            (GetWindowTextW(hwnd, &mut title), GetClassNameW(hwnd, &mut class))
        };
        list.push((
            pid,
            String::from_utf16_lossy(&title[..t.max(0) as usize]),
            String::from_utf16_lossy(&class[..c.max(0) as usize]),
        ));
        BOOL(1)
    }
    let mut list: Vec<(u32, String, String)> = Vec::new();
    // SAFETY: the callback only touches `list`, which outlives the call.
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&mut list as *mut _ as isize));
    }
    list
}

fn processes() -> Vec<Proc> {
    let mut out = Vec::new();
    // SAFETY: snapshot handle is valid until closed; the entry is sized.
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return out;
        };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut more = Process32FirstW(snap, &mut e).is_ok();
        while more {
            let len = e.szExeFile.iter().position(|c| *c == 0).unwrap_or(e.szExeFile.len());
            out.push(Proc {
                pid: e.th32ProcessID,
                parent: e.th32ParentProcessID,
                exe: String::from_utf16_lossy(&e.szExeFile[..len]),
            });
            more = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    out
}
