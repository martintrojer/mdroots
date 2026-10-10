//! Cache dir choice (docs/DECISIONS.md D5). Every candidate is a tempdir;
//! the real cache dir is never touched.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use mdroots_index::cache::{CacheEnv, cache_dir};
use mdroots_roots::probe::{FakeProbe, MountInfo, StdProbe};

fn env(tmp: &Path) -> CacheEnv {
    CacheEnv {
        xdg_cache_home: Some(tmp.join("xdg")),
        home: Some(tmp.join("home")),
        xdg_runtime_dir: Some(tmp.join("run")),
        uid: rustix::process::getuid().as_raw(),
        tmp_dir: tmp.join("vartmp"),
    }
}

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn picks_xdg_cache_home_when_local() {
    let tmp = tempfile::tempdir().unwrap();
    let e = env(tmp.path());
    std::fs::create_dir(tmp.path().join("xdg")).unwrap();
    let c = cache_dir(&StdProbe, &e).expect("a cache dir");
    let want = tmp.path().join("xdg/mdroots");
    assert_eq!(c.path, want);
    assert_eq!(c.reason, format!("{}: chosen", want.display()));
    assert_eq!(mode(&want), 0o700);
    assert!(!want.join(".mdroots-write-test").exists());
}

#[test]
fn existing_dir_is_reset_to_0700() {
    let tmp = tempfile::tempdir().unwrap();
    let e = env(tmp.path());
    let want = tmp.path().join("xdg/mdroots");
    std::fs::create_dir_all(&want).unwrap();
    std::fs::set_permissions(&want, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(cache_dir(&StdProbe, &e).unwrap().path, want);
    assert_eq!(mode(&want), 0o700);
}

#[test]
fn home_default_when_xdg_unset() {
    let tmp = tempfile::tempdir().unwrap();
    let mut e = env(tmp.path());
    e.xdg_cache_home = None;
    let c = cache_dir(&StdProbe, &e).unwrap();
    let rel = if cfg!(target_os = "macos") {
        "home/Library/Caches/mdroots"
    } else {
        "home/.cache/mdroots"
    };
    assert_eq!(c.path, tmp.path().join(rel));
    assert_eq!(mode(&c.path), 0o700);
}

#[test]
fn tmp_dir_fallback_is_owned_and_private() {
    let tmp = tempfile::tempdir().unwrap();
    let e = CacheEnv {
        xdg_cache_home: None,
        home: None,
        xdg_runtime_dir: None,
        ..env(tmp.path())
    };
    let c = cache_dir(&StdProbe, &e).unwrap();
    assert_eq!(c.path, tmp.path().join(format!("vartmp/mdroots-{}", e.uid)));
    assert_eq!(mode(&c.path), 0o700);
}

#[test]
fn nfs_candidate_skipped_next_chosen() {
    // FakeProbe only classifies; directories are then created for real, so
    // the candidates live under a tempdir.
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path();
    let e = env(t);
    let probe = FakeProbe::new()
        .dir(t.join("xdg"))
        .dir(t.join("home"))
        .mount(
            t.join("xdg"),
            MountInfo {
                fs_type: "nfs".into(),
                from: "server:/export".into(),
                local: false,
                dev: 9,
            },
        );
    let c = cache_dir(&probe, &e).unwrap();
    let home_default = if cfg!(target_os = "macos") {
        t.join("home/Library/Caches/mdroots")
    } else {
        t.join("home/.cache/mdroots")
    };
    assert_eq!(c.path, home_default);
    assert!(c.reason.contains("not local (nfs)"), "{}", c.reason);
    assert!(!t.join("xdg").exists(), "nothing created on the nfs mount");
}

#[test]
fn nothing_local_is_none() {
    let tmp = tempfile::tempdir().unwrap();
    let nfs = MountInfo {
        fs_type: "nfs".into(),
        from: "server:/export".into(),
        local: false,
        dev: 9,
    };
    let probe = FakeProbe::new().mount("/", nfs);
    assert_eq!(cache_dir(&probe, &env(tmp.path())), None);
}

/// Restores 0700 on a dir made read-only by a test, even on panic.
struct RestorePerms(PathBuf);

impl Drop for RestorePerms {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
    }
}

#[test]
fn read_only_candidate_skipped() {
    if rustix::process::geteuid().is_root() {
        return; // root ignores permission bits
    }
    let tmp = tempfile::tempdir().unwrap();
    let e = env(tmp.path());
    let xdg = tmp.path().join("xdg");
    std::fs::create_dir(&xdg).unwrap();
    std::fs::set_permissions(&xdg, std::fs::Permissions::from_mode(0o500)).unwrap();
    let _guard = RestorePerms(xdg.clone());
    let c = cache_dir(&StdProbe, &e).unwrap();
    assert_ne!(c.path, xdg.join("mdroots"));
    assert!(
        c.reason
            .starts_with(&format!("{}: create", xdg.join("mdroots").display())),
        "{}",
        c.reason
    );
    assert!(!xdg.join("mdroots").exists());
}

/// Only the shared `<tmp_dir>/mdroots-<uid>` candidate, for `uid`.
fn tmp_only(tmp: &Path, uid: u32) -> CacheEnv {
    CacheEnv {
        xdg_cache_home: None,
        home: None,
        xdg_runtime_dir: None,
        uid,
        ..env(tmp)
    }
}

