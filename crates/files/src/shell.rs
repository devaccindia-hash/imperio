//! Questions for Windows Explorer, used when a file is dragged out of the
//! remote picture:
//!
//! * on the computer being viewed: which files are selected in the window in
//!   front (that is what a drag that just began there is carrying);
//! * on the viewing computer: which folder is under the pointer (the Desktop,
//!   or an open Explorer window), so the files can be brought there.
//!
//! Both go through the shell's own automation objects, on a thread of their
//! own (they need a COM apartment that nothing else in the program shares).

use std::path::PathBuf;

use windows::core::Interface;
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, IDataObject, IDispatch, IServiceProvider, CLSCTX_ALL,
    COINIT_APARTMENTTHREADED, FORMATETC,
};
use windows::Win32::System::Ole::ReleaseStgMedium;
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::{
    DragQueryFileW, IFolderView, ILFree, IPersistFolder2, IShellBrowser, IShellView, IShellWindows, SHGetPathFromIDListW,
    ShellWindows, HDROP, SID_STopLevelBrowser, SVGIO_SELECTION, SWC_DESKTOP, SWFO_NEEDDISPATCH,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetClassNameW, GetForegroundWindow, IsWindowVisible, WindowFromPoint, GA_ROOT,
};

const CF_HDROP: u16 = 15;

/// One folder view on screen: its top-level window and the view itself.
struct View {
    top: HWND,
    view: IShellView,
    desktop: bool,
}

unsafe fn view_of(disp: &IDispatch, desktop: bool) -> Option<View> {
    let services: IServiceProvider = disp.cast().ok()?;
    let browser: IShellBrowser = services.QueryService(&SID_STopLevelBrowser).ok()?;
    let view = browser.QueryActiveShellView().ok()?;
    // An Explorer window with tabs has one browser per tab; only the tab
    // that is showing counts.
    let shown = view.GetWindow().ok().is_some_and(|w| IsWindowVisible(w).as_bool());
    if !shown && !desktop {
        return None;
    }
    let top = GetAncestor(browser.GetWindow().ok()?, GA_ROOT);
    Some(View { top, view, desktop })
}

/// Every folder view on screen: Explorer windows, then the Desktop.
unsafe fn views() -> Vec<View> {
    let mut out = Vec::new();
    let Ok(windows) = CoCreateInstance::<_, IShellWindows>(&ShellWindows, None, CLSCTX_ALL) else { return out };
    for i in 0..windows.Count().unwrap_or(0) {
        if let Ok(disp) = windows.Item(&VARIANT::from(i)) {
            out.extend(view_of(&disp, false));
        }
    }
    let mut hwnd = 0i32;
    let desktop = windows.FindWindowSW(&VARIANT::from(0i32), &VARIANT::default(), SWC_DESKTOP, &mut hwnd, SWFO_NEEDDISPATCH);
    if let Ok(disp) = desktop {
        out.extend(view_of(&disp, true));
    }
    out
}

unsafe fn class_of(hwnd: HWND) -> String {
    let mut buf = [0u16; 64];
    let n = GetClassNameW(hwnd, &mut buf).max(0) as usize;
    String::from_utf16_lossy(&buf[..n])
}

/// The Desktop's icons live in a window of one of these classes.
fn is_desktop_class(class: &str) -> bool {
    class == "Progman" || class == "WorkerW"
}

unsafe fn find(views: Vec<View>, top: HWND) -> Option<View> {
    let desktop = is_desktop_class(&class_of(top));
    views.into_iter().find(|v| if desktop { v.desktop } else { !v.desktop && v.top == top })
}

unsafe fn selection(view: &IShellView) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(data) = view.GetItemObject::<IDataObject>(SVGIO_SELECTION) else { return out };
    let format = FORMATETC { cfFormat: CF_HDROP, ptd: std::ptr::null_mut(), dwAspect: 1, lindex: -1, tymed: 1 };
    let Ok(mut medium) = data.GetData(&format) else { return out };
    let drop = HDROP(medium.u.hGlobal.0);
    let count = DragQueryFileW(drop, u32::MAX, None);
    for i in 0..count {
        let len = DragQueryFileW(drop, i, None) as usize;
        let mut buf = vec![0u16; len + 1];
        let got = DragQueryFileW(drop, i, Some(&mut buf)) as usize;
        out.push(PathBuf::from(String::from_utf16_lossy(&buf[..got.min(len)])));
    }
    ReleaseStgMedium(&mut medium);
    out
}

unsafe fn folder(view: &IShellView) -> Option<PathBuf> {
    let folder_view: IFolderView = view.cast().ok()?;
    let persist: IPersistFolder2 = folder_view.GetFolder().ok()?;
    let pidl = persist.GetCurFolder().ok()?;
    let mut buf = [0u16; 260];
    let ok = SHGetPathFromIDListW(pidl, &mut buf).as_bool();
    ILFree(Some(pidl));
    let len = buf.iter().position(|&c| c == 0).unwrap_or(0);
    (ok && len > 0).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..len])))
}

/// Run `f` on a fresh thread inside a COM apartment.
fn in_apartment<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    std::thread::Builder::new()
        .name("shell".into())
        .spawn(move || {
            // SAFETY: balanced with CoUninitialize on this same thread.
            unsafe {
                let init = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                let out = f();
                if init.is_ok() {
                    CoUninitialize();
                }
                out
            }
        })
        .ok()?
        .join()
        .ok()
}

/// The files and folders selected in the Explorer window (or on the
/// Desktop) that is in front. Empty when something else is in front.
pub fn selected_in_foreground() -> Vec<PathBuf> {
    in_apartment(|| {
        // SAFETY: shell objects are created, used and released on this thread.
        unsafe {
            let front = GetAncestor(GetForegroundWindow(), GA_ROOT);
            find(views(), front).map(|v| selection(&v.view)).unwrap_or_default()
        }
    })
    .unwrap_or_default()
}

/// The folder shown at a point of the screen: an open Explorer window's
/// folder, or the Desktop. `None` over anything else.
pub fn folder_at(x: i32, y: i32) -> Option<PathBuf> {
    in_apartment(move || {
        // SAFETY: as above.
        unsafe {
            let top = GetAncestor(WindowFromPoint(POINT { x, y }), GA_ROOT);
            if top.0.is_null() {
                return None;
            }
            find(views(), top).and_then(|v| folder(&v.view))
        }
    })
    .flatten()
}

/// Diagnostic: every folder view found, as text.
pub fn describe() -> Vec<String> {
    in_apartment(|| {
        // SAFETY: as above.
        unsafe {
            views()
                .iter()
                .map(|v| {
                    let shown = v.view.GetWindow().ok().is_some_and(|w| IsWindowVisible(w).as_bool());
                    format!(
                        "top={:?} class={} desktop={} view_shown={} folder={:?} selected={}",
                        v.top.0,
                        class_of(v.top),
                        v.desktop,
                        shown,
                        folder(&v.view),
                        selection(&v.view).len()
                    )
                })
                .collect()
        }
    })
    .unwrap_or_default()
}
