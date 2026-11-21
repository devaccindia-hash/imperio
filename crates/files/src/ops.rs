//! What is done to the local disk. Used by the viewed computer to carry out
//! requests, and by the Files window for the "This computer" side.
//!
//! Errors are sentences for the person at the keyboard, not codes.

use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use windows::core::{GUID, PCWSTR};
use windows::Win32::Storage::FileSystem::GetLogicalDrives;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{
    SHFileOperationW, SHGetKnownFolderPath, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT,
    FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FO_DELETE, KF_FLAG_DEFAULT, SHFILEOPSTRUCTW,
};

use crate::proto::{valid_name, Entry, Op, MAX_ENTRIES};

pub type OpResult<T = ()> = Result<T, String>;

/// A listing: the folder as it should be shown, the folder above it, what is
/// in it, and a note when the listing is incomplete.
pub struct Listing {
    pub path: String,
    pub parent: Option<String>,
    pub entries: Vec<Entry>,
    pub note: Option<String>,
}

pub fn computer_name() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "the other computer".into())
}

fn known_folder(id: &GUID) -> Option<PathBuf> {
    // SAFETY: the call returns a CoTaskMem string that we copy and free.
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s.map(PathBuf::from)
    }
}

pub fn home() -> PathBuf {
    std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("C:\\"))
}

pub fn desktop() -> PathBuf {
    known_folder(&FOLDERID_Desktop).unwrap_or_else(|| home().join("Desktop"))
}

pub fn downloads() -> PathBuf {
    known_folder(&FOLDERID_Downloads).unwrap_or_else(|| home().join("Downloads"))
}

fn documents() -> PathBuf {
    known_folder(&FOLDERID_Documents).unwrap_or_else(|| home().join("Documents"))
}

/// The starting places: the user's folders, then every drive.
pub fn roots() -> Vec<Entry> {
    let mut out = Vec::new();
    for (name, path) in [("Desktop", desktop()), ("Documents", documents()), ("Downloads", downloads())] {
        if path.is_dir() {
            out.push(Entry { name: name.into(), path: text(&path), dir: true, size: 0, modified: 0 });
        }
    }
    // SAFETY: plain call, returns a bit per drive letter.
    let mask = unsafe { GetLogicalDrives() };
    for i in 0..26u8 {
        if mask >> i & 1 == 1 {
            let root = format!("{}:\\", (b'A' + i) as char);
            out.push(Entry { name: root.clone(), path: root, dir: true, size: 0, modified: 0 });
        }
    }
    out
}

fn text(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// A full path typed or sent by the other side, tidied: `C:` becomes `C:\`,
/// forward slashes and trailing separators go.
pub fn full_path(path: &str) -> OpResult<PathBuf> {
    let mut s = path.trim().replace('/', "\\");
    if s.len() == 2 && s.ends_with(':') {
        s.push('\\');
    }
    while s.len() > 3 && s.ends_with('\\') {
        s.pop();
    }
    let p = PathBuf::from(&s);
    let rooted = matches!(p.components().next(), Some(Component::Prefix(_))) && p.is_absolute();
    if !rooted || p.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir)) {
        return Err("That is not a full path. Use one like C:\\Users\\Name\\Documents.".into());
    }
    Ok(p)
}

/// The folder above `path`: `Some("")` (the starting places) for a drive.
pub fn parent_of(path: &Path) -> Option<String> {
    Some(path.parent().map(text).unwrap_or_default())
}

fn modified_secs(meta: &std::fs::Metadata) -> u64 {
    meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn explain(e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::NotFound => "It is no longer there.".into(),
        io::ErrorKind::PermissionDenied => "Windows does not allow that here (no permission, or the file is in use).".into(),
        io::ErrorKind::AlreadyExists => "Something with that name already exists there.".into(),
        _ => match e.raw_os_error() {
            Some(32) => "The file is open in another program.".into(),
            Some(112) | Some(39) => "The disk is full.".into(),
            Some(145) => "The folder is not empty.".into(),
            Some(206) => "The name or path is too long.".into(),
            _ => e.to_string(),
        },
    }
}

/// List a folder; an empty path lists the starting places.
pub fn list(path: &str) -> OpResult<Listing> {
    if path.trim().is_empty() {
        return Ok(Listing { path: String::new(), parent: None, entries: roots(), note: None });
    }
    let dir = full_path(path)?;
    let read = std::fs::read_dir(&dir).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => "That folder does not exist.".to_string(),
        io::ErrorKind::PermissionDenied => "Windows does not allow this folder to be opened.".to_string(),
        _ => explain(&e),
    })?;
    let mut entries = Vec::new();
    let mut note = None;
    for item in read.flatten() {
        if entries.len() >= MAX_ENTRIES {
            note = Some(format!("Only the first {MAX_ENTRIES} items are shown."));
            break;
        }
        let Ok(meta) = item.metadata() else { continue };
        let path = item.path();
        // A link or junction counts as whatever it points to.
        let is_dir = if meta.file_type().is_symlink() { path.is_dir() } else { meta.is_dir() };
        entries.push(Entry {
            name: item.file_name().to_string_lossy().into_owned(),
            path: text(&path),
            dir: is_dir,
            size: if is_dir { 0 } else { meta.len() },
            modified: modified_secs(&meta),
        });
    }
    entries.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(Listing { parent: parent_of(&dir), path: text(&dir), entries, note })
}

