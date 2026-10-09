//! Corruption rebuild and DB file switching through the facade, on real
//! temp dirs with a temp cache dir (never the user's).

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use mdroots::index::{IndexDb, RootLocks, SCHEMA, SqliteRegistry};
use mdroots::{Cancel, NoEnumerator, Options, Role, StdFs, StdProbe, Workspace};
use mdroots_roots::registry::Registry;
use tempfile::TempDir;

/// A marker root at `<tmp>/vault` with two notes, and a temp cache dir.
struct Vault {
    _tmp: TempDir,
    cache: TempDir,
    dir: PathBuf,
}

impl Vault {
    fn new() -> Vault {
        let tmp = tempfile::tempdir().unwrap();
        let dir = fs::canonicalize(tmp.path()).unwrap().join("vault");
        fs::create_dir_all(dir.join(".zk")).unwrap();
        fs::write(dir.join("a.md"), "# A\n\n[[b]]\n").unwrap();
        fs::write(dir.join("b.md"), "# B\n").unwrap();
        Vault {
            _tmp: tmp,
            cache: tempfile::tempdir().unwrap(),
            dir,
        }
    }

    fn open(&self) -> Workspace {
        let opts = Options::default()
            .fs(Arc::new(StdFs))
            .probe(Arc::new(StdProbe))
            .enumerator(Arc::new(NoEnumerator))
            .cache_dir(self.cache.path().to_path_buf());
        Workspace::open_for(&self.dir.join("a.md"), opts).unwrap()
    }

    fn registry(&self) -> SqliteRegistry {
        SqliteRegistry::open(&self.cache.path().join("roots.v1.db")).unwrap()
    }

    fn root_id(&self) -> String {
        self.registry().all()[0].root_id.clone()
    }

    fn roots(&self) -> PathBuf {
        self.cache.path().join("roots")
    }

    fn recorded(&self) -> String {
        self.registry().db_file(&self.root_id()).unwrap()
    }
}

/// Overwrite the first 100 bytes (the [SQLite](https://sqlite.org) header) of `p`.
fn corrupt(p: &std::path::Path) {
    let mut bytes = fs::read(p).unwrap();
    bytes[..100].fill(0xa5);
    fs::write(p, bytes).unwrap();
}

fn name(p: &std::path::Path) -> String {
    p.file_name().unwrap().to_string_lossy().into_owned()
}

#[test]
fn a_corrupt_db_is_rebuilt_into_a_new_generation_file() {
    let v = Vault::new();
    let old = v.open().cache().unwrap();
    assert_eq!(name(&old), format!("{}.v{SCHEMA}.db", v.root_id()));
    // Nobody holds the root now.
    corrupt(&old);

    let ws = v.open();
    assert_eq!(ws.role(), Some(Role::Reconciler));
    assert_eq!(ws.files().len(), 2);
    let new = ws.cache().unwrap();
    let stem = format!("{}.v{SCHEMA}-", v.root_id());
    let gen8 = name(&new)
        .strip_prefix(&stem)
        .and_then(|n| n.strip_suffix(".db").map(str::to_owned))
        .unwrap_or_else(|| panic!("{new:?}"));
    assert_eq!(gen8.len(), 8);
    assert_eq!(v.recorded(), name(&new));
    assert!(
        old.exists(),
        "the corrupt file is never renamed or unlinked"
    );
    let db = IndexDb::open(&new).unwrap();
    assert!(db.generation().unwrap().starts_with(&gen8));
    assert_eq!(db.rows().unwrap().len(), 2);
    // No new lock files: one scope per root and schema.
    let locks: Vec<String> = fs::read_dir(v.roots())
        .unwrap()
        .map(|e| name(&e.unwrap().path()))
        .filter(|n| n.ends_with(".lock") || n.ends_with(".open"))
        .collect();
    assert_eq!(locks.len(), 2, "{locks:?}");

    // The next process uses the recorded generation, not the base name.
    drop(ws);
    assert_eq!(v.open().cache(), Some(new));
}

#[test]
fn a_peer_of_a_corrupt_db_serves_from_memory_and_follows_the_rebuild() {
    let v = Vault::new();
    let old = v.open().cache().unwrap();
    corrupt(&old);
    // Someone else is the reconciler, so the next open is a peer.
    let stem = format!("{}.v{SCHEMA}", v.root_id());
    let other = RootLocks::acquire(&v.roots(), &stem).unwrap();
    let peer = v.open();
    assert_eq!(peer.role(), Some(Role::Peer));
    assert_eq!(peer.files().len(), 2, "served from the listing");
    drop(other);

    let rec = v.open();
    assert_eq!(rec.role(), Some(Role::Reconciler));
    let new = rec.cache().unwrap();
    assert_ne!(new, old);
    peer.refresh(&Cancel::new()).unwrap();
    assert_eq!(peer.role(), Some(Role::Peer));
    assert_eq!(peer.cache(), Some(new));
    assert_eq!(peer.files(), rec.files());
}

#[test]
fn a_peer_holding_the_old_file_reopens_the_recorded_one_on_refresh() {
    let v = Vault::new();
    let rec = v.open();
    let peer = v.open();
    assert_eq!(peer.role(), Some(Role::Peer));
    let old = peer.cache().unwrap();
    // Another process rebuilt the root into a new generation.
    let id = v.root_id();
    let (mut db, gen_name) =
        IndexDb::open_new_generation(&v.roots(), &format!("{id}.v{SCHEMA}")).unwrap();
    let rows = IndexDb::open(&old).unwrap().rows().unwrap();
    let changes: Vec<_> = rows
        .into_iter()
        .map(mdroots::index::Change::Upsert)
        .collect();
    db.apply(&changes).unwrap();
    v.registry().set_db_file(&id, &gen_name);

    peer.refresh(&Cancel::new()).unwrap();
    assert_eq!(peer.cache(), Some(v.roots().join(&gen_name)));
    assert_eq!(peer.files(), rec.files());
    // A point refresh after the switch is a plain one again.
    let changed = peer
        .refresh_paths(&[v.dir.join("b.md")], &Cancel::new())
        .unwrap();
    assert!(changed.is_empty(), "{changed:?}");
    // The reconciler follows too, through refresh_paths.
    let all = rec
        .refresh_paths(&[v.dir.join("b.md")], &Cancel::new())
        .unwrap();
    assert_eq!(all, rec.files(), "a switch re-reads everything");
    assert_eq!(rec.cache(), Some(v.roots().join(&gen_name)));
}

#[test]
fn open_and_refresh_stamp_last_seen() {
    let v = Vault::new();
    let ws = v.open();
    let id = v.root_id();
    stamp(&v, &id, 1);
    ws.refresh(&Cancel::new()).unwrap();
    assert!(v.registry().all()[0].last_seen_ms > 1);
}

/// Set `last_seen_ms` of `id` through a registry `update`.
fn stamp(v: &Vault, id: &str, ms: u64) {
    let mut reg = v.registry();
    let mut r = reg.all().into_iter().find(|r| r.root_id == id).unwrap();
    r.last_seen_ms = ms;
    reg.update(r);
}
