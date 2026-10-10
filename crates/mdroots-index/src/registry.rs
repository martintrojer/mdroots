//! The persistent root registry, `roots.v1.db` in the cache dir
//! (docs/specs/roots.md §1 stage 1, §4, §6), on [SQLite](https://sqlite.org).
//!
//! [`SqliteRegistry`] implements [`Registry`] with the semantics of
//! [`mdroots_roots::registry::MemRegistry`], so discovery decisions survive
//! restarts and are shared by every process of the user. It also records
//! each root's per-root DB file name ([`SqliteRegistry::set_db_file`]).
//!
//! The [`Registry`] trait is infallible, and the registry is a cache: the
//! next discovery re-decides anything lost. So SQLite errors on reads yield
//! `None` or an empty list, writes through [`Registry::update`] and
//! [`SqliteRegistry::set_db_file`] ignore them, and [`Registry::insert`]
//! reports them as an [`Overlap`] with the new path. Rows that do not decode
//! (an unknown mode, say, from a newer binary) are skipped.
//!
//! Every write is one `BEGIN IMMEDIATE` transaction, because a deferred
//! read-then-write transaction gets `SQLITE_BUSY` at once and ignores
//! `busy_timeout`. That also makes the overlap check and the insert atomic
//! across processes.

use std::path::{Path, PathBuf};
use std::time::Duration;

use mdroots_core::{Error, ErrorKind};
use mdroots_roots::registry::{
    Overlap, Registry, RootMode, RootRecord, VerdictSource, check_overlap,
};
use mdroots_roots::walk::WalkStats;
use rusqlite::{Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params};

use crate::db::sql_err;

/// Registry schema version, stored in `meta.schema`.
const SCHEMA: u32 = 1;

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS roots(
    root_id TEXT PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    mode TEXT,
    marker TEXT,
    marker_ino INTEGER,
    volume_id TEXT,
    dev INTEGER,
    fs_type TEXT,
    stats TEXT,
    verdict_source TEXT,
    rate_confirmations INTEGER,
    decided_at_ms INTEGER,
    reason TEXT,
    last_seen_ms INTEGER,
    db_file TEXT
);
CREATE TABLE IF NOT EXISTS meta(
    key TEXT PRIMARY KEY,
    value TEXT
);
";

/// Every [`RootRecord`] column, in [`decode`] order.
const COLUMNS: &str = "root_id, path, mode, marker, marker_ino, volume_id, dev, fs_type, \
     stats, verdict_source, rate_confirmations, decided_at_ms, reason, last_seen_ms";

/// The root registry in one SQLite file.
#[derive(Debug)]
pub struct SqliteRegistry {
    conn: Connection,
}

fn mode_str(m: RootMode) -> &'static str {
    match m {
        RootMode::Marker => "marker",
        RootMode::Vcs => "vcs",
        RootMode::TrackedOnly => "tracked",
        RootMode::IndexDriven => "index-driven",
        RootMode::Loose => "loose",
        RootMode::SingleFile => "single-file",
        RootMode::Lazy => "lazy",
        RootMode::VcsEnumerated => "vcs-enumerated",
        _ => "unknown",
    }
}

fn parse_mode(s: &str) -> Option<RootMode> {
    Some(match s {
        "marker" => RootMode::Marker,
        "vcs" => RootMode::Vcs,
        "tracked" => RootMode::TrackedOnly,
        "index-driven" => RootMode::IndexDriven,
        "loose" => RootMode::Loose,
        "single-file" => RootMode::SingleFile,
        "lazy" => RootMode::Lazy,
        "vcs-enumerated" => RootMode::VcsEnumerated,
        _ => return None,
    })
}

fn source_str(v: VerdictSource) -> &'static str {
    match v {
        VerdictSource::Fs => "fs",
        VerdictSource::Eden => "eden",
        VerdictSource::Budget => "budget",
        VerdictSource::Rate => "rate",
        VerdictSource::User => "user",
        _ => "unknown",
    }
}

fn parse_source(s: &str) -> Option<VerdictSource> {
    Some(match s {
        "fs" => VerdictSource::Fs,
        "eden" => VerdictSource::Eden,
        "budget" => VerdictSource::Budget,
        "rate" => VerdictSource::Rate,
        "user" => VerdictSource::User,
        _ => return None,
    })
}

