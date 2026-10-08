//! mdroots-roots: root discovery (docs/specs/roots.md). Every filesystem
//! access goes through [`probe::Probe`], so tests can count and forbid
//! `read_dir` calls.
#![forbid(unsafe_code)]

pub mod gitindex;
pub mod markers;
pub mod probe;
pub mod registry;
pub mod walk;

pub use probe::{Counting, FakeProbe, FsClass, FsStat, MountInfo, Probe, StdProbe, classify};
pub use registry::{
    DiscoverLock, MemRegistry, Overlap, Registry, RootMode, RootRecord, VerdictSource, detect_move,
    lookup_valid, new_root_id,
};
pub use walk::{Abort, Budget, WalkOptions, WalkOutcome, WalkStats, walk};
