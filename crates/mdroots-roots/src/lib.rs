//! mdroots-roots: root discovery (docs/specs/roots.md). Every filesystem
//! access goes through [`probe::Probe`], so tests can count and forbid
//! `read_dir` calls.
#![forbid(unsafe_code)]

pub mod discover;
pub mod gitindex;
pub mod loose;
pub mod markers;
pub mod probe;
pub mod registry;
pub mod walk;

pub use discover::{
    Decision, DiscoverOptions, Enumerator, NoEnumerator, SlFiles, discover, explain, list_root,
};
pub use loose::{LooseOutcome, find_loose_root, is_denied};
pub use probe::{Counting, FakeProbe, FsClass, FsStat, MountInfo, Probe, StdProbe, classify};
pub use registry::{
    DiscoverLock, EDITOR_MARKER, MemRegistry, Overlap, Registry, RootMode, RootRecord,
    VerdictSource, detect_move, is_editor, lookup_valid, lookup_valid_for, new_root_id,
};
pub use walk::{Abort, Budget, WalkOptions, WalkOutcome, WalkStats, pruned_dir, walk};
