//! The native file watcher of a reconciler workspace (docs/specs/index.md
//! §1.3): [notify](https://crates.io/crates/notify) reports changes under
//! the root, a thread named `mdroots-watch` debounces them and calls
//! [`Workspace::refresh_paths`].
//!
//! Only a [`Workspace`] opened with [`Options::watch`](crate::Options::watch)
//! that is the reconciler of a DB-backed marker, VCS or loose root on a
//! local filesystem watches (never virtual, remote or cloud folders, and
//! never lazy, single-file, tracked-only, index-driven or vcs-enumerated
//! roots). The thread holds only a weak reference: it ends when the last
//! clone of the workspace drops (which drops the notify watcher and closes
//! its channel), and it never keeps the process alive.

use std::path::{Path, PathBuf};
use std::sync::Weak;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use mdroots_core::{Cancel, FileSystem};
use mdroots_index::Role;
use mdroots_roots::probe::Probe;
use mdroots_roots::{FsClass, RootMode, classify, pruned_dir};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::workspace::{Inner, Workspace, is_note};

/// Quiet time after the last event before a batch is applied ...
const QUIET: Duration = Duration::from_millis(200);
/// ... but never later than this after the batch's first event.
const CAP: Duration = Duration::from_secs(1);

type Raw = notify::Result<Event>;

/// Whether a workspace may run the watcher: a marker, VCS or loose root
/// (tracked-only roots can be a home dir; vcs-enumerated roots never walk),
/// this process its reconciler, a DB present, on a local filesystem.
pub(crate) fn watch_eligible(
    mode: RootMode,
    role: Option<Role>,
    has_db: bool,
    class: &FsClass,
) -> bool {
    matches!(mode, RootMode::Marker | RootMode::Vcs | RootMode::Loose)
        && role == Some(Role::Reconciler)
        && has_db
        && *class == FsClass::Local
}

/// The filesystem class of `root`; `None` when its mount cannot be read.
pub(crate) fn fs_class(probe: &dyn Probe, root: &Path) -> Option<FsClass> {
    let m = probe.mount(root).ok()?;
    Some(classify(&m, root, probe.home().as_deref()))
}

/// Start watching `root` for `inner`. `None` when notify cannot watch it
/// or the thread does not start: the workspace then just does not watch.
pub(crate) fn start(inner: Weak<Inner>, root: &Path) -> Option<RecommendedWatcher> {
    let (tx, rx) = mpsc::channel::<Raw>();
    let mut watcher = notify::recommended_watcher(move |r: Raw| {
        let _ = tx.send(r);
    })
    .ok()?;
    watcher.watch(root, RecursiveMode::Recursive).ok()?;
    let root = root.to_path_buf();
    std::thread::Builder::new()
        .name("mdroots-watch".to_owned())
        .spawn(move || run(&inner, &root, &rx))
        .ok()?;
    Some(watcher)
}

/// What one debounced batch asks for.
#[derive(Default)]
struct Batch {
    paths: Vec<PathBuf>,
    /// notify lost events (or failed): re-list the whole root.
    rescan: bool,
}

impl Batch {
    fn add(&mut self, r: Raw) {
        match r {
            Ok(e) if e.need_rescan() => self.rescan = true,
            // Reads (ours included) are not changes.
            Ok(Event {
                kind: EventKind::Access(_),
                ..
            }) => {}
            Ok(e) => self.paths.extend(e.paths),
            Err(_) => self.rescan = true,
        }
    }
}

/// Until the channel closes (the watcher dropped) or the workspace is gone.
fn run(inner: &Weak<Inner>, root: &Path, rx: &Receiver<Raw>) {
    while let Some(batch) = next_batch(rx) {
        // A strong reference only while applying the batch.
        let Some(inner) = inner.upgrade() else {
            return;
        };
        let ws = Workspace::from_inner(inner);
        apply(&ws, root, batch);
    }
}

/// Blocks for the first event, then collects until [`QUIET`] passes
/// without one or [`CAP`] after the first. `None` once the channel closes.
fn next_batch(rx: &Receiver<Raw>) -> Option<Batch> {
    let mut batch = Batch::default();
    batch.add(rx.recv().ok()?);
    let first = Instant::now();
    loop {
        let left = CAP.saturating_sub(first.elapsed());
        if left.is_zero() {
            return Some(batch);
        }
        match rx.recv_timeout(QUIET.min(left)) {
            Ok(r) => batch.add(r),
            Err(RecvTimeoutError::Timeout) => return Some(batch),
            // Apply what was collected; the next recv sees the close.
            Err(RecvTimeoutError::Disconnected) => return Some(batch),
        }
    }
}

