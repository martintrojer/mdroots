//! [`Workspace`]: one root, discovered and indexed in memory.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mdroots_core::{Cancel, Diagnostic, Error, ErrorKind, FileSystem, MemStore};
use mdroots_resolve::ResolveStep;
use mdroots_resolve::dialect::{Setting, explain};
use mdroots_resolve::ladder::{LinkStatus, outside_rel};
use mdroots_roots::discover::{DiscoverOptions, Enumerator, discover};
use mdroots_roots::probe::Probe;
use mdroots_roots::registry::{DiscoverLock, Registry};
use mdroots_roots::{MemRegistry, RootMode};
use mdroots_syntax::{Context, Dialect, Document, Link, LinkKind, PositionEncoding, parse};

use crate::indexing::{self, CacheCtx, IndexState, Listing};
use crate::watch;

/// Most entries a lazy working set takes from the opened file's directory.
const WORKING_SET_CAP: usize = 2_000;

/// Where a workspace keeps its index.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IndexMode {
    /// A per-root [SQLite](https://sqlite.org) DB in the cache dir: the
    /// [`Options::cache_dir`] if set; else in memory when `fs` or `probe`
    /// was set (in-memory trees); else the user's cache dir
    /// (docs/specs/roots.md §5), or memory if none is usable.
    #[default]
    Auto,
    /// In memory only; the cache dir is never touched.
    Memory,
}

/// How to open a [`Workspace`]. `fs` and `probe` are set together or not at
/// all (then [`StdFs`](mdroots_core::StdFs) and
/// [`StdProbe`](mdroots_roots::probe::StdProbe)).
#[derive(Clone, Default)]
pub struct Options {
    workspace_folders: Vec<PathBuf>,
    enumerator: Option<Arc<dyn Enumerator>>,
    cancel: Cancel,
    fs: Option<Arc<dyn FileSystem>>,
    probe: Option<Arc<dyn Probe>>,
    index: IndexMode,
    cache_dir: Option<PathBuf>,
    watch: bool,
    code_dirs: Vec<PathBuf>,
    /// The cache a [`Workspaces`](crate::Workspaces) shares with its
    /// workspaces: `Some(None)` is a resolved in-memory index.
    shared: Option<Option<CacheCtx>>,
}

impl Options {
    /// Editor workspace folders, absolute: they bound the marker climb.
    pub fn workspace_folders(mut self, v: Vec<PathBuf>) -> Self {
        self.workspace_folders = v;
        self
    }

    /// Lists the markdown of a virtual checkout (vcs-enumerated mode).
    /// Default [`SlFiles`](mdroots_roots::SlFiles) (runs
    /// [Sapling](https://sapling-scm.com/)); [`NoEnumerator`](crate::NoEnumerator)
    /// turns enumeration off.
    pub fn enumerator(mut self, e: Arc<dyn Enumerator>) -> Self {
        self.enumerator = Some(e);
        self
    }

    /// Checked during discovery and indexing.
    pub fn cancel(mut self, c: Cancel) -> Self {
        self.cancel = c;
        self
    }

    pub fn fs(mut self, fs: Arc<dyn FileSystem>) -> Self {
        self.fs = Some(fs);
        self
    }

    pub fn probe(mut self, p: Arc<dyn Probe>) -> Self {
        self.probe = Some(p);
        self
    }

    /// Where the index lives; default [`IndexMode::Auto`].
    pub fn index(mut self, m: IndexMode) -> Self {
        self.index = m;
        self
    }

    /// The cache dir to use instead of the user's (tests use a temp dir).
    /// Created if missing. Ignored with [`IndexMode::Memory`].
    pub fn cache_dir(mut self, p: PathBuf) -> Self {
        self.cache_dir = Some(p);
        self
    }

    /// Watch the root for changes on disk (default off). Only a workspace
    /// that is the reconciler of a DB-backed marker, VCS or loose root on a
    /// local filesystem watches; see [`Workspace::watching`]. The
    /// [notify](https://crates.io/crates/notify) watcher and its thread
    /// stop when the last clone of the workspace drops.
    pub fn watch(mut self, on: bool) -> Self {
        self.watch = on;
        self
    }

    /// Extra dirs, absolute, that code mentions
    /// ([`LinkKind::CodeMention`]) resolve against: a relative mention tries
    /// the linking note's dir, then these in order, then the root. Each is
    /// canonicalized when a workspace opens; non-absolute dirs and dirs that
    /// cannot be canonicalized are ignored, duplicates dropped (first kept).
    /// A hit outside the root is a [`LinkStatus::Unindexed`] target.
    pub fn code_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.code_dirs = dirs;
        self
    }

    /// [`code_dirs`](Self::code_dirs), canonical and deduplicated.
    fn canonical_code_dirs(&self, fs: &dyn FileSystem) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        for d in self.code_dirs.iter().filter(|d| d.is_absolute()) {
            if let Ok(c) = fs.canonicalize(d)
                && !out.contains(&c)
            {
                out.push(c);
            }
        }
        out
    }

    pub(crate) fn index_mode(&self) -> IndexMode {
        self.index
    }

    pub(crate) fn explicit_cache_dir(&self) -> Option<&Path> {
        self.cache_dir.as_deref()
    }

    pub(crate) fn has_explicit_io(&self) -> bool {
        self.fs.is_some() || self.probe.is_some()
    }

    /// Resolve the cache once, for every workspace opened with the result.
    pub(crate) fn with_shared_cache(mut self) -> Self {
        if self.shared.is_none() {
            self.shared = Some(CacheCtx::for_options(&self));
        }
        self
    }

    fn cache(&self) -> Option<CacheCtx> {
        match &self.shared {
            Some(c) => c.clone(),
            None => CacheCtx::for_options(self),
        }
    }

    pub(crate) fn io(&self) -> Result<Io, Error> {
        match (&self.fs, &self.probe) {
            (Some(fs), Some(p)) => Ok((fs.clone(), p.clone())),
            (None, None) => default_io(),
            _ => Err(Error::new(
                ErrorKind::Unsupported,
                "fs and probe must both be set",
            )),
        }
    }

    fn enumerator_or_default(&self) -> Arc<dyn Enumerator> {
        self.enumerator
            .clone()
            .unwrap_or_else(|| Arc::new(mdroots_roots::SlFiles))
    }
}

