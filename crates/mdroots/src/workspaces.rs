//! [`Workspaces`]: the workspace of any file, cached per root.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use mdroots_core::Error;

use crate::workspace::{Options, Workspace};

/// Maps files to [`Workspace`]s, opening each root once. Cheap to clone
/// (clones share the cache); `Send + Sync`. Every workspace shares one
/// cache dir and root registry, resolved in [`Workspaces::new`].
#[derive(Clone)]
pub struct Workspaces {
    opts: Options,
    cache: Arc<Mutex<Vec<Entry>>>,
}

/// A cached workspace; `file` is set for a single-file or rootless lazy
/// one, the only file it serves.
#[derive(Clone)]
struct Entry {
    file: Option<PathBuf>,
    ws: Workspace,
}

const _: () = {
    const fn assert<T: Clone + Send + Sync>() {}
    assert::<Workspaces>();
};

impl Workspaces {
    pub fn new(opts: Options) -> Workspaces {
        Workspaces {
            opts: opts.with_shared_cache(),
            cache: Arc::default(),
        }
    }

    /// The workspace serving `file`: the cached one with the longest root
    /// containing it (not under one of that root's nested roots), else a
    /// new [`Workspace::open_for`]. A single-file or rootless lazy
    /// workspace serves only the file it was opened for.
    pub fn for_path(&self, file: &Path) -> Result<Workspace, Error> {
        let (fs, _) = self.opts.io()?;
        let file = fs.canonicalize(file)?;
        if let Some(ws) = self.lookup(&file) {
            return Ok(ws);
        }
        // Opened without the lock held: discovery and indexing are slow.
        let ws = Workspace::open_for(&file, self.opts.clone())?;
        let entry = Entry {
            file: ws.is_single().then(|| file.clone()),
            ws,
        };
        let mut cache = self.lock();
        if let Some(hit) = cache.iter().find(|c| same(c, &entry)) {
            return Ok(hit.ws.clone());
        }
        cache.push(entry.clone());
        Ok(entry.ws)
    }

    /// The cached workspace serving `file`, as [`for_path`](Self::for_path)
    /// picks it, without opening one: never discovers or indexes. `None`
    /// when no cached workspace serves it or it cannot be canonicalized.
    pub fn get(&self, file: &Path) -> Option<Workspace> {
        let (fs, _) = self.opts.io().ok()?;
        let file = fs.canonicalize(file).ok()?;
        self.lookup(&file)
    }

    /// Every cached workspace, sorted by root path.
    pub fn all(&self) -> Vec<Workspace> {
        let mut v = self.lock().clone();
        v.sort_by(|a, b| (a.ws.root().path, &a.file).cmp(&(b.ws.root().path, &b.file)));
        v.into_iter().map(|e| e.ws).collect()
    }

    fn lookup(&self, file: &Path) -> Option<Workspace> {
        let cache = self.lock();
        cache
            .iter()
            .filter(|e| serves(e, file))
            .max_by_key(|e| e.ws.root().path.components().count())
            .map(|e| e.ws.clone())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Entry>> {
        self.cache.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Whether the cached entry serves the canonical `file`.
fn serves(e: &Entry, file: &Path) -> bool {
    if let Some(f) = &e.file {
        return f == file;
    }
    let root = e.ws.root();
    file.starts_with(&root.path)
        && file != root.path
        && !root.nested_roots.iter().any(|n| file.starts_with(n))
}

/// Whether the cached `c` is the workspace `e` just opened.
fn same(c: &Entry, e: &Entry) -> bool {
    match (&c.file, &e.file) {
        (Some(a), Some(b)) => a == b,
        (None, None) => c.ws.root().path == e.ws.root().path,
        _ => false,
    }
}
