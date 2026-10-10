//! The [SQLite](https://sqlite.org) root registry, in tempdirs: the
//! MemRegistry semantics re-run, plus persistence and concurrency.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::thread;

use mdroots_index::registry::SqliteRegistry;
use mdroots_roots::probe::{FakeProbe, Probe};
use mdroots_roots::registry::{
    Overlap, Registry, RootMode, RootRecord, VerdictSource, detect_move, lookup_valid, new_root_id,
};
use mdroots_roots::walk::WalkStats;
use tempfile::TempDir;

fn rec(path: &str, mode: RootMode, marker: Option<&str>) -> RootRecord {
    RootRecord {
        root_id: new_root_id(Path::new(path), 1),
        path: PathBuf::from(path),
        mode,
        marker: marker.map(str::to_owned),
        marker_ino: None,
        volume_id: "fake:1".into(),
        dev: 1,
        fs_type: "apfs".into(),
        stats: WalkStats::default(),
        verdict_source: VerdictSource::Fs,
        rate_confirmations: 0,
        decided_at_ms: 1,
        reason: "test".into(),
        last_seen_ms: 1,
    }
}

fn git(path: &str) -> RootRecord {
    rec(path, RootMode::Vcs, Some(".git"))
}

fn zk(path: &str) -> RootRecord {
    rec(path, RootMode::Marker, Some(".zk"))
}

fn loose(path: &str) -> RootRecord {
    rec(path, RootMode::Loose, None)
}

fn found(reg: &SqliteRegistry, p: &str) -> Option<PathBuf> {
    reg.lookup(Path::new(p)).map(|r| r.path)
}

fn overlap(p: &str) -> Result<(), Overlap> {
    Err(Overlap {
        existing: PathBuf::from(p),
    })
}

fn db_path(tmp: &TempDir) -> PathBuf {
    tmp.path().join("cache").join("roots.v1.db")
}

/// A fresh registry in its own tempdir (kept alive by the returned guard).
fn fresh() -> (TempDir, SqliteRegistry) {
    let tmp = tempfile::tempdir().unwrap();
    let reg = SqliteRegistry::open(&db_path(&tmp)).unwrap();
    (tmp, reg)
}

// ---------------------------------------------------------------- lookup

#[test]
fn lookup_is_component_wise() {
    let (_t, mut reg) = fresh();
    reg.insert(git("/a/b")).unwrap();
    assert_eq!(found(&reg, "/a/b/x.md"), Some("/a/b".into()));
    assert_eq!(found(&reg, "/a/b"), Some("/a/b".into()));
    assert_eq!(found(&reg, "/a/bc/x.md"), None);
    assert_eq!(found(&reg, "/a/x.md"), None);
}

// ---------------------------------------------------------------- overlap

#[test]
fn overlap_rejected_both_ways() {
    let (_t, mut reg) = fresh();
    reg.insert(git("/n")).unwrap();
    assert_eq!(
        reg.insert(rec("/n/sub", RootMode::Vcs, None)),
        overlap("/n")
    );

    let (_t, mut reg) = fresh();
    reg.insert(rec("/n/sub", RootMode::Vcs, None)).unwrap();
    assert_eq!(reg.insert(git("/n")), overlap("/n/sub"));

    let (_t, mut reg) = fresh();
    reg.insert(git("/n")).unwrap();
    assert_eq!(reg.insert(git("/n")), overlap("/n"));
    assert_eq!(reg.all().len(), 1);
}

#[test]
fn nested_marker_root_allowed_nearest_wins() {
    let (_t, mut reg) = fresh();
    reg.insert(zk("/nb")).unwrap();
    reg.insert(git("/nb/proj")).unwrap();
    assert_eq!(found(&reg, "/nb/proj/docs/a.md"), Some("/nb/proj".into()));
    assert_eq!(found(&reg, "/nb/a.md"), Some("/nb".into()));
}

#[test]
fn outer_after_inner_marker_nesting_allowed() {
    let (_t, mut reg) = fresh();
    reg.insert(git("/nb/proj")).unwrap();
    reg.insert(zk("/nb")).unwrap();
    assert_eq!(found(&reg, "/nb/other/a.md"), Some("/nb".into()));
    assert_eq!(found(&reg, "/nb/proj/a.md"), Some("/nb/proj".into()));
}