/// The filesystem and probe a workspace reads through.
pub(crate) type Io = (Arc<dyn FileSystem>, Arc<dyn Probe>);

#[cfg(unix)]
fn default_io() -> Result<Io, Error> {
    Ok((
        Arc::new(mdroots_core::StdFs),
        Arc::new(mdroots_roots::probe::StdProbe),
    ))
}

#[cfg(not(unix))]
fn default_io() -> Result<Io, Error> {
    Err(Error::new(
        ErrorKind::Unsupported,
        "no default filesystem on this platform: set fs and probe",
    ))
}

/// The root a workspace serves and why it was chosen.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct RootInfo {
    /// Canonical, absolute.
    pub path: PathBuf,
    pub mode: RootMode,
    /// One line, as discovery explains it.
    pub reason: String,
    pub nested_roots: Vec<PathBuf>,
}

/// Whether the index covers the whole root.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Every note of the root is indexed.
    Fresh,
    /// A working set only (lazy and single-file roots).
    Lazy,
}

/// The outcome of [`Workspace::resolve`].
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Resolution {
    /// Absolute, best first; may lie outside the root.
    pub targets: Vec<PathBuf>,
    pub step: Option<ResolveStep>,
    pub status: LinkStatus,
    /// Found by the Partial step only.
    pub hint: bool,
}

/// A link to a note, seen from the note it is in.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Backlink {
    /// The note containing the link, absolute.
    pub from: PathBuf,
    /// Frontmatter title, else the first level-1 heading, else the file stem.
    pub from_title: String,
    /// Byte range of the link in `from`.
    pub range: Range<usize>,
    /// 0-based line of `range.start`.
    pub line: u32,
    pub in_code: bool,
}

#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct NoteSummary {
    pub path: PathBuf,
    pub title: String,
    /// Tag names, deduplicated, in document order.
    pub tags: Vec<String>,
    /// The file's modification time on disk; `None` when unknown (an
    /// overlay-only note, a failed stat, or a filesystem without times).
    pub modified: Option<SystemTime>,
    /// When the note was created: its frontmatter `date` (zk's creation
    /// key), else `created`, when either parses as `YYYY-MM-DD`,
    /// `YYYY-MM-DD[T ]HH:MM[:SS]` or RFC 3339 (a time without an offset is
    /// UTC); else the file's birth time ([`FileSystem::created`]). `None` when
    /// neither is known. Birth times do not survive a copy or a
    /// `git clone`, which is why the frontmatter wins.
    pub created: Option<SystemTime>,
}

/// One link of a document, resolved.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct DocLink {
    pub range: Range<usize>,
    pub text_range: Range<usize>,
    pub kind: LinkKind,
    pub context: Context,
    /// The best target, absolute, when Resolved, Ambiguous or Unindexed.
    pub target: Option<PathBuf>,
    /// The text after the first `#` of the target as written (`^b` for a
    /// block anchor); `None` when there is none or it is empty.
    pub anchor: Option<String>,
    /// Line of a code mention `path:LINE`.
    pub line: Option<u32>,
    pub status: LinkStatus,
}

pub(crate) struct Inner {
    fs: Arc<dyn FileSystem>,
    probe: Arc<dyn Probe>,
    enumerator: Arc<dyn Enumerator>,
    root: RootInfo,
    freshness: Freshness,
    /// A single-file or lazy-without-root workspace: only its opened file
    /// belongs to it (see [`Workspaces`](crate::Workspaces)).
    single: bool,
    /// How [`Workspace::refresh`] re-lists the root.
    listing: Listing,
    /// The file the workspace was opened for, root-relative (empty for
    /// [`Workspace::open_at`]).
    opened: String,
    /// The persistent index; `None` in memory mode.
    index: Mutex<Option<IndexState>>,
    /// [`Options::code_dirs`], canonical; re-applied after a refresh
    /// rebuilds the store.
    code_dirs: Vec<PathBuf>,
    /// Editor overlays by root-relative path, re-applied after a refresh
    /// rebuilds the store.
    overlays: Mutex<BTreeMap<String, String>>,
    store: RwLock<MemStore>,
    /// [`Options::watch`] was set.
    want_watch: bool,
    /// The running watcher; dropping it closes the watch thread's channel.
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
    /// Receivers of [`Workspace::subscribe`].
    subscribers: Mutex<Vec<Sender<Vec<PathBuf>>>>,
}

/// What [`Workspace::new`] assembles.
struct Parts {
    fs: Arc<dyn FileSystem>,
    probe: Arc<dyn Probe>,
    enumerator: Arc<dyn Enumerator>,
    root: RootInfo,
    freshness: Freshness,
    single: bool,
    listing: Listing,
    opened: String,
    index: Option<IndexState>,
    store: MemStore,
    watch: bool,
    code_dirs: Vec<PathBuf>,
}

/// One root, served from memory and, unless in memory mode, cached in a
/// per-root DB that one process (the reconciler) writes and the others
/// (peers) read. Cheap to clone; `Send + Sync`.
///
/// All work is synchronous: unless [`Options::watch`] is set there is no
/// background thread, so a short-lived process leaves nothing running
/// (docs/specs/index.md §1.5). Changes on disk are picked up by
/// [`Workspace::refresh`], [`Workspace::refresh_paths`], or the watcher.
#[derive(Clone)]
pub struct Workspace {
    inner: Arc<Inner>,
}

const _: () = {
    const fn assert<T: Clone + Send + Sync>() {}
    assert::<Workspace>();
};

