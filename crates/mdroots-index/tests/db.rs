//! The per-root [SQLite](https://sqlite.org) DB, in tempdirs.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use mdroots_core::ErrorKind;
use mdroots_index::db::{Change, FileRow, IndexDb};

fn row(path: &str, body: &str) -> FileRow {
    FileRow {
        path: path.into(),
        ino: u64::MAX - 1,
        ctime_ns: 1_700_000_000_000_000_001,
        mtime_ns: -5,
        size: body.len() as u64,
        content: Arc::from(body.as_bytes()),
    }
}

fn change_log(db: &Path) -> Vec<(String, String)> {
    let c = rusqlite::Connection::open(db).unwrap();
    let mut st = c
        .prepare("SELECT path, kind FROM change_log ORDER BY seq")
        .unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn kinds(v: &[(&str, &str)]) -> Vec<(String, String)> {
    v.iter()
        .map(|(p, k)| (p.to_string(), k.to_string()))
        .collect()
}

#[test]
fn two_connections_share_generation() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("roots/r.v1.db"); // parent not yet created
    let a = IndexDb::open(&p).unwrap();
    let b = IndexDb::open(&p).unwrap();
    let g = a.generation().unwrap();
    assert_eq!(g.len(), 16);
    assert!(g.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(b.generation().unwrap(), g);
    drop((a, b));
    assert_eq!(IndexDb::open(&p).unwrap().generation().unwrap(), g);
    let other = IndexDb::open(&tmp.path().join("other.db")).unwrap();
    assert_ne!(other.generation().unwrap(), g);
}

#[test]
fn apply_upsert_remove_upsert() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("r.db");
    let mut db = IndexDb::open(&p).unwrap();
    db.apply(&[
        Change::Upsert(row("b.md", "bee")),
        Change::Upsert(row("a.md", "ay")),
    ])
    .unwrap();
    assert_eq!(
        db.rows().unwrap(),
        vec![row("a.md", "ay"), row("b.md", "bee")]
    );

    db.apply(&[
        Change::Upsert(row("a.md", "ay2")),
        Change::Remove("b.md".into()),
        Change::Remove("missing.md".into()),
    ])
    .unwrap();
    assert_eq!(db.rows().unwrap(), vec![row("a.md", "ay2")]);

    db.apply(&[Change::Upsert(row("b.md", "again"))]).unwrap();
    assert_eq!(
        db.rows().unwrap(),
        vec![row("a.md", "ay2"), row("b.md", "again")]
    );
    assert_eq!(
        change_log(&p),
        kinds(&[
            ("b.md", "add"),
            ("a.md", "add"),
            ("a.md", "mod"),
            ("b.md", "del"),
            ("b.md", "add"),
        ])
    );
}

#[test]
fn data_version_changes_for_other_connection() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("r.db");
    let mut writer = IndexDb::open(&p).unwrap();
    let reader = IndexDb::open(&p).unwrap();
    let before = reader.data_version().unwrap();
    assert_eq!(reader.data_version().unwrap(), before);
    writer.apply(&[Change::Upsert(row("a.md", "x"))]).unwrap();
    assert_ne!(reader.data_version().unwrap(), before);
    assert_eq!(reader.rows().unwrap(), vec![row("a.md", "x")]);
}

#[test]
fn schema_mismatch_is_corrupt() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("r.db");
    drop(IndexDb::open(&p).unwrap());
    let c = rusqlite::Connection::open(&p).unwrap();
    c.execute("UPDATE meta SET value = '99' WHERE key = 'schema'", [])
        .unwrap();
    drop(c);
    let e = IndexDb::open(&p).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Corrupt);
    assert_eq!(e.message(), "schema 99");
}

#[test]
fn not_a_database_is_corrupt() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("r.db");
    std::fs::write(&p, vec![b'x'; 4096]).unwrap();
    let e = IndexDb::open(&p).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Corrupt, "{e}");
}

#[test]
fn thousand_rows_under_a_second() {
    let tmp = tempfile::tempdir().unwrap();
    let mut db = IndexDb::open(&tmp.path().join("r.db")).unwrap();
    let body = "# Title\n\nSome text with a [[link]].\n".repeat(20);
    let changes: Vec<Change> = (0..1000)
        .map(|i| Change::Upsert(row(&format!("n/{i:04}.md"), &body)))
        .collect();
    let t = Instant::now();
    db.apply(&changes).unwrap();
    let took = t.elapsed();
    assert!(took.as_secs_f64() < 1.0, "{took:?}");
    assert_eq!(db.rows().unwrap().len(), 1000);
}