/// `entries,md,files,dirs,ms,ms_per_dir,max_depth`; floats by `Display`
/// (shortest exact round trip), `ms_per_dir` `None` as an empty field.
fn stats_str(s: &WalkStats) -> String {
    let per_dir = s.ms_per_dir.map(|v| v.to_string()).unwrap_or_default();
    format!(
        "{},{},{},{},{},{},{}",
        s.entries, s.md, s.files, s.dirs, s.ms, per_dir, s.max_depth
    )
}

fn parse_stats(s: &str) -> Option<WalkStats> {
    let f: Vec<&str> = s.split(',').collect();
    let [entries, md, files, dirs, ms, per_dir, max_depth] = f[..] else {
        return None;
    };
    Some(WalkStats {
        entries: entries.parse().ok()?,
        md: md.parse().ok()?,
        files: files.parse().ok()?,
        dirs: dirs.parse().ok()?,
        ms: ms.parse().ok()?,
        ms_per_dir: if per_dir.is_empty() {
            None
        } else {
            Some(per_dir.parse().ok()?)
        },
        max_depth: max_depth.parse().ok()?,
    })
}

/// One row selected with [`COLUMNS`], or `None` if any column does not
/// decode. u64 values are stored bit-cast to SQLite's signed integers.
fn decode(r: &Row<'_>) -> Option<RootRecord> {
    let mode = parse_mode(&r.get::<_, String>(2).ok()?)?;
    let verdict_source = parse_source(&r.get::<_, String>(9).ok()?)?;
    Some(RootRecord {
        root_id: r.get(0).ok()?,
        path: PathBuf::from(r.get::<_, String>(1).ok()?),
        mode,
        marker: r.get(3).ok()?,
        marker_ino: r.get::<_, Option<i64>>(4).ok()?.map(|v| v as u64),
        volume_id: r.get(5).ok()?,
        dev: r.get::<_, i64>(6).ok()? as u64,
        fs_type: r.get(7).ok()?,
        stats: parse_stats(&r.get::<_, String>(8).ok()?)?,
        verdict_source,
        rate_confirmations: u32::try_from(r.get::<_, i64>(10).ok()?).ok()?,
        decided_at_ms: r.get::<_, i64>(11).ok()? as u64,
        reason: r.get(12).ok()?,
        last_seen_ms: r.get::<_, i64>(13).ok()? as u64,
    })
}

/// Rows of `sql` (which selects [`COLUMNS`]) that decode.
fn query(
    conn: &Connection,
    sql: &str,
    p: impl rusqlite::Params,
) -> rusqlite::Result<Vec<RootRecord>> {
    let mut st = conn.prepare_cached(sql)?;
    let mut rows = st.query(p)?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        out.extend(decode(r));
    }
    Ok(out)
}

/// Every row that decodes, sorted by [`PathBuf`] order (component-wise,
/// unlike SQLite's byte order: `/a/b` sorts before `/a-b`).
fn all_rows(conn: &Connection) -> rusqlite::Result<Vec<RootRecord>> {
    let mut v = query(conn, &format!("SELECT {COLUMNS} FROM roots"), [])?;
    v.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(v)
}

/// Insert `r`, or overwrite every [`RootRecord`] column of the row with its
/// `root_id` (`db_file` is kept).
fn upsert(tx: &Transaction<'_>, r: &RootRecord, path: &str) -> rusqlite::Result<()> {
    tx.prepare_cached(&format!(
        "INSERT INTO roots({COLUMNS})
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
         ON CONFLICT(root_id) DO UPDATE SET
           path = excluded.path, mode = excluded.mode, marker = excluded.marker,
           marker_ino = excluded.marker_ino, volume_id = excluded.volume_id,
           dev = excluded.dev, fs_type = excluded.fs_type, stats = excluded.stats,
           verdict_source = excluded.verdict_source,
           rate_confirmations = excluded.rate_confirmations,
           decided_at_ms = excluded.decided_at_ms, reason = excluded.reason,
           last_seen_ms = excluded.last_seen_ms"
    ))?
    .execute(params![
        r.root_id,
        path,
        mode_str(r.mode),
        r.marker,
        r.marker_ino.map(|v| v as i64),
        r.volume_id,
        r.dev as i64,
        r.fs_type,
        stats_str(&r.stats),
        source_str(r.verdict_source),
        i64::from(r.rate_confirmations),
        r.decided_at_ms as i64,
        r.reason,
        r.last_seen_ms as i64,
    ])?;
    Ok(())
}

