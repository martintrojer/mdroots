//! mdroots: one notes root as a [`Workspace`]. [`Workspace::open_for`] finds
//! the root of a file with the discovery rules of `mdroots-roots` (never
//! listing a tree before it is known to be local and bounded), indexes it in
//! memory, and answers link, backlink, tag and diagnostics queries
//! (docs/specs/library.md §3.2–§3.4).
//!
//! Public paths are absolute and canonical (through the workspace's
//! [`FileSystem`]); a path outside the root is an `Unsupported` error.
#![forbid(unsafe_code)]

mod notes;
mod rename;
mod workspace;
mod workspaces;

#[cfg(unix)]
pub use mdroots_core::StdFs;
pub use mdroots_core::{Cancel, DiagCode, Diagnostic, Error, ErrorKind, FileSystem, Severity};
pub use mdroots_resolve::{ResolveStep, ladder::LinkStatus};
pub use mdroots_roots::RootMode;
pub use mdroots_roots::discover::{Enumerator, NoEnumerator};
pub use mdroots_roots::probe::Probe;
#[cfg(unix)]
pub use mdroots_roots::probe::StdProbe;
pub use mdroots_syntax as syntax;

pub use notes::Preview;
pub use rename::{TextEdit, WorkspaceEdit};
pub use workspace::{
    Backlink, DocLink, Freshness, NoteSummary, Options, Resolution, RootInfo, Workspace,
};
pub use workspaces::Workspaces;
