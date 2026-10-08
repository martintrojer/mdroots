//! mdroots-index: the persistent per-root index cache (docs/DECISIONS.md D3,
//! D4, D5). [`cache`] picks the cache dir, [`lock`] elects the one writer per
//! root, and [`db`] is the per-root [SQLite](https://sqlite.org) DB.
#![forbid(unsafe_code)]

pub mod cache;
pub mod db;
pub mod lock;

pub use cache::{CacheDir, CacheEnv, cache_dir};
pub use db::{Change, FileRow, IndexDb, SCHEMA};
pub use lock::{Role, RootLocks};

use std::io;
use std::path::Path;

/// Create `dir` and missing parents, each new one mode 0700 (before umask),
/// because the cache holds copies of the user's notes.
pub(crate) fn create_private_dir(dir: &Path) -> io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir)
}