impl SqliteRegistry {
    /// Open or create the registry at `path` (creating its parent dir,
    /// 0700), set `busy_timeout` 2 s, then WAL, and create the schema in one
    /// `BEGIN IMMEDIATE` transaction.
    ///
    /// A registry of another schema version is [`ErrorKind::Corrupt`].
    pub fn open(path: &Path) -> Result<SqliteRegistry, Error> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            crate::create_private_dir(parent)?;
        }
        let mut conn = Connection::open(path).map_err(sql_err)?;
        // Before WAL: switching the journal mode needs a lock another
        // process may briefly hold.
        conn.busy_timeout(Duration::from_secs(2)).map_err(sql_err)?;
        conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get::<_, String>(0))
            .map_err(sql_err)?;
        conn.execute_batch("PRAGMA synchronous=NORMAL;")
            .map_err(sql_err)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        tx.execute_batch(SCHEMA_SQL).map_err(sql_err)?;
        tx.execute(
            "INSERT OR IGNORE INTO meta(key, value) VALUES ('schema', ?1)",
            [SCHEMA.to_string()],
        )
        .map_err(sql_err)?;
        let schema: Option<String> = tx
            .query_row("SELECT value FROM meta WHERE key = 'schema'", [], |r| {
                r.get(0)
            })
            .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        let schema = schema.unwrap_or_default();
        if schema != SCHEMA.to_string() {
            return Err(Error::new(ErrorKind::Corrupt, format!("schema {schema}")));
        }
        Ok(SqliteRegistry { conn })
    }

    /// Record the per-root DB file name of `root_id`; a missing row is left
    /// alone.
    pub fn set_db_file(&mut self, root_id: &str, name: &str) {
        let res = (|| {
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute(
                "UPDATE roots SET db_file = ?2 WHERE root_id = ?1",
                params![root_id, name],
            )?;
            tx.commit()
        })();
        // Errors are ignored: the registry is a cache and the next
        // discovery re-decides.
        let _ = res;
    }

    /// The per-root DB file name recorded for `root_id`.
    pub fn db_file(&self, root_id: &str) -> Option<String> {
        self.conn
            .query_row(
                "SELECT db_file FROM roots WHERE root_id = ?1",
                [root_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
            .ok()
            .flatten()
            .flatten()
    }
}

/// How often [`SqliteRegistry::touch`] re-stamps a root's `last_seen_ms`.
const TOUCH_EVERY_MS: u64 = 3_600_000;

/// How often GC runs per cache dir (docs/specs/roots.md §6).
pub(crate) const GC_EVERY_MS: u64 = 24 * 3_600_000;

/// What GC needs of one registry row.
#[derive(Debug, Clone)]
pub(crate) struct GcRow {
    pub(crate) root_id: String,
    pub(crate) path: PathBuf,
    pub(crate) last_seen_ms: u64,
    pub(crate) db_file: Option<String>,
}

impl SqliteRegistry {
    /// Stamp `last_seen_ms` of `root_id` with `now_ms`, unless it was
    /// stamped less than an hour before; a missing row is left alone.
    pub fn touch(&mut self, root_id: &str, now_ms: u64) {
        let res = (|| {
            let seen: Option<i64> = self
                .conn
                .query_row(
                    "SELECT last_seen_ms FROM roots WHERE root_id = ?1",
                    [root_id],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(seen) = seen else {
                return Ok(());
            };
            if now_ms.saturating_sub(seen as u64) < TOUCH_EVERY_MS {
                return Ok(());
            }
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute(
                "UPDATE roots SET last_seen_ms = ?2 WHERE root_id = ?1",
                params![root_id, now_ms as i64],
            )?;
            tx.commit()
        })();
        // Errors are ignored: a missed stamp only makes GC see the root as
        // older than it is, and the next open stamps it again.
        let _: rusqlite::Result<()> = res;
    }

    /// Delete the row of `root_id`; a missing row is fine.
    pub fn remove(&mut self, root_id: &str) {
        let res = (|| {
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute("DELETE FROM roots WHERE root_id = ?1", [root_id])?;
            tx.commit()
        })();
        // Errors are ignored, as for every registry write.
        let _ = res;
    }

    /// Whether GC is due: `meta.gc_at` is unset, unreadable, or at least
    /// 24 h before `now_ms`. A plain read, cheap enough for every open.
    pub fn gc_due(&self, now_ms: u64) -> bool {
        self.gc_at()
            .is_none_or(|at| now_ms.saturating_sub(at) >= GC_EVERY_MS)
    }

    fn gc_at(&self) -> Option<u64> {
        self.conn
            .query_row("SELECT value FROM meta WHERE key = 'gc_at'", [], |r| {
                r.get::<_, String>(0)
            })
            .ok()?
            .parse()
            .ok()
    }

    /// Claim this GC run: in one `BEGIN IMMEDIATE` transaction, check that
    /// GC is due (unless `force`) and set `meta.gc_at` to `now_ms`. Of
    /// several processes, only the first gets `true`.
    pub(crate) fn claim_gc(&mut self, now_ms: u64, force: bool) -> Result<bool, Error> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        let at: Option<String> = tx
            .query_row("SELECT value FROM meta WHERE key = 'gc_at'", [], |r| {
                r.get(0)
            })
            .optional()
            .map_err(sql_err)?;
        let at = at.and_then(|v| v.parse::<u64>().ok());
        if !force && at.is_some_and(|at| now_ms.saturating_sub(at) < GC_EVERY_MS) {
            return Ok(false);
        }
        tx.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES ('gc_at', ?1)",
            [now_ms.to_string()],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        Ok(true)
    }

    /// Every row's id, path, `last_seen_ms` and `db_file`, by `root_id`.
    pub(crate) fn gc_rows(&self) -> Result<Vec<GcRow>, Error> {
        let mut st = self
            .conn
            .prepare("SELECT root_id, path, last_seen_ms, db_file FROM roots ORDER BY root_id")
            .map_err(sql_err)?;
        let rows = st
            .query_map([], |r| {
                Ok(GcRow {
                    root_id: r.get(0)?,
                    path: PathBuf::from(r.get::<_, String>(1)?),
                    last_seen_ms: r.get::<_, Option<i64>>(2)?.unwrap_or(0) as u64,
                    db_file: r.get(3)?,
                })
            })
            .map_err(sql_err)?;
        rows.collect::<Result<_, _>>().map_err(sql_err)
    }
}

impl Registry for SqliteRegistry {
    fn lookup(&self, path: &Path) -> Option<RootRecord> {
        all_rows(&self.conn)
            .ok()?
            .into_iter()
            .filter(|r| path.starts_with(&r.path))
            .max_by_key(|r| r.path.components().count())
    }

    fn by_marker(&self, volume_id: &str, ino: u64) -> Option<RootRecord> {
        query(
            &self.conn,
            &format!("SELECT {COLUMNS} FROM roots WHERE volume_id = ?1 AND marker_ino = ?2"),
            params![volume_id, ino as i64],
        )
        .ok()?
        .into_iter()
        .min_by(|a, b| a.path.cmp(&b.path))
    }

    /// Enforces [`check_overlap`] against every row inside one
    /// `BEGIN IMMEDIATE` transaction, so two processes inserting the same
    /// path get one row and one [`Overlap`]. A path that is not UTF-8, or
    /// any SQLite error, is reported as an [`Overlap`] with `r.path`.
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
        let fail = || Overlap {
            existing: r.path.clone(),
        };
        let path = r.path.to_str().ok_or_else(fail)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| fail())?;
        let existing = all_rows(&tx).map_err(|_| fail())?;
        check_overlap(&existing, &r)?;
        upsert(&tx, &r, path).map_err(|_| fail())?;
        tx.commit().map_err(|_| fail())
    }

    /// Upsert by `root_id`, keeping its `db_file`. A different row already
    /// at the new path is deleted first, in the same transaction.
    fn update(&mut self, r: RootRecord) {
        if r.mode == RootMode::SingleFile {
            return;
        }
        let Some(path) = r.path.to_str() else {
            return;
        };
        let res = (|| {
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute(
                "DELETE FROM roots WHERE path = ?1 AND root_id <> ?2",
                params![path, r.root_id],
            )?;
            upsert(&tx, &r, path)?;
            tx.commit()
        })();
        // Errors are ignored: the registry is a cache and the next
        // discovery re-decides.
        let _ = res;
    }

    fn all(&self) -> Vec<RootRecord> {
        all_rows(&self.conn).unwrap_or_default()
    }

    fn remove(&mut self, root_id: &str) {
        SqliteRegistry::remove(self, root_id);
    }
}
