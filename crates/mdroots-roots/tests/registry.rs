use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;

use mdroots_roots::probe::{FakeProbe, MountInfo, Probe};
use mdroots_roots::registry::{
    DiscoverLock, EDITOR_MARKER, MemRegistry, Overlap, Registry, RootMode, RootRecord,
    VerdictSource, detect_move, is_editor, lookup_valid, lookup_valid_for, new_root_id,
};
use mdroots_roots::walk::WalkStats;

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

fn found(reg: &MemRegistry, p: &str) -> Option<PathBuf> {
    reg.lookup(Path::new(p)).map(|r| r.path)
}

fn overlap(p: &str) -> Result<(), Overlap> {
    Err(Overlap {
        existing: PathBuf::from(p),
    })
}

// ---------------------------------------------------------------- lookup

#[test]
fn lookup_is_component_wise() {
    let mut reg = MemRegistry::new();
    reg.insert(git("/a/b")).unwrap();
    assert_eq!(found(&reg, "/a/b/x.md"), Some("/a/b".into()));
    assert_eq!(found(&reg, "/a/b"), Some("/a/b".into()));
    assert_eq!(found(&reg, "/a/bc/x.md"), None);
    assert_eq!(found(&reg, "/a/x.md"), None);
}

#[test]
fn root_ids_are_stable_and_distinct() {
    let a = new_root_id(Path::new("/n"), 5);
    assert_eq!(a, new_root_id(Path::new("/n"), 5));
    assert_eq!(a.len(), 16);
    assert_ne!(a, new_root_id(Path::new("/n"), 6));
    assert_ne!(a, new_root_id(Path::new("/m"), 5));
}

// ---------------------------------------------------------------- overlap

#[test]
fn overlap_rejected_both_ways() {
    // A markerless root cannot nest under, or contain, another root.
    let mut reg = MemRegistry::new();
    reg.insert(git("/n")).unwrap();
    assert_eq!(
        reg.insert(rec("/n/sub", RootMode::Vcs, None)),
        overlap("/n")
    );

    let mut reg = MemRegistry::new();
    reg.insert(rec("/n/sub", RootMode::Vcs, None)).unwrap();
    assert_eq!(reg.insert(git("/n")), overlap("/n/sub"));

    let mut reg = MemRegistry::new();
    reg.insert(git("/n")).unwrap();
    assert_eq!(reg.insert(git("/n")), overlap("/n"));
    assert_eq!(reg.all().len(), 1);
}

#[test]
fn nested_marker_root_allowed_nearest_wins() {
    let mut reg = MemRegistry::new();
    reg.insert(zk("/nb")).unwrap();
    reg.insert(git("/nb/proj")).unwrap();
    assert_eq!(found(&reg, "/nb/proj/docs/a.md"), Some("/nb/proj".into()));
    assert_eq!(found(&reg, "/nb/a.md"), Some("/nb".into()));
}

#[test]
fn outer_after_inner_marker_nesting_allowed() {
    let mut reg = MemRegistry::new();
    reg.insert(git("/nb/proj")).unwrap();
    reg.insert(zk("/nb")).unwrap();
    assert_eq!(found(&reg, "/nb/other/a.md"), Some("/nb".into()));
    assert_eq!(found(&reg, "/nb/proj/a.md"), Some("/nb/proj".into()));
}

#[test]
fn marker_root_inside_loose_allowed() {
    let mut reg = MemRegistry::new();
    reg.insert(loose("/notes")).unwrap();
    reg.insert(git("/notes/repo")).unwrap();
    assert_eq!(found(&reg, "/notes/repo/a.md"), Some("/notes/repo".into()));
    assert_eq!(found(&reg, "/notes/a.md"), Some("/notes".into()));
}

#[test]
fn loose_containing_marker_rejected() {
    let mut reg = MemRegistry::new();
    reg.insert(git("/notes/repo")).unwrap();
    assert_eq!(reg.insert(loose("/notes")), overlap("/notes/repo"));
}

#[test]
fn loose_roots_never_nest() {
    let mut reg = MemRegistry::new();
    reg.insert(loose("/notes")).unwrap();
    assert_eq!(reg.insert(loose("/notes/sub")), overlap("/notes"));
    let mut reg = MemRegistry::new();
    reg.insert(loose("/notes/sub")).unwrap();
    assert_eq!(reg.insert(loose("/notes")), overlap("/notes/sub"));
    // A loose root is never inside a marker root either.
    let mut reg = MemRegistry::new();
    reg.insert(git("/r")).unwrap();
    assert_eq!(reg.insert(loose("/r/sub")), overlap("/r"));
}

#[test]
fn disjoint_roots_coexist() {
    let mut reg = MemRegistry::new();
    reg.insert(loose("/a")).unwrap();
    reg.insert(loose("/ab")).unwrap();
    let paths: Vec<_> = reg.all().into_iter().map(|r| r.path).collect();
    assert_eq!(paths, [PathBuf::from("/a"), PathBuf::from("/ab")]);
}