impl Workspace {
    /// Discover the root of the existing file `path` and index it: the
    /// whole root, or for lazy and single-file decisions a working set (the
    /// file's directory, one level; or the file alone). The opened file is
    /// always indexed, and read even if dataless.
    ///
    /// With a cache (see [`IndexMode`]) discovery runs under the cache's
    /// `discover.lock` with the persistent registry, and a registered root
    /// is served from its DB: the reconciler (first process) re-lists and
    /// writes it, a peer reads the DB and re-reads only changed files. A
    /// single-file or rootless workspace, or a root without a registry row,
    /// stays in memory.
    pub fn open_for(path: &Path, opts: Options) -> Result<Workspace, Error> {
        let (fs, probe) = opts.io()?;
        let cancel = &opts.cancel;
        cancel.check()?;
        let file = fs.canonicalize(path)?;
        let meta = fs.stat(&file)?;
        if !meta.is_file {
            return Err(Error::new(
                ErrorKind::Io,
                format!("not a file: {}", file.display()),
            ));
        }
        let dopts = DiscoverOptions {
            workspace_folders: opts.workspace_folders.clone(),
            now_ms: now_ms(),
            ..Default::default()
        };
        let enumerator = opts.enumerator_or_default();
        let cache = opts.cache();
        let (d, root_id) = match &cache {
            Some(c) => {
                let _lock = DiscoverLock::acquire(&c.dir)?;
                let mut reg = c.registry();
                let d = discover(&*probe, &mut *reg, &*enumerator, &file, &dopts, cancel);
                let id = d.root.as_deref().and_then(|r| {
                    reg.lookup(r)
                        .filter(|rec| rec.path == r)
                        .map(|rec| rec.root_id)
                });
                (d, id)
            }
            None => {
                let mut reg = MemRegistry::new();
                let d = discover(&*probe, &mut reg, &*enumerator, &file, &dopts, cancel);
                (d, None)
            }
        };
        // Discovery reports cancellation as a lazy decision.
        cancel.check()?;
        let parent = file.parent().map(Path::to_path_buf).unwrap_or_default();
        let name = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let in_root = d.root.as_deref().and_then(|r| {
            file.strip_prefix(r)
                .ok()
                .map(|rel| (r.to_path_buf(), slash(rel)))
        });
        let (root, listing, opened, freshness, single) = match (d.mode, in_root) {
            (RootMode::Lazy, Some((root, rel))) => {
                let l = Listing::WorkingSet {
                    dir: parent,
                    name: name.clone(),
                };
                (root, l, rel, Freshness::Lazy, false)
            }
            (RootMode::Lazy | RootMode::SingleFile, _) | (_, None) => {
                (parent, Listing::Single, name, Freshness::Lazy, true)
            }
            (mode, Some((root, rel))) => (root, Listing::Root(mode), rel, Freshness::Fresh, false),
        };
        let index = match (&cache, root_id) {
            (Some(c), Some(id)) if !single => Some(c.open_index(&id, dopts.now_ms)?),
            _ => None,
        };
        let (index, store) = match index {
            Some(mut ix) => {
                let io = indexing::Io {
                    fs: &*fs,
                    probe: &*probe,
                    enumerator: &*enumerator,
                    root: &root,
                };
                let found =
                    indexing::sync(&io, &mut ix, &listing, &opened, Some(d.md), &[], cancel)?;
                let store = MemStore::from_contents(fs.clone(), root, found, cancel)?;
                (Some(ix), store)
            }
            None => {
                let files = match &listing {
                    Listing::WorkingSet { dir, name } => {
                        working_set(&*fs, &root, dir, name, &opened)
                    }
                    Listing::Single => vec![opened.clone()],
                    _ => {
                        let mut files = d.md;
                        files.push(opened.clone());
                        files
                    }
                };
                let store = MemStore::open_files(
                    fs.clone(),
                    root,
                    files,
                    std::slice::from_ref(&opened),
                    cancel,
                )?;
                (None, store)
            }
        };
        let info = RootInfo {
            path: store.root().to_path_buf(),
            mode: d.mode,
            reason: d.reason,
            nested_roots: d.nested_roots,
        };
        let code_dirs = opts.canonical_code_dirs(&*fs);
        let ws = Workspace::new(Parts {
            fs,
            probe,
            enumerator,
            root: info,
            freshness,
            single,
            listing,
            opened,
            index,
            store,
            watch: opts.watch,
            code_dirs,
        });
        ws.maybe_watch();
        // Housekeeping on this thread, after the workspace is ready; a cheap
        // registry read when GC is not due. Only over the real filesystem:
        // GC checks root paths on disk, which an explicit fs may not mirror.
        if let Some(c) = cache.as_ref().filter(|_| !opts.has_explicit_io()) {
            c.maybe_gc(now_ms());
        }
        Ok(ws)
    }

    /// Index the existing file `path` alone, without discovery: no cache
    /// dir, registry or discovery lock is touched, and nothing is cached.
    /// The root is the file's directory, the mode
    /// [`RootMode::SingleFile`], freshness [`Freshness::Lazy`]. Cheap enough
    /// to serve a file at once while [`Workspace::open_for`] runs elsewhere.
    pub fn open_single(path: &Path, opts: Options) -> Result<Workspace, Error> {
        let (fs, probe) = opts.io()?;
        opts.cancel.check()?;
        let file = fs.canonicalize(path)?;
        if !fs.stat(&file)?.is_file {
            return Err(Error::new(
                ErrorKind::Io,
                format!("not a file: {}", file.display()),
            ));
        }
        let parent = file.parent().map(Path::to_path_buf).unwrap_or_default();
        let name = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let store = MemStore::open_files(
            fs.clone(),
            parent,
            vec![name.clone()],
            std::slice::from_ref(&name),
            &opts.cancel,
        )?;
        let info = RootInfo {
            path: store.root().to_path_buf(),
            mode: RootMode::SingleFile,
            reason: "single-file: opened without discovery".to_owned(),
            nested_roots: Vec::new(),
        };
        let code_dirs = opts.canonical_code_dirs(&*fs);
        Ok(Workspace::new(Parts {
            fs,
            probe,
            enumerator: opts.enumerator_or_default(),
            root: info,
            freshness: Freshness::Lazy,
            single: true,
            listing: Listing::Single,
            opened: name,
            index: None,
            store,
            watch: false,
            code_dirs,
        }))
    }

