//! Files on the Windows clipboard ("Copy" in Explorer), and the staging
//! folders that copied files from the other computer are received into.
//!
//! Copy on one computer, paste on the other: the files are first received
//! into a fresh folder under `%TEMP%\Imperio-clipboard`, and the clipboard of
//! the receiving computer is then pointed at them, so the paste that follows
//! is an ordinary local paste. Files that already sit in a staging folder are
//! never sent back the other way; that is what keeps two clipboards from
//! echoing each other.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};

use windows::Win32::Foundation::{HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber, IsClipboardFormatAvailable, OpenClipboard,
    SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::UI::Shell::{DragQueryFileW, DROPFILES, HDROP};

const CF_HDROP: u32 = 15;
const STAGING: &str = "Imperio-clipboard";

/// As the destination of an upload: "into a staging folder, then onto your
/// clipboard".
pub const CLIPBOARD_DEST: &str = "::clipboard";

/// Changes every time anything is put on the clipboard.
pub fn sequence() -> u32 {
    // SAFETY: plain call.
    unsafe { GetClipboardSequenceNumber() }
}

/// True when the clipboard holds copied files (cheap: nothing is opened).
pub fn has_files() -> bool {
    // SAFETY: plain call.
    unsafe { IsClipboardFormatAvailable(CF_HDROP).is_ok() }
}

/// The clipboard is busy for a moment whenever another program reads it.
fn open() -> bool {
    for _ in 0..10 {
        // SAFETY: plain call; closed by the caller.
        if unsafe { OpenClipboard(None) }.is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// The files on the clipboard, if that is what it holds.
pub fn read_files() -> Vec<PathBuf> {
    let mut out = Vec::new();
    // SAFETY: the clipboard is open while the handle is used; the handle
    // belongs to the clipboard and is not freed here.
    unsafe {
        if IsClipboardFormatAvailable(CF_HDROP).is_err() || !open() {
            return out;
        }
        if let Ok(h) = GetClipboardData(CF_HDROP) {
            let drop = HDROP(h.0);
            let count = DragQueryFileW(drop, u32::MAX, None);
            for i in 0..count {
                let len = DragQueryFileW(drop, i, None) as usize;
                let mut buf = vec![0u16; len + 1];
                let got = DragQueryFileW(drop, i, Some(&mut buf)) as usize;
                out.push(PathBuf::from(String::from_utf16_lossy(&buf[..got.min(len)])));
            }
        }
        let _ = CloseClipboard();
    }
    out
}

/// Put `paths` on the clipboard as copied files. Returns the clipboard's new
/// sequence number.
pub fn write_files(paths: &[PathBuf]) -> Option<u32> {
    if paths.is_empty() {
        return None;
    }
    let mut list: Vec<u16> = Vec::new();
    for p in paths {
        list.extend(p.as_os_str().to_string_lossy().encode_utf16());
        list.push(0);
    }
    list.push(0);
    let header = std::mem::size_of::<DROPFILES>();
    let bytes = header + list.len() * 2;
    // SAFETY: the block is allocated, filled within its size, and handed to
    // the clipboard, which owns it from then on.
    unsafe {
        let mem: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, bytes).ok()?;
        let ptr = GlobalLock(mem) as *mut u8;
        if ptr.is_null() {
            return None;
        }
        std::ptr::write_bytes(ptr, 0, bytes);
        let head = ptr as *mut DROPFILES;
        (*head).pFiles = header as u32;
        (*head).fWide = true.into();
        std::ptr::copy_nonoverlapping(list.as_ptr() as *const u8, ptr.add(header), list.len() * 2);
        let _ = GlobalUnlock(mem);
        if !open() {
            return None;
        }
        let ok = EmptyClipboard().is_ok() && SetClipboardData(CF_HDROP, Some(HANDLE(mem.0))).is_ok();
        let _ = CloseClipboard();
        ok.then(sequence)
    }
}

pub fn staging_root() -> PathBuf {
    std::env::temp_dir().join(STAGING)
}

/// True for a path inside any staging folder (this computer's or, by name,
/// the other one's).
pub fn in_staging(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str().eq_ignore_ascii_case(STAGING))
}

/// A fresh, empty staging folder.
pub fn new_staging_dir() -> std::io::Result<PathBuf> {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let dir = staging_root().join(format!("{stamp}-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Remove staging folders older than a day (a paste made from them is long
/// finished). Called when a session starts.
pub fn clean_staging() {
    let Ok(items) = std::fs::read_dir(staging_root()) else { return };
    for item in items.flatten() {
        let old = item
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > Duration::from_secs(24 * 3600));
        if old {
            let _ = std::fs::remove_dir_all(item.path());
        }
    }
}

/// The top-level files and folders in a staging folder.
pub fn staged_items(dir: &Path) -> Vec<PathBuf> {
    let mut items: Vec<PathBuf> = std::fs::read_dir(dir).map(|r| r.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    items.sort();
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staging_paths_are_recognised() {
        let dir = new_staging_dir().unwrap();
        assert!(in_staging(&dir.join("a.txt")));
        assert!(in_staging(Path::new("D:\\Temp\\imperio-CLIPBOARD\\17-2-0\\x")));
        assert!(!in_staging(Path::new("C:\\Users\\me\\Documents\\Imperio-clipboard.txt")));
        std::fs::write(dir.join("b.txt"), b"x").unwrap();
        std::fs::create_dir(dir.join("a")).unwrap();
        assert_eq!(staged_items(&dir), [dir.join("a"), dir.join("b.txt")]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
