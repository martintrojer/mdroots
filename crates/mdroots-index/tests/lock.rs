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

#[cfg(unix)]
#[test]
fn existing_parent_is_made_private() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("roots");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _l = mdroots_index::lock::RootLocks::acquire(&dir, "r.v1").unwrap();
    let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
}
