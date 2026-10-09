//! Many-process fixtures of docs/specs/roots.md §7 (12–18): real `mdroots`
//! processes on a copy of tests/corpus/zk-min in a temp dir, with
//! `MDROOTS_CACHE_DIR` (and `HOME`, `XDG_CACHE_HOME`) pointed into it, so
//! the real cache dir is never used.
//!
//! The hidden `mdroots __open PATH --hold-ms N [--refresh-every MS]` opens
//! a workspace, prints `role: ...`, `files: N` and `db: PATH`, keeps it
//! open for N ms, and prints `files:` and `db:` again after each refresh.
//! The hidden `mdroots __gc --now-ms N --force` runs cache GC and prints
//! `deleted: PATH` / `skipped: PATH` lines.
//!
//! Not here: 19 (FSEvents replay / takeover diff after the reconciler
//! dies) needs FSEvents replay, deferred.

use std::fs;
use std::io::{BufRead, BufReader, Lines};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime};

use tempfile::TempDir;

/// A temp dir with the vault at `<tmp>/vault` and the cache at `<tmp>/cache`.
struct Env {
    _tmp: TempDir,
    canon: PathBuf,
}

impl Env {
    fn new() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let canon = fs::canonicalize(tmp.path()).unwrap();
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus/zk-min");
        copy_dir(&src, &canon.join("vault"));
        Env { _tmp: tmp, canon }
    }

    fn vault(&self) -> PathBuf {
        self.canon.join("vault")
    }

    fn note(&self) -> PathBuf {
        self.vault().join("a.md")
    }

    fn cache(&self) -> PathBuf {
        self.canon.join("cache")
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_mdroots"));
        c.args(args)
            .current_dir(self.vault())
            .env("MDROOTS_CACHE_DIR", self.cache())
            .env("XDG_CACHE_HOME", self.canon.join("xdg"))
            .env("HOME", self.canon.join("home"));
        c
    }

    /// `__open` that holds for `hold_ms`, stdout piped.
    fn spawn_open(&self, hold_ms: u64) -> Child {
        let note = self.note();
        self.cmd(&[
            "__open",
            note.to_str().unwrap(),
            "--hold-ms",
            &hold_ms.to_string(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
    }

    /// `__open` with no hold, run to completion.
    fn open_now(&self) -> Opened {
        let out = self.spawn_open(0).wait_with_output().unwrap();
        Opened::from_output(&out)
    }
}

#[derive(Debug)]
struct Opened {
    role: String,
    files: usize,
}

impl Opened {
    fn from_output(out: &Output) -> Opened {
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{:?}\n{stdout}\n{stderr}", out.status);
        Opened::parse(&stdout)
    }

    fn parse(stdout: &str) -> Opened {
        let field = |k: &str| {
            stdout
                .lines()
                .find_map(|l| l.strip_prefix(k))
                .unwrap_or_else(|| panic!("no {k} in {stdout:?}"))
                .to_owned()
        };
        Opened {
            role: field("role: "),
            files: field("files: ").parse().unwrap(),
        }
    }
}

/// Read a held child's three lines (it then sleeps).
fn read_opened(child: &mut Child) -> Opened {
    let mut r = BufReader::new(child.stdout.take().unwrap());
    let mut s = String::new();
    for _ in 0..3 {
        assert!(r.read_line(&mut s).unwrap() > 0, "early EOF: {s:?}");
    }
    Opened::parse(&s)
}

fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for e in fs::read_dir(src).unwrap() {
        let e = e.unwrap();
        let to = dst.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &to);
        } else {
            fs::copy(e.path(), to).unwrap();
        }
    }
}

fn signal(child: &Child, sig: &str) {
    let ok = Command::new("kill")
        .args([sig, &child.id().to_string()])
        .status()
        .unwrap()
        .success();
    assert!(ok, "kill {sig}");
}

/// Long enough that every process of a herd is still open when the last
/// one starts.
const HOLD_MS: u64 = 3_000;

