//! Cache GC and corruption rebuild pieces, in temp cache dirs: which DB
//! files go, which stay because a process holds the root, the daily gate,
//! and the registry's `touch` and `remove`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use mdroots_index::{Change, FileRow, GcOptions, IndexDb, RootLocks, SCHEMA, SqliteRegistry, gc};
use mdroots_roots::registry::{Registry, RootMode, RootRecord, VerdictSource, new_root_id};
use mdroots_roots::walk::WalkStats;
use tempfile::TempDir;

const DAY_MS: u64 = 24 * 3_600_000;
/// A fixed "now", well after every `last_seen_ms` used here.
const NOW: u64 = 1_000 * DAY_MS;

/// A temp dir with the cache at `<tmp>/cache` and roots under `<tmp>/notes`.
struct Cache {
    tmp: TempDir,
}

impl Cache {
    fn new() -> Cache {
        Cache {
            tmp: tempfile::tempdir().unwrap(),
        }
    }

    fn dir(&self) -> PathBuf {
        self.tmp.path().join("cache")
    }

    fn roots(&self) -> PathBuf {
        self.dir().join("roots")
    }

    fn registry(&self) -> SqliteRegistry {
        SqliteRegistry::open(&self.dir().join("roots.v1.db")).unwrap()
    }

    /// Register an existing root dir `name`, seen at `last_seen_ms`, with
    /// a DB file of `size` bytes recorded as its `db_file`. Returns its id.
    fn root(&self, name: &str, last_seen_ms: u64, size: usize) -> String {
        let path = self.tmp.path().join("notes").join(name);
        fs::create_dir_all(&path).unwrap();
        let id = new_root_id(&path, last_seen_ms);
        let mut reg = self.registry();
        reg.insert(RootRecord {
            root_id: id.clone(),
            path,
            mode: RootMode::Marker,
            marker: Some(".zk".into()),
            marker_ino: None,
            volume_id: "dev:1".into(),
            dev: 1,
            fs_type: "apfs".into(),
            stats: WalkStats::default(),
            verdict_source: VerdictSource::Fs,
            rate_confirmations: 0,
            decided_at_ms: last_seen_ms,
            reason: "test".into(),
            last_seen_ms,
        })
        .unwrap();
        let name = format!("{id}.v{SCHEMA}.db");
        reg.set_db_file(&id, &name);
        self.file(&name, size);
        id
    }

    fn file(&self, name: &str, size: usize) -> PathBuf {
        fs::create_dir_all(self.roots()).unwrap();
        let p = self.roots().join(name);
        fs::write(&p, vec![0u8; size]).unwrap();
        p
    }

    fn gc(&self, opts: GcOptions) -> Option<mdroots_index::GcReport> {
        gc(&self.dir(), NOW, opts).unwrap()
    }

    fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(self.roots())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }
}

fn forced() -> GcOptions {
    GcOptions {
        force: true,
        ..Default::default()
    }
}

fn set_mtime(p: &Path, t: SystemTime) {
    fs::File::options()
        .write(true)
        .open(p)
        .unwrap()
        .set_modified(t)
        .unwrap();
}

fn file_row(path: &str) -> FileRow {
    FileRow {
        path: path.into(),
        ino: 1,
        ctime_ns: 1,
        mtime_ns: 1,
        size: 1,
        content: b"x".as_slice().into(),
    }
}

#[test]
fn defaults_match_the_spec() {
    let d = GcOptions::default();
    assert_eq!(
        (d.unseen_days, d.budget_bytes, d.force),
        (30, 1 << 30, false)
    );
}

#[test]
fn deletes_old_generations_and_old_schemas_but_not_the_live_file() {
    let c = Cache::new();
    let id = c.root("r", NOW, 10);
    let live = c.roots().join(format!("{id}.v{SCHEMA}.db"));
    let old_gen = c.file(&format!("{id}.v{SCHEMA}-0123abcd.db"), 10);
    let old_gen_wal = c.file(&format!("{id}.v{SCHEMA}-0123abcd.db-wal"), 10);
    let v1 = c.file(&format!("{id}.v1.db"), 10);
    let v1_shm = c.file(&format!("{id}.v1.db-shm"), 10);
    let week_ago = SystemTime::UNIX_EPOCH + Duration::from_millis(NOW - 8 * DAY_MS);
    set_mtime(&v1, week_ago);
    set_mtime(&v1_shm, week_ago);
    // A recent old-schema file: an older binary may still use it.
    let recent = c.file("00ff.v1.db", 10);
    set_mtime(
        &recent,
        SystemTime::UNIX_EPOCH + Duration::from_millis(NOW - DAY_MS),
    );
    let lock = c.file(&format!("{id}.v{SCHEMA}.lock"), 0);
    let other = c.file("notes.txt", 3);

    let r = c.gc(forced()).unwrap();
    let mut want = vec![old_gen, old_gen_wal, v1, v1_shm];
    want.sort();
    assert_eq!(r.deleted, want);
    assert!(r.skipped_busy.is_empty());
    for kept in [&live, &recent, &lock, &other] {
        assert!(kept.exists(), "{kept:?}");
    }
    assert_eq!(c.registry().all().len(), 1);
}

