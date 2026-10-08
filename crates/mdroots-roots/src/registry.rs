//! The root registry (docs/specs/roots.md §1 stage 1, §4, §5, §6).
//!
//! M2 ships the data model, the [`Registry`] trait, an in-memory
//! [`MemRegistry`] and the global [`DiscoverLock`]. The SQLite-backed
//! registry (`roots.v<k>.db`) implements the same trait later.

use std::fs::{File, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

use crate::probe::Probe;
use crate::walk::WalkStats;

/// How a root is indexed (§6 `kind`).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootMode {
    Marker,
    Vcs,
    TrackedOnly,
    IndexDriven,
    Loose,
    /// Only the open file; never registered.
    SingleFile,
    Lazy,
    VcsEnumerated,
}

/// What produced the verdict (§6 `verdict_source`).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerdictSource {
    Fs,
    Eden,
    Budget,
    Rate,
    User,
}

/// One registry row (§6).
#[derive(Debug, Clone, PartialEq)]
pub struct RootRecord {
    /// Stable for the life of the root, including moves; see [`new_root_id`].
    pub root_id: String,
    pub path: PathBuf,
    pub mode: RootMode,
    /// Marker name relative to `path` (e.g. `.git`, `.zk`).
    pub marker: Option<String>,
    pub marker_ino: Option<u64>,
    /// [`Probe::volume_id`] of the root.
    pub volume_id: String,
    /// `st_dev` of the root directory ([`crate::probe::FsStat::dev`]).
    pub dev: u64,
    pub fs_type: String,
    pub stats: WalkStats,
    pub verdict_source: VerdictSource,
    pub rate_confirmations: u32,
    pub decided_at_ms: u64,
    pub reason: String,
    pub last_seen_ms: u64,
}

/// An insert overlapping an existing root in a way §4 forbids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overlap {
    pub existing: PathBuf,
}

impl std::fmt::Display for Overlap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "overlaps existing root {}", self.existing.display())
    }
}

impl std::error::Error for Overlap {}

pub trait Registry {
    /// The nearest root containing `path` (component-wise prefix).
    fn lookup(&self, path: &Path) -> Option<RootRecord>;
    /// The root whose marker is inode `ino` on volume `volume_id`.
    fn by_marker(&self, volume_id: &str, ino: u64) -> Option<RootRecord>;
    /// Add a root, enforcing the overlap rules of §4.
    fn insert(&mut self, r: RootRecord) -> Result<(), Overlap>;
    /// Replace the row with the same `root_id` (its path may change: a
    /// move), or add it if there is none. No overlap check.
    fn update(&mut self, r: RootRecord);
    /// Every row, sorted by path.
    fn all(&self) -> Vec<RootRecord>;
}

