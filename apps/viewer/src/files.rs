//! File access from the session window.
//!
//! The session window owns the link, so it owns the file engine. The Files
//! window is a separate process (`imperio files-window`) that this module
//! starts on demand and talks to over its standard input and output; see
//! `imperio_files::ipc`. Files dropped on the picture are sent to the other
//! computer's Desktop from here, with the Files window opened to show them.
//!
//! Copy and paste of files between the two computers also lives here:
//!
//! * Copy files on this computer, press Ctrl+V in the session window: the
//!   files are sent to the other computer's clipboard first, and the paste is
//!   passed on once they are there.
//! * Copy files on the other computer: it says so, the files (up to
//!   [`AUTO_FETCH_BYTES`]) are fetched into a staging folder here, and this
//!   computer's clipboard is pointed at them, ready to paste anywhere.

use std::io::BufReader;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use imperio_files::ipc::{self, UiCmd, UiEvent, SESSION_XFER_BASE};
use imperio_files::{clip, Event, Peer, Role};
use imperio_protocol::{ControlMessage, InputEvent};
use imperio_transport::link::SecureLink;
use imperio_transport::reliable::{Reliable, Wire};

/// Files copied on the other computer are fetched without being asked for
/// up to this size; larger ones are left to the Files window.
pub const AUTO_FETCH_BYTES: u64 = 200 * 1024 * 1024;

/// What the other computer said about itself and its files.
#[derive(Clone)]
struct Welcome {
    allowed: bool,
    computer: String,
    desktop: String,
}

struct Window {
    child: Child,
    stdin: ChildStdin,
}

/// Where the two clipboards stand. Sequence numbers are this computer's
/// (`clip::sequence`); 0 means "none".
#[derive(Default)]
struct ClipState {
    /// This computer's clipboard as set here from the other computer's files.
    from_remote: u32,
    /// This computer's clipboard whose files are already on the other one.
    uploaded: u32,
    /// This computer's clipboard at the moment the other computer's changed:
    /// the other one is newer, so a paste there is a plain paste.
    stale: u32,
    /// Ctrl+V upload in progress: (transfer, clipboard it carries).
    pasting: Option<(u32, u32)>,
    /// Fetch of the other computer's copied files in progress.
    fetching: Option<(u32, PathBuf)>,
}

struct Shared {
    link: Arc<SecureLink>,
    peer: OnceLock<Peer>,
    window: Mutex<Option<Window>>,
    welcome: Mutex<Option<Welcome>>,
    /// Uploads started by a drop while no Files window could be opened: their
    /// result is shown in a message box instead.
    announce: Mutex<Vec<u32>>,
    clip: Mutex<ClipState>,
    next_xfer: AtomicU32,
}

impl Shared {
    /// Pass an event to the Files window, if one is open.
    fn to_window(&self, ev: &UiEvent) {
        let mut slot = self.window.lock().unwrap();
        if let Some(w) = slot.as_mut() {
            if ipc::write_frame(&mut w.stdin, ev).is_err() {
                *slot = None; // it was closed
            }
        }
    }

    fn xfer(&self) -> u32 {
        self.next_xfer.fetch_add(1, Ordering::Relaxed)
    }

    /// The other computer's clipboard changed.
    fn remote_clip(&self, files: &[String], bytes: u64) {
        let Some(peer) = self.peer.get() else { return };
        // The lock is not held across calls into the engine: those can
        // report a finished transfer, which comes back here.
        let old = {
            let mut st = self.clip.lock().unwrap();
            st.stale = clip::sequence();
            st.uploaded = 0;
            st.fetching.take()
        };
        if let Some((old, dir)) = old {
            peer.cancel(old);
            let _ = std::fs::remove_dir_all(dir);
        }
        if files.is_empty() {
            return;
        }
        if bytes > AUTO_FETCH_BYTES {
            tracing::info!(count = files.len(), "files copied on the other computer are too large to fetch automatically");
            return;
        }
        let Ok(dir) = clip::new_staging_dir() else { return };
        let xfer = self.xfer();
        tracing::info!(count = files.len(), bytes, "fetching the files copied on the other computer");
        self.clip.lock().unwrap().fetching = Some((xfer, dir.clone()));
        peer.download(xfer, files.to_vec(), dir);
    }

    /// A transfer ended; if it was one of the clipboard's, finish that.
    fn clip_finished(&self, xfer: u32, ok: bool) {
        let mut st = self.clip.lock().unwrap();
        if st.fetching.as_ref().is_some_and(|(x, _)| *x == xfer) {
            let (_, dir) = st.fetching.take().expect("checked");
            if ok {
                if let Some(seq) = clip::write_files(&clip::staged_items(&dir)) {
                    st.from_remote = seq;
                    tracing::info!("the other computer's copied files are on this clipboard");
                }
            } else {
                let _ = std::fs::remove_dir_all(dir);
            }
        } else if st.pasting.is_some_and(|(x, _)| x == xfer) {
            let (_, seq) = st.pasting.take().expect("checked");
            if ok {
                st.uploaded = seq;
                drop(st);
                // The files are on the other clipboard: now the paste itself.
                for (scancode, down) in [(0x1D, true), (0x2F, true), (0x2F, false), (0x1D, false)] {
                    let key = InputEvent::Key { scancode, extended: false, down, unicode: None };
                    let _ = self.link.send_control(&ControlMessage::Input(key));
                }
            }
        }
    }
}

