//! mdroots: one notes root as a [`Workspace`]. [`Workspace::open_for`] finds
//! the root of a file with the discovery rules of `mdroots-roots` (never
//! listing a tree before it is known to be local and bounded), indexes it in
//! memory, and answers link, backlink, tag and diagnostics queries
//! (docs/specs/library.md §3.2–§3.4). Unless [`IndexMode::Memory`] is
//! chosen, the notes are also cached in a per-root DB in the cache dir
//! ([`index`], docs/specs/index.md §1), so later processes start without
//! reading unchanged notes.
//!
//! Public paths are absolute and canonical (through the workspace's
//! [`FileSystem`]); a path outside the root is an `Unsupported` error.
#![forbid(unsafe_code)]

mod extract;
mod goto;
mod graph;
mod indexing;
mod links;
pub mod names;
mod notes;
pub mod query;
mod rename;
mod search;
mod watch;
mod workspace;
mod workspaces;

#[cfg(unix)]
pub use mdroots_core::StdFs;
pub use mdroots_core::{Cancel, DiagCode, Diagnostic, Error, ErrorKind, FileSystem, Severity};
pub use mdroots_index as index;
pub use mdroots_index::lock::Role;
pub use mdroots_resolve::dialect::{DialectMarker, LinkStyle, Setting, Source};
pub use mdroots_resolve::{ResolveStep, ladder::LinkStatus};
pub use mdroots_roots::RootMode;
pub use mdroots_roots::discover::{Enumerator, NoEnumerator};
#[cfg(unix)]
pub use mdroots_roots::probe::StdProbe;
pub use mdroots_roots::probe::{FsStat, MountInfo, Probe};
pub use mdroots_syntax as syntax;

pub use goto::Goto;
pub use notes::Preview;
pub use rename::{TextEdit, WorkspaceEdit};
pub use search::Hit;
pub use workspace::{
    Backlink, DocLink, Freshness, IndexMode, NoteSummary, Options, Resolution, RootInfo, Workspace,
};
pub use workspaces::Workspaces;
