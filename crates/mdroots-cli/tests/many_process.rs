//! Many-process fixtures of docs/specs/roots.md §7 (12, 13, 16, 17, 18),
//! adapted to M4 (no native watcher): real `mdroots` processes on a copy of
//! tests/corpus/zk-min in a temp dir, with `MDROOTS_CACHE_DIR` (and `HOME`,
//! `XDG_CACHE_HOME`) pointed into it, so the real cache dir is never used.
//!
//! The hidden `mdroots __open PATH --hold-ms N` opens a workspace, prints
//! `role: ...` and `files: N`, and keeps it open for N ms.
//!
//! Not here, M6: 14 (GC with peers holding an aged DB), 15 (corruption
//! rebuild under peers) and 19 (FSEvents replay / takeover diff after the
//! reconciler dies) need GC, corruption handling and a native watcher.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
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

/// Read a held child's two lines (it then sleeps).
fn read_opened(child: &mut Child) -> Opened {
    let mut r = BufReader::new(child.stdout.take().unwrap());
    let mut s = String::new();
    for _ in 0..2 {
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
