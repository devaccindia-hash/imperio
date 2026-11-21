//! Two file engines joined by an in-memory wire: the viewing side browses,
//! uploads, downloads, changes and cancels; the viewed side obeys only while
//! its switch is on.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use imperio_files::{Event, Op, Peer, Role};
use imperio_transport::reliable::{Reliable, Wire};

struct Pipe(Mutex<mpsc::Sender<(bool, Vec<u8>)>>);

impl Wire for Pipe {
    fn send_stream(&self, ack: bool, body: &[u8]) -> std::io::Result<()> {
        let _ = self.0.lock().unwrap().send((ack, body.to_vec()));
        Ok(())
    }
}

fn carry(rx: mpsc::Receiver<(bool, Vec<u8>)>, dest: Arc<OnceLock<Reliable>>) {
    std::thread::spawn(move || {
        let mut n = 0u32;
        while let Ok((ack, body)) = rx.recv() {
            n += 1;
            if n.is_multiple_of(97) {
                continue; // lose about one datagram in a hundred
            }
            if let Some(d) = dest.get() {
                d.handle(ack, Bytes::from(body));
            }
        }
    });
}

struct Rig {
    viewer: Peer,
    events: mpsc::Receiver<Event>,
    allowed: Arc<AtomicBool>,
    dir: PathBuf,
    chans: (Reliable, Reliable),
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.chans.0.close();
        self.chans.1.close();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn rig(name: &str) -> Rig {
    let dir = std::env::temp_dir().join(format!("imperio-files-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("local")).unwrap();
    std::fs::create_dir_all(dir.join("remote")).unwrap();

    let (v_tx, v_rx) = mpsc::channel();
    let (h_tx, h_rx) = mpsc::channel();
    let (v_slot, h_slot) = (Arc::new(OnceLock::new()), Arc::new(OnceLock::new()));
    carry(v_rx, h_slot.clone());
    carry(h_rx, v_slot.clone());
    let (v_chan, v_in) = Reliable::start(Arc::new(Pipe(Mutex::new(v_tx))));
    let (h_chan, h_in) = Reliable::start(Arc::new(Pipe(Mutex::new(h_tx))));
    let _ = v_slot.set(v_chan.clone());
    let _ = h_slot.set(h_chan.clone());

    let allowed = Arc::new(AtomicBool::new(true));
    Peer::start(h_chan.clone(), h_in, Role::Host { allowed: allowed.clone() }, |_| {});
    let (ev_tx, events) = mpsc::channel();
    let ev_tx = Mutex::new(ev_tx);
    let viewer = Peer::start(v_chan.clone(), v_in, Role::Viewer, move |e| {
        let _ = ev_tx.lock().unwrap().send(e);
    });
    Rig { viewer, events, allowed, dir, chans: (v_chan, h_chan) }
}

impl Rig {
    fn s(&self, rel: &str) -> String {
        self.dir.join(rel).to_string_lossy().into_owned()
    }

    /// Wait for the event `pick` accepts, skipping progress and the rest.
    fn wait<T>(&self, mut pick: impl FnMut(Event) -> Option<T>) -> T {
        loop {
            let ev = self.events.recv_timeout(Duration::from_secs(30)).expect("an answer arrives");
            if let Some(t) = pick(ev) {
                return t;
            }
        }
    }

    fn finished(&self, want: u32) -> (Option<String>, u32, u64) {
        self.wait(|e| match e {
            Event::Finished { xfer, error, files, bytes } if xfer == want => Some((error, files, bytes)),
            _ => None,
        })
    }

    fn op(&self, id: u32, op: Op) -> Option<String> {
        self.viewer.op(id, op);
        self.wait(|e| match e {
            Event::OpDone { id: got, error } if got == id => Some(error),
            _ => None,
        })
    }

    fn names(&self, id: u32, path: String) -> Result<Vec<String>, String> {
        self.viewer.list(id, path);
        self.wait(|e| match e {
            Event::Listing { id: got, entries, error, .. } if got == id => Some(match error {
                Some(e) if entries.is_empty() => Err(e),
                _ => Ok(entries.into_iter().map(|e| e.name).collect()),
            }),
            _ => None,
        })
    }
}

fn pattern(len: usize, salt: u8) -> Vec<u8> {
    (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(salt)).collect()
}

fn make_tree(root: &Path) {
    std::fs::create_dir_all(root.join("docs").join("deep").join("deeper")).unwrap();
    std::fs::create_dir_all(root.join("docs").join("empty")).unwrap();
    std::fs::write(root.join("docs").join("a.txt"), b"alpha").unwrap();
    std::fs::write(root.join("docs").join("zero.bin"), b"").unwrap();
    std::fs::write(root.join("docs").join("deep").join("big.bin"), pattern(3_300_123, 1)).unwrap();
    std::fs::write(root.join("docs").join("deep").join("deeper").join("ünï ✓.dat"), pattern(70_000, 9)).unwrap();
    std::fs::write(root.join("single.bin"), pattern(600_001, 5)).unwrap();
}

fn same_tree(a: &Path, b: &Path) {
    let mut names: Vec<_> = std::fs::read_dir(a).unwrap().map(|e| e.unwrap().file_name()).collect();
    let mut other: Vec<_> = std::fs::read_dir(b).unwrap().map(|e| e.unwrap().file_name()).collect();
    names.sort();
    other.sort();
    assert_eq!(names, other, "contents of {} and {}", a.display(), b.display());
    for n in names {
        let (pa, pb) = (a.join(&n), b.join(&n));
        if pa.is_dir() {
            same_tree(&pa, &pb);
        } else {
            assert!(std::fs::read(&pa).unwrap() == std::fs::read(&pb).unwrap(), "{} differs", pa.display());
        }
    }
}

#[test]
fn hello_browse_and_change() {
    let r = rig("browse");
    r.viewer.hello();
    let allowed = r.wait(|e| match e {
        Event::Welcome { allowed, .. } => Some(allowed),
        _ => None,
    });
    assert!(allowed);

    assert_eq!(r.op(1, Op::NewFolder { parent: r.s("remote"), name: "made".into() }), None);
    std::fs::write(r.dir.join("remote").join("made").join("f.txt"), b"x").unwrap();
    assert_eq!(r.op(2, Op::Rename { path: r.s("remote/made"), name: "renamed".into() }), None);
    assert_eq!(r.op(3, Op::Copy { from: vec![r.s("remote/renamed")], to_dir: r.s("remote") }), None);
    assert_eq!(r.op(4, Op::NewFolder { parent: r.s("remote"), name: "box".into() }), None);
    assert_eq!(r.op(5, Op::Move { from: vec![r.s("remote/renamed (2)")], to_dir: r.s("remote/box") }), None);
    assert_eq!(r.names(6, r.s("remote")).unwrap(), ["box", "renamed"]);
    assert_eq!(r.names(7, r.s("remote/box/renamed (2)")).unwrap(), ["f.txt"]);
    assert!(r.op(8, Op::Rename { path: r.s("remote/box"), name: "a/b".into() }).is_some());
    assert!(r.names(9, r.s("remote/missing")).is_err());
    assert!(r.names(10, "relative\\path".into()).is_err());
    // The starting places list at least one drive.
    assert!(r.names(11, String::new()).unwrap().iter().any(|n| n.ends_with(":\\")));
}

#[test]
fn upload_then_download_a_tree() {
    let r = rig("tree");
    make_tree(&r.dir.join("local"));

    r.viewer.upload(1, vec![r.dir.join("local").join("docs"), r.dir.join("local").join("single.bin")], r.s("remote"));
    let (error, files, bytes) = r.finished(1);
    assert_eq!(error, None);
    assert_eq!((files, bytes), (5, 5 + 3_300_123 + 70_000 + 600_001));
    same_tree(&r.dir.join("local"), &r.dir.join("remote"));

    // Again: nothing is overwritten, the second copy is numbered.
    r.viewer.upload(2, vec![r.dir.join("local").join("docs"), r.dir.join("local").join("single.bin")], r.s("remote"));
    assert_eq!(r.finished(2).0, None);
    assert!(r.dir.join("remote").join("docs (2)").join("deep").join("big.bin").exists());
    assert!(r.dir.join("remote").join("single (2).bin").exists());

    std::fs::create_dir_all(r.dir.join("back")).unwrap();
    r.viewer.download(3, vec![r.s("remote/docs"), r.s("remote/single.bin")], r.dir.join("back"));
    let (error, files, _) = r.finished(3);
    assert_eq!((error, files), (None, 5));
    same_tree(&r.dir.join("local"), &r.dir.join("back"));
    // No half-written file is left anywhere.
    fn no_parts(dir: &Path) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            assert!(!e.file_name().to_string_lossy().ends_with(".imperio-part"), "{}", e.path().display());
            if e.path().is_dir() {
                no_parts(&e.path());
            }
        }
    }
    no_parts(&r.dir);
}

#[test]
fn switched_off_refuses_everything() {
    let r = rig("off");
    std::fs::write(r.dir.join("local").join("f.txt"), b"data").unwrap();
    std::fs::write(r.dir.join("remote").join("secret.txt"), b"secret").unwrap();
    r.allowed.store(false, Ordering::Relaxed);

    r.viewer.hello();
    assert!(!r.wait(|e| match e {
        Event::Welcome { allowed, .. } => Some(allowed),
        _ => None,
    }));
    assert!(r.names(1, r.s("remote")).is_err());
    assert!(r.op(2, Op::Delete { paths: vec![r.s("remote/secret.txt")] }).is_some());
    r.viewer.upload(3, vec![r.dir.join("local").join("f.txt")], r.s("remote"));
    assert!(r.finished(3).0.is_some());
    std::fs::create_dir_all(r.dir.join("back")).unwrap();
    r.viewer.download(4, vec![r.s("remote/secret.txt")], r.dir.join("back"));
    assert!(r.finished(4).0.is_some());
    assert!(r.dir.join("remote").join("secret.txt").exists());
    assert!(!r.dir.join("remote").join("f.txt").exists());
    assert_eq!(std::fs::read_dir(r.dir.join("back")).unwrap().count(), 0);
}

#[test]
fn problems_are_reported_not_fatal() {
    let r = rig("problems");
    std::fs::write(r.dir.join("local").join("ok.txt"), b"fine").unwrap();
    // One source is missing: the rest still arrives, with a note.
    r.viewer.upload(1, vec![r.dir.join("local").join("ok.txt"), r.dir.join("local").join("gone.txt")], r.s("remote"));
    let (error, files, _) = r.finished(1);
    assert!(error.is_some_and(|e| e.contains("gone.txt")));
    assert_eq!(files, 1);
    assert!(r.dir.join("remote").join("ok.txt").exists());
    // A destination that is not a folder is refused.
    r.viewer.upload(2, vec![r.dir.join("local").join("ok.txt")], r.s("remote/nowhere"));
    assert!(r.finished(2).0.is_some());
    r.viewer.download(3, vec![r.s("remote/ok.txt")], r.dir.join("nowhere"));
    assert!(r.finished(3).0.is_some());
}

#[test]
fn cancel_leaves_no_partial_file() {
    let r = rig("cancel");
    std::fs::write(r.dir.join("local").join("huge.bin"), pattern(120_000_000, 3)).unwrap();
    r.viewer.upload(1, vec![r.dir.join("local").join("huge.bin")], r.s("remote"));
    // Let it get going, then stop it.
    r.wait(|e| matches!(e, Event::Progress { xfer: 1, .. }).then_some(()));
    r.viewer.cancel(1);
    assert_eq!(r.finished(1).0.as_deref(), Some("Cancelled"));
    // The viewed side removes what it had written.
    for _ in 0..100 {
        if std::fs::read_dir(r.dir.join("remote")).unwrap().count() == 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("a partial file was left behind");
}