#[test]
fn marker_root_inside_loose_allowed() {
    let (_t, mut reg) = fresh();
    reg.insert(loose("/notes")).unwrap();
    reg.insert(git("/notes/repo")).unwrap();
    assert_eq!(found(&reg, "/notes/repo/a.md"), Some("/notes/repo".into()));
    assert_eq!(found(&reg, "/notes/a.md"), Some("/notes".into()));
}

#[test]
fn loose_containing_marker_rejected() {
    let (_t, mut reg) = fresh();
    reg.insert(git("/notes/repo")).unwrap();
    assert_eq!(reg.insert(loose("/notes")), overlap("/notes/repo"));
}

#[test]
fn loose_roots_never_nest() {
    let (_t, mut reg) = fresh();
    reg.insert(loose("/notes")).unwrap();
    assert_eq!(reg.insert(loose("/notes/sub")), overlap("/notes"));
    let (_t, mut reg) = fresh();
    reg.insert(loose("/notes/sub")).unwrap();
    assert_eq!(reg.insert(loose("/notes")), overlap("/notes/sub"));
    let (_t, mut reg) = fresh();
    reg.insert(git("/r")).unwrap();
    assert_eq!(reg.insert(loose("/r/sub")), overlap("/r"));
}

#[test]
fn disjoint_roots_coexist_sorted_by_path() {
    let (_t, mut reg) = fresh();
    reg.insert(loose("/a-b")).unwrap();
    reg.insert(loose("/ab")).unwrap();
    reg.insert(git("/a/b")).unwrap();
    let paths: Vec<_> = reg.all().into_iter().map(|r| r.path).collect();
    // PathBuf order, not SQLite byte order ('-' < '/').
    assert_eq!(
        paths,
        [
            PathBuf::from("/a/b"),
            PathBuf::from("/a-b"),
            PathBuf::from("/ab")
        ]
    );
}

#[test]
fn update_replaces_by_root_id() {
    let (_t, mut reg) = fresh();
    let r = git("/old");
    reg.insert(r.clone()).unwrap();
    reg.update(RootRecord {
        path: "/new".into(),
        ..r.clone()
    });
    assert_eq!(reg.all().len(), 1);
    assert_eq!(found(&reg, "/new/a.md"), Some("/new".into()));
    assert_eq!(found(&reg, "/old/a.md"), None);
}

#[test]
fn update_inserts_missing_and_evicts_other_row_at_path() {
    let (_t, mut reg) = fresh();
    reg.update(git("/a"));
    assert_eq!(found(&reg, "/a/x.md"), Some("/a".into()));
    // A different root moves onto /a: the stale row goes.
    let mover = RootRecord {
        root_id: "mover".into(),
        ..git("/a")
    };
    reg.update(mover);
    let all = reg.all();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].root_id, "mover");
}

#[test]
fn single_file_never_stored_by_update() {
    let (_t, mut reg) = fresh();
    reg.update(rec("/f", RootMode::SingleFile, None));
    assert!(reg.all().is_empty());
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "single-file roots are never registered")]
fn single_file_insert_asserts_in_debug() {
    let (_t, mut reg) = fresh();
    let _ = reg.insert(rec("/f", RootMode::SingleFile, None));
}

#[test]
#[cfg(not(debug_assertions))]
fn single_file_insert_is_dropped() {
    let (_t, mut reg) = fresh();
    assert_eq!(reg.insert(rec("/f", RootMode::SingleFile, None)), Ok(()));
    assert!(reg.all().is_empty());
}

#[cfg(unix)]
#[test]
fn non_utf8_path_is_rejected_as_overlap() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let (_t, mut reg) = fresh();
    let p = PathBuf::from(OsStr::from_bytes(b"/n\xff"));
    let r = RootRecord {
        path: p.clone(),
        ..git("/x")
    };
    assert_eq!(reg.insert(r), Err(Overlap { existing: p }));
    assert!(reg.all().is_empty());
}