/// A stable root id: FNV-1a 64 of the first path and decision time, in hex.
pub fn new_root_id(path: &Path, decided_at_ms: u64) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let bytes = path.as_os_str().as_encoded_bytes();
    for b in bytes
        .iter()
        .chain(&[0u8])
        .chain(&decided_at_ms.to_le_bytes())
    {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// An in-memory [`Registry`].
#[derive(Debug, Clone, Default)]
pub struct MemRegistry {
    rows: Vec<RootRecord>,
}

impl MemRegistry {
    pub fn new() -> Self {
        Self::default()
    }
}

/// May `r` sit inside another root? Only a marker root that is not loose.
fn nestable(r: &RootRecord) -> bool {
    r.marker.is_some() && !matches!(r.mode, RootMode::Loose | RootMode::SingleFile)
}

impl Registry for MemRegistry {
    fn lookup(&self, path: &Path) -> Option<RootRecord> {
        self.rows
            .iter()
            .filter(|r| path.starts_with(&r.path))
            .max_by_key(|r| r.path.components().count())
            .cloned()
    }

    fn by_marker(&self, volume_id: &str, ino: u64) -> Option<RootRecord> {
        self.rows
            .iter()
            .find(|r| r.volume_id == volume_id && r.marker_ino == Some(ino))
            .cloned()
    }

    /// Overlapping roots are allowed only when the inner one is a marker
    /// root (nearest wins) and the outer one is not loose, except that a new
    /// marker root may appear inside an existing loose root
    /// ([git](https://git-scm.com/) `git init` in a
    /// loose root). A new loose root never contains a marker root, and loose
    /// roots never nest. Identical paths always overlap.
    ///
    /// [`RootMode::SingleFile`] records are never registered: they are
    /// dropped (a debug assertion fires) and `Ok(())` is returned.
    fn insert(&mut self, r: RootRecord) -> Result<(), Overlap> {
        debug_assert!(
            r.mode != RootMode::SingleFile,
            "single-file roots are never registered"
        );
        if r.mode == RootMode::SingleFile {
            return Ok(());
        }
        for e in &self.rows {
            let ok = if e.path == r.path {
                false
            } else if r.path.starts_with(&e.path) {
                // new inside existing
                nestable(&r)
            } else if e.path.starts_with(&r.path) {
                // existing inside new
                nestable(e) && r.mode != RootMode::Loose
            } else {
                true
            };
            if !ok {
                return Err(Overlap {
                    existing: e.path.clone(),
                });
            }
        }
        self.rows.push(r);
        Ok(())
    }

    fn update(&mut self, r: RootRecord) {
        match self.rows.iter_mut().find(|e| e.root_id == r.root_id) {
            Some(e) => *e = r,
            None => self.rows.push(r),
        }
    }

    fn all(&self) -> Vec<RootRecord> {
        let mut v = self.rows.clone();
        v.sort_by(|a, b| a.path.cmp(&b.path));
        v
    }
}

/// Stage 1 lookup: the nearest registered root of `path`, if still valid.
///
/// Valid means the recorded marker still exists (skipped for markerless
/// roots), the root directory's `st_dev` is unchanged and its mount's
/// `fs_type` is unchanged. `None` tells the caller to re-decide.
pub fn lookup_valid(reg: &dyn Registry, probe: &dyn Probe, path: &Path) -> Option<RootRecord> {
    let rec = reg.lookup(path)?;
    if let Some(m) = &rec.marker {
        probe.stat(&rec.path.join(m)).ok()?;
    }
    if probe.stat(&rec.path).ok()?.dev != rec.dev {
        return None;
    }
    if probe.mount(&rec.path).ok()?.fs_type != rec.fs_type {
        return None;
    }
    Some(rec)
}

/// Root move detection (§1 "Root moves").
///
/// `marker_name` was found in `marker_dir`. If a row has the same volume and
/// marker inode, a different path, and its old path no longer holds the
/// marker, the root moved: the row is returned with `path` set to
/// `marker_dir` and `root_id` kept. A copy (the old path still holds the
/// marker) is a new root and yields `None`. Pure: the caller calls
/// [`Registry::update`].
pub fn detect_move(
    reg: &dyn Registry,
    probe: &dyn Probe,
    marker_dir: &Path,
    marker_name: &str,
) -> Option<RootRecord> {
    let volume_id = probe.volume_id(marker_dir).ok()?;
    let ino = probe.stat(&marker_dir.join(marker_name)).ok()?.ino;
    let mut rec = reg.by_marker(&volume_id, ino)?;
    if rec.path == marker_dir || rec.marker.as_deref() != Some(marker_name) {
        return None;
    }
    if probe.stat(&rec.path.join(marker_name)).is_ok() {
        return None;
    }
    rec.path = marker_dir.to_path_buf();
    Some(rec)
}

/// The global `discover.lock` (§5): an exclusive `flock` on
/// `<dir>/discover.lock`, held during discovery stages 2–4 and released on
/// drop.
///
/// The lock file is never unlinked: a process blocked on the old inode would
/// win a lock nobody else contends for while a newcomer locks a fresh file at
/// the same path, so two processes would both hold "the" lock.
#[derive(Debug)]
pub struct DiscoverLock {
    _file: File,
}

impl DiscoverLock {
    fn open(dir: &Path) -> io::Result<File> {
        File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("discover.lock"))
    }

    /// Block until the lock is held.
    pub fn acquire(dir: &Path) -> io::Result<DiscoverLock> {
        let file = Self::open(dir)?;
        file.lock()?;
        Ok(DiscoverLock { _file: file })
    }

    /// Take the lock if free; `Ok(None)` if another holder has it.
    pub fn try_acquire(dir: &Path) -> io::Result<Option<DiscoverLock>> {
        let file = Self::open(dir)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(DiscoverLock { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(e),
        }
    }
}