#[test]
fn update_replaces_by_root_id() {
    let mut reg = MemRegistry::new();
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

// ---------------------------------------------------------------- lookup_valid

fn mi(fs_type: &str, dev: u64) -> MountInfo {
    MountInfo {
        fs_type: fs_type.into(),
        from: "/dev/fake".into(),
        local: true,
        dev,
    }
}

#[test]
fn lookup_valid_accepts_unchanged_root() {
    let probe = FakeProbe::new().dir("/r/.git").file("/r/a.md", "");
    let mut reg = MemRegistry::new();
    reg.insert(git("/r")).unwrap();
    let got = lookup_valid(&reg, &probe, Path::new("/r/a.md"));
    assert_eq!(got.map(|r| r.path), Some("/r".into()));
    assert_eq!(lookup_valid(&reg, &probe, Path::new("/x/a.md")), None);
}

#[test]
fn lookup_valid_skips_marker_check_without_marker() {
    let probe = FakeProbe::new().dir("/notes");
    let mut reg = MemRegistry::new();
    reg.insert(loose("/notes")).unwrap();
    assert!(lookup_valid(&reg, &probe, Path::new("/notes/a.md")).is_some());
}

#[test]
fn lookup_valid_rejects_missing_marker() {
    let probe = FakeProbe::new().dir("/r").file("/r/a.md", "");
    let mut reg = MemRegistry::new();
    reg.insert(git("/r")).unwrap();
    assert_eq!(lookup_valid(&reg, &probe, Path::new("/r/a.md")), None);
}

#[test]
fn lookup_valid_rejects_dev_change() {
    let probe = FakeProbe::new().dir("/r/.git").mount("/r", mi("apfs", 7));
    let mut reg = MemRegistry::new();
    reg.insert(git("/r")).unwrap();
    assert_eq!(lookup_valid(&reg, &probe, Path::new("/r/a.md")), None);
}

#[test]
fn lookup_valid_rejects_fs_type_change() {
    let probe = FakeProbe::new()
        .dir("/r/.git")
        .mount("/r", mi("edenfs:", 1));
    let mut reg = MemRegistry::new();
    reg.insert(git("/r")).unwrap();
    assert_eq!(lookup_valid(&reg, &probe, Path::new("/r/a.md")), None);
}

#[test]
fn lookup_valid_skips_a_row_whose_marker_is_gone_for_its_parent() {
    let probe = FakeProbe::new().dir("/nb/.zk").file("/nb/proj/a.md", "");
    let mut reg = MemRegistry::new();
    reg.insert(zk("/nb")).unwrap();
    reg.insert(git("/nb/proj")).unwrap();
    let got = lookup_valid(&reg, &probe, Path::new("/nb/proj/a.md"));
    assert_eq!(got.map(|r| r.path), Some("/nb".into()));
    // Skipped, not removed.
    assert_eq!(reg.all().len(), 2);
}

#[test]
fn lookup_valid_does_not_skip_past_a_changed_filesystem() {
    let probe = FakeProbe::new()
        .dir("/nb/.zk")
        .dir("/nb/proj/.git")
        .mount("/nb/proj", mi("apfs", 7));
    let mut reg = MemRegistry::new();
    reg.insert(zk("/nb")).unwrap();
    reg.insert(git("/nb/proj")).unwrap();
    assert_eq!(lookup_valid(&reg, &probe, Path::new("/nb/proj/a.md")), None);
}

#[test]
fn lookup_valid_rejects_a_file_on_another_device_than_its_root() {
    // A mount below a registered root, with no row of its own.
    let probe = FakeProbe::new()
        .dir("/nb/.zk")
        .file("/nb/other/a.md", "")
        .mount("/nb/other", mi("apfs", 7));
    let mut reg = MemRegistry::new();
    reg.insert(zk("/nb")).unwrap();
    assert_eq!(
        lookup_valid(&reg, &probe, Path::new("/nb/other/a.md")),
        None
    );
    assert!(lookup_valid(&reg, &probe, Path::new("/nb/a.md")).is_some());
}

fn editor(path: &str) -> RootRecord {
    rec(path, RootMode::Marker, Some(EDITOR_MARKER))
}

#[test]
fn editor_rows_are_valid_only_for_their_workspace_folder() {
    let probe = FakeProbe::new().dir("/r/.git").dir("/r/ws");
    let mut reg = MemRegistry::new();
    reg.insert(editor("/r/ws")).unwrap();
    let file = Path::new("/r/ws/a.md");
    assert_eq!(lookup_valid(&reg, &probe, file), None);
    let other = [PathBuf::from("/r/other")];
    assert_eq!(lookup_valid_for(&reg, &probe, file, &other), None);
    let ws = [PathBuf::from("/r/ws")];
    let got = lookup_valid_for(&reg, &probe, file, &ws);
    assert_eq!(got.map(|r| r.path), Some("/r/ws".into()));

    // The enclosing git root registers around it and is found without the
    // folder.
    reg.insert(git("/r")).unwrap();
    let got = lookup_valid(&reg, &probe, file);
    assert_eq!(got.map(|r| r.path), Some("/r".into()));
}

#[test]
fn markerless_marker_rows_are_editor_rows() {
    // Rows registered before EDITOR_MARKER: mode Marker, no marker.
    let old = rec("/r/ws", RootMode::Marker, None);
    assert!(is_editor(&old) && is_editor(&editor("/r/ws")));
    assert!(!is_editor(&loose("/n")) && !is_editor(&zk("/n")));
    assert!(!is_editor(&rec("/n", RootMode::Lazy, None)));
    let probe = FakeProbe::new().dir("/r/.git").dir("/r/ws");
    let mut reg = MemRegistry::new();
    reg.insert(old).unwrap();
    reg.insert(git("/r")).unwrap();
    let got = lookup_valid(&reg, &probe, Path::new("/r/ws/a.md"));
    assert_eq!(got.map(|r| r.path), Some("/r".into()));
}

#[test]
fn remove_deletes_by_root_id() {
    let mut reg = MemRegistry::new();
    let r = git("/r");
    reg.insert(r.clone()).unwrap();
    reg.insert(loose("/n")).unwrap();
    reg.remove(&r.root_id);
    reg.remove("missing");
    let paths: Vec<_> = reg.all().into_iter().map(|r| r.path).collect();
    assert_eq!(paths, [PathBuf::from("/n")]);
}

// ---------------------------------------------------------------- detect_move

fn registered(probe: &FakeProbe, path: &str) -> RootRecord {
    let p = Path::new(path);
    RootRecord {
        marker_ino: Some(probe.stat(&p.join(".git")).unwrap().ino),
        volume_id: probe.volume_id(p).unwrap(),
        ..git(path)
    }
}

#[test]
fn detect_move_keeps_root_id() {
    let before = FakeProbe::new().dir("/old/.git");
    let r = registered(&before, "/old");
    let mut reg = MemRegistry::new();
    reg.insert(r.clone()).unwrap();

    let after = before.rename("/old", "/new");
    let moved = detect_move(&reg, &after, Path::new("/new"), ".git").expect("a move");
    assert_eq!(moved.root_id, r.root_id);
    assert_eq!(moved.path, PathBuf::from("/new"));
    // Pure: the registry is unchanged until the caller updates it.
    assert_eq!(found(&reg, "/old/a.md"), Some("/old".into()));
    reg.update(moved);
    assert_eq!(found(&reg, "/new/a.md"), Some("/new".into()));
    assert_eq!(reg.all().len(), 1);
}

#[test]
fn detect_move_ignores_copies_and_strangers() {
    let probe = FakeProbe::new().dir("/old/.git").dir("/copy/.git");
    let mut reg = MemRegistry::new();
    reg.insert(registered(&probe, "/old")).unwrap();
    // Different inode: no row matches.
    assert_eq!(detect_move(&reg, &probe, Path::new("/copy"), ".git"), None);
    // Same inode at the recorded path: not a move.
    assert_eq!(detect_move(&reg, &probe, Path::new("/old"), ".git"), None);
}

#[test]
fn detect_move_rejects_copy_with_same_inode() {
    // A hard-link-like copy where both paths report the marker inode.
    let probe = FakeProbe::new().dir("/old/.git").dir("/copy/.git");
    let ino = probe.stat(Path::new("/old/.git")).unwrap().ino;
    let probe = probe.ino("/copy/.git", ino);
    let mut reg = MemRegistry::new();
    reg.insert(registered(&probe, "/old")).unwrap();
    assert_eq!(detect_move(&reg, &probe, Path::new("/copy"), ".git"), None);
}

// ---------------------------------------------------------------- DiscoverLock

#[test]
fn discover_lock_excludes_other_threads_and_survives_drop() {
    let dir = tempfile::tempdir().unwrap();
    let held = DiscoverLock::try_acquire(dir.path())
        .unwrap()
        .expect("free");

    let other = |path: PathBuf| {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let got = DiscoverLock::try_acquire(&path).unwrap();
            tx.send(got.is_some()).unwrap();
        });
        rx.recv().unwrap()
    };
    assert!(!other(dir.path().to_path_buf()), "held elsewhere");
    drop(held);
    assert!(other(dir.path().to_path_buf()), "free after drop");
    assert!(dir.path().join("discover.lock").exists());

    let blocking = DiscoverLock::acquire(dir.path()).unwrap();
    assert!(!other(dir.path().to_path_buf()));
    drop(blocking);
    assert!(dir.path().join("discover.lock").exists());
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "single-file roots are never registered")]
fn single_file_insert_asserts_in_debug() {
    let mut reg = MemRegistry::new();
    let _ = reg.insert(rec("/f", RootMode::SingleFile, None));
}