/// 12 (adapted): with an existing DB, 10 processes at once: exactly one
/// reconciler, all see the same files, none errors (no `SQLITE_BUSY`).
#[test]
fn fixture_12_ten_processes_on_an_existing_db() {
    let env = Env::new();
    let first = env.open_now();
    assert_eq!(first.role, "reconciler");
    let herd: Vec<Child> = (0..10).map(|_| env.spawn_open(HOLD_MS)).collect();
    let outs: Vec<Opened> = herd
        .into_iter()
        .map(|c| Opened::from_output(&c.wait_with_output().unwrap()))
        .collect();
    let reconcilers = outs.iter().filter(|o| o.role == "reconciler").count();
    assert_eq!(reconcilers, 1, "{outs:?}");
    assert!(outs.iter().all(|o| o.role != "memory"), "{outs:?}");
    assert!(outs.iter().all(|o| o.files == first.files), "{outs:?}");
}

/// 13 (adapted): cold (no registry, no DB), 10 processes on one marker root
/// at once: one registry row for the root, no overlapping rows.
#[test]
fn fixture_13_cold_herd_registers_the_root_once() {
    let env = Env::new();
    let herd: Vec<Child> = (0..10).map(|_| env.spawn_open(HOLD_MS)).collect();
    let outs: Vec<Opened> = herd
        .into_iter()
        .map(|c| Opened::from_output(&c.wait_with_output().unwrap()))
        .collect();
    let reconcilers = outs.iter().filter(|o| o.role == "reconciler").count();
    assert_eq!(reconcilers, 1, "{outs:?}");
    let reg = rusqlite::Connection::open(env.cache().join("roots.v1.db")).unwrap();
    let mut q = reg.prepare("SELECT path FROM roots").unwrap();
    let paths: Vec<String> = q
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(paths, [env.vault().display().to_string()]);
}

/// 16: delete the cache dir while a process holds it open; another process
/// opens fine and rebuilds (reconciler), and the holder exits 0.
#[test]
fn fixture_16_cache_dir_removed_under_a_holder() {
    let env = Env::new();
    let mut holder = env.spawn_open(HOLD_MS);
    let held = read_opened(&mut holder);
    assert_eq!(held.role, "reconciler");
    fs::remove_dir_all(env.cache()).unwrap();
    let next = env.open_now();
    assert_eq!(next.role, "reconciler");
    assert_eq!(next.files, held.files);
    assert!(env.cache().join("roots.v1.db").exists());
    let status = holder.wait().unwrap();
    assert!(status.success(), "{status:?}");
}

/// 17: an mtime-regressing copy (new content, older mtime restored): the
/// next open sees the new content (ctime advanced).
#[test]
fn fixture_17_mtime_regressing_copy_is_reindexed() {
    let env = Env::new();
    let c = env.vault().join("c.md");
    fs::write(&c, "# C\n\nSee [A](a).\n").unwrap();
    let old = SystemTime::now() - Duration::from_secs(86_400);
    let set_old = |p: &Path| {
        let f = fs::File::options().write(true).open(p).unwrap();
        f.set_modified(old).unwrap();
    };
    set_old(&c);
    let check = || {
        let out = env.cmd(&["check", "c.md"]).output().unwrap();
        (
            out.status.code().unwrap(),
            String::from_utf8(out.stdout).unwrap(),
        )
    };
    assert_eq!(check(), (0, String::new()));
    assert_eq!(env.open_now().role, "reconciler", "the DB exists");
    // Same size, same (older) mtime, different target.
    fs::write(&c, "# C\n\nSee [A](z).\n").unwrap();
    set_old(&c);
    // zk-min rates a broken link a hint, so check still exits 0.
    let (_, stdout) = check();
    assert_eq!(stdout, "c.md:3:5: hint: broken link: z\n");
}

/// 18 (adapted, no watcher): SIGSTOP the reconciler; another process still
/// opens and answers from the DB as a peer within 5 s. After SIGCONT and
/// SIGKILL, a third process becomes the reconciler.
#[test]
fn fixture_18_stopped_then_killed_reconciler() {
    let env = Env::new();
    let mut rec = env.spawn_open(60_000);
    assert_eq!(read_opened(&mut rec).role, "reconciler");
    signal(&rec, "-STOP");
    let start = Instant::now();
    let peer = env.open_now();
    let took = start.elapsed();
    signal(&rec, "-CONT");
    assert_eq!(peer.role, "peer");
    assert!(took < Duration::from_secs(5), "{took:?}");
    rec.kill().unwrap();
    rec.wait().unwrap();
    let third = env.open_now();
    assert_eq!(third.role, "reconciler");
    assert_eq!(third.files, peer.files);
}

