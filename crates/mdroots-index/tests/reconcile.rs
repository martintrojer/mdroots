//! Reconcile against real files (StdFs) in tempdirs and a temp DB.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use mdroots_core::{Cancel, ErrorKind, FileSystem, FsKind, Meta, StdFs};
use mdroots_index::db::{FileRow, IndexDb};
use mdroots_index::reconcile::{ReconcileStats, reconcile};

type Contents = Vec<(String, Arc<[u8]>)>;

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    db_path: PathBuf,
    db: IndexDb,
}

fn fixture(files: &[(&str, &str)]) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    for (p, body) in files {
        write(&root, p, body);
    }
    let db_path = tmp.path().join("cache/r.v1.db");
    let db = IndexDb::open(&db_path).unwrap();
    Fixture {
        _tmp: tmp,
        root,
        db_path,
        db,
    }
}

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

/// Let the clock move past coarse ctime granularity before a change.
fn tick() {
    std::thread::sleep(Duration::from_millis(20));
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

impl Fixture {
    /// One reconciler run over `listed`.
    fn run(&mut self, listed: &[&str], force: &[&str]) -> (Contents, ReconcileStats) {
        let rows = self.db.rows().unwrap();
        let listed = strings(listed);
        reconcile(
            Some(&mut self.db),
            &rows,
            &StdFs,
            &self.root,
            Some(&listed),
            &strings(force),
            &Cancel::new(),
        )
        .unwrap()
    }

    fn row(&self, path: &str) -> Option<FileRow> {
        self.db.rows().unwrap().into_iter().find(|r| r.path == path)
    }

    fn change_log_len(&self) -> i64 {
        let c = rusqlite::Connection::open(&self.db_path).unwrap();
        c.query_row("SELECT COUNT(*) FROM change_log", [], |r| r.get(0))
            .unwrap()
    }
}

fn text(c: &Contents, path: &str) -> String {
    let (_, b) = c.iter().find(|(p, _)| p == path).unwrap();
    String::from_utf8(b.to_vec()).unwrap()
}

fn paths(c: &Contents) -> Vec<&str> {
    c.iter().map(|(p, _)| p.as_str()).collect()
}

const ABC: [&str; 3] = ["a.md", "b.md", "d/c.md"];

fn abc() -> Fixture {
    fixture(&[("a.md", "A"), ("b.md", "B"), ("d/c.md", "C")])
}

#[test]
fn first_run_reads_all_second_reuses_all() {
    let mut f = abc();
    let (c, s) = f.run(&ABC, &[]);
    assert_eq!(paths(&c), ABC);
    assert_eq!(text(&c, "d/c.md"), "C");
    assert_eq!(
        s,
        ReconcileStats {
            read: 3,
            reused: 0,
            removed: 0,
            batches: 1
        }
    );
    assert_eq!(f.db.rows().unwrap().len(), 3);

    let (c2, s2) = f.run(&ABC, &[]);
    assert_eq!(c2, c);
    assert_eq!(
        s2,
        ReconcileStats {
            read: 0,
            reused: 3,
            removed: 0,
            batches: 0
        }
    );
}

#[test]
fn force_read_rereads_an_unchanged_file() {
    let mut f = abc();
    f.run(&ABC, &[]);
    let (_, s) = f.run(&ABC, &["b.md"]);
    assert_eq!((s.read, s.reused, s.batches), (1, 2, 0));
}

#[test]
fn touch_rereads_one_and_updates_stat_only() {
    let mut f = abc();
    f.run(&ABC, &[]);
    let before = f.row("b.md").unwrap();
    let log = f.change_log_len();
    tick();
    let file = std::fs::File::options()
        .write(true)
        .open(f.root.join("b.md"))
        .unwrap();
    file.set_modified(SystemTime::now()).unwrap();
    drop(file);

    let (c, s) = f.run(&ABC, &[]);
    assert_eq!((s.read, s.reused, s.batches), (1, 2, 1));
    assert_eq!(text(&c, "b.md"), "B");
    let after = f.row("b.md").unwrap();
    assert_eq!(after.content, before.content);
    assert_ne!(after.ctime_ns, before.ctime_ns);
    // Stat columns only: no change_log entry for peers.
    assert_eq!(f.change_log_len(), log);
    // And the new stat is reused next time.
    assert_eq!(f.run(&ABC, &[]).1.reused, 3);
}

#[test]
fn mtime_regressing_copy_is_reread() {
    let mut f = abc();
    f.run(&ABC, &[]);
    let old = f.row("a.md").unwrap();
    let old_mtime = std::fs::metadata(f.root.join("a.md"))
        .unwrap()
        .modified()
        .unwrap();
    tick();
    // `cp -p` style: new content, then the mtime set back.
    write(&f.root, "a.md", "A, restored from a backup");
    let file = std::fs::File::options()
        .write(true)
        .open(f.root.join("a.md"))
        .unwrap();
    file.set_modified(old_mtime).unwrap();
    drop(file);
    assert_eq!(
        std::fs::metadata(f.root.join("a.md"))
            .unwrap()
            .modified()
            .unwrap(),
        old_mtime
    );

    let log = f.change_log_len();
    let (c, s) = f.run(&ABC, &[]);
    assert_eq!((s.read, s.reused), (1, 2));
    assert_eq!(text(&c, "a.md"), "A, restored from a backup");
    let new = f.row("a.md").unwrap();
    assert_eq!(&*new.content, b"A, restored from a backup");
    assert_eq!(new.mtime_ns, old.mtime_ns);
    assert_eq!(f.change_log_len(), log + 1);
}

#[test]
fn deleted_file_is_removed_from_db() {
    let mut f = abc();
    f.run(&ABC, &[]);
    std::fs::remove_file(f.root.join("b.md")).unwrap();
    // Still listed (a stale listing) and missing on disk.
    let (c, s) = f.run(&ABC, &[]);
    assert_eq!(paths(&c), ["a.md", "d/c.md"]);
    assert_eq!((s.removed, s.reused, s.batches), (1, 2, 1));
    assert!(f.row("b.md").is_none());
}

#[test]
fn unlisted_rows_invalid_paths_and_dirs_are_removed() {
    let mut f = abc();
    f.run(&ABC, &[]);
    std::fs::create_dir(f.root.join("dir.md")).unwrap();
    let (c, s) = f.run(&["a.md", "../b.md", "dir.md"], &[]);
    assert_eq!(paths(&c), ["a.md"]);
    assert_eq!(s.removed, 2); // b.md and d/c.md rows; no rows for the others
    assert_eq!(
        f.db.rows()
            .unwrap()
            .iter()
            .map(|r| r.path.as_str())
            .collect::<Vec<_>>(),
        ["a.md"]
    );
}

#[test]
fn peer_never_writes_but_sees_fresh_content() {
    let mut f = abc();
    f.run(&ABC, &[]);
    let observer = IndexDb::open(&f.db_path).unwrap();
    let version = observer.data_version().unwrap();
    let log = f.change_log_len();
    tick();
    write(&f.root, "a.md", "A changed");
    std::fs::remove_file(f.root.join("b.md")).unwrap();

    let rows = f.db.rows().unwrap();
    let (c, s) = reconcile(None, &rows, &StdFs, &f.root, None, &[], &Cancel::new()).unwrap();
    assert_eq!(paths(&c), ["a.md", "d/c.md"]);
    assert_eq!(text(&c, "a.md"), "A changed");
    assert_eq!(
        s,
        ReconcileStats {
            read: 1,
            reused: 1,
            removed: 1,
            batches: 0
        }
    );
    assert_eq!(observer.data_version().unwrap(), version);
    assert_eq!(f.change_log_len(), log);
    assert_eq!(&*f.row("a.md").unwrap().content, b"A");
    assert!(f.row("b.md").is_some());
}

fn many(n: usize) -> (Fixture, Vec<String>) {
    let f = fixture(&[]);
    let names: Vec<String> = (0..n).map(|i| format!("n/{i:04}.md")).collect();
    for (i, p) in names.iter().enumerate() {
        write(&f.root, p, &format!("note {i}"));
    }
    (f, names)
}

#[test]
fn thousand_files_commit_in_batches() {
    let (mut f, names) = many(1000);
    let rows = f.db.rows().unwrap();
    let (c, s) = reconcile(
        Some(&mut f.db),
        &rows,
        &StdFs,
        &f.root,
        Some(&names),
        &[],
        &Cancel::new(),
    )
    .unwrap();
    assert_eq!(c.len(), 1000);
    assert_eq!(s.read, 1000);
    assert!(s.batches >= 5, "{s:?}");
    assert_eq!(f.db.rows().unwrap().len(), 1000);
}

/// StdFs that cancels on its `cancel_at`-th read and counts reads.
struct CancelOnRead {
    cancel: Cancel,
    cancel_at: usize,
    reads: AtomicUsize,
}

impl FileSystem for CancelOnRead {
    fn read(&self, p: &Path) -> io::Result<(Arc<[u8]>, Meta)> {
        if self.reads.fetch_add(1, Ordering::SeqCst) + 1 == self.cancel_at {
            self.cancel.cancel();
        }
        StdFs.read(p)
    }
    fn stat(&self, p: &Path) -> io::Result<Meta> {
        StdFs.stat(p)
    }
    fn read_dir(&self, p: &Path) -> io::Result<Vec<(String, Meta)>> {
        StdFs.read_dir(p)
    }
    fn canonicalize(&self, p: &Path) -> io::Result<PathBuf> {
        StdFs.canonicalize(p)
    }
    fn case_sensitive(&self, dir: &Path) -> bool {
        StdFs.case_sensitive(dir)
    }
    fn fs_kind(&self, dir: &Path) -> FsKind {
        StdFs.fs_kind(dir)
    }
}

#[test]
fn cancel_midway_keeps_only_committed_batches() {
    let (mut f, names) = many(1000);
    let cancel = Cancel::new();
    let fs = CancelOnRead {
        cancel: cancel.clone(),
        cancel_at: 450,
        reads: AtomicUsize::new(0),
    };
    let err = reconcile(
        Some(&mut f.db),
        &[],
        &fs,
        &f.root,
        Some(&names),
        &[],
        &cancel,
    )
    .err()
    .unwrap();
    assert_eq!(err.kind(), ErrorKind::Cancelled);
    assert_eq!(fs.reads.load(Ordering::SeqCst), 450);
    let committed: Vec<String> = f.db.rows().unwrap().into_iter().map(|r| r.path).collect();
    // Batches fire every 200 files (400 rows here), or after 50 ms on a slow
    // machine; the batch holding the 450th file is never applied. The rows
    // are a prefix and equal the applied batches' entries in change_log.
    assert!(committed.len() < 450, "{}", committed.len());
    assert_eq!(committed[..], names[..committed.len()]);
    assert_eq!(f.change_log_len(), committed.len() as i64);
    // A fresh run resumes: committed rows are reused.
    let rows = f.db.rows().unwrap();
    let (_, s) = reconcile(
        Some(&mut f.db),
        &rows,
        &StdFs,
        &f.root,
        Some(&names),
        &[],
        &Cancel::new(),
    )
    .unwrap();
    assert_eq!(
        (s.reused, s.read),
        (committed.len(), 1000 - committed.len())
    );
}

#[test]
fn dataless_is_left_alone_unless_forced() {
    use mdroots_core::MemFs;
    let fs = MemFs::new()
        .with_file("a.md", "A")
        .with_file("cloud.md", "remote")
        .with_dataless("cloud.md");
    let listed = strings(&["a.md", "cloud.md"]);
    let run = |force: &[&str]| {
        reconcile(
            None,
            &[],
            &fs,
            Path::new("/"),
            Some(&listed),
            &strings(force),
            &Cancel::new(),
        )
        .unwrap()
    };
    let (c, s) = run(&[]);
    assert_eq!((paths(&c), s.read), (vec!["a.md"], 1));
    let (c, s) = run(&["cloud.md"]);
    assert_eq!((paths(&c), s.read), (vec!["a.md", "cloud.md"], 2));
}

/// MemFs whose stat fails with `stat_err` and whose reads report a new
/// ino each time (a file that keeps changing).
struct Flaky {
    inner: mdroots_core::MemFs,
    stat_err: Option<io::ErrorKind>,
    reads: AtomicUsize,
}

impl FileSystem for Flaky {
    fn read(&self, p: &Path) -> io::Result<(Arc<[u8]>, Meta)> {
        let n = self.reads.fetch_add(1, Ordering::SeqCst) as u64;
        let (b, mut m) = self.inner.read(p)?;
        m.ino = 1000 + n;
        Ok((b, m))
    }
    fn stat(&self, p: &Path) -> io::Result<Meta> {
        match self.stat_err {
            Some(k) => Err(io::Error::from(k)),
            None => self.inner.stat(p),
        }
    }
    fn read_dir(&self, p: &Path) -> io::Result<Vec<(String, Meta)>> {
        self.inner.read_dir(p)
    }
    fn canonicalize(&self, p: &Path) -> io::Result<PathBuf> {
        self.inner.canonicalize(p)
    }
    fn case_sensitive(&self, dir: &Path) -> bool {
        self.inner.case_sensitive(dir)
    }
    fn fs_kind(&self, dir: &Path) -> FsKind {
        self.inner.fs_kind(dir)
    }
}

#[test]
fn changing_file_is_retried_three_times_then_kept() {
    let mut f = fixture(&[]);
    let fs = Flaky {
        inner: mdroots_core::MemFs::new().with_file("a.md", "A"),
        stat_err: None,
        reads: AtomicUsize::new(0),
    };
    let listed = strings(&["a.md"]);
    let (c, s) = reconcile(
        Some(&mut f.db),
        &[],
        &fs,
        Path::new("/"),
        Some(&listed),
        &[],
        &Cancel::new(),
    )
    .unwrap();
    assert_eq!((paths(&c), s.read), (vec!["a.md"], 1));
    assert_eq!(fs.reads.load(Ordering::SeqCst), 4);
    assert_eq!(f.row("a.md").unwrap().ino, 1003);
}

#[test]
fn stat_error_other_than_not_found_keeps_the_row() {
    let mut f = abc();
    f.run(&ABC, &[]);
    let fs = Flaky {
        inner: mdroots_core::MemFs::new(),
        stat_err: Some(io::ErrorKind::PermissionDenied),
        reads: AtomicUsize::new(0),
    };
    let rows = f.db.rows().unwrap();
    let listed = strings(&ABC);
    let (c, s) = reconcile(
        Some(&mut f.db),
        &rows,
        &fs,
        &f.root,
        Some(&listed),
        &[],
        &Cancel::new(),
    )
    .unwrap();
    assert!(c.is_empty());
    assert_eq!(s, ReconcileStats::default());
    assert_eq!(f.db.rows().unwrap(), rows);
}

/// StdFs whose `read` fails with PermissionDenied (stat still works).
struct Unreadable;

impl FileSystem for Unreadable {
    fn read(&self, _: &Path) -> io::Result<(Arc<[u8]>, Meta)> {
        Err(io::Error::from(io::ErrorKind::PermissionDenied))
    }
    fn stat(&self, p: &Path) -> io::Result<Meta> {
        StdFs.stat(p)
    }
    fn read_dir(&self, p: &Path) -> io::Result<Vec<(String, Meta)>> {
        StdFs.read_dir(p)
    }
    fn canonicalize(&self, p: &Path) -> io::Result<PathBuf> {
        StdFs.canonicalize(p)
    }
    fn case_sensitive(&self, dir: &Path) -> bool {
        StdFs.case_sensitive(dir)
    }
    fn fs_kind(&self, dir: &Path) -> FsKind {
        StdFs.fs_kind(dir)
    }
}

#[test]
fn read_error_other_than_not_found_keeps_the_row() {
    let mut f = abc();
    f.run(&ABC, &[]);
    let rows = f.db.rows().unwrap();
    // Change a.md on disk so its stat differs and a read is attempted.
    std::fs::write(f.root.join("a.md"), "A changed").unwrap();
    let listed = strings(&ABC);
    let (c, s) = reconcile(
        Some(&mut f.db),
        &rows,
        &Unreadable,
        &f.root,
        Some(&listed),
        &[],
        &Cancel::new(),
    )
    .unwrap();
    assert_eq!(paths(&c), vec!["b.md", "d/c.md"]);
    assert_eq!((s.read, s.removed), (0, 0));
    assert_eq!(f.db.rows().unwrap(), rows);
}
