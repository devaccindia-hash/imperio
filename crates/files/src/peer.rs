//! The file engine of one side of a session.
//!
//! The computer being viewed ([`Role::Host`]) answers requests: list a
//! folder, change something, send files, take files. The viewing computer
//! ([`Role::Viewer`]) makes those requests and never answers any, so the
//! computer you connect to cannot browse or change yours.
//!
//! Received files are written under the destination folder that was named,
//! and nowhere else (see [`crate::proto::rel_parts`]). Nothing is ever
//! overwritten: a name that is taken gets a number. A file is written under a
//! temporary name and given its real one only when it is complete.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use imperio_transport::reliable::{Inbox, Reliable};
use serde::{Deserialize, Serialize};

use crate::clip;
use crate::ops;
use crate::proto::{self, Decoded, Entry, Msg, Op, DATA_BYTES};

/// Added to a file's name while it is being received.
pub const PART_SUFFIX: &str = ".imperio-part";

const OFF: &str = "File access is turned off on that computer.";
const CANCELLED: &str = "Cancelled";

pub enum Role {
    /// The computer being viewed. `allowed` is its "Allow file access"
    /// switch, read on every request.
    Host { allowed: Arc<AtomicBool> },
    Viewer,
}

/// What the engine reports to the program around it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    Welcome { allowed: bool, computer: String, home: String, desktop: String },
    Listing { id: u32, path: String, parent: Option<String>, entries: Vec<Entry>, error: Option<String> },
    OpDone { id: u32, error: Option<String> },
    Progress { xfer: u32, name: String, files_done: u32, files_total: u32, bytes_done: u64, bytes_total: u64 },
    Finished { xfer: u32, error: Option<String>, files: u32, bytes: u64 },
    /// The other computer's clipboard changed. `files` are what was copied
    /// there, if it was files (`bytes` is their size, `u64::MAX` when too
    /// many to count).
    RemoteClip { files: Vec<String>, bytes: u64 },
}

type Events = Box<dyn Fn(Event) + Send + Sync>;

#[derive(Default)]
struct OutState {
    /// Set when the other side stopped it, with its reason.
    stopped: Option<String>,
    /// Everything was sent; only the receiver's closing word is awaited.
    done: bool,
    /// What had to be left out, kept until that closing word.
    note: Option<String>,
}

/// A transfer this side is sending.
struct Outgoing {
    cancel: AtomicBool,
    state: Mutex<OutState>,
    sent: Mutex<(u32, u64)>,
}

struct Current {
    file: File,
    part: PathBuf,
    fin: PathBuf,
    modified: u64,
}

/// A transfer this side is receiving.
struct Incoming {
    dest: PathBuf,
    /// Top-level names as sent -> as written (numbered when taken).
    tops: HashMap<String, String>,
    cur: Option<Current>,
    name: String,
    files: u32,
    bytes: u64,
    total_files: u32,
    total_bytes: u64,
    last_progress: Instant,
    /// Received into a staging folder; goes onto the clipboard when complete.
    to_clipboard: bool,
}

struct Inner {
    chan: Reliable,
    /// Clipboard sequence number this side set itself (not to be announced).
    own_clip: AtomicU32,
    /// `Some` on the viewed computer.
    allowed: Option<Arc<AtomicBool>>,
    events: Events,
    incoming: Mutex<HashMap<u32, Incoming>>,
    outgoing: Mutex<HashMap<u32, Arc<Outgoing>>>,
}

#[derive(Clone)]
pub struct Peer {
    inner: Arc<Inner>,
}

