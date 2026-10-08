//! The stage-4 budgeted walk (docs/specs/roots.md §1 stage 4).
//!
//! Placeholder holding only [`WalkStats`], which the registry records; the
//! walk itself lands with its own task and replaces this file.

/// Counts and timing of one walk.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct WalkStats {
    pub entries: usize,
    pub md: usize,
    pub files: usize,
    pub dirs: usize,
    pub ms: f64,
    pub ms_per_dir: Option<f64>,
}
