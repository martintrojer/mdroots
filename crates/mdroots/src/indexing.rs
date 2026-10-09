//! The persistent index behind a [`Workspace`](crate::Workspace): the cache
//! dir, the shared root registry, the per-root DB and its reconciler lock
//! (docs/specs/index.md §1.5–§1.7, docs/specs/roots.md §5).
//!
//! Everything here is synchronous: a short-lived process (`nvim +wq`, CI, a
//! commit-message editor) does its reconcile inside `open_for` and `refresh`
//! and leaves nothing running. The only background thread is the opt-in
//! watcher (`watch.rs`), which calls the same code.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use mdroots_core::{Cancel, Error, ErrorKind, FileSystem};
use mdroots_index::gc::is_db_file_of;
use mdroots_index::{GcOptions, IndexDb, Role, RootLocks, SCHEMA, SqliteRegistry, gc, reconcile};
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
    /// taking its locks, and stamp the root as seen.
    ///
    /// The file is the registry's `db_file` when it names one of this
    /// schema for the root (`<id>.v<SCHEMA>.db` or a rebuilt generation
    /// `<id>.v<SCHEMA>-<gen8>.db`); otherwise `<id>.v<SCHEMA>.db`, which is
    /// then recorded. Every generation shares the lock files of
    /// `<id>.v<SCHEMA>`. A corrupt file is never renamed or unlinked: the
    /// reconciler builds a new generation (recorded after its first sync,
    /// see [`IndexState::publish`]); a peer serves from an empty in-memory
    /// DB until a refresh finds a good file.
    pub(crate) fn open_index(&self, root_id: &str, now_ms: u64) -> Result<IndexState, Error> {
        let dir = self.dir.join("roots");
        std::fs::create_dir_all(&dir)?;
        let stem = format!("{root_id}.v{SCHEMA}");
        let name = {
            let mut reg = self.registry();
            reg.touch(root_id, now_ms);
            match reg.db_file(root_id) {
                Some(n) if is_db_file_of(&n, root_id) => n,
                _ => {
                    let n = format!("{stem}.db");
                    reg.set_db_file(root_id, &n);
                    n
                }
            }
        };
        let locks = RootLocks::acquire(&dir, &stem)?;
        let role = locks.role();
        let opened = open_db(&dir, &stem, &name, role)?;
        Ok(IndexState {
            db_path: dir.join(opened.name.as_deref().unwrap_or(&name)),
            db: opened.db,
            locks,
            ctx: self.clone(),
            root_id: root_id.to_owned(),
            name: opened.name,
            pending: opened.rebuilt,
            checked: role == Role::Reconciler,
        })
    }

    /// The DB file name the registry records for `root_id`, if it is one of
    /// this schema for the root.
    fn recorded(&self, root_id: &str) -> Option<String> {
        self.registry()
            .db_file(root_id)
            .filter(|n| is_db_file_of(n, root_id))
    }

    /// Run [`gc`] with the defaults if the cache's last run was a day ago
    /// or more (a cheap registry read decides; GC itself claims the run).
    /// Errors are ignored: GC is housekeeping.
    pub(crate) fn maybe_gc(&self, now_ms: u64) {
        if self.registry().gc_due(now_ms) {
            let _ = gc(&self.dir, now_ms, GcOptions::default());
        }
    }
}

/// What [`open_db`] opened.
struct Opened {
    db: IndexDb,
    /// The file name; `None` for the in-memory stand-in.
    name: Option<String>,
    /// A new generation was built (it is still empty).
    rebuilt: bool,
}