    /// Index the directory `root` as a root, without discovery. Always in
    /// memory (M4 caches discovered roots only).
    pub fn open_at(root: &Path, opts: Options) -> Result<Workspace, Error> {
        let (fs, probe) = opts.io()?;
        let root = fs.canonicalize(root)?;
        let store = MemStore::open(fs.clone(), root, &opts.cancel)?;
        let path = store.root().to_path_buf();
        let info = RootInfo {
            reason: format!("opened at {}", path.display()),
            path,
            mode: RootMode::Marker,
            nested_roots: Vec::new(),
        };
        let code_dirs = opts.canonical_code_dirs(&*fs);
        Ok(Workspace::new(Parts {
            fs,
            probe,
            enumerator: opts.enumerator_or_default(),
            root: info,
            freshness: Freshness::Fresh,
            single: false,
            listing: Listing::Walk,
            opened: String::new(),
            index: None,
            store,
            watch: false,
            code_dirs,
        }))
    }

    /// Open the notebook containing the directory `dir`: the root found
    /// the way [`open_for`](Self::open_for) finds it for a note, so `dir`
    /// may be a subdirectory of a notebook (as `zk` treats a path argument
    /// as a filter inside the notebook). Discovery starts from the first
    /// note under `dir` (breadth first, by name, hidden entries skipped);
    /// when there is none, or the root found does not contain `dir` (the
    /// note sits in a nested root, or is opened alone), `dir` itself is
    /// opened with [`open_at`](Self::open_at). The result's root always
    /// contains `dir`.
    pub fn open_dir(dir: &Path, opts: Options) -> Result<Workspace, Error> {
        let (fs, _) = opts.io()?;
        opts.cancel.check()?;
        let dir = fs.canonicalize(dir)?;
        if !fs.stat(&dir)?.is_dir {
            return Err(Error::new(
                ErrorKind::Io,
                format!("not a directory: {}", dir.display()),
            ));
        }
        if let Some(note) = first_note(&*fs, &dir, &opts.cancel)? {
            let ws = Workspace::open_for(&note, opts.clone())?;
            if !ws.is_single() && dir.starts_with(&ws.root().path) {
                return Ok(ws);
            }
        }
        Workspace::open_at(&dir, opts)
    }

    fn new(mut p: Parts) -> Self {
        set_code_dirs(&mut p.store, &p.code_dirs);
        Workspace {
            inner: Arc::new(Inner {
                fs: p.fs,
                probe: p.probe,
                enumerator: p.enumerator,
                root: p.root,
                freshness: p.freshness,
                single: p.single,
                listing: p.listing,
                opened: p.opened,
                code_dirs: p.code_dirs,
                index: Mutex::new(p.index),
                overlays: Mutex::default(),
                store: RwLock::new(p.store),
                want_watch: p.watch,
                watcher: Mutex::default(),
                subscribers: Mutex::default(),
            }),
        }
    }

    pub(crate) fn from_inner(inner: Arc<Inner>) -> Self {
        Workspace { inner }
    }

    /// Start the watcher if [`Options::watch`] asked for it, it is not
    /// running, and [`watch::watch_eligible`] holds now. A root notify
    /// cannot watch is simply not watched.
    fn maybe_watch(&self) {
        let i = &*self.inner;
        if !i.want_watch || !matches!(i.listing, Listing::Root(_)) {
            return;
        }
        let mut watcher = i.watcher.lock().unwrap_or_else(|e| e.into_inner());
        if watcher.is_some() {
            return;
        }
        let (role, has_db) = {
            let index = self.index();
            (index.as_ref().map(|ix| ix.locks.role()), index.is_some())
        };
        let root = i.root.path.as_path();
        let Some(class) = watch::fs_class(&*i.probe, root) else {
            return;
        };
        if watch::watch_eligible(i.root.mode, role, has_db, &class) {
            *watcher = watch::start(Arc::downgrade(&self.inner), root);
        }
    }

    /// Whether a native watcher is running for the root (see
    /// [`Options::watch`]).
    pub fn watching(&self) -> bool {
        self.inner
            .watcher
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// A channel of the absolute paths each watcher-driven refresh changed
    /// (a lost-events rescan sends every indexed note). Explicit
    /// [`refresh`](Self::refresh) and [`refresh_paths`](Self::refresh_paths)
    /// calls send nothing: their caller already knows.
    pub fn subscribe(&self) -> Receiver<Vec<PathBuf>> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.subscribers().push(tx);
        rx
    }

    /// Send `paths` to every subscriber, dropping closed ones.
    pub(crate) fn notify_subscribers(&self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        self.subscribers()
            .retain(|tx| tx.send(paths.clone()).is_ok());
    }

    fn subscribers(&self) -> MutexGuard<'_, Vec<Sender<Vec<PathBuf>>>> {
        self.inner
            .subscribers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// This process's role for the root's DB; `None` in memory mode.
    pub fn role(&self) -> Option<mdroots_index::Role> {
        self.index().as_ref().map(|ix| ix.locks.role())
    }

    /// The root's DB file; `None` in memory mode.
    pub fn cache(&self) -> Option<PathBuf> {
        self.index().as_ref().map(|ix| ix.db_path.clone())
    }