const DAY_MS: u64 = 24 * 3_600_000;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// A process refreshing every 100 ms: its stdout as `files:`/`db:` lines.
struct Refresher {
    child: Child,
    lines: Lines<BufReader<ChildStdout>>,
    role: String,
    files: usize,
    db: String,
}

impl Refresher {
    fn spawn(env: &Env, hold_ms: u64) -> Refresher {
        let note = env.note();
        let mut child = env
            .cmd(&[
                "__open",
                note.to_str().unwrap(),
                "--hold-ms",
                &hold_ms.to_string(),
                "--refresh-every",
                "100",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut next = || lines.next().expect("early EOF").unwrap();
        let role = next().strip_prefix("role: ").unwrap().to_owned();
        let files = next().strip_prefix("files: ").unwrap().parse().unwrap();
        let db = next().strip_prefix("db: ").unwrap().to_owned();
        Refresher {
            child,
            lines,
            role,
            files,
            db,
        }
    }

    /// The next refresh's file count and DB file.
    fn next_refresh(&mut self) -> (usize, String) {
        let mut next = || self.lines.next().expect("early EOF").unwrap();
        let files = next().strip_prefix("files: ").unwrap().parse().unwrap();
        let db = next().strip_prefix("db: ").unwrap().to_owned();
        (files, db)
    }

    /// Wait for exit; it must succeed.
    fn finish(mut self) {
        // Drain stdout so the child never blocks on a full pipe.
        for l in self.lines.by_ref() {
            l.unwrap();
        }
        let out = self.child.wait_with_output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{:?}\n{stderr}", out.status);
    }
}

/// `__gc --now-ms N --force`: its deleted and skipped paths.
fn run_gc(env: &Env, now_ms: u64) -> (Vec<String>, Vec<String>) {
    let out = env
        .cmd(&["__gc", "--now-ms", &now_ms.to_string(), "--force"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let pick = |k: &str| {
        stdout
            .lines()
            .filter_map(|l| l.strip_prefix(k).map(str::to_owned))
            .collect::<Vec<_>>()
    };
    (pick("deleted: "), pick("skipped: "))
}

fn integrity_ok(db: &Path) {
    let conn = rusqlite::Connection::open(db).unwrap();
    let r: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(r, "ok");
}

/// The registry's `db_file` for the single root.
fn recorded_db(env: &Env) -> String {
    let reg = rusqlite::Connection::open(env.cache().join("roots.v1.db")).unwrap();
    reg.query_row("SELECT db_file FROM roots", [], |r| r.get(0))
        .unwrap()
}

/// 14: forced GC while 3 processes hold an aged root open skips it, and
/// they keep answering from an intact DB; once they exit, GC deletes it.
#[test]
fn fixture_14_gc_skips_an_aged_root_held_by_three_processes() {
    let env = Env::new();
    let first = env.open_now();
    assert_eq!(first.role, "reconciler");
    let mut held: Vec<Refresher> = (0..3).map(|_| Refresher::spawn(&env, HOLD_MS)).collect();
    let db = PathBuf::from(&held[0].db);
    assert!(held.iter().all(|h| h.db == held[0].db), "one DB file");
    assert!(held.iter().all(|h| h.role != "memory"));
    // Age the root: last seen at the epoch. The holders' refreshes stamp
    // it again with the real time, so GC runs 40 days in the future.
    let reg = rusqlite::Connection::open(env.cache().join("roots.v1.db")).unwrap();
    reg.execute("UPDATE roots SET last_seen_ms = 1", [])
        .unwrap();
    let now = now_ms() + 40 * DAY_MS;

    let (deleted, skipped) = run_gc(&env, now);
    assert!(deleted.is_empty(), "{deleted:?}");
    assert!(skipped.contains(&db.display().to_string()), "{skipped:?}");
    for h in &mut held {
        let (files, at) = h.next_refresh();
        assert_eq!((files, at.as_str()), (first.files, db.to_str().unwrap()));
    }
    integrity_ok(&db);

    for h in held {
        h.finish();
    }
    reg.execute("UPDATE roots SET last_seen_ms = 1", [])
        .unwrap();
    let (deleted, skipped) = run_gc(&env, now);
    assert!(skipped.is_empty(), "{skipped:?}");
    assert!(deleted.contains(&db.display().to_string()), "{deleted:?}");
    assert!(!db.exists());
    let rows: i64 = reg
        .query_row("SELECT COUNT(*) FROM roots", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 0, "the aged root's registry row goes with it");
}

/// 15: stop all processes, corrupt the DB, start 3 new ones: one rebuilds
/// into a new generation file and all answer; the corrupt file stays while
/// they run and is deleted by a GC after they exit.
#[test]
fn fixture_15_corrupt_db_is_rebuilt_under_three_processes() {
    let env = Env::new();
    let first: Vec<Child> = (0..3).map(|_| env.spawn_open(60_000)).collect();
    let old = {
        let mut c = env.spawn_open(0);
        let o = read_opened_db(&mut c);
        c.wait().unwrap();
        o
    };
    // The holders may still be indexing (the reconciler writes in batches):
    // wait until the DB holds every note before stopping them, so the file
    // to corrupt has pages to corrupt.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let rows: i64 = rusqlite::Connection::open(&old)
            .and_then(|c| c.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0)))
            .unwrap_or(0);
        if rows > 0 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the DB was never filled"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    for mut c in first {
        c.kill().unwrap();
        c.wait().unwrap();
    }
    {
        let conn = rusqlite::Connection::open(&old).unwrap();
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .unwrap();
    }
    let mut bytes = fs::read(&old).unwrap();
    assert!(bytes.len() >= 100, "DB file has {} bytes", bytes.len());
    bytes[..100].fill(0xa5);
    fs::write(&old, bytes).unwrap();

    let mut herd: Vec<Refresher> = (0..3).map(|_| Refresher::spawn(&env, HOLD_MS)).collect();
    let reconcilers = herd.iter().filter(|h| h.role == "reconciler").count();
    assert_eq!(
        reconcilers,
        1,
        "{:?}",
        herd.iter().map(|h| &h.role).collect::<Vec<_>>()
    );
    let rec = herd.iter().find(|h| h.role == "reconciler").unwrap();
    let new = PathBuf::from(&rec.db);
    let files = rec.files;
    assert!(files > 0);
    let stem = old.file_stem().unwrap().to_str().unwrap().to_owned();
    let new_name = new.file_name().unwrap().to_str().unwrap().to_owned();
    assert!(
        new_name.starts_with(&format!("{stem}-")) && new_name.len() == stem.len() + 12,
        "{new_name}"
    );
    // Every process answers; peers that opened before the rebuild was
    // recorded reopen it on a refresh.
    for h in &mut herd {
        assert_eq!(h.files, files, "{}", h.role);
        let start = Instant::now();
        while Path::new(&h.db) != new {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "{} never reopened",
                h.db
            );
            let (f, db) = h.next_refresh();
            assert_eq!(f, files);
            h.db = db;
        }
    }
    assert_eq!(recorded_db(&env), new_name);
    assert!(old.exists(), "never unlinked while open");
    integrity_ok(&new);

    for h in herd {
        h.finish();
    }
    let (deleted, skipped) = run_gc(&env, now_ms());
    assert!(skipped.is_empty(), "{skipped:?}");
    assert!(deleted.contains(&old.display().to_string()), "{deleted:?}");
    assert!(!old.exists() && new.exists());
}

/// A held child's `db:` line (its third).
fn read_opened_db(child: &mut Child) -> PathBuf {
    let mut r = BufReader::new(child.stdout.take().unwrap());
    let mut s = String::new();
    for _ in 0..3 {
        assert!(r.read_line(&mut s).unwrap() > 0, "early EOF: {s:?}");
    }
    PathBuf::from(s.lines().find_map(|l| l.strip_prefix("db: ")).unwrap())
}
