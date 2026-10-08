//! mdroots-core: the `FileSystem` trait (`StdFs`, `MemFs`), `Cancel`, the
//! markdown walk and `MemStore`, an in-memory index of one root that answers
//! link queries through the resolve ladder.
#![forbid(unsafe_code)]

pub mod cancel;
pub mod diagnostics;
pub mod error;
pub mod fs;
pub mod memstore;
pub mod walk;

pub use cancel::Cancel;
pub use diagnostics::{DiagCode, Diagnostic, DiagnosticPolicy};
pub use error::{Error, ErrorKind};
#[cfg(unix)]
pub use fs::StdFs;
pub use fs::{FileSystem, FsKind, MemFs, Meta};
pub use mdroots_resolve::dialect::Severity;
pub use mdroots_resolve::env::ResolveEnv;
pub use memstore::{AnchorStatus, MemStore, valid_rel};
pub use walk::walk_md;