    /// Pick up changes on disk, synchronously. A peer first tries to become
    /// the reconciler (the previous one may have exited). The reconciler
    /// re-lists the root (or the lazy working set) and writes what changed
    /// to the DB; a peer re-reads the DB and re-reads changed files without
    /// writing. In memory mode the root is re-listed and re-read. Overlays
    /// survive. The root is stamped as seen (at most hourly), and a process
    /// whose DB file the registry no longer names (another process rebuilt
    /// a corrupt one) reopens the new file first.
    pub fn refresh(&self, cancel: &Cancel) -> Result<(), Error> {
        cancel.check()?;
        let i = &*self.inner;
        let root = i.root.path.as_path();
        let io = indexing::Io {
            fs: &*i.fs,
            probe: &*i.probe,
            enumerator: &*i.enumerator,
            root,
        };
        // Lock order: index, overlays, store; the index is held until the
        // new store is in place, so point refreshes cannot interleave.
        let mut index = self.index();
        let mut store = {
            match index.as_mut() {
                Some(ix) => {
                    ix.revalidate(now_ms())?;
                    let current: Vec<String> = self.store().files().map(str::to_owned).collect();
                    let found =
                        indexing::sync(&io, ix, &i.listing, &i.opened, None, &current, cancel)?;
                    MemStore::from_contents(i.fs.clone(), root.to_path_buf(), found, cancel)?
                }
                None => {
                    let mut files = i
                        .listing
                        .list(&io, &i.opened, cancel)
                        .unwrap_or_else(|| self.store().files().map(str::to_owned).collect());
                    cancel.check()?;
                    let force: Vec<String> = match i.opened.is_empty() {
                        true => Vec::new(),
                        false => {
                            files.push(i.opened.clone());
                            vec![i.opened.clone()]
                        }
                    };
                    MemStore::open_files(i.fs.clone(), root.to_path_buf(), files, &force, cancel)?
                }
            }
        };
        set_code_dirs(&mut store, &i.code_dirs);
        let overlays = self.overlays();
        for (rel, text) in overlays.iter() {
            store.set_overlay(rel, text);
        }
        *self.store_mut() = store;
        drop(overlays);
        drop(index);
        // A peer that was promoted may watch now.
        self.maybe_watch();
        Ok(())
    }

    /// Pick up changes to just `paths` (absolute; outside the root they are
    /// ignored), synchronously: a note is re-read, added or dropped; an
    /// existing directory adds the notes under it (hidden and pruned dirs
    /// skipped) and re-checks the indexed ones; a gone path drops every
    /// indexed note at or under it. The reconciler writes the changes to
    /// the DB (a peer first tries to become it, as in
    /// [`refresh`](Self::refresh)). The store is updated in place, overlays
    /// survive. A single-file workspace only re-checks its file, a lazy one
    /// its working-set directory and indexed notes. Returns the absolute
    /// paths whose content changed, sorted; when the root's DB file was
    /// replaced (a corruption rebuild), a full [`refresh`](Self::refresh)
    /// runs instead and every indexed note is returned.
    pub fn refresh_paths(&self, paths: &[PathBuf], cancel: &Cancel) -> Result<Vec<PathBuf>, Error> {
        cancel.check()?;
        let i = &*self.inner;
        let root = i.root.path.as_path();
        let mut index = self.index();
        let candidates = self.candidates(paths, cancel)?;
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(ix) = index.as_mut()
            && ix.revalidate(now_ms())?
        {
            // A new DB file (a rebuild, or a peer following one): patching
            // a few paths is not enough, re-read everything.
            drop(index);
            self.refresh(cancel)?;
            return Ok(self.files());
        }
        let listed: Vec<String> = candidates.iter().cloned().collect();
        let contents = match index.as_mut() {
            Some(ix) => {
                let rows: Vec<_> = ix
                    .db
                    .rows()?
                    .into_iter()
                    .filter(|r| candidates.contains(&r.path))
                    .collect();
                let db = match ix.locks.role() {
                    mdroots_index::Role::Reconciler => Some(&mut ix.db),
                    mdroots_index::Role::Peer => None,
                };
                mdroots_index::reconcile(db, &rows, &*i.fs, root, Some(&listed), &[], cancel)?.0
            }
            None => {
                mdroots_index::reconcile(None, &[], &*i.fs, root, Some(&listed), &[], cancel)?.0
            }
        };
        let mut found: BTreeMap<String, Arc<[u8]>> = contents.into_iter().collect();
        let changes: Vec<(String, Option<Arc<[u8]>>)> = listed
            .into_iter()
            .filter_map(|rel| match found.remove(&rel) {
                Some(bytes) => Some((rel, Some(bytes))),
                // Still a file (dataless, unreadable): leave it as it is.
                None if i.fs.stat(&root.join(&rel)).is_ok_and(|m| m.is_file) => None,
                None => Some((rel, None)),
            })
            .collect();
        // Overlays live in the store's entries and survive apply_contents.
        let changed = self.store_mut().apply_contents(changes);
        Ok(changed.iter().map(|rel| self.abs(rel)).collect())
    }

