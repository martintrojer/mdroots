//! The persistent index behind a [`Workspace`](crate::Workspace): the cache
//! dir, the shared root registry, the per-root DB and its reconciler lock
//! (docs/specs/index.md §1.5–§1.7, docs/specs/roots.md §5).
//!
//! Everything here is synchronous: M4 runs no background thread, so a
//! short-lived process (`nvim +wq`, CI, a commit-message editor) does its
//! reconcile inside `open_for` and `refresh` and leaves nothing running.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use mdroots_core::{Cancel, Error, FileSystem};
use mdroots_index::{IndexDb, Role, RootLocks, SCHEMA, SqliteRegistry, reconcile};
use mdroots_roots::RootMode;
use mdroots_roots::discover::{Enumerator, list_root};
use mdroots_roots::probe::Probe;

use crate::workspace::{IndexMode, Options, working_set};

/// Root-relative paths and their bytes.
pub(crate) type Contents = Vec<(String, Arc<[u8]>)>;

/// The cache dir and the registry in it, shared by every workspace of one
/// [`Workspaces`](crate::Workspaces).
#[derive(Clone)]
pub(crate) struct CacheCtx {
    pub(crate) dir: PathBuf,
    registry: Arc<Mutex<SqliteRegistry>>,
}

impl CacheCtx {
    /// The cache `opts` ask for, or `None` for an in-memory index:
    /// [`IndexMode::Memory`]; `Auto` with an explicit fs or probe (tests on
    /// in-memory trees) and no explicit cache dir; no usable cache dir; or a
    /// registry that does not open (the cache is optional, so errors fall
    /// back to memory).
    pub(crate) fn for_options(opts: &Options) -> Option<CacheCtx> {
        let dir = match (opts.index_mode(), opts.explicit_cache_dir()) {
            (IndexMode::Memory, _) => return None,
            // Created (0700) by the registry open below.
            (_, Some(d)) => d.to_path_buf(),
            (_, None) if opts.has_explicit_io() => return None,
            (_, None) => default_cache_dir()?,
        };
        let registry = SqliteRegistry::open(&dir.join("roots.v1.db")).ok()?;
        Some(CacheCtx {
            dir,
            registry: Arc::new(Mutex::new(registry)),
        })
    }

    pub(crate) fn registry(&self) -> MutexGuard<'_, SqliteRegistry> {
        self.registry.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Open the DB of the registered root `root_id` under `<cache>/roots`,
    /// taking its locks and recording its file name in the registry.
    pub(crate) fn open_index(&self, root_id: &str) -> Result<IndexState, Error> {
        let dir = self.dir.join("roots");
        std::fs::create_dir_all(&dir)?;
        let stem = format!("{root_id}.v{SCHEMA}");
        let name = format!("{stem}.db");
        self.registry().set_db_file(root_id, &name);
        let locks = RootLocks::acquire(&dir, &stem)?;
        let db_path = dir.join(&name);
        let db = IndexDb::open(&db_path)?;
        Ok(IndexState { db, locks, db_path })
    }
}

#[cfg(unix)]
fn default_cache_dir() -> Option<PathBuf> {
    let env = mdroots_index::CacheEnv::from_env();
    mdroots_index::cache_dir(&mdroots_roots::probe::StdProbe, &env).map(|c| c.path)
}

#[cfg(not(unix))]
fn default_cache_dir() -> Option<PathBuf> {
    None
}

/// One root's open DB and this process's role for it.
pub(crate) struct IndexState {
    pub(crate) db: IndexDb,
    pub(crate) locks: RootLocks,
    pub(crate) db_path: PathBuf,
}

/// How a workspace re-lists its files on refresh.
pub(crate) enum Listing {
    /// A known root: [`list_root`] for its mode.
    Root(RootMode),
    /// Opened at a directory: walk it.
    Walk,
    /// A lazy root: the working set of the opened file's directory.
    WorkingSet { dir: PathBuf, name: String },
    /// Only the opened file.
    Single,
}

