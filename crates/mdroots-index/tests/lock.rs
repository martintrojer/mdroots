//! Reconciler election (docs/DECISIONS.md D3), in tempdirs.

use mdroots_index::lock::{Role, RootLocks};

#[test]
fn first_reconciler_second_peer_then_promote() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("roots"); // not yet created
    let a = RootLocks::acquire(&dir, "r1.v1").unwrap();
    assert_eq!(a.role(), Role::Reconciler);
    let mut b = RootLocks::acquire(&dir, "r1.v1").unwrap();
    assert_eq!(b.role(), Role::Peer);
    assert_eq!(b.try_promote().unwrap(), Role::Peer);
    drop(a);
    assert_eq!(b.try_promote().unwrap(), Role::Reconciler);
    assert_eq!(b.role(), Role::Reconciler);
    drop(b);
    assert!(dir.join("r1.v1.lock").exists());
    assert!(dir.join("r1.v1.open").exists());
}

#[test]
fn separate_stems_are_independent() {
    let tmp = tempfile::tempdir().unwrap();
    let a = RootLocks::acquire(tmp.path(), "a.v1").unwrap();
    let b = RootLocks::acquire(tmp.path(), "b.v1").unwrap();
    assert_eq!((a.role(), b.role()), (Role::Reconciler, Role::Reconciler));
}