    /// The root-relative notes `paths` ask [`refresh_paths`](Self::refresh_paths)
    /// to re-check.
    fn candidates(&self, paths: &[PathBuf], cancel: &Cancel) -> Result<BTreeSet<String>, Error> {
        let i = &*self.inner;
        let root = i.root.path.as_path();
        let store = self.store();
        let mut out = BTreeSet::new();
        for p in paths {
            cancel.check()?;
            let Ok(rel) = p.strip_prefix(root) else {
                continue;
            };
            if i.root.nested_roots.iter().any(|n| p.starts_with(n)) {
                continue;
            }
            // The root itself is a directory like any other.
            let rel = slash(rel);
            let prefix = match rel.is_empty() {
                true => String::new(),
                false => format!("{rel}/"),
            };
            let indexed_under = || {
                store
                    .files()
                    .filter(|f| *f == rel || f.starts_with(&prefix))
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            };
            match i.fs.stat(p) {
                Ok(m) if m.is_dir => {
                    let found = mdroots_core::walk_md(&*i.fs, p, cancel).unwrap_or_default();
                    out.extend(found.into_iter().map(|f| format!("{prefix}{f}")));
                    out.extend(indexed_under());
                }
                Ok(m) if m.is_file => {
                    let name = rel.rsplit('/').next().unwrap_or_default();
                    if is_note(name) {
                        out.insert(rel);
                    }
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => out.extend(indexed_under()),
                Err(_) => {}
            }
        }
        out.retain(|rel| {
            !rel.split('/')
                .any(|c| c.starts_with('.') || mdroots_roots::pruned_dir(c))
                && !i
                    .root
                    .nested_roots
                    .iter()
                    .any(|n| root.join(rel).starts_with(n))
                && match &i.listing {
                    Listing::Single => *rel == i.opened,
                    Listing::WorkingSet { dir, .. } => {
                        store.document(rel).is_some()
                            || root.join(rel).parent() == Some(dir.as_path())
                    }
                    Listing::Root(_) | Listing::Walk => true,
                }
        });
        Ok(out)
    }

    pub fn root(&self) -> RootInfo {
        self.inner.root.clone()
    }

    pub fn freshness(&self) -> Freshness {
        self.inner.freshness
    }

    /// Indexed notes, absolute, sorted.
    pub fn files(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = self.store().files().map(|f| self.abs(f)).collect();
        v.sort();
        v
    }

    /// Every indexed note, sorted by path.
    pub fn notes(&self) -> Vec<NoteSummary> {
        self.notes_where(|_| true)
    }

    /// The notes carrying `tag`, compared case-insensitively, sorted by
    /// path like [`notes`](Self::notes). Empty when none match.
    pub fn notes_with_tag(&self, tag: &str) -> Vec<NoteSummary> {
        let want = tag.to_lowercase();
        self.notes_where(|doc| doc.tags().any(|t| t.name.to_lowercase() == want))
    }

    fn notes_where(&self, keep: impl Fn(&Document) -> bool) -> Vec<NoteSummary> {
        let store = self.store();
        let mut v: Vec<NoteSummary> = store
            .files()
            .filter_map(|rel| {
                let doc = store.document(rel).filter(|d| keep(d))?;
                let path = self.abs(rel);
                Some(NoteSummary {
                    modified: self.modified(&path),
                    created: fm_created(doc).or_else(|| self.inner.fs.created(&path)),
                    title: title(rel, doc),
                    tags: tag_names(doc),
                    path,
                })
            })
            .collect();
        v.sort_by(|a, b| a.path.cmp(&b.path));
        v
    }

    /// `path`'s modification time on disk; `None` when the stat fails or
    /// the filesystem reports no time (exactly zero, as in-memory
    /// filesystems do). Times before the epoch are kept.
    fn modified(&self, path: &Path) -> Option<SystemTime> {
        let ns = self.inner.fs.stat(path).ok()?.mtime_ns;
        if ns == 0 {
            return None;
        }
        let secs = u64::try_from(ns.unsigned_abs() / 1_000_000_000).ok()?;
        let nanos = (ns.unsigned_abs() % 1_000_000_000) as u32;
        let d = Duration::new(secs, nanos);
        if ns > 0 {
            UNIX_EPOCH.checked_add(d)
        } else {
            UNIX_EPOCH.checked_sub(d)
        }
    }

    /// Each tag with the number of notes carrying it. Tags differing only
    /// in case are one tag (as [`notes_with_tag`](Self::notes_with_tag)
    /// matches them): its label is the spelling most notes use (on a tie,
    /// the one met first in path order) and a note counts once. Sorted by
    /// lowercase name.
    pub fn tags(&self) -> Vec<(String, usize)> {
        self.tags_under(&[])
    }

    /// [`tags`](Self::tags) over the notes at or under any of `paths`
    /// (absolute; canonicalized where they exist); every note when `paths`
    /// is empty. `mdroots tags PATH` lists a subdirectory's tags with it.
    pub fn tags_under(&self, paths: &[PathBuf]) -> Vec<(String, usize)> {
        let within: Vec<PathBuf> = paths
            .iter()
            .map(|p| self.inner.fs.canonicalize(p).unwrap_or_else(|_| p.clone()))
            .collect();
        let store = self.store();
        // Lowercase name -> (notes, spellings with their note counts in
        // first-seen order).
        type Group = (usize, Vec<(String, usize)>);
        let mut groups: BTreeMap<String, Group> = BTreeMap::new();
        for rel in store.files() {
            let Some(doc) = store.document(rel) else {
                continue;
            };
            if !within.is_empty() {
                let path = self.abs(rel);
                if !within.iter().any(|w| path.starts_with(w)) {
                    continue;
                }
            }
            let mut seen = BTreeSet::new();
            for t in tag_names(doc) {
                let key = t.to_lowercase();
                let g = groups.entry(key.clone()).or_default();
                if seen.insert(key) {
                    g.0 += 1;
                }
                match g.1.iter_mut().find(|(s, _)| *s == t) {
                    Some(s) => s.1 += 1,
                    None => g.1.push((t, 1)),
                }
            }
        }
        groups
            .into_values()
            .map(|(n, spellings)| {
                // max_by_key keeps the last maximum: reverse for the first.
                let label = spellings
                    .into_iter()
                    .rev()
                    .max_by_key(|(_, c)| *c)
                    .map(|(s, _)| s)
                    .unwrap_or_default();
                (label, n)
            })
            .collect()
    }

    /// The root's effective settings (link style, tag syntaxes, broken-link
    /// severity, docs dir) and where each came from: a tool config, a
    /// marker's default, the convention vote or mdroots' default
    /// ([`explain`](mdroots_resolve::dialect::explain)). `mdroots roots`
    /// prints them.
    pub fn settings(&self) -> Vec<Setting> {
        let store = self.store();
        explain(store.conventions(), &store.vote())
    }

    /// Resolve the first link in `link_text` (e.g. `[[note]]`), written in
    /// `from`, with goto semantics (the Partial step is allowed).
    pub fn resolve(&self, from: &Path, link_text: &str) -> Result<Resolution, Error> {
        let rel = self.rel(from)?;
        let doc = parse(link_text, Dialect::detect_from_path(Path::new(&rel)));
        let link = doc
            .links()
            .next()
            .ok_or_else(|| Error::new(ErrorKind::Unsupported, "not a link"))?;
        let r = self.store().resolve_link(&rel, link, true);
        Ok(Resolution {
            targets: r.targets.iter().map(|t| self.abs(t)).collect(),
            step: r.step,
            status: r.status,
            hint: r.hint,
        })
    }

    /// Every link of the note, in source order. Empty for a path inside
    /// the root that is not indexed.
    pub fn document_links(&self, path: &Path) -> Result<Vec<DocLink>, Error> {
        let rel = self.rel(path)?;
        let store = self.store();
        Ok(store
            .links(&rel)
            .into_iter()
            .map(|(l, r)| {
                let target = match r.status {
                    LinkStatus::Resolved | LinkStatus::Ambiguous | LinkStatus::Unindexed => {
                        r.targets.first().map(|t| self.abs(t))
                    }
                    _ => None,
                };
                let anchor = l
                    .target
                    .raw
                    .split_once('#')
                    .map(|(_, a)| a.to_owned())
                    .filter(|a| !a.is_empty());
                DocLink {
                    range: l.range,
                    text_range: l.text_range,
                    kind: l.kind,
                    context: l.context,
                    target,
                    anchor,
                    line: l.target.line,
                    status: r.status,
                }
            })
            .collect())
    }

    /// Links to the note from indexed notes, sorted by source path. Empty
    /// for a path inside the root that is not indexed.
    pub fn backlinks(&self, path: &Path) -> Result<Vec<Backlink>, Error> {
        let rel = self.rel(path)?;
        let store = self.store();
        if store.document(&rel).is_none() {
            return Ok(Vec::new());
        }
        Ok(store
            .backlinks(&rel)
            .into_iter()
            .filter_map(|(from, l)| self.backlink(&store, from, &l))
            .collect())
    }

    /// `l`, a link in `from` (root-relative), as a [`Backlink`].
    pub(crate) fn backlink(&self, store: &MemStore, from: String, l: &Link) -> Option<Backlink> {
        let doc = store.document(&from)?;
        let (line, _) =
            doc.line_index()
                .line_col(doc.source(), l.range.start, PositionEncoding::Utf8);
        Some(Backlink {
            from: self.abs(&from),
            from_title: title(&from, doc),
            range: l.range.clone(),
            line,
            in_code: matches!(l.context, Context::CodeBlock | Context::InlineCode),
        })
    }

    /// The note's diagnostics under the root's
    /// [`DiagnosticPolicy`](mdroots_core::DiagnosticPolicy)
    /// (computed once, kept until the next content change or refresh).
    /// Empty for a path inside the root that is not indexed.
    pub fn diagnostics(&self, path: &Path, cancel: &Cancel) -> Result<Vec<Diagnostic>, Error> {
        cancel.check()?;
        let rel = self.rel(path)?;
        let store = self.store();
        if store.document(&rel).is_none() {
            return Ok(Vec::new());
        }
        let policy = store.policy(self.inner.freshness == Freshness::Lazy);
        Ok(policy.diagnostics(&store, &rel))
    }

    /// Unsaved editor text for a note inside the root; adds it to the index
    /// if it is not indexed.
    pub fn set_overlay(&self, path: &Path, text: &str) -> Result<(), Error> {
        let rel = self.rel(path)?;
        let mut overlays = self.overlays();
        self.store_mut().set_overlay(&rel, text);
        overlays.insert(rel, text.to_owned());
        Ok(())
    }

    pub fn clear_overlay(&self, path: &Path) -> Result<(), Error> {
        let rel = self.rel(path)?;
        let mut overlays = self.overlays();
        self.store_mut().clear_overlay(&rel);
        overlays.remove(&rel);
        Ok(())
    }

    /// 0-based (line, column) of byte `offset` in the note's current text
    /// (the overlay wins). `Unsupported` for a note that is not indexed.
    pub fn line_col(
        &self,
        path: &Path,
        offset: usize,
        enc: PositionEncoding,
    ) -> Result<(u32, u32), Error> {
        let rel = self.rel(path)?;
        let store = self.store();
        let doc = store
            .document(&rel)
            .ok_or_else(|| Error::new(ErrorKind::Unsupported, format!("{rel}: not indexed")))?;
        Ok(doc.line_index().line_col(doc.source(), offset, enc))
    }

    /// Whether only the opened file belongs to this workspace.
    pub(crate) fn is_single(&self) -> bool {
        self.inner.single
    }

    pub(crate) fn fs(&self) -> &dyn FileSystem {
        &*self.inner.fs
    }

    pub(crate) fn store(&self) -> RwLockReadGuard<'_, MemStore> {
        self.inner.store.read().unwrap_or_else(|e| e.into_inner())
    }

