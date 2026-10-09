//! Cache GC (docs/specs/roots.md §5, §6): delete per-root DB files nobody
//! needs, under the `.open` lock rule, at most daily per cache dir.
//!
//! A root's lock scope is `<root_id>.v<schema>.open`; it covers every DB file
//! of that root and schema, `<root_id>.v<schema>.db` and its generation files
//! `<root_id>.v<schema>-<gen8>.db`, each with `-wal` and `-shm`. GC deletes
//! a root's DB files only while it holds `LOCK_EX|LOCK_NB` on that `.open`,
//! so it never unlinks a DB some process has open; a held root is skipped
//! until the next run. Lock files are never deleted: they are tiny, and
//! unlinking one a process waits on would let a second process take it.
//!
//! GC reads and deletes only regular files directly in `<cache>/roots`,
//! never follows a symlink, and never touches anything outside the cache.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use mdroots_core::Error;

use crate::db::SCHEMA;
use crate::registry::{GcRow, SqliteRegistry};

const DAY_MS: u64 = 24 * 3_600_000;

/// DB files of an old schema version are deleted once this old (by mtime).
const OLD_SCHEMA_AGE: Duration = Duration::from_secs(7 * 24 * 3_600);

/// What [`gc`] deletes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GcOptions {
    /// A root not seen for this many days (or whose path is gone) is
    /// deleted, with its registry row.
    pub unseen_days: u64,
    /// Over this many bytes of DB files, the least recently seen roots are
    /// deleted until the rest fits.
    pub budget_bytes: u64,
    /// Run even if the last run was less than a day ago (tests).
    pub force: bool,
}

impl Default for GcOptions {
    fn default() -> Self {
        GcOptions {
            unseen_days: 30,
            budget_bytes: 1 << 30,
            force: false,
        }
    }
}

/// What one [`gc`] run did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcReport {
    /// Files deleted, sorted.
    pub deleted: Vec<PathBuf>,
    /// Files that were candidates but whose root's `.open` was held, sorted.
    pub skipped_busy: Vec<PathBuf>,
}

/// A DB file name split into its lock scope and generation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct DbName {
    root_id: String,
    schema: u32,
    /// The DB the file belongs to: `<root_id>.v<schema>[-<gen8>].db`.
    db: String,
}

/// Parse `<root_id>.v<schema>.db` or `<root_id>.v<schema>-<gen8>.db`, with
/// an optional `-wal` or `-shm` suffix. `root_id` is hex.
fn parse_name(name: &str) -> Option<DbName> {
    let db = name
        .strip_suffix("-wal")
        .or_else(|| name.strip_suffix("-shm"))
        .unwrap_or(name);
    let base = db.strip_suffix(".db")?;
    let (root_id, rest) = base.split_once(".v")?;
    let (schema, generation) = match rest.split_once('-') {
        Some((s, g)) => (s, Some(g)),
        None => (rest, None),
    };
    let hex = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit());
    if !hex(root_id) || !schema.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if generation.is_some_and(|g| g.len() != 8 || !hex(g)) {
        return None;
    }
    Some(DbName {
        root_id: root_id.to_owned(),
        schema: schema.parse().ok()?,
        db: db.to_owned(),
    })
}

/// Whether `name` is a DB file name of this binary's schema for `root_id`:
/// `<root_id>.v<SCHEMA>.db` or a generation `<root_id>.v<SCHEMA>-<gen8>.db`.
pub fn is_db_file_of(name: &str, root_id: &str) -> bool {
    parse_name(name).is_some_and(|n| n.root_id == root_id && n.schema == SCHEMA && n.db == name)
}

/// One regular file of `<cache>/roots`.
struct Found {
    path: PathBuf,
    name: DbName,
    size: u64,
    mtime: SystemTime,
}

