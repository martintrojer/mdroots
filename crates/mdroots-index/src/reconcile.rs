//! Reconcile a root's DB rows with the files on disk (docs/specs/index.md
//! §1.1-§1.3): decide per file whether its cached content can be reused or
//! the file must be read, write the changes when the caller is the
//! reconciler, and return every note's current bytes to hydrate a
//! [`MemStore`](mdroots_core::MemStore) with
//! [`from_contents`](mdroots_core::MemStore::from_contents).

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mdroots_core::{Cancel, Error, FileSystem, Meta, valid_rel};

use crate::db::{Change, FileRow, IndexDb};

/// A batch is applied once it holds this many files ...
const BATCH_FILES: usize = 200;
/// ... or this long after it started.
const BATCH_TIME: Duration = Duration::from_millis(50);
/// Re-reads when a file changes between its stat and the read's fstat.
const RETRIES: usize = 3;

/// Root-relative paths and their bytes (a private alias: the public
/// signature stays `Vec<(String, Arc<[u8]>)>`).
type Contents = Vec<(String, Arc<[u8]>)>;

/// What one [`reconcile`] did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReconcileStats {
    /// Files read from disk (new, changed, touched or forced).
    pub read: usize,
    /// Files whose cached content was reused without opening them.
    pub reused: usize,
    /// Rows dropped: the file is gone, not a regular file, has an invalid
    /// path, or is no longer listed.
    pub removed: usize,
    /// [`IndexDb::apply`] calls (0 when `db` is `None`).
    pub batches: usize,
}

/// Bring `rows` (the DB's file rows) up to date with the files under `root`.
///
/// The file set is `listed` when given (the reconciler re-listed the root;
/// rows not in it are removed), else the rows' paths (a peer keeps the DB's
/// file set), plus `force_read`. Per path, in order: an invalid path (see
/// [`valid_rel`]) is dropped; a missing file or non-file is dropped; a row
/// whose `(ino, ctime_ns, size)` equals the stat is reused unless the path
/// is in `force_read`; a dataless file (cloud placeholder) is left alone
/// unless forced, because reading it would download it; otherwise the file
/// is read. A read whose fstat differs from the stat before it is retried
/// (re-stat, re-read) up to 3 times, then the last read is kept. A stat or
/// read error other than not-found keeps the row as it is and returns no
/// content for that path.
///
/// New or changed content is a [`Change::Upsert`]; the same content with a
/// new stat (`touch`) is a [`Change::Stat`]. With `db`, changes are applied
/// in batches of at most 200 files or 50 ms; with `None` nothing is written.
/// Cancellation is checked per file: the pending batch is discarded and
/// `Err(Cancelled)` returned, so the DB holds only whole batches.
///
/// Returns the current bytes of every note, sorted by path, and the stats.
pub fn reconcile(
    mut db: Option<&mut IndexDb>,
    rows: &[FileRow],
    fs: &dyn FileSystem,
    root: &Path,
    listed: Option<&[String]>,
    force_read: &[String],
    cancel: &Cancel,
) -> Result<(Contents, ReconcileStats), Error> {
    let by_path: BTreeMap<&str, &FileRow> = rows.iter().map(|r| (r.path.as_str(), r)).collect();
    let force: BTreeSet<&str> = force_read.iter().map(String::as_str).collect();
    let mut set: BTreeSet<&str> = match listed {
        Some(l) => l.iter().map(String::as_str).collect(),
        None => by_path.keys().copied().collect(),
    };
    set.extend(&force);
    // Rows outside the set are visited too, to be removed.
    let paths: BTreeSet<&str> = set.iter().chain(by_path.keys()).copied().collect();

    let mut out = Vec::new();
    let mut stats = ReconcileStats::default();
    let mut batch = Batch::new();
    for rel in paths {
        cancel.check()?;
        let row = by_path.get(rel).copied();
        match visit(fs, root, rel, row, set.contains(rel), force.contains(rel)) {
            Outcome::Keep => {}
            Outcome::Gone => {
                if row.is_some() {
                    stats.removed += 1;
                    batch.push(Change::Remove(rel.to_owned()));
                }
            }
            Outcome::Reused(content) => {
                stats.reused += 1;
                out.push((rel.to_owned(), content));
            }
            Outcome::Read(new) => {
                stats.read += 1;
                out.push((rel.to_owned(), new.content.clone()));
                match row {
                    Some(old) if *old == new => {}
                    Some(old) if old.content == new.content => batch.push(Change::Stat(new)),
                    _ => batch.push(Change::Upsert(new)),
                }
            }
        }
        // Cancelled during this file: its batch is discarded, not applied.
        cancel.check()?;
        if let Some(db) = db.as_deref_mut() {
            batch.flush_if_due(db, &mut stats)?;
        }
    }
    if let Some(db) = db {
        batch.flush(db, &mut stats)?;
    }
    Ok((out, stats))
}

