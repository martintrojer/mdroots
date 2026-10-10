//! Tests for the marker probe and climb. Tools named here: [git](https://git-scm.com),
//! [zk](https://github.com/zk-org/zk), [Obsidian](https://obsidian.md),
//! [marksman](https://github.com/artempyanykh/marksman), [jj](https://jj-vcs.github.io/jj/),
//! [Mercurial](https://www.mercurial-scm.org), [Sapling](https://sapling-scm.com/),
//! [EdenFS](https://github.com/facebook/sapling), [Buck2](https://buck2.build),
//! [Bazel](https://bazel.build). All trees are in-memory (FakeProbe).

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use mdroots_roots::markers::{Climb, FoundMarker, MarkerClass, StopReason, climb, markers_at};
use mdroots_roots::probe::{Counting, FakeProbe, FsStat, MountInfo, Probe};

/// Records every `stat`/`lstat` path; delegates everything.
struct StatLog<P: Probe> {
    inner: P,
    log: Mutex<Vec<PathBuf>>,
}

impl<P: Probe> StatLog<P> {
    fn new(inner: P) -> Self {
        StatLog {
            inner,
            log: Mutex::default(),
        }
    }

    fn stats(&self) -> Vec<PathBuf> {
        self.log.lock().unwrap().clone()
    }

    fn record(&self, p: &Path) {
        self.log.lock().unwrap().push(p.to_path_buf());
    }
}

impl<P: Probe> Probe for StatLog<P> {
    fn stat(&self, p: &Path) -> io::Result<FsStat> {
        self.record(p);
        self.inner.stat(p)
    }
    fn lstat(&self, p: &Path) -> io::Result<FsStat> {
        self.record(p);
        self.inner.lstat(p)
    }
    fn mount(&self, p: &Path) -> io::Result<MountInfo> {
        self.inner.mount(p)
    }
    fn read_dir(&self, p: &Path) -> io::Result<Vec<(OsString, FsStat)>> {
        self.inner.read_dir(p)
    }
    fn read_link(&self, p: &Path) -> io::Result<PathBuf> {
        self.inner.read_link(p)
    }
    fn read_small(&self, p: &Path, cap: usize) -> io::Result<Vec<u8>> {
        self.inner.read_small(p, cap)
    }
    fn read_prefix(&self, p: &Path, n: usize) -> io::Result<Vec<u8>> {
        self.inner.read_prefix(p, n)
    }
    fn volume_id(&self, p: &Path) -> io::Result<String> {
        self.inner.volume_id(p)
    }
    fn home(&self) -> Option<PathBuf> {
        self.inner.home()
    }
    fn now(&self) -> Duration {
        self.inner.now()
    }
}

type Probed = StatLog<Counting<FakeProbe>>;

/// Wrap `fake` so every `read_dir` is a violation and every stat is logged.
fn probed(fake: FakeProbe) -> Probed {
    let c = Counting::new(fake);
    c.forbid_read_dir_outside(&[]);
    StatLog::new(c)
}

fn run(p: &Probed, start: &str, folders: &[&str]) -> Climb {
    let folders: Vec<PathBuf> = folders.iter().map(PathBuf::from).collect();
    let c = climb(p, Path::new(start), &folders);
    assert_eq!(p.inner.read_dir_total(), 0, "read_dir called");
    assert!(p.inner.violations().is_empty());
    c
}

/// `markers_at` with the same zero-`read_dir` checks as [`run`].
fn at(p: &Probed, dir: &str) -> Vec<FoundMarker> {
    let m = markers_at(p, Path::new(dir));
    assert_eq!(p.inner.read_dir_total(), 0, "read_dir called");
    assert!(p.inner.violations().is_empty());
    m
}

fn mi(fs_type: &str, local: bool, dev: u64) -> MountInfo {
    MountInfo {
        fs_type: fs_type.into(),
        from: fs_type.into(),
        local,
        dev,
    }
}

fn marker(dir: &str, name: &str, class: MarkerClass) -> Option<FoundMarker> {
    Some(FoundMarker {
        dir: dir.into(),
        name: name.into(),
        class,
    })
}

fn home() -> FakeProbe {
    FakeProbe::new().home("/home/alice")
}

