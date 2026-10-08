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