    fn store_mut(&self) -> RwLockWriteGuard<'_, MemStore> {
        self.inner.store.write().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn index(&self) -> MutexGuard<'_, Option<IndexState>> {
        self.inner.index.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Lock order: the index (when taken), overlays, then the store.
    pub(crate) fn overlays(&self) -> MutexGuard<'_, BTreeMap<String, String>> {
        self.inner
            .overlays
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// `p` canonicalized and made root-relative (`/`-separated).
    pub(crate) fn rel(&self, p: &Path) -> Result<String, Error> {
        let canon = self.inner.fs.canonicalize(p)?;
        let rel = canon
            .strip_prefix(&self.inner.root.path)
            .map_err(|_| Error::new(ErrorKind::Unsupported, "outside root"))?;
        if rel.as_os_str().is_empty() {
            return Err(Error::new(ErrorKind::Unsupported, "the root is not a note"));
        }
        Ok(slash(rel))
    }

    /// A root-relative path (possibly `../`-relative) as a clean absolute
    /// path.
    pub(crate) fn abs(&self, rel: &str) -> PathBuf {
        clean(&self.inner.root.path.join(rel))
    }
}

/// The lazy working set: the opened file plus the notes of its directory
/// (one level, no hidden, editor-temp or dataless entries, the first
/// [`WORKING_SET_CAP`] by name, opened file included), root-relative. An unreadable directory
/// leaves the opened file alone.
pub(crate) fn working_set(
    fs: &dyn FileSystem,
    root: &Path,
    dir: &Path,
    name: &str,
    opened: &str,
) -> Vec<String> {
    let dir_rel = dir.strip_prefix(root).map(slash).unwrap_or_default();
    let mut out: Vec<String> = fs
        .read_dir(dir)
        .unwrap_or_default()
        .into_iter()
        .filter(|(n, m)| n != name && m.is_file && !m.dataless && is_note(n) && !is_temp(n))
        .take(WORKING_SET_CAP - 1) // room for the opened file
        .map(|(n, _)| match dir_rel.is_empty() {
            true => n,
            false => format!("{dir_rel}/{n}"),
        })
        .collect();
    out.push(opened.to_owned());
    out
}