#[test]
fn zk_inside_git_repo_wins() {
    let p = probed(
        home()
            .dir("/home/alice/repo/.git")
            .dir("/home/alice/repo/docs/nb/.zk")
            .dir("/home/alice/repo/docs/nb/daily"),
    );
    let c = run(&p, "/home/alice/repo/docs/nb/daily", &[]);
    assert_eq!(c.root, Some("/home/alice/repo/docs/nb".into()));
    assert_eq!(
        c.marker,
        marker("/home/alice/repo/docs/nb", ".zk", MarkerClass::NotesTool)
    );
    assert_eq!(c.stop, StopReason::Marker);
    assert!(!c.huge && !c.ignore_here && !c.inside_virtual_mount);
    // Never stats past the nearest root's directory, except one parent
    // stat per level for the st_dev check.
    let nb = Path::new("/home/alice/repo/docs/nb");
    for s in p.stats() {
        assert!(
            s.starts_with(nb) || nb.starts_with(&s),
            "unexpected stat {}",
            s.display()
        );
    }
}

#[test]
fn git_file_worktree_is_a_root() {
    let p = probed(
        home()
            .dir("/home/alice/repo/.git")
            .file("/home/alice/repo/wt/.git", "gitdir: ../.git/worktrees/wt\n")
            .dir("/home/alice/repo/wt/docs"),
    );
    let c = run(&p, "/home/alice/repo/wt/docs", &[]);
    assert_eq!(c.root, Some("/home/alice/repo/wt".into()));
    assert_eq!(
        c.marker,
        marker("/home/alice/repo/wt", ".git", MarkerClass::Vcs)
    );
}

#[test]
fn explicit_mdroots() {
    let p = probed(
        home()
            .file("/home/alice/notes/.mdroots", "")
            .dir("/home/alice/notes/a"),
    );
    let c = run(&p, "/home/alice/notes/a", &[]);
    assert_eq!(c.root, Some("/home/alice/notes".into()));
    assert_eq!(
        c.marker,
        marker("/home/alice/notes", ".mdroots", MarkerClass::Explicit)
    );
}

#[test]
fn conf_py_needs_index_md() {
    let p = probed(home().file("/home/alice/proj/conf.py", ""));
    assert!(at(&p, "/home/alice/proj").is_empty());
    let c = run(&p, "/home/alice/proj", &[]);
    assert_eq!(c.root, None);
    assert_eq!(c.stop, StopReason::Home);

    let p = probed(
        home()
            .file("/home/alice/proj/conf.py", "")
            .file("/home/alice/proj/index.md", "# hi\n"),
    );
    assert_eq!(
        at(&p, "/home/alice/proj"),
        vec![marker("/home/alice/proj", "conf.py", MarkerClass::DocsTool).unwrap()]
    );
}

#[test]
fn index_md_is_not_statted_without_conf_py() {
    let p = probed(home().dir("/home/alice/proj"));
    at(&p, "/home/alice/proj");
    assert!(!p.stats().iter().any(|s| s.ends_with("index.md")));
    assert!(p.stats().len() <= 26);
}

#[test]
fn monorepo_marker_sets_huge() {
    let p = probed(
        home()
            .dir("/home/alice/mono/.git")
            .file("/home/alice/mono/.buckconfig", "")
            .dir("/home/alice/mono/docs"),
    );
    let c = run(&p, "/home/alice/mono/docs", &[]);
    assert_eq!(c.root, Some("/home/alice/mono".into()));
    assert_eq!(
        c.marker,
        marker("/home/alice/mono", ".git", MarkerClass::Vcs)
    );
    assert!(c.huge);
}

#[test]
fn monorepo_only_dir_is_a_root() {
    let p = probed(
        home()
            .file("/home/alice/mono/MODULE.bazel", "")
            .dir("/home/alice/mono/docs"),
    );
    let c = run(&p, "/home/alice/mono/docs", &[]);
    assert_eq!(c.root, Some("/home/alice/mono".into()));
    assert_eq!(
        c.marker,
        marker("/home/alice/mono", "MODULE.bazel", MarkerClass::Monorepo)
    );
    assert!(c.huge);
    assert_eq!(c.stop, StopReason::Marker);
}