// ---------------------------------------------------------------- round trip

#[test]
fn every_field_round_trips() {
    let (_t, mut reg) = fresh();
    let modes = [
        RootMode::Marker,
        RootMode::Vcs,
        RootMode::TrackedOnly,
        RootMode::IndexDriven,
        RootMode::Loose,
        RootMode::Lazy,
        RootMode::VcsEnumerated,
    ];
    let sources = [
        VerdictSource::Fs,
        VerdictSource::Eden,
        VerdictSource::Budget,
        VerdictSource::Rate,
        VerdictSource::User,
    ];
    let mut want = Vec::new();
    for (i, mode) in modes.into_iter().enumerate() {
        let path = format!("/r{i}");
        let r = RootRecord {
            marker_ino: if i % 2 == 0 { Some(u64::MAX) } else { None },
            dev: u64::MAX - i as u64,
            stats: WalkStats {
                entries: 10 + i,
                md: 3,
                files: 7,
                dirs: 2,
                ms: 0.1 + 0.2,
                ms_per_dir: if i % 2 == 0 { Some(1e-7 / 3.0) } else { None },
                max_depth: 4,
            },
            verdict_source: sources[i % sources.len()],
            rate_confirmations: u32::MAX,
            decided_at_ms: u64::MAX,
            last_seen_ms: 1_700_000_000_123,
            reason: "lazy: rate 7.1 ms/dir, 1 of 2".into(),
            ..rec(&path, mode, Some(".git"))
        };
        reg.insert(r.clone()).unwrap();
        want.push(r);
    }
    assert_eq!(reg.all(), want);
    let ino = reg.by_marker("fake:1", u64::MAX).map(|r| r.path);
    assert_eq!(ino, Some(PathBuf::from("/r0")));
    assert_eq!(reg.by_marker("fake:2", u64::MAX), None);
}

#[test]
fn rows_with_unknown_values_are_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let mut reg = SqliteRegistry::open(&db_path(&tmp)).unwrap();
    reg.insert(git("/good")).unwrap();
    reg.insert(git("/future")).unwrap();
    reg.insert(git("/badstats")).unwrap();
    let c = rusqlite::Connection::open(db_path(&tmp)).unwrap();
    c.execute(
        "UPDATE roots SET mode = 'hologram' WHERE path = '/future'",
        [],
    )
    .unwrap();
    c.execute(
        "UPDATE roots SET stats = '1,2' WHERE path = '/badstats'",
        [],
    )
    .unwrap();
    let paths: Vec<_> = reg.all().into_iter().map(|r| r.path).collect();
    assert_eq!(paths, [PathBuf::from("/good")]);
    assert_eq!(found(&reg, "/future/a.md"), None);
}

// ---------------------------------------------------------------- db_file

#[test]
fn db_file_is_set_and_survives_update() {
    let (_t, mut reg) = fresh();
    let r = git("/r");
    reg.insert(r.clone()).unwrap();
    assert_eq!(reg.db_file(&r.root_id), None);
    reg.set_db_file(&r.root_id, "abc.v1.db");
    assert_eq!(reg.db_file(&r.root_id).as_deref(), Some("abc.v1.db"));
    reg.update(RootRecord {
        path: "/moved".into(),
        last_seen_ms: 99,
        ..r.clone()
    });
    assert_eq!(reg.db_file(&r.root_id).as_deref(), Some("abc.v1.db"));
    // Unknown root: nothing stored, nothing returned.
    reg.set_db_file("nope", "x.db");
    assert_eq!(reg.db_file("nope"), None);
}

// ---------------------------------------------------------------- lookup_valid / detect_move

#[test]
fn lookup_valid_works_over_sqlite() {
    let probe = FakeProbe::new().dir("/r/.git").file("/r/a.md", "");
    let (_t, mut reg) = fresh();
    reg.insert(git("/r")).unwrap();
    let got = lookup_valid(&reg, &probe, Path::new("/r/a.md"));
    assert_eq!(got.map(|r| r.path), Some("/r".into()));
    let gone = FakeProbe::new().dir("/r").file("/r/a.md", "");
    assert_eq!(lookup_valid(&reg, &gone, Path::new("/r/a.md")), None);
}