impl Peer {
    /// Start the engine on a reliable stream. It runs until the stream closes.
    pub fn start(chan: Reliable, inbox: Inbox, role: Role, events: impl Fn(Event) + Send + Sync + 'static) -> Peer {
        let inner = Arc::new(Inner {
            chan,
            allowed: match role {
                Role::Host { allowed } => Some(allowed),
                Role::Viewer => None,
            },
            events: Box::new(events),
            incoming: Mutex::new(HashMap::new()),
            outgoing: Mutex::new(HashMap::new()),
            own_clip: AtomicU32::new(0),
        });
        if inner.is_host() {
            clip::clean_staging();
            inner.clone().watch_clipboard();
        }
        let run = inner.clone();
        std::thread::Builder::new()
            .name("files".into())
            .spawn(move || {
                while let Some(raw) = inbox.recv() {
                    run.handle(&raw);
                }
                run.shutdown();
            })
            .expect("files thread");
        Peer { inner }
    }

    // ---- requests, made by the viewing computer --------------------------

    pub fn hello(&self) {
        self.inner.send(&Msg::Hello);
    }

    pub fn list(&self, id: u32, path: String) {
        self.inner.send(&Msg::List { id, path });
    }

    pub fn op(&self, id: u32, op: Op) {
        self.inner.send(&Msg::Op { id, op });
    }

    /// Send local `paths` into the folder `dest` of the other computer.
    pub fn upload(&self, xfer: u32, paths: Vec<PathBuf>, dest: String) {
        if !self.inner.send(&Msg::Offer { xfer, dest }) {
            self.inner.finished(xfer, Some("The connection is closed.".into()), 0, 0);
            return;
        }
        self.inner.clone().spawn_sender(xfer, paths);
    }

    /// Fetch `paths` of the other computer into the local folder `dest`.
    pub fn download(&self, xfer: u32, paths: Vec<String>, dest: PathBuf) {
        if !dest.is_dir() {
            self.inner.finished(xfer, Some("The destination folder does not exist.".into()), 0, 0);
            return;
        }
        self.inner.incoming.lock().unwrap().insert(xfer, Incoming::new(dest));
        if !self.inner.send(&Msg::Fetch { xfer, paths }) {
            self.inner.incoming.lock().unwrap().remove(&xfer);
            self.inner.finished(xfer, Some("The connection is closed.".into()), 0, 0);
        }
    }

    /// Send local `paths` to the other computer's clipboard, ready to paste.
    pub fn upload_to_clipboard(&self, xfer: u32, paths: Vec<PathBuf>) {
        self.upload(xfer, paths, clip::CLIPBOARD_DEST.to_string());
    }

    /// Stop a transfer in either direction.
    pub fn cancel(&self, xfer: u32) {
        let out = self.inner.outgoing.lock().unwrap().get(&xfer).cloned();
        if let Some(out) = out {
            out.cancel.store(true, Ordering::Relaxed); // its thread says so to the other side
            return;
        }
        let inc = self.inner.incoming.lock().unwrap().remove(&xfer);
        if let Some(mut inc) = inc {
            inc.discard_current();
            self.inner.send(&Msg::Stop { xfer, reason: CANCELLED.into() });
            self.inner.finished(xfer, Some(CANCELLED.into()), inc.files, inc.bytes);
        }
    }
}

impl Incoming {
    fn new(dest: PathBuf) -> Self {
        Self {
            dest,
            tops: HashMap::new(),
            cur: None,
            name: String::new(),
            files: 0,
            bytes: 0,
            total_files: 0,
            total_bytes: 0,
            last_progress: Instant::now(),
            to_clipboard: false,
        }
    }

    fn discard_current(&mut self) {
        if let Some(cur) = self.cur.take() {
            drop(cur.file);
            let _ = std::fs::remove_file(&cur.part);
        }
    }