/// The DB files of `dir`, by lock scope `(root_id, schema)`. Symlinks and
/// anything not a regular file are ignored.
fn list(dir: &Path) -> io::Result<BTreeMap<(String, u32), Vec<Found>>> {
    let mut out: BTreeMap<_, Vec<Found>> = BTreeMap::new();
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        // `DirEntry::metadata` does not follow symlinks.
        let Ok(m) = e.metadata() else { continue };
        if !m.file_type().is_file() {
            continue;
        }
        let Some(name) = e.file_name().to_str().and_then(parse_name) else {
            continue;
        };
        out.entry((name.root_id.clone(), name.schema))
            .or_default()
            .push(Found {
                path: e.path(),
                size: m.len(),
                mtime: m.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                name,
            });
    }
    Ok(out)
}

/// Take `LOCK_EX|LOCK_NB` on `<dir>/<root_id>.v<schema>.open` (creating it
/// if missing, never unlinking it); `None` while anyone holds it.
fn lock_scope(dir: &Path, root_id: &str, schema: u32) -> io::Result<Option<File>> {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(format!("{root_id}.v{schema}.open")))?;
    match f.try_lock() {
        Ok(()) => Ok(Some(f)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

/// Run GC on the cache dir `cache` at `now_ms`, unless it ran less than a
/// day before (`meta.gc_at` in `roots.v1.db`, claimed in a `BEGIN
/// IMMEDIATE` transaction, so of several processes one runs); then
/// `Ok(None)`.
///
/// Candidates, each deleted only while its root's `.open` can be taken
/// exclusively (else reported in [`GcReport::skipped_busy`]):
///
/// 1. DB files of this schema not referenced by their root's registry
///    `db_file` (old generations), or of a root without a registry row.
/// 2. DB files of another schema version whose newest file is older than
///    7 days.
/// 3. Roots not seen for [`GcOptions::unseen_days`], or whose path is
///    gone: their DB files and their registry row.
/// 4. While the DB files left exceed [`GcOptions::budget_bytes`]: whole
///    roots, least recently seen first, registry row included.
pub fn gc(cache: &Path, now_ms: u64, opts: GcOptions) -> Result<Option<GcReport>, Error> {
    let mut reg = SqliteRegistry::open(&cache.join("roots.v1.db"))?;
    if !reg.claim_gc(now_ms, opts.force)? {
        return Ok(None);
    }
    let mut report = GcReport::default();
    let dir = cache.join("roots");
    // A symlinked or missing roots dir is left alone.
    match std::fs::symlink_metadata(&dir) {
        Ok(m) if m.file_type().is_dir() => {}
        _ => return Ok(Some(report)),
    }
    let mut groups = list(&dir)?;
    let rows: BTreeMap<String, GcRow> = reg
        .gc_rows()?
        .into_iter()
        .map(|r| (r.root_id.clone(), r))
        .collect();
    let unseen_ms = opts.unseen_days.saturating_mul(DAY_MS);
    let now = SystemTime::UNIX_EPOCH + Duration::from_millis(now_ms);
    let mut gone_rows = BTreeSet::new();

    for ((root_id, schema), files) in &mut groups {
        let doomed: Vec<usize> = if *schema != SCHEMA {
            let newest = files.iter().map(|f| f.mtime).max();
            let old =
                newest.is_some_and(|t| now.duration_since(t).unwrap_or_default() >= OLD_SCHEMA_AGE);
            match old {
                true => (0..files.len()).collect(),
                false => Vec::new(),
            }
        } else {
            match rows.get(root_id) {
                None => (0..files.len()).collect(),
                Some(r) if stale(r, now_ms, unseen_ms) => {
                    gone_rows.insert(root_id.clone());
                    (0..files.len()).collect()
                }
                Some(r) => {
                    let live = r
                        .db_file
                        .clone()
                        .unwrap_or_else(|| format!("{root_id}.v{SCHEMA}.db"));
                    (0..files.len())
                        .filter(|&i| files[i].name.db != live)
                        .collect()
                }
            }
        };
        let drop_row = gone_rows.contains(root_id);
        let lock = match doomed.is_empty() {
            true => None,
            false => delete(&dir, root_id, *schema, files, &doomed, &mut report)?,
        };
        match lock {
            // Under the lock, so no process opens the root in between.
            Some(_lock) if drop_row => reg.remove(root_id),
            Some(_) => {}
            None => {
                gone_rows.remove(root_id);
            }
        }
    }

    // Aged rows without any DB file left: nothing to lock or delete.
    for r in rows.values() {
        let has_files = groups.contains_key(&(r.root_id.clone(), SCHEMA));
        if !has_files && stale(r, now_ms, unseen_ms) {
            reg.remove(&r.root_id);
            gone_rows.insert(r.root_id.clone());
        }
    }

    // Over budget: least recently seen roots first.
    let mut total: u64 = groups.values().flatten().map(|f| f.size).sum();
    if total > opts.budget_bytes {
        let mut by_age: Vec<&GcRow> = rows
            .values()
            .filter(|r| !gone_rows.contains(&r.root_id))
            .collect();
        by_age.sort_by_key(|r| (r.last_seen_ms, r.root_id.clone()));
        for r in by_age {
            if total <= opts.budget_bytes {
                break;
            }
            let Some(files) = groups.get_mut(&(r.root_id.clone(), SCHEMA)) else {
                continue;
            };
            if files.is_empty() {
                continue;
            }
            let all: Vec<usize> = (0..files.len()).collect();
            if let Some(_lock) = delete(&dir, &r.root_id, SCHEMA, files, &all, &mut report)? {
                reg.remove(&r.root_id);
                total = groups.values().flatten().map(|f| f.size).sum();
            }
        }
    }
    report.deleted.sort();
    report.skipped_busy.sort();
    Ok(Some(report))
}

/// Not seen for `unseen_ms`, or its path is gone.
fn stale(r: &GcRow, now_ms: u64, unseen_ms: u64) -> bool {
    now_ms.saturating_sub(r.last_seen_ms) > unseen_ms
        || matches!(std::fs::symlink_metadata(&r.path), Err(e) if e.kind() == io::ErrorKind::NotFound)
}

/// Delete `files[doomed]` under the scope's exclusive `.open` lock, and drop
/// them from `files`. Returns the lock, still held, if it was taken; if
/// not, the files are reported as busy.
fn delete(
    dir: &Path,
    root_id: &str,
    schema: u32,
    files: &mut Vec<Found>,
    doomed: &[usize],
    report: &mut GcReport,
) -> Result<Option<File>, Error> {
    let Some(lock) = lock_scope(dir, root_id, schema)? else {
        report
            .skipped_busy
            .extend(doomed.iter().map(|&i| files[i].path.clone()));
        return Ok(None);
    };
    // The main file first: its -wal and -shm are useless without it.
    let mut order: Vec<usize> = doomed.to_vec();
    order.sort_by_key(|&i| files[i].path.as_os_str().len());
    for &i in &order {
        // `remove_file` unlinks a symlink itself, never its target; `list`
        // already skipped symlinks.
        match std::fs::remove_file(&files[i].path) {
            Ok(()) => report.deleted.push(files[i].path.clone()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    let mut i = 0;
    files.retain(|_| {
        i += 1;
        !doomed.contains(&(i - 1))
    });
    Ok(Some(lock))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_parse_into_lock_scopes() {
        let n = parse_name("00ab.v2-0123abcd.db-wal").unwrap();
        assert_eq!((n.root_id.as_str(), n.schema), ("00ab", 2));
        assert_eq!(n.db, "00ab.v2-0123abcd.db");
        assert_eq!(parse_name("00ab.v1.db").unwrap().schema, 1);
        for bad in [
            "00ab.v2.lock",
            "00ab.v2.open",
            "roots.v1.db",
            "00ab.v2-xyz.db",
            "00ab.v2-0123abc.db",
            "00ab.vx.db",
            ".v2.db",
        ] {
            assert_eq!(parse_name(bad), None, "{bad}");
        }
        assert!(is_db_file_of(
            &format!("00ab.v{SCHEMA}-0123abcd.db"),
            "00ab"
        ));
        assert!(!is_db_file_of(&format!("00ab.v{SCHEMA}.db-wal"), "00ab"));
        assert!(!is_db_file_of(&format!("00ab.v{SCHEMA}.db"), "00ac"));
    }
}
