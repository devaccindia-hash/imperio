//! Session window <-> Files window.
//!
//! The Files window is its own process (the session window's event loop
//! cannot host a second window toolkit). The session window owns the link
//! and the file engine; the Files window sends it [`UiCmd`]s on its standard
//! output and reads [`UiEvent`]s from its standard input. Each is a frame:
//! `length u32 (LE) | postcard`.

use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::peer::Event;
use crate::proto::Op;

/// Transfers the session window starts by itself (files dropped on the
/// picture) are numbered from here, so they never clash with the Files
/// window's own.
pub const SESSION_XFER_BASE: u32 = 0x8000_0000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UiCmd {
    Hello,
    List { id: u32, path: String },
    Op { id: u32, op: Op },
    /// Local paths into a folder of the other computer.
    Upload { xfer: u32, paths: Vec<String>, dest: String },
    /// Paths of the other computer into a local folder.
    Download { xfer: u32, paths: Vec<String>, dest: String },
    Cancel { xfer: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UiEvent {
    Engine(Event),
    /// The session window started an upload on its own (a drop on the picture).
    Started { xfer: u32, label: String, dest: String },
    /// Bring the Files window to the front.
    Focus,
}

const MAX_FRAME: usize = 32 << 20;

pub fn write_frame<T: Serialize>(w: &mut impl Write, value: &T) -> io::Result<()> {
    let body = postcard::to_allocvec(value).map_err(io::Error::other)?;
    w.write_all(&(body.len() as u32).to_le_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

pub fn read_frame<T: DeserializeOwned>(r: &mut impl Read) -> io::Result<T> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::other("frame too large"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    postcard::from_bytes(&body).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_roundtrip() {
        let mut pipe = Vec::new();
        let a = UiCmd::Upload { xfer: 4, paths: vec!["C:\\a.txt".into()], dest: "D:\\in".into() };
        let b = UiCmd::Cancel { xfer: 4 };
        write_frame(&mut pipe, &a).unwrap();
        write_frame(&mut pipe, &b).unwrap();
        let mut r = pipe.as_slice();
        assert_eq!(read_frame::<UiCmd>(&mut r).unwrap(), a);
        assert_eq!(read_frame::<UiCmd>(&mut r).unwrap(), b);
        assert!(read_frame::<UiCmd>(&mut r).is_err());
    }
}
