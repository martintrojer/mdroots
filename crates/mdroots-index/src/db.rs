//! The per-root [SQLite](https://sqlite.org) DB (docs/DECISIONS.md D4,
//! docs/specs/index.md §1). It caches each indexed note's content and stat,
//! so a process can hydrate its in-memory index without opening unchanged
//! files. The DB is a disposable cache: correctness never depends on it.
//! An [FTS5](https://sqlite.org/fts5.html) table `fts` mirrors each row's
//! content for [`IndexDb::search`]; its rowid is the `files` row id.
//!
//! Only the reconciler (see [`crate::lock`]) calls [`IndexDb::apply`]. Every
//! write is one `BEGIN IMMEDIATE` transaction, because a deferred
//! read-then-write transaction gets `SQLITE_BUSY` at once and ignores
//! `busy_timeout`.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mdroots_core::{Error, ErrorKind};
use rusqlite::{Connection, ErrorCode, OptionalExtension, TransactionBehavior, params};

/// Schema version; also part of the DB file name, so binaries with
/// different schemas never share a file.
pub const SCHEMA: u32 = 2;

/// `change_log` keeps this many newest entries; peers that fall further
/// behind re-read the file list.
const CHANGE_LOG_KEEP: i64 = 10_000;

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS files(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    path TEXT UNIQUE NOT NULL,
    ino INTEGER,
    ctime_ns INTEGER,
    mtime_ns INTEGER,
    size INTEGER,
    content BLOB NOT NULL,
    indexed_at INTEGER
);
CREATE TABLE IF NOT EXISTS change_log(
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    path TEXT,
    kind TEXT
);
CREATE TABLE IF NOT EXISTS meta(
    key TEXT PRIMARY KEY,
    value TEXT
);
CREATE VIRTUAL TABLE IF NOT EXISTS fts USING fts5(
    path UNINDEXED,
    body,
    tokenize = 'unicode61 remove_diacritics 2'
);
";

/// One cached file: its root-relative path, the stat it was read with and
/// its bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct FileRow {
    pub path: String,
    pub ino: u64,
    pub ctime_ns: i64,
    pub mtime_ns: i64,
    pub size: u64,
    pub content: Arc<[u8]>,
}

/// One write for [`IndexDb::apply`].
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// Insert or replace the row with this path.
    Upsert(FileRow),
    /// Delete the row with this root-relative path.
    Remove(String),
    /// Update only the stat columns of an existing row (same content, e.g.
    /// after `touch`); no `change_log` entry, so peers do not re-read it.
    /// A missing row is left alone.
    Stat(FileRow),
}

/// A connection to one root's DB.
#[derive(Debug)]
pub struct IndexDb {
    conn: Connection,
}

/// Map a SQLite error: corruption is [`ErrorKind::Corrupt`] (the caller
/// rebuilds), everything else [`ErrorKind::Io`] with the message kept.
pub(crate) fn sql_err(e: rusqlite::Error) -> Error {
    let corrupt = matches!(
        e.sqlite_error_code(),
        Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase)
    );
    let kind = if corrupt {
        ErrorKind::Corrupt
    } else {
        ErrorKind::Io
    };
    Error::new(kind, e.to_string())
}

/// 16 hex digits, different per DB creation: std's per-process random hash
/// keys over the time and pid.
pub(crate) fn new_generation() -> String {
    let mut h = RandomState::new().build_hasher();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    h.write_u128(now.as_nanos());
    h.write_u32(std::process::id());
    format!("{:016x}", h.finish())
}