    /// Begin the next file or folder. An error ends the whole transfer.
    fn start(&mut self, rel: &str, dir: bool, modified: u64) -> Result<(), String> {
        self.discard_current();
        let parts = proto::rel_parts(rel).ok_or("The other computer sent a file name that cannot be used here.")?;
        let top = match self.tops.get(parts[0]) {
            Some(t) => t.clone(),
            None => {
                let is_dir = dir || parts.len() > 1;
                let chosen = ops::unique(&self.dest, parts[0], is_dir);
                let name = chosen.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| parts[0].to_string());
                self.tops.insert(parts[0].to_string(), name.clone());
                name
            }
        };
        let mut path = self.dest.join(top);
        for p in &parts[1..] {
            path.push(p);
        }
        self.name = parts[parts.len() - 1].to_string();
        if dir {
            return std::fs::create_dir_all(&path).map_err(|e| format!("{}: {}", self.name, ops::explain(&e)));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {}", self.name, ops::explain(&e)))?;
        }
        let mut part = path.clone().into_os_string();
        part.push(PART_SUFFIX);
        let part = PathBuf::from(part);
        let file = File::create(&part).map_err(|e| format!("{}: {}", self.name, ops::explain(&e)))?;
        self.cur = Some(Current { file, part, fin: path, modified });
        Ok(())
    }

    fn data(&mut self, bytes: &[u8]) -> Result<(), String> {
        let Some(cur) = self.cur.as_mut() else { return Ok(()) };
        cur.file.write_all(bytes).map_err(|e| format!("{}: {}", self.name, ops::explain(&e)))?;
        self.bytes += bytes.len() as u64;
        Ok(())
    }

    fn end(&mut self) -> Result<(), String> {
        let Some(cur) = self.cur.take() else { return Ok(()) };
        let fail = |e: std::io::Error| format!("{}: {}", self.name, ops::explain(&e));
        if cur.modified > 0 {
            let _ = cur.file.set_modified(UNIX_EPOCH + Duration::from_secs(cur.modified));
        }
        drop(cur.file);
        std::fs::rename(&cur.part, &cur.fin).map_err(|e| {
            let _ = std::fs::remove_file(&cur.part);
            fail(e)
        })?;
        self.files += 1;
        Ok(())
    }
}

/// Counts for the progress line of a transfer being sent.
struct SendProgress {
    xfer: u32,
    files_total: u32,
    bytes_total: u64,
    files_done: u32,
    bytes_done: u64,
    last: Instant,
}

impl Inner {
    fn send(&self, msg: &Msg) -> bool {
        self.chan.send(&proto::encode(msg)).is_ok()
    }

    fn emit(&self, ev: Event) {
        (self.events)(ev);
    }

    fn finished(&self, xfer: u32, error: Option<String>, files: u32, bytes: u64) {
        self.emit(Event::Finished { xfer, error, files, bytes });
    }

    fn is_host(&self) -> bool {
        self.allowed.is_some()
    }

    fn host_allows(&self) -> bool {
        self.allowed.as_ref().is_some_and(|a| a.load(Ordering::Relaxed))
    }