#[test]
fn deletes_an_aged_root_and_its_row_and_a_root_whose_path_is_gone() {
    let c = Cache::new();
    let fresh = c.root("fresh", NOW - DAY_MS, 10);
    let aged = c.root("aged", NOW - 31 * DAY_MS, 10);
    let gone = c.root("gone", NOW - DAY_MS, 10);
    fs::remove_dir(c.tmp.path().join("notes/gone")).unwrap();
    let r = c.gc(forced()).unwrap();
    assert_eq!(r.deleted.len(), 2, "{r:?}");
    let ids: Vec<String> = c.registry().all().into_iter().map(|r| r.root_id).collect();
    assert_eq!(ids, std::slice::from_ref(&fresh));
    let mut names = c.names();
    names.retain(|n| n.ends_with(".db"));
    assert_eq!(names, [format!("{fresh}.v{SCHEMA}.db")]);
    // Their lock files are created and kept, never deleted.
    for id in [aged, gone] {
        assert!(c.roots().join(format!("{id}.v{SCHEMA}.open")).exists());
    }
}

#[test]
fn skips_a_root_whose_open_lock_is_held() {
    let c = Cache::new();
    let aged = c.root("aged", NOW - 40 * DAY_MS, 10);
    let stale_gen = c.file(&format!("{aged}.v{SCHEMA}-0123abcd.db"), 10);
    let db = c.roots().join(format!("{aged}.v{SCHEMA}.db"));
    let held = RootLocks::acquire(&c.roots(), &format!("{aged}.v{SCHEMA}")).unwrap();
    let r = c.gc(forced()).unwrap();
    assert!(r.deleted.is_empty(), "{r:?}");
    let mut want = vec![db.clone(), stale_gen.clone()];
    want.sort();
    assert_eq!(r.skipped_busy, want);
    assert!(db.exists() && stale_gen.exists());
    assert_eq!(c.registry().all().len(), 1, "the row stays with its DB");
    drop(held);
    let r = c.gc(forced()).unwrap();
    assert_eq!(r.deleted, want);
    assert!(c.registry().all().is_empty());
}

#[test]
fn runs_at_most_daily_per_cache_dir() {
    let c = Cache::new();
    let id = c.root("r", NOW, 10);
    let old_gen = format!("{id}.v{SCHEMA}-0123abcd.db");
    assert!(c.gc(GcOptions::default()).is_some(), "first run is due");
    c.file(&old_gen, 10);
    assert_eq!(c.gc(GcOptions::default()), None, "ran just now");
    assert!(c.registry().gc_due(NOW + DAY_MS));
    assert!(!c.registry().gc_due(NOW + DAY_MS - 1));
    let later = gc(&c.dir(), NOW + DAY_MS, GcOptions::default()).unwrap();
    assert_eq!(later.unwrap().deleted.len(), 1);
    // Forced runs ignore the gate (and still record it).
    assert!(c.gc(forced()).is_some());
}

#[test]
fn over_budget_deletes_least_recently_seen_roots_first() {
    let c = Cache::new();
    let oldest = c.root("oldest", NOW - 3 * DAY_MS, 100);
    let middle = c.root("middle", NOW - 2 * DAY_MS, 100);
    let newest = c.root("newest", NOW - DAY_MS, 100);
    let r = c
        .gc(GcOptions {
            budget_bytes: 150,
            ..forced()
        })
        .unwrap();
    let want: Vec<PathBuf> = [&oldest, &middle]
        .iter()
        .map(|id| c.roots().join(format!("{id}.v{SCHEMA}.db")))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    assert_eq!(r.deleted, want);
    let ids: Vec<String> = c.registry().all().into_iter().map(|r| r.root_id).collect();
    assert_eq!(ids, [newest]);
}

#[test]
fn never_follows_symlinks() {
    let c = Cache::new();
    let outside = c.tmp.path().join("outside.v1.db");
    fs::write(&outside, b"keep").unwrap();
    fs::create_dir_all(c.roots()).unwrap();
    std::os::unix::fs::symlink(&outside, c.roots().join("00aa.v1.db")).unwrap();
    let r = c.gc(forced()).unwrap();
    assert!(r.deleted.is_empty(), "{r:?}");
    assert_eq!(fs::read(&outside).unwrap(), b"keep");
}

#[test]
fn touch_stamps_at_most_hourly_and_remove_deletes_the_row() {
    let c = Cache::new();
    let id = c.root("r", NOW, 0);
    let seen = |reg: &SqliteRegistry| reg.all()[0].last_seen_ms;
    let mut reg = c.registry();
    reg.touch(&id, NOW + 3_599_999);
    assert_eq!(seen(&reg), NOW);
    reg.touch(&id, NOW + 3_600_000);
    assert_eq!(seen(&reg), NOW + 3_600_000);
    reg.touch("nope", NOW);
    reg.remove("nope");
    assert_eq!(reg.all().len(), 1);
    reg.remove(&id);
    assert!(reg.all().is_empty());
}

#[test]
fn quick_check_fails_on_a_corrupt_file_and_a_new_generation_starts_with_its_name() {
    let c = Cache::new();
    let p = c.roots().join("00aa.v2.db");
    {
        let mut db = IndexDb::open(&p).unwrap();
        db.apply(&[Change::Upsert(file_row("a.md"))]).unwrap();
        db.quick_check().unwrap();
    }
    // Garbage over the header while no connection is open.
    let mut bytes = fs::read(&p).unwrap();
    bytes[..100].fill(0xa5);
    fs::write(&p, bytes).unwrap();
    let e = IndexDb::open(&p)
        .and_then(|db| db.quick_check())
        .unwrap_err();
    assert_eq!(e.kind(), mdroots_core::ErrorKind::Corrupt, "{e:?}");

    let (db, name) = IndexDb::open_new_generation(&c.roots(), "00aa.v2").unwrap();
    let gen8 = name
        .strip_prefix("00aa.v2-")
        .and_then(|n| n.strip_suffix(".db"))
        .unwrap();
    assert_eq!(gen8.len(), 8);
    assert!(db.generation().unwrap().starts_with(gen8));
    db.quick_check().unwrap();
    assert!(p.exists(), "the corrupt file is left alone");
}