/// `dir\name`, or `dir\name (2)`, `(3)`… when that is taken. A file keeps its
/// extension: `report (2).docx`.
pub fn unique(dir: &Path, name: &str, is_dir: bool) -> PathBuf {
    let first = dir.join(name);
    if std::fs::symlink_metadata(&first).is_err() {
        return first;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if !is_dir && i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    for n in 2..10_000 {
        let candidate = dir.join(format!("{stem} ({n}){ext}"));
        if std::fs::symlink_metadata(&candidate).is_err() {
            return candidate;
        }
    }
    first
}

pub fn new_folder(parent: &str, name: &str) -> OpResult {
    let name = name.trim();
    if !valid_name(name) {
        return Err("That name cannot be used for a folder.".into());
    }
    let parent = full_path(parent)?;
    std::fs::create_dir(parent.join(name)).map_err(|e| explain(&e))
}

pub fn rename(path: &str, name: &str) -> OpResult {
    let name = name.trim();
    if !valid_name(name) {
        return Err("That name cannot be used.".into());
    }
    let from = full_path(path)?;
    let (Some(parent), Some(old)) = (from.parent(), from.file_name()) else {
        return Err("A drive cannot be renamed here.".into());
    };
    let to = parent.join(name);
    let same = old.to_string_lossy().to_lowercase() == name.to_lowercase();
    if !same && std::fs::symlink_metadata(&to).is_ok() {
        return Err("Something with that name already exists there.".into());
    }
    std::fs::rename(&from, &to).map_err(|e| explain(&e))
}

/// True when `inner` is `outer` or lies inside it.
fn within(inner: &Path, outer: &Path) -> bool {
    let low = |p: &Path| p.components().map(|c| c.as_os_str().to_string_lossy().to_lowercase()).collect::<Vec<_>>();
    let (i, o) = (low(inner), low(outer));
    i.len() >= o.len() && i[..o.len()] == o[..]
}

fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    let meta = std::fs::symlink_metadata(from)?;
    if meta.file_type().is_symlink() {
        return Ok(()); // links and junctions are not followed
    }
    if meta.is_dir() {
        std::fs::create_dir(to)?;
        for item in std::fs::read_dir(from)? {
            let item = item?;
            copy_tree(&item.path(), &to.join(item.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(from, to).map(|_| ())
    }
}

fn source(path: &str) -> OpResult<(PathBuf, String, bool)> {
    let from = full_path(path)?;
    let name = from.file_name().map(|n| n.to_string_lossy().into_owned()).ok_or("A whole drive cannot be used here.")?;
    let meta = std::fs::symlink_metadata(&from).map_err(|e| format!("{name}: {}", explain(&e)))?;
    Ok((from, name, meta.is_dir()))
}

fn dest_dir(to_dir: &str) -> OpResult<PathBuf> {
    let dir = full_path(to_dir)?;
    if !dir.is_dir() {
        return Err("The destination folder does not exist.".into());
    }
    Ok(dir)
}

/// Copy each of `from` into `to_dir`. A name already there is kept, and the
/// copy is numbered.
pub fn copy_into(from: &[String], to_dir: &str) -> OpResult {
    let dir = dest_dir(to_dir)?;
    for path in from {
        let (src, name, is_dir) = source(path)?;
        if is_dir && within(&dir, &src) {
            return Err(format!("{name} cannot be copied into itself."));
        }
        let dst = unique(&dir, &name, is_dir);
        copy_tree(&src, &dst).map_err(|e| format!("{name}: {}", explain(&e)))?;
    }
    Ok(())
}

/// Move each of `from` into `to_dir`. Nothing already there is replaced.
pub fn move_into(from: &[String], to_dir: &str) -> OpResult {
    let dir = dest_dir(to_dir)?;
    for path in from {
        let (src, name, is_dir) = source(path)?;
        if src.parent().is_some_and(|p| within(p, &dir) && within(&dir, p)) {
            continue; // already there
        }
        if is_dir && within(&dir, &src) {
            return Err(format!("{name} cannot be moved into itself."));
        }
        let dst = dir.join(&name);
        if std::fs::symlink_metadata(&dst).is_ok() {
            return Err(format!("{name} already exists in the destination."));
        }
        if std::fs::rename(&src, &dst).is_ok() {
            continue;
        }
        // Another drive: copy, then remove the original.
        copy_tree(&src, &dst).map_err(|e| format!("{name}: {}", explain(&e)))?;
        let removed = if is_dir { std::fs::remove_dir_all(&src) } else { std::fs::remove_file(&src) };
        removed.map_err(|e| format!("{name} was copied, but the original could not be removed: {}", explain(&e)))?;
    }
    Ok(())
}

/// Move `paths` to the Recycle Bin (where the drive has one).
pub fn delete(paths: &[String]) -> OpResult {
    if paths.is_empty() {
        return Ok(());
    }
    // The shell wants every path ending in NUL and the list ending in another.
    let mut list: Vec<u16> = Vec::new();
    for path in paths {
        let (src, _, _) = source(path)?;
        list.extend(src.as_os_str().to_string_lossy().encode_utf16());
        list.push(0);
    }
    list.push(0);
    let mut op = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: PCWSTR(list.as_ptr()),
        fFlags: (FOF_ALLOWUNDO.0 | FOF_NOCONFIRMATION.0 | FOF_SILENT.0 | FOF_NOERRORUI.0) as u16,
        ..Default::default()
    };
    // SAFETY: `op` and the list it points to outlive the call.
    let code = unsafe { SHFileOperationW(&mut op) };
    if code != 0 || op.fAnyOperationsAborted.as_bool() {
        return Err(match code {
            0 => "Not everything could be deleted.".into(),
            2 | 3 | 0x7C => "It is no longer there.".into(),
            5 | 0x78 | 0x86 => "Windows does not allow that to be deleted (no permission).".into(),
            32 => "The file is open in another program.".into(),
            _ => format!("Windows could not delete it (code {code})."),
        });
    }
    Ok(())
}

/// Carry out one requested change on this computer.
pub fn run(op: &Op) -> OpResult {
    match op {
        Op::NewFolder { parent, name } => new_folder(parent, name),
        Op::Rename { path, name } => rename(path, name),
        Op::Move { from, to_dir } => move_into(from, to_dir),
        Op::Copy { from, to_dir } => copy_into(from, to_dir),
        Op::Delete { paths } => delete(paths),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("imperio-ops-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn paths_must_be_full() {
        assert_eq!(full_path("C:").unwrap(), PathBuf::from("C:\\"));
        assert_eq!(full_path("C:/Users/x/").unwrap(), PathBuf::from("C:\\Users\\x"));
        for bad in ["", "docs", "\\docs", "..\\x", "C:\\a\\..\\b", "C:docs"] {
            assert!(full_path(bad).is_err(), "{bad:?}");
        }
        assert_eq!(parent_of(Path::new("C:\\")), Some(String::new()));
        assert_eq!(parent_of(Path::new("C:\\a\\b")), Some("C:\\a".into()));
    }

    #[test]
    fn folder_work() {
        let d = scratch("work");
        let s = |p: &Path| p.to_string_lossy().into_owned();
        new_folder(&s(&d), "one").unwrap();
        assert!(new_folder(&s(&d), "one").is_err());
        assert!(new_folder(&s(&d), "bad/name").is_err());
        std::fs::write(d.join("one").join("a.txt"), b"hello").unwrap();
        std::fs::create_dir(d.join("one").join("sub")).unwrap();
        std::fs::write(d.join("one").join("sub").join("b.bin"), [1u8; 3000]).unwrap();

        rename(&s(&d.join("one")), "two").unwrap();
        assert!(d.join("two").join("sub").join("b.bin").exists());

        // Copy twice: the second copy is numbered, nothing is overwritten.
        new_folder(&s(&d), "dest").unwrap();
        copy_into(&[s(&d.join("two"))], &s(&d.join("dest"))).unwrap();
        copy_into(&[s(&d.join("two")), s(&d.join("two").join("a.txt"))], &s(&d.join("dest"))).unwrap();
        assert_eq!(std::fs::read(d.join("dest").join("two (2)").join("a.txt")).unwrap(), b"hello");
        assert!(d.join("dest").join("a.txt").exists());
        assert!(copy_into(&[s(&d.join("two"))], &s(&d.join("two").join("sub"))).is_err());

        // Move: refuses to replace, otherwise moves.
        assert!(move_into(&[s(&d.join("two"))], &s(&d.join("dest"))).is_err());
        new_folder(&s(&d), "away").unwrap();
        move_into(&[s(&d.join("two"))], &s(&d.join("away"))).unwrap();
        assert!(!d.join("two").exists() && d.join("away").join("two").join("a.txt").exists());

        let l = list(&s(&d)).unwrap();
        assert_eq!(l.entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["away", "dest"]);
        assert!(l.entries.iter().all(|e| e.dir));
        let l = list(&s(&d.join("dest"))).unwrap();
        let file = l.entries.iter().find(|e| e.name == "a.txt").unwrap();
        assert!(!file.dir && file.size == 5 && file.modified > 0);
        assert!(list(&s(&d.join("nope"))).is_err());

        assert_eq!(unique(&d.join("dest"), "a.txt", false), d.join("dest").join("a (2).txt"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn starting_places_include_a_drive() {
        let l = list("").unwrap();
        assert!(l.parent.is_none());
        assert!(l.entries.iter().any(|e| e.path.ends_with(":\\")));
    }
}
