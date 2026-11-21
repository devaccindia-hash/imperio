//! File access between the two computers of a session.
//!
//! * [`proto`]: the messages, carried by the reliable stream of the link.
//! * [`ops`]: what is done to the local disk (list, new folder, rename, copy,
//!   move, delete to the Recycle Bin).
//! * [`peer`]: the engine on each side. The computer being viewed answers
//!   requests; the viewing computer makes them. Either can send or receive a
//!   tree of files.
//! * [`ipc`]: how the session window talks to the Files window, which is a
//!   separate process.

pub mod clip;
pub mod ipc;
pub mod ops;
pub mod peer;
pub mod proto;

pub use peer::{Event, Peer, Role};
pub use proto::{Entry, Op};