/// One command-line transfer.
pub enum Step {
    /// Send a local file or folder into a folder of the other computer.
    Push { local: PathBuf, remote_dir: String },
    /// Fetch a file or folder of the other computer into a local folder.
    Pull { remote: String, local_dir: PathBuf },
}

pub struct FileBridge {
    chan: Reliable,
    peer: Peer,
    shared: Arc<Shared>,
    /// This program can show the Files window (true in `imperio`, false in
    /// the bare `imperio-viewer`).
    has_window: bool,
    /// Keys go to the other computer (false in a view-only session).
    send_input: bool,
}

impl FileBridge {
    pub fn start(link: Arc<SecureLink>, has_window: bool, send_input: bool) -> Arc<Self> {
        let wire: Arc<dyn Wire> = link.clone();
        let (chan, inbox) = Reliable::start(wire);
        clip::clean_staging();
        let shared = Arc::new(Shared {
            link,
            peer: OnceLock::new(),
            window: Mutex::new(None),
            welcome: Mutex::new(None),
            announce: Mutex::new(Vec::new()),
            clip: Mutex::new(ClipState::default()),
            next_xfer: AtomicU32::new(SESSION_XFER_BASE),
        });
        let sh = shared.clone();
        let peer = Peer::start(chan.clone(), inbox, Role::Viewer, move |ev| {
            match &ev {
                Event::Welcome { allowed, computer, desktop, .. } => {
                    tracing::info!(allowed, computer = %computer, "file access: the other computer answered");
                    *sh.welcome.lock().unwrap() =
                        Some(Welcome { allowed: *allowed, computer: computer.clone(), desktop: desktop.clone() });
                }
                Event::RemoteClip { files, bytes } => sh.remote_clip(files, *bytes),
                Event::Finished { xfer, error, files, bytes } => {
                    tracing::info!(xfer, files, bytes, error = error.as_deref().unwrap_or("none"), "file transfer finished");
                    sh.clip_finished(*xfer, error.is_none());
                    let mut announce = sh.announce.lock().unwrap();
                    if let Some(i) = announce.iter().position(|x| x == xfer) {
                        announce.swap_remove(i);
                        let text = match error {
                            Some(e) => format!("The files were not all sent.\n{e}"),
                            None => format!("{files} file{} sent to the other computer\u{2019}s Desktop.", if *files == 1 { "" } else { "s" }),
                        };
                        // Off this thread: a message box must not hold up the engine.
                        let _ = std::thread::Builder::new().spawn(move || imperio_ui::info_box("Imperio files", &text));
                    }
                }
                _ => {}
            }
            sh.to_window(&UiEvent::Engine(ev));
        });
        let _ = shared.peer.set(peer.clone());
        // Ask once now; the answer says whether files are allowed and where
        // dropped files should go. Repeated by the stream until it arrives.
        peer.hello();
        Arc::new(Self { chan, peer, shared, has_window, send_input })
    }