fn now_ms() -> i64 {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

impl IndexDb {
    /// Open or create the DB at `path` (creating its parent dir, 0700), set
    /// WAL, `synchronous=NORMAL`, `busy_timeout` 2 s and foreign keys, and
    /// create the schema in one `BEGIN IMMEDIATE` transaction.
    ///
    /// A DB of another schema version is [`ErrorKind::Corrupt`].
    pub fn open(path: &Path) -> Result<IndexDb, Error> {
        IndexDb::open_with_generation(path, &new_generation())
    }

    /// [`IndexDb::open`], with `generation` as `meta.generation` if the DB
    /// is created now (an existing DB keeps its own).
    pub fn open_with_generation(path: &Path, generation: &str) -> Result<IndexDb, Error> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            crate::create_private_dir(parent)?;
        }
        let conn = Connection::open(path).map_err(sql_err)?;
        IndexDb::init(conn, generation)
    }

    /// An empty DB in memory, never shared: the stand-in a peer serves from
    /// while the root's DB file is corrupt and not yet rebuilt.
    pub fn open_in_memory() -> Result<IndexDb, Error> {
        let conn = Connection::open_in_memory().map_err(sql_err)?;
        IndexDb::init(conn, &new_generation())
    }

    /// Build a new generation file for a corrupt DB: `<dir>/<stem>-<gen8>.db`
    /// where `gen8` is the first 8 hex digits of its new `meta.generation`.
    /// Returns the DB and its file name. The old file is left alone (it may
    /// still be open elsewhere); GC deletes it once nobody holds the root.
    pub fn open_new_generation(dir: &Path, stem: &str) -> Result<(IndexDb, String), Error> {
        let generation = new_generation();
        let name = format!("{stem}-{}.db", &generation[..8]);
        let db = IndexDb::open_with_generation(&dir.join(&name), &generation)?;
        Ok((db, name))
    }

    /// `PRAGMA quick_check`; anything but `ok` is [`ErrorKind::Corrupt`].
    pub fn quick_check(&self) -> Result<(), Error> {
        let mut st = self.conn.prepare("PRAGMA quick_check").map_err(sql_err)?;
        let lines = st
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(sql_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_err)?;
        match lines.as_slice() {
            [ok] if ok == "ok" => Ok(()),
            _ => Err(Error::new(
                ErrorKind::Corrupt,
                format!("quick_check: {}", lines.join("; ")),
            )),
        }
    }

    fn init(mut conn: Connection, generation: &str) -> Result<IndexDb, Error> {
        conn.busy_timeout(Duration::from_secs(2)).map_err(sql_err)?;
        conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get::<_, String>(0))
            .map_err(sql_err)?;
        conn.execute_batch("PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;")
            .map_err(sql_err)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        tx.execute_batch(SCHEMA_SQL).map_err(sql_err)?;
        tx.execute(
            "INSERT OR IGNORE INTO meta(key, value) VALUES ('schema', ?1), ('generation', ?2)",
            params![SCHEMA.to_string(), generation],
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
        Ok(IndexDb { conn })
    }

    /// `meta.generation`: changes only when the DB is rebuilt.
    pub fn generation(&self) -> Result<String, Error> {
        self.conn
            .query_row("SELECT value FROM meta WHERE key = 'generation'", [], |r| {
                r.get(0)
            })
            .map_err(sql_err)
    }

    /// `PRAGMA data_version`: changes when another connection commits.
    pub fn data_version(&self) -> Result<i64, Error> {
        self.conn
            .query_row("PRAGMA data_version", [], |r| r.get(0))
            .map_err(sql_err)
    }

    /// Every `files` row, sorted by path.
    pub fn rows(&self) -> Result<Vec<FileRow>, Error> {
        let mut st = self
            .conn
            .prepare_cached(
                "SELECT path, ino, ctime_ns, mtime_ns, size, content FROM files ORDER BY path",
            )
            .map_err(sql_err)?;
        let rows = st
            .query_map([], |r| {
                // Stored as SQLite's signed 64-bit integers; bit-cast back.
                Ok(FileRow {
                    path: r.get(0)?,
                    ino: r.get::<_, i64>(1)? as u64,
                    ctime_ns: r.get(2)?,
                    mtime_ns: r.get(3)?,
                    size: r.get::<_, i64>(4)? as u64,
                    content: Arc::from(r.get::<_, Vec<u8>>(5)?),
                })
            })
            .map_err(sql_err)?;
        rows.collect::<Result<_, _>>().map_err(sql_err)
    }

    /// Apply `changes` in one `BEGIN IMMEDIATE` transaction, logging each
    /// effective change to `change_log` (`add`: new row, `mod`: replaced
    /// row, `del`: deleted row; removing a missing row logs nothing), then
    /// trim `change_log` to its newest 10,000 entries.
    pub fn apply(&mut self, changes: &[Change]) -> Result<(), Error> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        {
            let mut id_of = tx
                .prepare_cached("SELECT id FROM files WHERE path = ?1")
                .map_err(sql_err)?;
            let mut fts_del = tx
                .prepare_cached("DELETE FROM fts WHERE rowid = ?1")
                .map_err(sql_err)?;
            let mut fts_add = tx
                .prepare_cached("INSERT INTO fts(rowid, path, body) VALUES (?1, ?2, ?3)")
                .map_err(sql_err)?;
            let mut upsert = tx
                .prepare_cached(
                    "INSERT INTO files(path, ino, ctime_ns, mtime_ns, size, content, indexed_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(path) DO UPDATE SET
                       ino = excluded.ino, ctime_ns = excluded.ctime_ns,
                       mtime_ns = excluded.mtime_ns, size = excluded.size,
                       content = excluded.content, indexed_at = excluded.indexed_at",
                )
                .map_err(sql_err)?;
            let mut remove = tx
                .prepare_cached("DELETE FROM files WHERE path = ?1")
                .map_err(sql_err)?;
            let mut stat = tx
                .prepare_cached(
                    "UPDATE files SET ino = ?2, ctime_ns = ?3, mtime_ns = ?4, size = ?5,
                       indexed_at = ?6 WHERE path = ?1",
                )
                .map_err(sql_err)?;
            let mut log = tx
                .prepare_cached("INSERT INTO change_log(path, kind) VALUES (?1, ?2)")
                .map_err(sql_err)?;
            let now = now_ms();
            for c in changes {
                match c {
                    Change::Upsert(f) => {
                        let had: Option<i64> = id_of
                            .query_row([&f.path], |r| r.get(0))
                            .optional()
                            .map_err(sql_err)?;
                        upsert
                            .execute(params![
                                f.path,
                                f.ino as i64,
                                f.ctime_ns,
                                f.mtime_ns,
                                f.size as i64,
                                &f.content[..],
                                now
                            ])
                            .map_err(sql_err)?;
                        // The id survives ON CONFLICT DO UPDATE.
                        let id: i64 = id_of.query_row([&f.path], |r| r.get(0)).map_err(sql_err)?;
                        fts_del.execute([id]).map_err(sql_err)?;
                        let body = String::from_utf8_lossy(&f.content);
                        fts_add
                            .execute(params![id, f.path, body.as_ref()])
                            .map_err(sql_err)?;
                        let kind = if had.is_some() { "mod" } else { "add" };
                        log.execute(params![f.path, kind]).map_err(sql_err)?;
                    }
                    Change::Stat(f) => {
                        stat.execute(params![
                            f.path,
                            f.ino as i64,
                            f.ctime_ns,
                            f.mtime_ns,
                            f.size as i64,
                            now
                        ])
                        .map_err(sql_err)?;
                    }
                    Change::Remove(p) => {
                        let id: Option<i64> = id_of
                            .query_row([p], |r| r.get(0))
                            .optional()
                            .map_err(sql_err)?;
                        if let Some(id) = id {
                            fts_del.execute([id]).map_err(sql_err)?;
                        }
                        if remove.execute([p]).map_err(sql_err)? > 0 {
                            log.execute(params![p, "del"]).map_err(sql_err)?;
                        }
                    }
                }
            }
            tx.execute(
                "DELETE FROM change_log WHERE seq <= (SELECT MAX(seq) FROM change_log) - ?1",
                [CHANGE_LOG_KEEP],
            )
            .map_err(sql_err)?;
        }
        tx.commit().map_err(sql_err)
    }

    /// Full-text search over the cached contents: up to `limit` notes as
    /// `(path, snippet)`, best first (bm25, then path). The snippet is
    /// FTS5's `snippet()` of about 12 tokens, unmarked, raw (it may span
    /// lines).
    ///
    /// `query` is plain text, never FTS5 syntax: it is split on whitespace,
    /// terms without a letter or digit are dropped, each remaining term is
    /// quoted (so `AND`, `NEAR(`, `-x` or `"` match as text), all terms must
    /// match, and the last one also matches as a prefix. A query without
    /// terms finds nothing.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<(String, String)>, Error> {
        let Some(q) = fts_query(query) else {
            return Ok(Vec::new());
        };
        let mut st = self
            .conn
            .prepare_cached(
                "SELECT path, snippet(fts, 1, '', '', '…', 12) FROM fts
                 WHERE fts MATCH ?1 ORDER BY bm25(fts), path LIMIT ?2",
            )
            .map_err(sql_err)?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = st
            .query_map(params![q, limit], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(sql_err)?;
        rows.collect::<Result<_, _>>().map_err(sql_err)
    }
}

/// The FTS5 query for the plain-text `query` (see [`IndexDb::search`]);
/// `None` without terms.
fn fts_query(query: &str) -> Option<String> {
    let terms: Vec<String> = query
        .split_whitespace()
        .filter(|t| t.chars().any(char::is_alphanumeric))
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    if terms.is_empty() {
        return None;
    }
    Some(terms.join(" AND ") + "*")
}