    /// The stream closed: nothing half-received is left behind.
    fn shutdown(&self) {
        for (xfer, mut inc) in self.incoming.lock().unwrap().drain() {
            inc.discard_current();
            if !self.is_host() {
                self.finished(xfer, Some("The connection was lost.".into()), inc.files, inc.bytes);
            }
        }
        for out in self.outgoing.lock().unwrap().values() {
            out.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// On the viewed computer: tell the viewer whenever the clipboard
    /// changes, and which files were copied if it was files. What this side
    /// put there itself (the viewer's own files) is not announced.
    fn watch_clipboard(self: Arc<Self>) {
        spawn("files-clipboard", move || {
            let mut last = clip::sequence();
            while !self.chan.is_closed() {
                std::thread::sleep(Duration::from_millis(400));
                let seq = clip::sequence();
                if seq == last {
                    continue;
                }
                last = seq;
                if !self.host_allows() || seq == self.own_clip.load(Ordering::Relaxed) {
                    continue;
                }
                let mut files = clip::read_files();
                if files.iter().any(|f| clip::in_staging(f)) {
                    files.clear();
                }
                let bytes = clip_size(&files);
                let files = files.iter().map(|f| f.to_string_lossy().into_owned()).collect();
                self.send(&Msg::Clip { files, bytes });
            }
        });
    }

    fn handle(self: &Arc<Self>, raw: &[u8]) {
        match proto::decode(raw) {
            Some(Decoded::Data { xfer, bytes }) => self.receive(xfer, |inc| inc.data(bytes)),
            Some(Decoded::Msg(msg)) => self.handle_msg(msg),
            None => tracing::debug!("file message not understood"),
        }
    }

    fn handle_msg(self: &Arc<Self>, msg: Msg) {
        match msg {
            // ---- requests: only the viewed computer answers them ---------
            Msg::Hello if self.is_host() => {
                self.send(&Msg::Welcome {
                    allowed: self.host_allows(),
                    computer: ops::computer_name(),
                    home: ops::home().to_string_lossy().into_owned(),
                    desktop: ops::desktop().to_string_lossy().into_owned(),
                });
            }
            Msg::List { id, path } if self.is_host() => {
                if !self.host_allows() {
                    self.send(&Msg::Listing { id, path, parent: None, entries: Vec::new(), error: Some(OFF.into()) });
                    return;
                }
                let me = self.clone();
                spawn("files-list", move || {
                    let reply = match ops::list(&path) {
                        Ok(l) => Msg::Listing { id, path: l.path, parent: l.parent, entries: l.entries, error: l.note },
                        Err(e) => Msg::Listing { id, path, parent: None, entries: Vec::new(), error: Some(e) },
                    };
                    me.send(&reply);
                });
            }
            Msg::Op { id, op } if self.is_host() => {
                if !self.host_allows() {
                    self.send(&Msg::OpDone { id, error: Some(OFF.into()) });
                    return;
                }
                tracing::info!(?op, "file access: change requested by the viewer");
                let me = self.clone();
                spawn("files-op", move || {
                    let error = ops::run(&op).err();
                    if let Some(e) = &error {
                        tracing::info!(error = %e, "file access: change failed");
                    }
                    me.send(&Msg::OpDone { id, error });
                });
            }
            Msg::Fetch { xfer, paths } if self.is_host() => {
                if !self.host_allows() {
                    self.send(&Msg::Stop { xfer, reason: OFF.into() });
                    return;
                }
                tracing::info!(?paths, "file access: sending to the viewer");
                let mut sources = Vec::new();
                for p in &paths {
                    match ops::full_path(p) {
                        Ok(p) => sources.push(p),
                        Err(reason) => {
                            self.send(&Msg::Stop { xfer, reason });
                            return;
                        }
                    }
                }
                self.clone().spawn_sender(xfer, sources);
            }
            Msg::Offer { xfer, dest } if self.is_host() => {
                let refuse = if !self.host_allows() {
                    Some(OFF.to_string())
                } else {
                    let to_clipboard = dest == clip::CLIPBOARD_DEST;
                    let target = if to_clipboard {
                        clip::new_staging_dir().map_err(|e| ops::explain(&e))
                    } else {
                        ops::full_path(&dest)
                    };
                    match target {
                        Ok(dir) if dir.is_dir() => {
                            tracing::info!(dest = %dir.display(), "file access: receiving from the viewer");
                            let mut inc = Incoming::new(dir);
                            inc.to_clipboard = to_clipboard;
                            self.incoming.lock().unwrap().insert(xfer, inc);
                            None
                        }
                        Ok(_) => Some("The destination folder does not exist.".to_string()),
                        Err(e) => Some(e),
                    }
                };
                if let Some(reason) = refuse {
                    self.send(&Msg::Stop { xfer, reason });
                }
            }

            // ---- answers: only the viewing computer listens --------------
            Msg::Welcome { allowed, computer, home, desktop } if !self.is_host() => {
                self.emit(Event::Welcome { allowed, computer, home, desktop })
            }
            Msg::Listing { id, path, parent, entries, error } if !self.is_host() => {
                self.emit(Event::Listing { id, path, parent, entries, error })
            }
            Msg::OpDone { id, error } if !self.is_host() => self.emit(Event::OpDone { id, error }),
            Msg::Clip { files, bytes } if !self.is_host() => self.emit(Event::RemoteClip { files, bytes }),

            // ---- a transfer this side is receiving -----------------------
            Msg::Total { xfer, files, bytes } => self.receive(xfer, |inc| {
                inc.total_files = files;
                inc.total_bytes = bytes;
                Ok(())
            }),
            Msg::Start { xfer, rel, dir, modified, .. } => self.receive(xfer, |inc| inc.start(&rel, dir, modified)),
            Msg::End { xfer } => self.receive(xfer, |inc| inc.end()),
            Msg::Skip { xfer } => self.receive(xfer, |inc| {
                inc.discard_current();
                Ok(())
            }),
            Msg::Finish { xfer, error } => {
                let inc = self.incoming.lock().unwrap().remove(&xfer);
                if let Some(mut inc) = inc {
                    inc.discard_current();
                    if inc.to_clipboard && self.is_host() {
                        // Before the closing word: the viewer pastes on it.
                        match clip::write_files(&clip::staged_items(&inc.dest)) {
                            Some(seq) => {
                                self.own_clip.store(seq, Ordering::Relaxed);
                                tracing::info!(files = inc.files, "file access: the viewer's copied files are on the clipboard");
                            }
                            None => tracing::warn!("file access: the clipboard could not be set"),
                        }
                    }
                    self.send(&Msg::Done { xfer, error: None, files: inc.files, bytes: inc.bytes });
                    self.finished(xfer, error, inc.files, inc.bytes);
                }
            }

            // ---- a transfer this side is sending -------------------------
            Msg::Done { xfer, error, files, bytes } => {
                let out = self.outgoing.lock().unwrap().remove(&xfer);
                if let Some(out) = out {
                    let note = out.state.lock().unwrap().note.take();
                    self.finished(xfer, error.or(note), files, bytes);
                }
            }
            Msg::Stop { xfer, reason } => {
                let reason: String = reason.chars().filter(|c| !c.is_control()).take(300).collect();
                let inc = self.incoming.lock().unwrap().remove(&xfer);
                if let Some(mut inc) = inc {
                    inc.discard_current();
                    self.finished(xfer, Some(reason), inc.files, inc.bytes);
                    return;
                }
                let out = self.outgoing.lock().unwrap().get(&xfer).cloned();
                if let Some(out) = out {
                    let mut state = out.state.lock().unwrap();
                    state.stopped = Some(reason.clone());
                    let done = state.done;
                    drop(state);
                    out.cancel.store(true, Ordering::Relaxed);
                    if done {
                        // Its thread has finished; close it here.
                        self.outgoing.lock().unwrap().remove(&xfer);
                        let (files, bytes) = *out.sent.lock().unwrap();
                        self.finished(xfer, Some(reason), files, bytes);
                    }
                }
            }
            other => tracing::debug!(?other, "file message ignored"),
        }
    }

    /// Apply `f` to the transfer `xfer` being received. A failure ends the
    /// transfer and tells the sender why.
    fn receive(&self, xfer: u32, f: impl FnOnce(&mut Incoming) -> Result<(), String>) {
        let mut map = self.incoming.lock().unwrap();
        let Some(inc) = map.get_mut(&xfer) else { return };
        match f(inc) {
            Ok(()) => {
                if inc.last_progress.elapsed() >= Duration::from_millis(100) {
                    inc.last_progress = Instant::now();
                    let ev = Event::Progress {
                        xfer,
                        name: inc.name.clone(),
                        files_done: inc.files,
                        files_total: inc.total_files,
                        bytes_done: inc.bytes,
                        bytes_total: inc.total_bytes,
                    };
                    drop(map);
                    self.emit(ev);
                }
            }
            Err(reason) => {
                let mut inc = map.remove(&xfer).expect("present");
                drop(map);
                inc.discard_current();
                tracing::info!(%reason, "file transfer stopped while receiving");
                self.send(&Msg::Stop { xfer, reason: reason.clone() });
                self.finished(xfer, Some(reason), inc.files, inc.bytes);
            }
        }
    }

    fn spawn_sender(self: Arc<Self>, xfer: u32, paths: Vec<PathBuf>) {
        let out = Arc::new(Outgoing {
            cancel: AtomicBool::new(false),
            state: Mutex::new(OutState::default()),
            sent: Mutex::new((0, 0)),
        });
        self.outgoing.lock().unwrap().insert(xfer, out.clone());
        spawn("files-send", move || {
            let mut result = self.send_all(xfer, &paths, &out);
            {
                // Stopped by the other side in the last moment, or done.
                let mut state = out.state.lock().unwrap();
                match (&result, state.stopped.clone()) {
                    (Ok(_), Some(reason)) => result = Err(reason),
                    (Ok(note), None) => {
                        state.done = true;
                        state.note = note.clone();
                    }
                    _ => {}
                }
            }
            let (files, bytes) = *out.sent.lock().unwrap();
            match result {
                // Sent in full: the receiver's `Done` closes it.
                Ok(note) => {
                    if !self.send(&Msg::Finish { xfer, error: note }) {
                        self.outgoing.lock().unwrap().remove(&xfer);
                        self.finished(xfer, Some("The connection was lost.".into()), files, bytes);
                    }
                }
                Err(reason) => {
                    self.outgoing.lock().unwrap().remove(&xfer);
                    // Unless the other side is the one that stopped it, say so.
                    if out.state.lock().unwrap().stopped.is_none() {
                        self.send(&Msg::Stop { xfer, reason: reason.clone() });
                    }
                    self.finished(xfer, Some(reason), files, bytes);
                }
            }
        });
    }

    /// Send every path. `Ok(note)` when the end was reached (the note says
    /// what could not be read); `Err` when the transfer was stopped.
    fn send_all(&self, xfer: u32, paths: &[PathBuf], out: &Outgoing) -> Result<Option<String>, String> {
        let stopped = || -> Result<(), String> {
            if out.cancel.load(Ordering::Relaxed) {
                return Err(out.state.lock().unwrap().stopped.clone().unwrap_or_else(|| CANCELLED.into()));
            }
            if self.chan.is_closed() {
                return Err("The connection was lost.".into());
            }
            Ok(())
        };
        // Count first, so the other side can show how far along it is.
        let (mut files_total, mut bytes_total) = (0u32, 0u64);
        for p in paths {
            measure(p, &mut files_total, &mut bytes_total, &stopped)?;
        }
        self.send(&Msg::Total { xfer, files: files_total, bytes: bytes_total });

        let mut progress = SendProgress { xfer, files_total, bytes_total, files_done: 0, bytes_done: 0, last: Instant::now() };
        let mut problems: Vec<String> = Vec::new();
        let mut used = HashSet::new();
        let mut buf = Vec::with_capacity(DATA_BYTES + 8);
        let mut chunk = vec![0u8; DATA_BYTES];
        for p in paths {
            let Some(name) = p.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                problems.push(format!("{}: a whole drive cannot be sent; open it and choose what is inside.", p.display()));
                continue;
            };
            // Two sources with the same name must not land on each other.
            let mut top = name.clone();
            let mut n = 2;
            while !used.insert(top.to_lowercase()) {
                top = format!("{name} ({n})");
                n += 1;
            }
            self.send_path(p, &top, out, &mut progress, &mut problems, &mut buf, &mut chunk, &stopped)?;
        }
        *out.sent.lock().unwrap() = (progress.files_done, progress.bytes_done);
        Ok(match problems.len() {
            0 => None,
            1 => Some(format!("Left out: {}", problems[0])),
            n => Some(format!("{n} items were left out. The first: {}", problems[0])),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn send_path(
        &self,
        path: &Path,
        rel: &str,
        out: &Outgoing,
        progress: &mut SendProgress,
        problems: &mut Vec<String>,
        buf: &mut Vec<u8>,
        chunk: &mut [u8],
        stopped: &dyn Fn() -> Result<(), String>,
    ) -> Result<(), String> {
        stopped()?;
        let xfer = progress.xfer;
        let name = rel.rsplit('/').next().unwrap_or(rel).to_string();
        let meta = match std::fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) => {
                problems.push(format!("{name}: {}", ops::explain(&e)));
                return Ok(());
            }
        };
        if meta.file_type().is_symlink() {
            return Ok(()); // links and junctions are not followed
        }
        if !proto::valid_name(&name) {
            problems.push(format!("{name}: the name cannot be used on the other computer."));
            return Ok(());
        }
        let modified = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
        if meta.is_dir() {
            self.send(&Msg::Start { xfer, rel: rel.to_string(), dir: true, size: 0, modified });
            let items = match std::fs::read_dir(path) {
                Ok(i) => i,
                Err(e) => {
                    problems.push(format!("{name}: {}", ops::explain(&e)));
                    return Ok(());
                }
            };
            for item in items.flatten() {
                let child = format!("{rel}/{}", item.file_name().to_string_lossy());
                self.send_path(&item.path(), &child, out, progress, problems, buf, chunk, stopped)?;
            }
            return Ok(());
        }
        let mut file = match File::open(path) {
            Ok(f) => f,
            Err(e) => {
                problems.push(format!("{name}: {}", ops::explain(&e)));
                return Ok(());
            }
        };
        self.send(&Msg::Start { xfer, rel: rel.to_string(), dir: false, size: meta.len(), modified });
        loop {
            stopped()?;
            let n = match file.read(chunk) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    problems.push(format!("{name}: {}", ops::explain(&e)));
                    self.send(&Msg::Skip { xfer });
                    return Ok(());
                }
            };
            proto::encode_data(xfer, &chunk[..n], buf);
            if self.chan.send(buf).is_err() {
                return Err("The connection was lost.".into());
            }
            progress.bytes_done += n as u64;
            if progress.last.elapsed() >= Duration::from_millis(100) {
                progress.last = Instant::now();
                *out.sent.lock().unwrap() = (progress.files_done, progress.bytes_done);
                self.emit(Event::Progress {
                    xfer,
                    name: name.clone(),
                    files_done: progress.files_done,
                    files_total: progress.files_total,
                    bytes_done: progress.bytes_done,
                    // A file that grew while being read must not show 104 %.
                    bytes_total: progress.bytes_total.max(progress.bytes_done),
                });
            }
        }
        self.send(&Msg::End { xfer });
        progress.files_done += 1;
        Ok(())
    }
}