/// What to do with one path.
enum Outcome {
    /// Leave the row (if any) unchanged; no content.
    Keep,
    /// Drop the row (if any); no content.
    Gone,
    Reused(Arc<[u8]>),
    Read(FileRow),
}

fn visit(
    fs: &dyn FileSystem,
    root: &Path,
    rel: &str,
    row: Option<&FileRow>,
    in_set: bool,
    force: bool,
) -> Outcome {
    if !in_set || !valid_rel(rel) {
        return Outcome::Gone;
    }
    let abs = root.join(rel);
    let mut meta = match stat_file(fs, &abs) {
        Ok(m) => m,
        Err(o) => return o,
    };
    if let Some(r) = row.filter(|r| !force && same_stat(r, &meta)) {
        return Outcome::Reused(r.content.clone());
    }
    if meta.dataless && !force {
        return Outcome::Keep;
    }
    let mut attempt = 0;
    loop {
        let (content, read_meta) = match fs.read(&abs) {
            Ok(r) => r,
            Err(e) => return io_outcome(&e),
        };
        if version(&read_meta) == version(&meta) || attempt == RETRIES {
            return Outcome::Read(file_row(rel, &read_meta, content));
        }
        attempt += 1;
        meta = match stat_file(fs, &abs) {
            Ok(m) => m,
            Err(o) => return o,
        };
    }
}

/// Stat a file; a non-file is missing.
fn stat_file(fs: &dyn FileSystem, abs: &Path) -> Result<Meta, Outcome> {
    match fs.stat(abs) {
        Ok(m) if m.is_file => Ok(m),
        Ok(_) => Err(Outcome::Gone),
        Err(e) => Err(io_outcome(&e)),
    }
}

fn io_outcome(e: &io::Error) -> Outcome {
    match e.kind() {
        io::ErrorKind::NotFound => Outcome::Gone,
        _ => Outcome::Keep,
    }
}

/// The file version key: a change in any of these means re-read.
fn version(m: &Meta) -> (u64, i128, u64) {
    (m.ino, m.ctime_ns, m.size)
}

/// Whether the row was read at this stat. A ctime that does not fit the
/// row's `i64` never matches.
fn same_stat(r: &FileRow, m: &Meta) -> bool {
    r.ino == m.ino && i64::try_from(m.ctime_ns) == Ok(r.ctime_ns) && r.size == m.size
}

fn saturate(ns: i128) -> i64 {
    i64::try_from(ns).unwrap_or(if ns < 0 { i64::MIN } else { i64::MAX })
}

fn file_row(rel: &str, m: &Meta, content: Arc<[u8]>) -> FileRow {
    FileRow {
        path: rel.to_owned(),
        ino: m.ino,
        ctime_ns: saturate(m.ctime_ns),
        mtime_ns: saturate(m.mtime_ns),
        size: m.size,
        content,
    }
}

/// Pending changes and when the batch started.
struct Batch {
    changes: Vec<Change>,
    started: Instant,
}

impl Batch {
    fn new() -> Batch {
        Batch {
            changes: Vec::new(),
            started: Instant::now(),
        }
    }

    fn push(&mut self, c: Change) {
        if self.changes.is_empty() {
            self.started = Instant::now();
        }
        self.changes.push(c);
    }

    fn flush_if_due(&mut self, db: &mut IndexDb, stats: &mut ReconcileStats) -> Result<(), Error> {
        if self.changes.len() >= BATCH_FILES
            || (!self.changes.is_empty() && self.started.elapsed() >= BATCH_TIME)
        {
            self.flush(db, stats)?;
        }
        Ok(())
    }

    fn flush(&mut self, db: &mut IndexDb, stats: &mut ReconcileStats) -> Result<(), Error> {
        if self.changes.is_empty() {
            return Ok(());
        }
        db.apply(&self.changes)?;
        self.changes.clear();
        stats.batches += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saturates_out_of_range_times() {
        assert_eq!(saturate(i128::MAX), i64::MAX);
        assert_eq!(saturate(i128::MIN), i64::MIN);
        assert_eq!(saturate(-5), -5);
    }
}
