//! mdroots-index: the persistent per-root index cache (docs/DECISIONS.md D3,
//! D4, D5). [`cache`] picks the cache dir, [`lock`] elects the one writer per
//! root, [`db`] is the per-root [SQLite](https://sqlite.org) DB,
//! [`reconcile`] brings its rows up to date with the files on disk and
//! [`registry`] is the persistent root registry; [`gc`] deletes DB files
//! nobody needs.
#![forbid(unsafe_code)]

pub mod cache;
pub mod db;
pub mod gc;
pub mod lock;
pub mod reconcile;
pub mod registry;

pub use cache::{CacheDir, CacheEnv, cache_dir};
pub use db::{Change, FileRow, IndexDb, SCHEMA};
pub use gc::{GcOptions, GcReport, gc};
pub use lock::{Role, RootLocks};
pub use reconcile::{ReconcileStats, reconcile};
pub use registry::SqliteRegistry;

use std::io;
use std::path::Path;

/// Create `dir` and missing parents, each new one mode 0700 (before umask),
/// and set `dir` itself to 0700 if it already existed, because the cache
/// holds copies of the user's notes.
pub(crate) fn create_private_dir(dir: &Path) -> io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        b.mode(0o700);
        b.create(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    b.create(dir)
}