    /// Ctrl+V was pressed in the session window. If this computer's
    /// clipboard holds files the other one does not have yet, send them to
    /// its clipboard and paste afterwards: returns true, and the key press
    /// must not be passed on. Otherwise false: an ordinary paste.
    pub fn paste_files(self: &Arc<Self>) -> bool {
        if !self.send_input {
            return false;
        }
        let seq = clip::sequence();
        let mut st = self.shared.clip.lock().unwrap();
        if st.pasting.is_some() {
            return true; // still sending; the paste follows by itself
        }
        if seq == st.from_remote || seq == st.uploaded || seq == st.stale {
            return false;
        }
        let files = clip::read_files();
        if files.is_empty() || files.iter().any(|f| clip::in_staging(f)) {
            return false;
        }
        if !self.shared.welcome.lock().unwrap().as_ref().is_some_and(|w| w.allowed) {
            return false;
        }
        let xfer = self.shared.xfer();
        st.pasting = Some((xfer, seq));
        drop(st);
        let label = match files.as_slice() {
            [one] => format!("Paste: {}", one.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
            many => format!("Paste: {} items", many.len()),
        };
        tracing::info!(count = files.len(), "sending copied files to the other computer's clipboard");
        self.shared.to_window(&UiEvent::Started { xfer, label: label.clone(), dest: String::new() });
        self.peer.upload_to_clipboard(xfer, files);
        // A paste that takes a while should be seen to be working.
        if self.has_window {
            let me = self.clone();
            let _ = std::thread::Builder::new().spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(1500));
                let still = me.shared.clip.lock().unwrap().pasting.is_some_and(|(x, _)| x == xfer);
                let open = me.shared.window.lock().unwrap().is_some();
                if still && !open {
                    me.open_window();
                    me.shared.to_window(&UiEvent::Started { xfer, label, dest: String::new() });
                }
            });
        }
        true
    }

    /// The stream that carries file traffic; the network thread feeds it.
    pub fn channel(&self) -> Reliable {
        self.chan.clone()
    }

    /// Session > Files: open the Files window, or bring it forward.
    pub fn open_window(&self) {
        if !self.has_window {
            imperio_ui::info_box(
                "Imperio files",
                "The Files window is part of Imperio.exe. Start the session from Imperio to browse files.\n\n\
                 You can still drop files onto this window to send them to the other computer\u{2019}s Desktop.",
            );
            return;
        }
        {
            let mut slot = self.shared.window.lock().unwrap();
            if let Some(w) = slot.as_mut() {
                if matches!(w.child.try_wait(), Ok(None)) && ipc::write_frame(&mut w.stdin, &UiEvent::Focus).is_ok() {
                    return;
                }
                *slot = None;
            }
        }
        if let Err(e) = self.spawn_window() {
            tracing::warn!(error = %e, "files window could not be started");
            imperio_ui::info_box("Imperio files", &format!("The Files window could not be opened.\n{e}"));
        }
    }

    fn spawn_window(&self) -> std::io::Result<()> {
        let exe = std::env::current_exe()?;
        let mut child = Command::new(exe)
            .arg("files-window")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| std::io::Error::other("no pipe"))?;
        let stdout = child.stdout.take().ok_or_else(|| std::io::Error::other("no pipe"))?;
        *self.shared.window.lock().unwrap() = Some(Window { child, stdin });

        // Commands from the window, until it closes.
        let peer = self.peer.clone();
        let shared = self.shared.clone();
        std::thread::Builder::new().name("files-window".into()).spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Ok(cmd) = ipc::read_frame::<UiCmd>(&mut reader) {
                match cmd {
                    UiCmd::Hello => peer.hello(),
                    UiCmd::List { id, path } => peer.list(id, path),
                    UiCmd::Op { id, op } => peer.op(id, op),
                    UiCmd::Upload { xfer, paths, dest } => {
                        peer.upload(xfer, paths.into_iter().map(PathBuf::from).collect(), dest)
                    }
                    UiCmd::Download { xfer, paths, dest } => peer.download(xfer, paths, PathBuf::from(dest)),
                    UiCmd::Cancel { xfer } => peer.cancel(xfer),
                }
            }
            // Closed: transfers keep running; reopening shows new ones.
            if let Some(mut w) = shared.window.lock().unwrap().take() {
                let _ = w.child.wait();
            }
        })?;
        Ok(())
    }

    /// Files were dropped on the picture: send them to the other computer's
    /// Desktop.
    pub fn send_dropped(&self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        let welcome = self.shared.welcome.lock().unwrap().clone();
        let Some(welcome) = welcome else {
            imperio_ui::info_box(
                "Imperio files",
                "The other computer has not answered about files. It needs Imperio 0.3.16 or newer; update it and connect again.",
            );
            return;
        };
        if !welcome.allowed {
            imperio_ui::info_box(
                "Imperio files",
                &format!(
                    "File access is turned off on {}.\nIt can be turned on there under Sharing settings \u{203a} Allow file access.",
                    welcome.computer
                ),
            );
            return;
        }
        let xfer = self.shared.xfer();
        let label = match paths.as_slice() {
            [one] => one.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| one.display().to_string()),
            many => format!("{} items", many.len()),
        };
        tracing::info!(count = paths.len(), dest = %welcome.desktop, "sending dropped files");
        if self.has_window {
            self.open_window();
            self.shared.to_window(&UiEvent::Started { xfer, label, dest: welcome.desktop.clone() });
        } else {
            self.shared.announce.lock().unwrap().push(xfer);
        }
        self.peer.upload(xfer, paths, welcome.desktop);
    }

    /// Command-line transfers (`--push`, `--pull`): run them once the other
    /// computer has answered. Results appear in the log.
    pub fn run_script(self: &Arc<Self>, steps: Vec<Step>) {
        if steps.is_empty() {
            return;
        }
        let me = self.clone();
        let _ = std::thread::Builder::new().name("files-script".into()).spawn(move || {
            let asked = std::time::Instant::now();
            while me.shared.welcome.lock().unwrap().is_none() {
                if asked.elapsed() > std::time::Duration::from_secs(15) {
                    tracing::warn!("file script: the other computer did not answer about files");
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            for step in steps {
                let xfer = me.shared.xfer();
                match step {
                    Step::Push { local, remote_dir } => me.peer.upload(xfer, vec![local], remote_dir),
                    Step::Pull { remote, local_dir } => me.peer.download(xfer, vec![remote], local_dir),
                }
            }
        });
    }

    /// The session is over: stop transfers and close the Files window.
    pub fn shutdown(&self) {
        self.chan.close();
        if let Some(mut w) = self.shared.window.lock().unwrap().take() {
            drop(w.stdin); // the window exits when its input closes
            std::thread::sleep(std::time::Duration::from_millis(300));
            let _ = w.child.kill();
            let _ = w.child.wait();
        }
    }
}