/// What listing and reconciling read through.
pub(crate) struct Io<'a> {
    pub(crate) fs: &'a dyn FileSystem,
    pub(crate) probe: &'a dyn Probe,
    pub(crate) enumerator: &'a dyn Enumerator,
    pub(crate) root: &'a Path,
}

impl Listing {
    /// The files of the root now, root-relative, without the opened file
    /// unless the listing finds it; `None` when the root cannot be listed
    /// (over budget, aborted, unreadable).
    pub(crate) fn list(&self, io: &Io<'_>, opened: &str, cancel: &Cancel) -> Option<Vec<String>> {
        match self {
            Listing::Root(mode) => list_root(io.probe, io.root, *mode, io.enumerator, cancel),
            Listing::Walk => mdroots_core::walk_md(io.fs, io.root, cancel).ok(),
            Listing::WorkingSet { dir, name } => {
                Some(working_set(io.fs, io.root, dir, name, opened))
            }
            Listing::Single => Some(Vec::new()),
        }
    }

    fn is_lazy(&self) -> bool {
        matches!(self, Listing::WorkingSet { .. })
    }
}

/// Bring the DB up to date (when this process is the reconciler) and return
/// every note's current bytes.
///
/// - Reconciler, lazy root: the DB's file set plus the working-set files not
///   in it (never removing rows outside the working set).
/// - Reconciler, other roots: `discovered` (what discovery listed this
///   call), else a fresh listing, plus the opened file; rows not listed are
///   removed. An unlistable root keeps the DB's file set.
/// - Peer: the DB's file set, re-statting files and reading changed ones
///   without writing; a peer of a still-empty DB (another process is
///   indexing) uses the working set, else `discovered`, else `current` (the
///   files it already serves).
///
/// The opened file is read when it has no row or is dataless.
pub(crate) fn sync(
    io: &Io<'_>,
    ix: &mut IndexState,
    listing: &Listing,
    opened: &str,
    discovered: Option<Vec<String>>,
    current: &[String],
    cancel: &Cancel,
) -> Result<Contents, Error> {
    let rows = ix.db.rows()?;
    let known: BTreeSet<&str> = rows.iter().map(|r| r.path.as_str()).collect();
    let mut force = Vec::new();
    let dataless = io.fs.stat(&io.root.join(opened)).is_ok_and(|m| m.dataless);
    if !known.contains(opened) || dataless {
        force.push(opened.to_owned());
    }
    let working_set_missing = || -> Vec<String> {
        listing
            .list(io, opened, cancel)
            .unwrap_or_default()
            .into_iter()
            .filter(|f| !known.contains(f.as_str()) && f != opened)
            .collect()
    };
    let (contents, _) = match ix.locks.role() {
        Role::Reconciler if listing.is_lazy() => {
            force.extend(working_set_missing());
            reconcile(
                Some(&mut ix.db),
                &rows,
                io.fs,
                io.root,
                None,
                &force,
                cancel,
            )?
        }
        Role::Reconciler => {
            let listed = discovered
                .filter(|d| !d.is_empty())
                .or_else(|| listing.list(io, opened, cancel))
                .map(|mut l| {
                    l.push(opened.to_owned());
                    l
                });
            let db = Some(&mut ix.db);
            reconcile(db, &rows, io.fs, io.root, listed.as_deref(), &force, cancel)?
        }
        Role::Peer if rows.is_empty() => {
            // Like the reconciler: a registry hit (empty decision list)
            // re-lists the root; the current set is the last resort.
            let mut listed = match listing.is_lazy() {
                true => listing.list(io, opened, cancel).unwrap_or_default(),
                false => discovered
                    .filter(|d| !d.is_empty())
                    .or_else(|| listing.list(io, opened, cancel))
                    .unwrap_or_else(|| current.to_vec()),
            };
            listed.push(opened.to_owned());
            reconcile(None, &[], io.fs, io.root, Some(&listed), &force, cancel)?
        }
        Role::Peer => {
            if listing.is_lazy() {
                force.extend(working_set_missing());
            }
            reconcile(None, &rows, io.fs, io.root, None, &force, cancel)?
        }
    };
    Ok(contents)
}