fn is_temp(name: &str) -> bool {
    name.starts_with('.')
        || name.ends_with('~')
        || (name.len() > 1 && name.starts_with('#') && name.ends_with('#'))
}

pub(crate) fn is_note(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(stem, ext)| {
        !stem.is_empty()
            && ["md", "markdown", "org"]
                .iter()
                .any(|e| ext.eq_ignore_ascii_case(e))
    })
}

pub(crate) fn title(rel: &str, doc: &Document) -> String {
    doc.frontmatter()
        .and_then(|f| f.title())
        .map(str::to_owned)
        .or_else(|| {
            doc.headings()
                .find(|h| h.level == 1)
                .map(|h| h.text.clone())
        })
        .unwrap_or_else(|| {
            Path::new(rel)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
}

/// The first note under `dir`, breadth first, entries by name, hidden
/// entries skipped; at most [`WORKING_SET_CAP`] directories are listed.
fn first_note(fs: &dyn FileSystem, dir: &Path, cancel: &Cancel) -> Result<Option<PathBuf>, Error> {
    let mut queue = std::collections::VecDeque::from([dir.to_path_buf()]);
    let mut listed = 0;
    while let Some(d) = queue.pop_front() {
        cancel.check()?;
        listed += 1;
        if listed > WORKING_SET_CAP {
            break;
        }
        let Ok(entries) = fs.read_dir(&d) else {
            continue;
        };
        let mut subdirs = Vec::new();
        for (name, m) in entries {
            if is_temp(&name) {
                continue;
            }
            if m.is_file && is_note(&name) {
                return Ok(Some(d.join(name)));
            }
            if m.is_dir {
                subdirs.push(d.join(name));
            }
        }
        queue.extend(subdirs);
    }
    Ok(None)
}

/// The creation time in the frontmatter: `date`, else `created`.
fn fm_created(doc: &Document) -> Option<SystemTime> {
    let fm = doc.frontmatter()?;
    ["date", "created"].iter().find_map(|k| match fm.get(k)? {
        mdroots_syntax::Value::Str(s) => parse_fm_date(s),
        _ => None,
    })
}

/// A frontmatter date: `YYYY-MM-DD`, `YYYY-MM-DD[T ]HH:MM[:SS[.frac]]`,
/// optionally followed by `Z` or `±HH:MM` (RFC 3339). A time without an
/// offset is UTC. Surrounding quotes and blanks are ignored.
pub(crate) fn parse_fm_date(s: &str) -> Option<SystemTime> {
    let s = s.trim().trim_matches(|c| c == '"' || c == '\'');
    let b = s.as_bytes();
    let num = |r: Range<usize>| -> Option<i64> {
        let t = s.get(r)?;
        t.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| t.parse().ok())?
    };
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    if !(1..=12).contains(&mo) || d < 1 || d > days_in_month(y, mo) {
        return None;
    }
    let mut secs = days_from_civil(y, mo, d) * 86_400;
    let mut nanos = 0u32;
    let mut rest = &s[10..];
    if let Some(t) = rest.strip_prefix(['T', 't', ' ']) {
        let tb = t.as_bytes();
        if tb.len() < 5 || tb[2] != b':' {
            return None;
        }
        let two = |i: usize| -> Option<i64> {
            let x = t.get(i..i + 2)?;
            x.bytes()
                .all(|c| c.is_ascii_digit())
                .then(|| x.parse().ok())?
        };
        let (h, mi) = (two(0)?, two(3)?);
        let mut used = 5;
        let mut sec = 0;
        if tb.get(5) == Some(&b':') {
            sec = two(6)?;
            used = 8;
            if tb.get(8) == Some(&b'.') {
                let frac: String = t[9..].chars().take_while(char::is_ascii_digit).collect();
                if frac.is_empty() {
                    return None;
                }
                let digits: String = frac.chars().chain("000000000".chars()).take(9).collect();
                nanos = digits.parse().ok()?;
                used = 9 + frac.len();
            }
        }
        if h > 23 || mi > 59 || sec > 60 {
            return None;
        }
        secs += h * 3600 + mi * 60 + sec;
        rest = &t[used..];
        match rest.as_bytes() {
            [] => {}
            [b'Z' | b'z'] => rest = "",
            [sign @ (b'+' | b'-'), ..] if rest.len() == 6 && rest.as_bytes()[3] == b':' => {
                let oh: i64 = rest[1..3].parse().ok()?;
                let om: i64 = rest[4..6].parse().ok()?;
                let off = oh * 3600 + om * 60;
                secs -= if *sign == b'+' { off } else { -off };
                rest = "";
            }
            _ => return None,
        }
    }
    if !rest.is_empty() {
        return None;
    }
    let d = Duration::new(secs.unsigned_abs(), nanos);
    if secs >= 0 {
        UNIX_EPOCH.checked_add(d)
    } else {
        UNIX_EPOCH
            .checked_sub(Duration::from_secs(secs.unsigned_abs()))?
            .checked_add(Duration::new(0, nanos))
    }
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days from 1970-01-01 to the proleptic Gregorian date `y-m-d`
/// (Howard Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn tag_names(doc: &Document) -> Vec<String> {
    let mut seen = BTreeSet::new();
    doc.tags()
        .filter(|t| seen.insert(t.name.clone()))
        .map(|t| t.name.clone())
        .collect()
}

/// `p`'s components joined with `/`.
pub(crate) fn slash(p: &Path) -> String {
    p.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Hand `dirs` (absolute) to the store, root-relative.
fn set_code_dirs(store: &mut MemStore, dirs: &[PathBuf]) {
    let root = store.root().to_path_buf();
    let rel = dirs
        .iter()
        .filter_map(|d| outside_rel(Some(&root), d))
        .collect();
    store.set_code_dirs(rel);
}

/// `p` with `.` and `..` removed lexically.
fn clean(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}