#[test]
fn workspace_dir_is_not_a_monorepo_marker() {
    let p = probed(
        home()
            .dir("/home/alice/proj/workspace")
            .dir("/home/alice/proj/WORKSPACE"),
    );
    assert!(at(&p, "/home/alice/proj").is_empty());
}

#[test]
fn climb_stops_at_home_and_never_stats_above() {
    let p = probed(
        home()
            .dir("/.git")
            .dir("/home/.git")
            .dir("/home/alice/a/b/c"),
    );
    let c = run(&p, "/home/alice/a/b/c", &[]);
    assert_eq!(c.root, None);
    assert_eq!(c.marker, None);
    assert_eq!(c.stop, StopReason::Home);
    for s in p.stats() {
        assert!(
            s.starts_with("/home/alice"),
            "stat above home: {}",
            s.display()
        );
    }
}

#[test]
fn climb_without_home_reaches_fs_root() {
    let p = probed(FakeProbe::new().dir("/srv/a"));
    let c = run(&p, "/srv/a", &[]);
    assert_eq!(c.stop, StopReason::FsRoot);
    assert_eq!(c.root, None);
}

#[test]
fn per_level_stat_budget() {
    let p = probed(FakeProbe::new().dir("/srv/a/b"));
    run(&p, "/srv/a/b", &[]);
    // Three levels below and including /: /srv/a/b, /srv/a, /srv, /.
    let per_level = p.stats().len() as f64 / 4.0;
    assert!(per_level <= 27.0, "{per_level} stats per level");
}

#[test]
fn local_mount_inside_virtual_parent_stops_at_boundary() {
    let p = probed(
        home()
            .mount("/home/alice/repo", mi("edenfs:", false, 3))
            .mount("/home/alice/repo/out", mi("apfs", true, 2))
            .dir("/home/alice/repo/.eden")
            .dir("/home/alice/repo/out/gen"),
    );
    let c = run(&p, "/home/alice/repo/out/gen", &[]);
    assert_eq!(c.stop, StopReason::MountBoundary);
    assert!(c.inside_virtual_mount);
    assert_eq!(c.root, None);
    // No marker stats in the virtual parent beyond the .eden check.
    for s in p.stats() {
        assert!(
            s.starts_with("/home/alice/repo/out")
                || s == Path::new("/home/alice/repo")
                || s == Path::new("/home/alice/repo/.eden"),
            "unexpected stat {}",
            s.display()
        );
    }
}

#[test]
fn local_mount_inside_local_parent_is_not_virtual() {
    let p = probed(
        home()
            .mount("/home/alice/disk", mi("apfs", true, 2))
            .dir("/home/alice/disk/a"),
    );
    let c = run(&p, "/home/alice/disk/a", &[]);
    assert_eq!(c.stop, StopReason::MountBoundary);
    assert!(!c.inside_virtual_mount);
}

#[test]
fn marker_below_mount_inside_virtual_repo() {
    let p = probed(
        home()
            .mount("/home/alice/repo", mi("edenfs:", false, 3))
            .mount("/home/alice/repo/out", mi("apfs", true, 2))
            .dir("/home/alice/repo/.eden")
            .dir("/home/alice/repo/out/sub/.git")
            .dir("/home/alice/repo/out/sub/docs"),
    );
    let c = run(&p, "/home/alice/repo/out/sub/docs", &[]);
    assert_eq!(c.stop, StopReason::Marker);
    assert_eq!(c.root, Some("/home/alice/repo/out/sub".into()));
    assert!(c.inside_virtual_mount);
    // Above the marker only parent stats: no marker names under /out.
    assert!(!p.stats().iter().any(|s| {
        s.parent() == Some(Path::new("/home/alice/repo/out"))
            && s.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with('.'))
    }));
}

#[test]
fn virtual_start_with_eden_root() {
    let p = probed(
        home()
            .mount("/home/alice/mono", mi("edenfs:", false, 3))
            .symlink("/home/alice/mono/a/b/.eden/root", "/home/alice/mono")
            .dir("/home/alice/mono/.git"),
    );
    let c = run(&p, "/home/alice/mono/a/b", &[]);
    assert_eq!(c.stop, StopReason::Virtual);
    assert_eq!(c.root, Some("/home/alice/mono".into()));
    assert_eq!(c.virtual_root, Some("/home/alice/mono".into()));
    assert_eq!(
        c.marker,
        marker("/home/alice/mono", ".eden", MarkerClass::Monorepo)
    );
    assert!(c.huge);
    assert!(p.stats().is_empty(), "stats: {:?}", p.stats());
}

