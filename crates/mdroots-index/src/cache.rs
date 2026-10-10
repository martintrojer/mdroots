//! Choosing the cache dir (docs/DECISIONS.md D5): the first candidate that is
//! on a local filesystem and writable. flock and WAL are unsafe on network
//! filesystems, so a remote, virtual or cloud candidate is never created.

use std::io;
use std::path::{Path, PathBuf};

use mdroots_roots::probe::{FsClass, Probe, classify};

/// Name of the file created and removed to prove a candidate is writable.
const WRITE_TEST: &str = ".mdroots-write-test";

/// The environment the cache dir is chosen from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEnv {
    pub xdg_cache_home: Option<PathBuf>,
    pub home: Option<PathBuf>,
    pub xdg_runtime_dir: Option<PathBuf>,
    pub uid: u32,
    /// Parent of the last-resort `mdroots-<uid>` dir; `/var/tmp` from the
    /// environment, a tempdir in tests.
    pub tmp_dir: PathBuf,
}

impl CacheEnv {
    /// Read `XDG_CACHE_HOME`, `HOME`, `XDG_RUNTIME_DIR` and the real uid.
    /// Empty variables count as unset.
    pub fn from_env() -> CacheEnv {
        let var = |k: &str| {
            std::env::var_os(k)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        CacheEnv {
            xdg_cache_home: var("XDG_CACHE_HOME"),
            home: var("HOME"),
            xdg_runtime_dir: var("XDG_RUNTIME_DIR"),
            uid: current_uid(),
            tmp_dir: PathBuf::from("/var/tmp"),
        }
    }
}

#[cfg(unix)]
fn current_uid() -> u32 {
    rustix::process::getuid().as_raw()
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
}

/// The chosen cache dir and why it was chosen.
#[derive(Debug, Clone, PartialEq)]
pub struct CacheDir {
    pub path: PathBuf,
    /// `"<candidate>: chosen"`, preceded by the rejected candidates and why.
    pub reason: String,
}

/// One candidate: the path and whether its owner must be checked.
struct Candidate {
    path: PathBuf,
    check_owner: bool,
}

fn candidates(env: &CacheEnv) -> Vec<Candidate> {
    let plain = |path: PathBuf| Candidate {
        path,
        check_owner: false,
    };
    let mut out = Vec::new();
    if let Some(x) = &env.xdg_cache_home {
        out.push(plain(x.join("mdroots")));
    }
    if let Some(h) = &env.home {
        let default = if cfg!(target_os = "macos") {
            h.join("Library/Caches/mdroots")
        } else {
            h.join(".cache/mdroots")
        };
        out.push(plain(default));
    }
    if let Some(r) = &env.xdg_runtime_dir {
        out.push(plain(r.join("mdroots")));
    }
    out.push(Candidate {
        path: env.tmp_dir.join(format!("mdroots-{}", env.uid)),
        check_owner: true,
    });
    out
}

/// Pick, create (mode 0700) and return the cache dir; `None` means no
/// candidate qualifies and the index stays in memory.
///
/// Each candidate's filesystem is classified on its nearest existing ancestor
/// before anything is created, so nothing is ever created on a rejected
/// mount.
pub fn cache_dir(probe: &dyn Probe, env: &CacheEnv) -> Option<CacheDir> {
    let mut rejected = Vec::new();
    for c in candidates(env) {
        match check(probe, env, &c) {
            Ok(()) => {
                rejected.push(format!("{}: chosen", c.path.display()));
                return Some(CacheDir {
                    path: c.path,
                    reason: rejected.join("; "),
                });
            }
            Err(why) => rejected.push(format!("{}: {why}", c.path.display())),
        }
    }
    None
}

/// `Ok` if `c` is usable (it then exists, mode 0700); else why not.
fn check(probe: &dyn Probe, env: &CacheEnv, c: &Candidate) -> Result<(), String> {
    let anc = c
        .path
        .ancestors()
        .find(|a| probe.stat(a).is_ok())
        .ok_or("no existing ancestor")?;
    let m = probe
        .mount(anc)
        .map_err(|e| format!("mount of {}: {e}", anc.display()))?;
    match classify(&m, &c.path, env.home.as_deref()) {
        FsClass::Local => {}
        FsClass::Cloud => return Err("cloud folder".into()),
        FsClass::Virtual(t) => return Err(format!("virtual filesystem ({t})")),
        FsClass::Remote(t) => return Err(format!("not local ({t})")),
        other => return Err(format!("not local ({other:?})")),
    }
    if c.check_owner {
        create_owned_dir(&c.path, env.uid)?;
    } else {
        create_missing(&c.path).map_err(|e| format!("create: {e}"))?;
    }
    probe_then_make_private(&c.path)
}

/// Create `path` and its missing parents, each new one mode 0700 (before
/// umask). An existing dir keeps its mode.
fn create_missing(path: &Path) -> io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(path)
}