/// Open `<dir>/<name>` for `role`. The reconciler checks it with
/// `PRAGMA quick_check` and, if it is corrupt, builds a new generation
/// file instead; a peer of a corrupt file gets an empty in-memory DB.
fn open_db(dir: &Path, stem: &str, name: &str, role: Role) -> Result<Opened, Error> {
    let db = IndexDb::open(&dir.join(name)).and_then(|db| match role {
        Role::Reconciler => db.quick_check().map(|()| db),
        Role::Peer => Ok(db),
    });
    match (db, role) {
        (Ok(db), _) => Ok(Opened {
            db,
            name: Some(name.to_owned()),
            rebuilt: false,
        }),
        (Err(e), Role::Reconciler) if e.kind() == ErrorKind::Corrupt => {
            let (db, name) = IndexDb::open_new_generation(dir, stem)?;
            Ok(Opened {
                db,
                name: Some(name),
                rebuilt: true,
            })
        }
        (Err(e), Role::Peer) if e.kind() == ErrorKind::Corrupt => Ok(Opened {
            db: IndexDb::open_in_memory()?,
            name: None,
            rebuilt: false,
        }),
        (Err(e), _) => Err(e),
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
    /// The DB file (for the in-memory stand-in, the file it stands in for).
    pub(crate) db_path: PathBuf,
    ctx: CacheCtx,
    root_id: String,
    /// The file name `db` was opened from; `None` for the in-memory
    /// stand-in of a corrupt file.
    name: Option<String>,
    /// `name` is a rebuilt generation not yet recorded in the registry.
    pending: bool,
    /// The reconciler ran `quick_check` on `db`.
    checked: bool,
}

impl IndexState {
    /// Before a refresh: try to become the reconciler, stamp the root as
    /// seen, and reopen when the registry names another DB file than the
    /// one held (a peer after a rebuild) or the held DB is the in-memory
    /// stand-in. A newly promoted reconciler checks the DB it took over
    /// and rebuilds it if corrupt. The lock files stay the same.
    ///
    /// Returns whether `db` was replaced (its rows are not the ones a
    /// point refresh can patch).
    pub(crate) fn revalidate(&mut self, now_ms: u64) -> Result<bool, Error> {
        let role = self.locks.try_promote()?;
        let dir = self.ctx.dir.join("roots");
        let stem = format!("{}.v{SCHEMA}", self.root_id);
        self.ctx.registry().touch(&self.root_id, now_ms);
        if self.pending {
            // Our own rebuild, not yet filled: keep it.
            return Ok(true);
        }
        let wanted = self
            .ctx
            .recorded(&self.root_id)
            .unwrap_or_else(|| format!("{stem}.db"));
        let stale = self.name.as_deref() != Some(wanted.as_str());
        let unchecked = role == Role::Reconciler && !self.checked;
        if stale || unchecked {
            let opened = match (stale, &self.name) {
                (false, Some(name)) => open_db(&dir, &stem, name, role)?,
                _ => open_db(&dir, &stem, &wanted, role)?,
            };
            self.db_path = dir.join(opened.name.as_deref().unwrap_or(&wanted));
            self.db = opened.db;
            self.name = opened.name;
            self.pending = opened.rebuilt;
        }
        self.checked = role == Role::Reconciler;
        Ok(stale || self.pending)
    }

    /// The held DB turned out corrupt mid-use: the reconciler builds a new
    /// generation, a peer switches to the in-memory stand-in.
    fn replace_corrupt(&mut self) -> Result<(), Error> {
        let dir = self.ctx.dir.join("roots");
        match self.locks.role() {
            Role::Reconciler => {
                let stem = format!("{}.v{SCHEMA}", self.root_id);
                let (db, name) = IndexDb::open_new_generation(&dir, &stem)?;
                self.db_path = dir.join(&name);
                self.db = db;
                self.name = Some(name);
                self.pending = true;
                self.checked = true;
            }
            Role::Peer => {
                self.db = IndexDb::open_in_memory()?;
                self.name = None;
            }
        }
        Ok(())
    }

    /// After a successful sync: record a rebuilt generation in the
    /// registry, now that it holds the root's notes, so peers switch to a
    /// full DB.
    pub(crate) fn publish(&mut self) {
        if let (true, Some(name)) = (self.pending, &self.name) {
            self.ctx.registry().set_db_file(&self.root_id, name);
            self.pending = false;
        }
    }
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
///
/// A corrupt DB is replaced (see [`IndexState::replace_corrupt`]) and the
/// sync retried once; a rebuilt generation is published when it succeeds.
pub(crate) fn sync(
    io: &Io<'_>,
    ix: &mut IndexState,
    listing: &Listing,
    opened: &str,
    discovered: Option<Vec<String>>,
    current: &[String],
    cancel: &Cancel,
) -> Result<Contents, Error> {
    let first = sync_once(io, ix, listing, opened, discovered.clone(), current, cancel);
    let contents = match first {
        Err(e) if e.kind() == ErrorKind::Corrupt => {
            ix.replace_corrupt()?;
            sync_once(io, ix, listing, opened, discovered, current, cancel)?
        }
        r => r?,
    };
    ix.publish();
    Ok(contents)
}

fn sync_once(
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