/// Size of what was copied, giving up (`u64::MAX`) on a very large tree so a
/// copied drive is not walked end to end.
fn clip_size(paths: &[PathBuf]) -> u64 {
    fn walk(path: &Path, bytes: &mut u64, left: &mut u32) -> bool {
        if *left == 0 {
            return false;
        }
        *left -= 1;
        let Ok(meta) = std::fs::symlink_metadata(path) else { return true };
        if meta.file_type().is_symlink() {
            return true;
        }
        if meta.is_dir() {
            if let Ok(items) = std::fs::read_dir(path) {
                for item in items.flatten() {
                    if !walk(&item.path(), bytes, left) {
                        return false;
                    }
                }
            }
        } else {
            *bytes += meta.len();
        }
        true
    }
    let (mut bytes, mut left) = (0u64, 20_000u32);
    for p in paths {
        if !walk(p, &mut bytes, &mut left) {
            return u64::MAX;
        }
    }
    bytes
}

/// Add up the files under `path` (links are not followed).
fn measure(path: &Path, files: &mut u32, bytes: &mut u64, stopped: &dyn Fn() -> Result<(), String>) -> Result<(), String> {
    stopped()?;
    let Ok(meta) = std::fs::symlink_metadata(path) else { return Ok(()) };
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    if meta.is_dir() {
        if let Ok(items) = std::fs::read_dir(path) {
            for item in items.flatten() {
                measure(&item.path(), files, bytes, stopped)?;
            }
        }
    } else {
        *files += 1;
        *bytes += meta.len();
    }
    Ok(())
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) {
    let _ = std::thread::Builder::new().name(name.into()).spawn(f);
}