#[test]
fn tmp_dir_symlink_to_other_dir_rejected_untouched() {
    // A symlink planted at the shared candidate, pointing at a dir its
    // owner keeps at 0755; the expected uid does not match.
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path();
    let e = tmp_only(t, rustix::process::getuid().as_raw().wrapping_add(1));
    let victim = t.join("victim");
    std::fs::create_dir(&victim).unwrap();
    std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::create_dir(t.join("vartmp")).unwrap();
    std::os::unix::fs::symlink(&victim, t.join(format!("vartmp/mdroots-{}", e.uid))).unwrap();
    assert_eq!(cache_dir(&StdProbe, &e), None);
    assert_eq!(mode(&victim), 0o755, "symlink target chmodded");
}

#[test]
fn tmp_dir_symlink_to_own_dir_rejected_untouched() {
    // Even with a matching owner, the shared candidate is never followed.
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path();
    let e = tmp_only(t, rustix::process::getuid().as_raw());
    let target = t.join("target");
    std::fs::create_dir(&target).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::create_dir(t.join("vartmp")).unwrap();
    std::os::unix::fs::symlink(&target, t.join(format!("vartmp/mdroots-{}", e.uid))).unwrap();
    assert_eq!(cache_dir(&StdProbe, &e), None);
    assert_eq!(mode(&target), 0o755, "symlink target chmodded");
}

#[test]
fn tmp_dir_not_owned_rejected_without_chmod() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path();
    let e = tmp_only(t, rustix::process::getuid().as_raw().wrapping_add(1));
    let cand = t.join(format!("vartmp/mdroots-{}", e.uid));
    std::fs::create_dir_all(&cand).unwrap();
    std::fs::set_permissions(&cand, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(cache_dir(&StdProbe, &e), None);
    assert_eq!(mode(&cand), 0o755, "rejected dir chmodded");
}

#[test]
fn tmp_dir_file_at_candidate_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path();
    let e = tmp_only(t, rustix::process::getuid().as_raw());
    std::fs::create_dir(t.join("vartmp")).unwrap();
    let cand = t.join(format!("vartmp/mdroots-{}", e.uid));
    std::fs::write(&cand, b"").unwrap();
    assert_eq!(cache_dir(&StdProbe, &e), None);
}

#[test]
fn tmp_dir_existing_own_dir_reset_to_0700() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path();
    let e = tmp_only(t, rustix::process::getuid().as_raw());
    let cand = t.join(format!("vartmp/mdroots-{}", e.uid));
    std::fs::create_dir_all(&cand).unwrap();
    std::fs::set_permissions(&cand, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(cache_dir(&StdProbe, &e).unwrap().path, cand);
    assert_eq!(mode(&cand), 0o700);
}

#[test]
fn tmp_dir_owned_unreadable_dir_reset_to_0700() {
    // The owner may have stripped read (0300) or every bit (0000) from its
    // own dir; it can still chmod it, so the candidate is kept.
    for initial in [0o300, 0o000] {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        let e = tmp_only(t, rustix::process::getuid().as_raw());
        let cand = t.join(format!("vartmp/mdroots-{}", e.uid));
        std::fs::create_dir_all(&cand).unwrap();
        let _guard = RestorePerms(cand.clone());
        std::fs::set_permissions(&cand, std::fs::Permissions::from_mode(initial)).unwrap();
        let c = cache_dir(&StdProbe, &e);
        assert_eq!(c.map(|c| c.path), Some(cand.clone()), "initial {initial:o}");
        assert_eq!(mode(&cand), 0o700, "initial {initial:o}");
    }
}

#[test]
fn plain_unreadable_dir_reset_to_0700() {
    let tmp = tempfile::tempdir().unwrap();
    let e = env(tmp.path());
    let want = tmp.path().join("xdg/mdroots");
    std::fs::create_dir_all(&want).unwrap();
    let _guard = RestorePerms(want.clone());
    std::fs::set_permissions(&want, std::fs::Permissions::from_mode(0o000)).unwrap();
    assert_eq!(cache_dir(&StdProbe, &e).unwrap().path, want);
    assert_eq!(mode(&want), 0o700);
}

/// Make the write probe fail in `dir`: a dir sits where its file goes.
fn block_write_probe(dir: &Path) {
    std::fs::create_dir_all(dir.join(".mdroots-write-test")).unwrap();
}

#[test]
fn tmp_dir_rejected_by_write_probe_keeps_mode() {
    for initial in [0o755, 0o000] {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        let e = tmp_only(t, rustix::process::getuid().as_raw());
        let cand = t.join(format!("vartmp/mdroots-{}", e.uid));
        block_write_probe(&cand);
        let _guard = RestorePerms(cand.clone());
        std::fs::set_permissions(&cand, std::fs::Permissions::from_mode(initial)).unwrap();
        assert_eq!(cache_dir(&StdProbe, &e), None, "initial {initial:o}");
        assert_eq!(
            mode(&cand),
            initial,
            "initial {initial:o}: rejected dir chmodded"
        );
    }
}

#[test]
fn plain_dir_rejected_by_write_probe_keeps_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let e = env(tmp.path());
    let xdg = tmp.path().join("xdg/mdroots");
    block_write_probe(&xdg);
    std::fs::set_permissions(&xdg, std::fs::Permissions::from_mode(0o755)).unwrap();
    let c = cache_dir(&StdProbe, &e).unwrap();
    assert_ne!(c.path, xdg);
    assert!(
        c.reason
            .starts_with(&format!("{}: not writable", xdg.display())),
        "{}",
        c.reason
    );
    assert_eq!(mode(&xdg), 0o755, "rejected dir chmodded");
}