/// Prove `dir` writable, then make it 0700, so a dir rejected here keeps its
/// mode. A dir its owner made unwritable (0500, 0000) is made 0700 to probe
/// it again; if that probe fails too, its mode is put back.
fn probe_then_make_private(dir: &Path) -> Result<(), String> {
    let why = |(what, e): (&str, io::Error)| format!("{what}: {e}");
    match write_probe(dir) {
        Ok(()) => {}
        Err((_, e)) if e.kind() == io::ErrorKind::PermissionDenied => {
            let before = std::fs::metadata(dir)
                .map_err(|e| format!("stat: {e}"))?
                .permissions();
            // Not the owner: chmod fails and the first error is the reason.
            if set_private(dir).is_err() {
                return Err(why(("not writable", e)));
            }
            if let Err(err) = write_probe(dir) {
                let _ = std::fs::set_permissions(dir, before);
                return Err(why(err));
            }
        }
        Err(err) => return Err(why(err)),
    }
    set_private(dir).map_err(|e| format!("chmod 0700: {e}"))
}

/// Create and remove [`WRITE_TEST`] in `dir`; on failure, the failed step.
fn write_probe(dir: &Path) -> Result<(), (&'static str, io::Error)> {
    let file = dir.join(WRITE_TEST);
    std::fs::write(&file, b"").map_err(|e| ("not writable", e))?;
    std::fs::remove_file(&file).map_err(|e| ("remove write test", e))
}

#[cfg(unix)]
fn set_private(p: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private(_: &Path) -> io::Result<()> {
    Ok(())
}

/// Create `path` in a shared dir such as `/var/tmp`, where another user may
/// have planted it first, and accept it only as a real dir owned by `uid`.
/// A symlink is never followed. The check is on the entry itself (no read
/// or search permission needed, so an owner-only 0000 dir passes); later
/// steps reach the dir by path, which in a sticky dir only its owner, the
/// parent's owner or root can replace.
#[cfg(unix)]
fn create_owned_dir(path: &Path, uid: u32) -> Result<(), String> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    if let Some(parent) = path.parent() {
        // Missing parents only: an existing shared parent keeps its mode.
        create_missing(parent).map_err(|e| format!("create: {e}"))?;
    }
    // mkdir never follows a symlink in the last component.
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("create: {e}")),
    }
    let entry = std::fs::symlink_metadata(path).map_err(|e| format!("stat: {e}"))?;
    if entry.file_type().is_symlink() {
        return Err("symlink, not followed".into());
    }
    if !entry.is_dir() {
        return Err("not a directory".into());
    }
    if entry.uid() != uid {
        return Err(format!("not owned by uid {uid}"));
    }
    Ok(())
}

#[cfg(not(unix))]
fn create_owned_dir(path: &Path, _: u32) -> Result<(), String> {
    create_missing(path).map_err(|e| format!("create: {e}"))
}