fn apply(ws: &Workspace, root: &Path, batch: Batch) {
    let cancel = Cancel::new();
    if batch.rescan {
        if ws.refresh(&cancel).is_ok() {
            ws.notify_subscribers(ws.files());
        }
        return;
    }
    // An event on the root itself (a backend may coalesce to the watched
    // dir) says only "something changed here": re-list the whole root.
    if batch.paths.iter().any(|p| p == root) {
        if ws.refresh(&cancel).is_ok() {
            ws.notify_subscribers(ws.files());
        }
        return;
    }
    let mut paths: Vec<PathBuf> = batch
        .paths
        .into_iter()
        .filter(|p| relevant(ws.fs(), root, p))
        .collect();
    paths.sort();
    paths.dedup();
    if paths.is_empty() {
        return;
    }
    match ws.refresh_paths(&paths, &cancel) {
        Ok(changed) if !changed.is_empty() => ws.notify_subscribers(changed),
        Ok(_) => {}
        // A failed point refresh (a DB error, say) falls back to a full one.
        Err(_) => {
            if ws.refresh(&cancel).is_ok() {
                ws.notify_subscribers(ws.files());
            }
        }
    }
}

/// Whether an event path matters: strictly under `root`, no root-relative
/// component hidden (leading `.`) or in the walk's prune list, and a note,
/// a directory, or gone (a removed directory leaves no trace to check).
/// Only root-relative components count, so a root inside a hidden
/// directory (a temp dir, say) still gets its events.
pub(crate) fn relevant(fs: &dyn FileSystem, root: &Path, p: &Path) -> bool {
    let Ok(rel) = p.strip_prefix(root) else {
        return false;
    };
    let names: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let Some(last) = names.last() else {
        return false;
    };
    if names.iter().any(|n| n.starts_with('.') || pruned_dir(n)) {
        return false;
    }
    if is_note(last) {
        return true;
    }
    match fs.stat(p) {
        Ok(m) => m.is_dir,
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mdroots_core::MemFs;

    #[test]
    fn eligible_only_for_local_reconciler_db_walked_roots() {
        let local = FsClass::Local;
        let r = Some(Role::Reconciler);
        for mode in [RootMode::Marker, RootMode::Vcs, RootMode::Loose] {
            assert!(watch_eligible(mode, r, true, &local), "{mode:?}");
        }
        for mode in [
            RootMode::TrackedOnly,
            RootMode::IndexDriven,
            RootMode::VcsEnumerated,
            RootMode::Lazy,
            RootMode::SingleFile,
        ] {
            assert!(!watch_eligible(mode, r, true, &local), "{mode:?}");
        }
        for class in [
            FsClass::Virtual("edenfs".into()),
            FsClass::Remote("nfs".into()),
            FsClass::Cloud,
        ] {
            assert!(
                !watch_eligible(RootMode::Marker, r, true, &class),
                "{class:?}"
            );
        }
        assert!(!watch_eligible(
            RootMode::Marker,
            Some(Role::Peer),
            true,
            &local
        ));
        assert!(!watch_eligible(RootMode::Marker, None, false, &local));
        assert!(!watch_eligible(RootMode::Marker, r, false, &local));
    }

    #[test]
    fn relevant_filters_root_relative_components_only() {
        let fs = MemFs::new()
            .with_file("/t/.tmp1/v/a.md", "")
            .with_file("/t/.tmp1/v/x.txt", "")
            .with_dir("/t/.tmp1/v/sub");
        let root = Path::new("/t/.tmp1/v");
        let yes = |p: &str| relevant(&fs, root, Path::new(p));
        assert!(yes("/t/.tmp1/v/a.md"));
        assert!(yes("/t/.tmp1/v/sub"));
        assert!(yes("/t/.tmp1/v/removed-dir"));
        assert!(yes("/t/.tmp1/v/new.org"));
        assert!(!yes("/t/.tmp1/v/x.txt"));
        // The root itself is handled by `apply` (a full refresh), not here.
        assert!(!yes("/t/.tmp1/v"));
        assert!(!yes("/t/.tmp1/other/a.md"));
        assert!(!yes("/t/.tmp1/v/.git/HEAD"));
        assert!(!yes("/t/.tmp1/v/.hidden.md"));
        assert!(!yes("/t/.tmp1/v/node_modules/a.md"));
        assert!(!yes("/t/.tmp1/v/sub/target/a.md"));
    }
}

#[cfg(test)]
mod apply_tests {
    use super::*;
    use crate::{IndexMode, NoEnumerator, Options, StdFs, StdProbe};
    use std::sync::Arc;

    /// A batch holding only the root path (a coalesced backend event)
    /// re-lists the root, so a note added at the top level appears.
    #[test]
    fn a_root_only_event_relists_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap().join("v");
        std::fs::create_dir_all(root.join(".zk")).unwrap();
        std::fs::write(root.join("a.md"), "# A\n").unwrap();
        let opts = Options::default()
            .fs(Arc::new(StdFs))
            .probe(Arc::new(StdProbe))
            .enumerator(Arc::new(NoEnumerator))
            .index(IndexMode::Memory);
        let ws = Workspace::open_for(&root.join("a.md"), opts).unwrap();
        std::fs::write(root.join("b.md"), "# B\n").unwrap();
        let rx = ws.subscribe();
        let batch = Batch {
            paths: vec![root.clone()],
            rescan: false,
        };
        apply(&ws, &root, batch);
        assert!(ws.files().contains(&root.join("b.md")), "{:?}", ws.files());
        assert!(rx.try_recv().is_ok(), "subscribers notified");
    }
}
