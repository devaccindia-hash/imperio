//! Messages of the file service.
//!
//! Each travels as one message of the reliable stream. The first byte says
//! how to read the rest: [`TAG_MSG`] is a postcard-encoded [`Msg`];
//! [`TAG_DATA`] is `xfer u32 (LE) | file bytes`, kept out of postcard so file
//! contents are not copied byte by byte through a serializer.

use serde::{Deserialize, Serialize};

pub const TAG_MSG: u8 = 0;
pub const TAG_DATA: u8 = 1;

/// File bytes per data message.
pub const DATA_BYTES: usize = 256 * 1024;

/// A listing is cut off here, with a note, rather than grow without bound.
pub const MAX_ENTRIES: usize = 50_000;

/// One file or folder in a listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    /// Full path on the computer that listed it.
    pub path: String,
    pub dir: bool,
    pub size: u64,
    /// Seconds since 1970, 0 when unknown.
    pub modified: u64,
}

/// A change to the disk of the computer that receives it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    NewFolder { parent: String, name: String },
    Rename { path: String, name: String },
    /// Move each of `from` into the folder `to_dir`.
    Move { from: Vec<String>, to_dir: String },
    /// Copy each of `from` into the folder `to_dir`.
    Copy { from: Vec<String>, to_dir: String },
    /// To the Recycle Bin.
    Delete { paths: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Msg {
    // ---- the viewing computer asks, the viewed computer answers ----------
    Hello,
    Welcome {
        /// False when file access is switched off on that computer.
        allowed: bool,
        computer: String,
        home: String,
        desktop: String,
    },
    /// `path` empty = the starting places (user folders and drives).
    List { id: u32, path: String },
    Listing {
        id: u32,
        path: String,
        /// Folder above this one; `Some("")` is the starting places.
        parent: Option<String>,
        entries: Vec<Entry>,
        error: Option<String>,
    },
    Op { id: u32, op: Op },
    OpDone { id: u32, error: Option<String> },
    /// "Send me these" (download).
    Fetch { xfer: u32, paths: Vec<String> },
    /// "I am about to send into this folder of yours" (upload).
    Offer { xfer: u32, dest: String },

    // ---- a transfer, from whichever side sends ---------------------------
    Total { xfer: u32, files: u32, bytes: u64 },
    /// Next file or folder. `rel` is relative to the destination, parts
    /// separated by `/`. A file's bytes follow as data messages, then `End`.
    Start { xfer: u32, rel: String, dir: bool, size: u64, modified: u64 },
    End { xfer: u32 },
    /// The sender could not finish the current file: discard it.
    Skip { xfer: u32 },
    /// Everything was sent. `error` notes what had to be left out.
    Finish { xfer: u32, error: Option<String> },
    /// The receiver's closing word.
    Done { xfer: u32, error: Option<String>, files: u32, bytes: u64 },
    /// Either side gives up on the transfer (cancelled, disk full, refused).
    Stop { xfer: u32, reason: String },
    /// The viewed computer's clipboard changed; `files` are the copied
    /// files, if any (`bytes`: their size, `u64::MAX` if too many to count).
    Clip { files: Vec<String>, bytes: u64 },
    /// "A drag just began on your screen: which files is it carrying?"
    DragQuery,
    /// The files selected in the window in front of the viewed computer.
    DragItems { paths: Vec<String> },
}

pub fn encode(msg: &Msg) -> Vec<u8> {
    let mut out = vec![TAG_MSG];
    out.extend(postcard::to_allocvec(msg).expect("file message serialises"));
    out
}

/// A data message for `xfer`; `buf` is reused between calls.
pub fn encode_data(xfer: u32, bytes: &[u8], buf: &mut Vec<u8>) {
    buf.clear();
    buf.push(TAG_DATA);
    buf.extend_from_slice(&xfer.to_le_bytes());
    buf.extend_from_slice(bytes);
}

pub enum Decoded<'a> {
    Msg(Msg),
    Data { xfer: u32, bytes: &'a [u8] },
}

pub fn decode(raw: &[u8]) -> Option<Decoded<'_>> {
    match *raw.first()? {
        TAG_MSG => postcard::from_bytes(&raw[1..]).ok().map(Decoded::Msg),
        TAG_DATA if raw.len() >= 5 => Some(Decoded::Data {
            xfer: u32::from_le_bytes(raw[1..5].try_into().unwrap()),
            bytes: &raw[5..],
        }),
        _ => None,
    }
}

/// True for a name that may be created on Windows as one path component:
/// not empty, no separators or reserved characters, not `.`/`..`, no
/// trailing dot or space (Windows drops those, so two names would collide),
/// and not a device name such as `CON` or `LPT1`.
pub fn valid_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 255 || name == "." || name == ".." {
        return false;
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return false;
    }
    if name.chars().any(|c| c.is_control() || matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|')) {
        return false;
    }
    let stem = name.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    !device
}

/// The parts of a transfer's relative path, or `None` if any part could step
/// outside the destination. This is what stops the other computer from
/// writing anywhere but the folder that was chosen.
pub fn rel_parts(rel: &str) -> Option<Vec<&str>> {
    let parts: Vec<&str> = rel.split('/').collect();
    if parts.is_empty() || parts.len() > 64 || !parts.iter().all(|p| valid_name(p)) {
        return None;
    }
    Some(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for ok in ["a", "report.final.docx", "Ünïcode ✓", ".gitignore", "com", "COM10", "console.txt"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", ".", "..", "a/b", "a\\b", "c:", "x?", "trail.", "trail ", "CON", "con.txt", "LPT1", "nul.tar.gz", "a\u{0}b"] {
            assert!(!valid_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn relative_paths_cannot_escape() {
        assert_eq!(rel_parts("a/b/c.txt"), Some(vec!["a", "b", "c.txt"]));
        for bad in ["", "/a", "a/", "a//b", "../x", "a/../x", "C:/x", "a\\..\\x", "\\\\server\\share"] {
            assert!(rel_parts(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn framing() {
        let m = Msg::Start { xfer: 7, rel: "a/b".into(), dir: false, size: 9, modified: 1 };
        match decode(&encode(&m)) {
            Some(Decoded::Msg(got)) => assert_eq!(got, m),
            _ => panic!("message"),
        }
        let mut buf = Vec::new();
        encode_data(3, b"bytes", &mut buf);
        match decode(&buf) {
            Some(Decoded::Data { xfer: 3, bytes: b"bytes" }) => {}
            _ => panic!("data"),
        }
        assert!(decode(&[]).is_none());
        assert!(decode(&[9, 1, 2]).is_none());
    }
}