#[test]
fn trait_remove_deletes_the_row() {
    let (_t, mut reg) = fresh();
    let r = git("/r");
    reg.insert(r.clone()).unwrap();
    reg.insert(loose("/n")).unwrap();
    Registry::remove(&mut reg, &r.root_id);
    Registry::remove(&mut reg, "missing");
    let paths: Vec<_> = reg.all().into_iter().map(|r| r.path).collect();
    assert_eq!(paths, [PathBuf::from("/n")]);
    // The path is free again.
    reg.insert(git("/r")).unwrap();
}

#[test]
fn detect_move_keeps_root_id() {
    let before = FakeProbe::new().dir("/old/.git");
    let p = Path::new("/old");
    let r = RootRecord {
        marker_ino: Some(before.stat(&p.join(".git")).unwrap().ino),
        volume_id: before.volume_id(p).unwrap(),
        ..git("/old")
    };
    let (_t, mut reg) = fresh();
    reg.insert(r.clone()).unwrap();
    let after = before.rename("/old", "/new");
    let moved = detect_move(&reg, &after, Path::new("/new"), ".git").expect("a move");
    assert_eq!(moved.root_id, r.root_id);
    reg.update(moved);
    assert_eq!(found(&reg, "/new/a.md"), Some("/new".into()));
    assert_eq!(reg.all().len(), 1);
}

// ---------------------------------------------------------------- persistence / sharing

#[test]
fn rows_persist_across_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    let r = git("/r");
    {
        let mut reg = SqliteRegistry::open(&db_path(&tmp)).unwrap();
        reg.insert(r.clone()).unwrap();
        reg.set_db_file(&r.root_id, "r.v1.db");
    }
    let reg = SqliteRegistry::open(&db_path(&tmp)).unwrap();
    assert_eq!(reg.all(), std::slice::from_ref(&r));
    assert_eq!(reg.db_file(&r.root_id).as_deref(), Some("r.v1.db"));
}

#[test]
fn two_connections_see_each_others_inserts() {
    let tmp = tempfile::tempdir().unwrap();
    let mut a = SqliteRegistry::open(&db_path(&tmp)).unwrap();
    let mut b = SqliteRegistry::open(&db_path(&tmp)).unwrap();
    a.insert(git("/a")).unwrap();
    assert_eq!(found(&b, "/a/x.md"), Some("/a".into()));
    b.insert(git("/b")).unwrap();
    assert_eq!(found(&a, "/b/x.md"), Some("/b".into()));
    // The overlap check sees the other connection's rows.
    assert_eq!(b.insert(loose("/a/sub")), overlap("/a"));
}

#[test]
fn concurrent_inserts_of_same_path_yield_one_row() {
    for _ in 0..10 {
        let tmp = tempfile::tempdir().unwrap();
        let path = db_path(&tmp);
        SqliteRegistry::open(&path).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let handles: Vec<_> = (0..2u64)
            .map(|i| {
                let path = path.clone();
                let barrier = barrier.clone();
                thread::spawn(move || {
                    let mut reg = SqliteRegistry::open(&path).unwrap();
                    let r = RootRecord {
                        root_id: new_root_id(Path::new("/same"), i),
                        ..git("/same")
                    };
                    barrier.wait();
                    reg.insert(r)
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(
            results.iter().filter(|r| r.is_ok()).count(),
            1,
            "{results:?}"
        );
        assert!(results.contains(&overlap("/same")));
        let reg = SqliteRegistry::open(&path).unwrap();
        assert_eq!(reg.all().len(), 1);
    }
}

#[test]
fn open_creates_parent_and_rejects_other_schema() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("a").join("b").join("roots.v1.db");
    SqliteRegistry::open(&path).unwrap();
    assert!(path.exists());
    let c = rusqlite::Connection::open(&path).unwrap();
    c.execute("UPDATE meta SET value = '9' WHERE key = 'schema'", [])
        .unwrap();
    let err = SqliteRegistry::open(&path).unwrap_err();
    assert_eq!(err.kind(), mdroots_core::ErrorKind::Corrupt);
}