#[test]
fn virtual_start_without_eden_root() {
    let p = probed(
        home()
            .mount("/home/alice/nfs", mi("nfs", false, 4))
            .dir("/home/alice/nfs/.git")
            .dir("/home/alice/nfs/a"),
    );
    let c = run(&p, "/home/alice/nfs/a", &[]);
    assert_eq!(c.stop, StopReason::Virtual);
    assert_eq!((c.root, c.marker, c.huge), (None, None, false));
    assert!(p.stats().is_empty());
}

#[test]
fn workspace_folder_is_a_strong_marker() {
    let p = probed(
        home()
            .dir("/home/alice/repo/.git")
            .dir("/home/alice/repo/nb/day"),
    );
    let c = run(&p, "/home/alice/repo/nb/day", &["/home/alice/repo/nb"]);
    assert_eq!(c.root, Some("/home/alice/repo/nb".into()));
    assert_eq!(c.marker.unwrap().class, MarkerClass::Editor);
}

#[test]
fn class_priority_within_one_dir() {
    let base = || {
        home()
            .dir("/home/alice/p/.git")
            .file("/home/alice/p/WORKSPACE", "")
            .file("/home/alice/p/mkdocs.yml", "")
            .dir("/home/alice/p/.obsidian")
            .file("/home/alice/p/.mdroots", "")
    };
    let classes: Vec<MarkerClass> = at(&probed(base()), "/home/alice/p")
        .into_iter()
        .map(|m| m.class)
        .collect();
    use MarkerClass::*;
    assert_eq!(classes, vec![Explicit, NotesTool, DocsTool, Vcs, Monorepo]);

    // Editor ranks above Vcs but below DocsTool.
    let folders = ["/home/alice/p"];
    let p = probed(home().dir("/home/alice/p/.git"));
    assert_eq!(
        run(&p, "/home/alice/p", &folders).marker.unwrap().class,
        Editor
    );
    let p = probed(
        home()
            .dir("/home/alice/p/.git")
            .file("/home/alice/p/book.toml", ""),
    );
    let c = run(&p, "/home/alice/p", &folders);
    assert_eq!(c.marker.unwrap().class, DocsTool);
    let c = run(&probed(base()), "/home/alice/p", &folders);
    assert_eq!(c.marker.unwrap().class, Explicit);
    assert!(c.huge);
}

#[test]
fn mdrootsignore_patterns() {
    let fake = || {
        home()
            .dir("/home/alice/notes/.git")
            .file("/home/alice/notes/.mdrootsignore", "drafts/\n")
            .dir("/home/alice/notes/notes")
            .dir("/home/alice/notes/drafts/old")
    };
    let p = probed(fake());
    let c = run(&p, "/home/alice/notes/notes", &[]);
    assert_eq!(c.root, Some("/home/alice/notes".into()));
    assert!(!c.ignore_here);
    let c = run(&probed(fake()), "/home/alice/notes/drafts", &[]);
    assert!(c.ignore_here);
    let c = run(&probed(fake()), "/home/alice/notes/drafts/old", &[]);
    assert!(c.ignore_here);
    // Not a marker.
    assert!(
        !at(&probed(fake()), "/home/alice/notes")
            .iter()
            .any(|m| m.name == ".mdrootsignore")
    );
}

#[test]
fn empty_mdrootsignore_ignores_and_stops_climb() {
    let p = probed(
        home()
            .dir("/home/alice/repo/.git")
            .file("/home/alice/repo/tmp/.mdrootsignore", "")
            .dir("/home/alice/repo/tmp/x"),
    );
    let c = run(&p, "/home/alice/repo/tmp/x", &[]);
    assert!(c.ignore_here);
    assert_eq!(c.root, None);
    assert_eq!(c.stop, StopReason::Marker);
    assert!(
        !p.stats()
            .iter()
            .any(|s| s.starts_with("/home/alice/repo/.git"))
    );
}
