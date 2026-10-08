//! The I/O the resolver needs, declared here and implemented by core.
//!
//! Paths are root-relative `&str` (as stored in keys), `case_sensitive` is
//! per root, and `is_file` and `read_config` let the ladder tell files from
//! dirs and dialect detection read marker configs without its own I/O
//! (docs/specs/library.md §3.5).

use std::path::Path;

/// Filesystem facts about one root, as seen by the resolver.
pub trait ResolveEnv: Send + Sync {
    /// A file or dir (any type) exists at `root_rel` under the root.
    fn exists(&self, root_rel: &str) -> bool;
    /// A regular file exists at `root_rel` under the root.
    fn is_file(&self, root_rel: &str) -> bool;
    /// Whether the root's filesystem is case-sensitive.
    fn case_sensitive(&self) -> bool;
    fn home_dir(&self) -> Option<&Path>;
    /// Contents of a small config file (≤ 256 KiB) such as
    /// `.zk/config.toml`; `None` if missing, too large or unreadable.
    fn read_config(&self, root_rel: &str) -> Option<String>;
}
