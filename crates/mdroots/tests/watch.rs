//! The native watcher on real temp dirs (StdFs, StdProbe, a temp cache
//! dir). Every wait polls with a generous timeout; nothing sleeps a fixed
//! time. Roots live under tempfile's hidden `.tmp*` dirs, so these also
//! check that only root-relative components are filtered.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use mdroots::{Cancel, IndexMode, NoEnumerator, Options, Role, StdFs, StdProbe, Workspace};
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(5);

/// A marker root at `<tmp>/vault` and a temp cache dir.
struct Vault {
    _tmp: TempDir,
    cache: TempDir,
    dir: PathBuf,
}

impl Vault {
    fn new() -> Vault {
        let tmp = tempfile::tempdir().unwrap();
        let dir = fs::canonicalize(tmp.path()).unwrap().join("vault");
        fs::create_dir_all(dir.join(".zk")).unwrap();
        fs::write(dir.join("a.md"), "# A\n\n[[b]]\n").unwrap();
        Vault {
            _tmp: tmp,
            cache: tempfile::tempdir().unwrap(),
            dir,
        }
    }

    fn opts(&self) -> Options {
        Options::default()
            .fs(Arc::new(StdFs))
            .probe(Arc::new(StdProbe))
            .enumerator(Arc::new(NoEnumerator))
            .cache_dir(self.cache.path().to_path_buf())
    }

    fn open(&self, opts: Options) -> Workspace {
        Workspace::open_for(&self.dir.join("a.md"), opts).unwrap()
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }
}

/// Polls `f` until it holds or [`TIMEOUT`] passes.
fn eventually(what: &str, f: impl Fn() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(start.elapsed() < TIMEOUT, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Waits for a notification containing `p`.
fn notified(rx: &Receiver<Vec<PathBuf>>, p: &Path) {
    let start = Instant::now();
    loop {
        let left = TIMEOUT.saturating_sub(start.elapsed());
        let got = rx
            .recv_timeout(left)
            .unwrap_or_else(|_| panic!("no notification for {}", p.display()));
        if got.iter().any(|g| g == p) {
            return;
        }
    }
}

#[test]
fn a_watched_root_follows_creates_edits_and_deletes() {
    let v = Vault::new();
    let ws = v.open(v.opts().watch(true));
    assert_eq!(ws.role(), Some(Role::Reconciler));
    assert!(ws.watching());
    let rx = ws.subscribe();

    let b = v.path("b.md");
    fs::write(&b, "# B\n").unwrap();
    notified(&rx, &b);
    eventually("b.md indexed", || ws.files().contains(&b));
    assert!(
        ws.diagnostics(&v.path("a.md"), &Cancel::new())
            .unwrap()
            .is_empty()
    );

    fs::write(&b, "# B edited\n").unwrap();
    eventually("b.md re-read", || {
        ws.text(&b).is_ok_and(|t| t == "# B edited\n")
    });

    fs::remove_file(&b).unwrap();
    eventually("b.md removed", || !ws.files().contains(&b));
    // The DB followed too: a fresh peer sees the same files.
    let peer = v.open(v.opts());
    assert_eq!(peer.role(), Some(Role::Peer));
    assert_eq!(peer.files(), ws.files());
}

#[test]
fn a_new_directory_of_notes_is_picked_up_and_hidden_ones_are_not() {
    let v = Vault::new();
    let ws = v.open(v.opts().watch(true));
    assert!(ws.watching());
    let rx = ws.subscribe();
    fs::create_dir_all(v.path(".hidden")).unwrap();
    fs::write(v.path(".hidden/h.md"), "# H\n").unwrap();
    fs::create_dir_all(v.path("node_modules")).unwrap();
    fs::write(v.path("node_modules/n.md"), "# N\n").unwrap();
    fs::create_dir_all(v.path("sub/deep")).unwrap();
    fs::write(v.path("sub/deep/c.md"), "# C\n").unwrap();
    notified(&rx, &v.path("sub/deep/c.md"));
    assert_eq!(ws.files(), [v.path("a.md"), v.path("sub/deep/c.md")]);

    fs::remove_dir_all(v.path("sub")).unwrap();
    eventually("sub removed", || ws.files() == [v.path("a.md")]);
}

#[test]
fn refresh_paths_updates_only_the_given_notes() {
    let v = Vault::new();
    let ws = v.open(v.opts());
    assert!(!ws.watching());
    fs::write(v.path("b.md"), "# B\n").unwrap();
    fs::write(v.path("c.md"), "# C\n").unwrap();
    let changed = ws.refresh_paths(&[v.path("b.md")], &Cancel::new()).unwrap();
    assert_eq!(changed, [v.path("b.md")]);
    // c.md was not asked about, and a.md is still indexed.
    assert_eq!(ws.files(), [v.path("a.md"), v.path("b.md")]);
    // Nothing changed: nothing reported.
    assert!(
        ws.refresh_paths(&[v.path("b.md")], &Cancel::new())
            .unwrap()
            .is_empty()
    );
    // An overlay survives a point refresh of its note.
    ws.set_overlay(&v.path("b.md"), "# Overlay\n").unwrap();
    fs::write(v.path("b.md"), "# B2\n").unwrap();
    let changed = ws.refresh_paths(&[v.path("b.md")], &Cancel::new()).unwrap();
    assert_eq!(changed, [v.path("b.md")]);
    assert_eq!(ws.text(&v.path("b.md")).unwrap(), "# Overlay\n");
    // A directory path picks up the notes in it.
    let changed = ws
        .refresh_paths(std::slice::from_ref(&v.dir), &Cancel::new())
        .unwrap();
    assert_eq!(changed, [v.path("c.md")]);
    // A gone path drops the note.
    fs::remove_file(v.path("c.md")).unwrap();
    let changed = ws.refresh_paths(&[v.path("c.md")], &Cancel::new()).unwrap();
    assert_eq!(changed, [v.path("c.md")]);
    assert_eq!(ws.files(), [v.path("a.md"), v.path("b.md")]);
}

#[test]
fn peers_memory_mode_and_watch_off_do_not_watch() {
    let v = Vault::new();
    let ws = v.open(v.opts().watch(true));
    assert!(ws.watching());
    let peer = v.open(v.opts().watch(true));
    assert_eq!(peer.role(), Some(Role::Peer));
    assert!(!peer.watching());
    let mem = v.open(v.opts().index(IndexMode::Memory).watch(true));
    assert!(!mem.watching());
    drop(ws);
    // The reconciler is gone; watch(false) never watches even as one.
    let off = v.open(v.opts());
    assert_eq!(off.role(), Some(Role::Reconciler));
    assert!(!off.watching());
}

#[test]
fn dropping_the_last_clone_stops_the_watcher() {
    let v = Vault::new();
    let ws = v.open(v.opts().watch(true));
    let clone = ws.clone();
    assert!(clone.watching());
    drop(ws);
    drop(clone);
    // The lock is released, so a new workspace is the reconciler again
    // and starts its own watcher.
    let again = v.open(v.opts().watch(true));
    assert_eq!(again.role(), Some(Role::Reconciler));
    assert!(again.watching());
}
