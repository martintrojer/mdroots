//! Tests for the discovery orchestrator (docs/specs/roots.md §1–§4): one
//! test per stage-3 table row, the stage-1 hit path, nested roots, the rate
//! confirmation flow and overlaps. Trees are in-memory (FakeProbe) with a
//! fake home `/h`; git indexes ([git](https://git-scm.com)) are built by
//! hand; virtual mounts use the `edenfs` type name of
//! [EdenFS](https://github.com/facebook/sapling) (the virtual filesystem from
//! the [Sapling](https://sapling-scm.com/) project).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use mdroots_core::Cancel;
use mdroots_roots::discover::{
    Decision, DiscoverOptions, Enumerator, NoEnumerator, discover, explain, list_root,
};
use mdroots_roots::probe::{Counting, FakeProbe, MountInfo, StdProbe};
use mdroots_roots::registry::{MemRegistry, Registry, RootMode, RootRecord, VerdictSource};
use mdroots_roots::walk::WalkStats;

fn p(s: &str) -> PathBuf {
    PathBuf::from(s)
}

const NOW: u64 = 1_000_000_000;

fn opts() -> DiscoverOptions {
    DiscoverOptions {
        now_ms: NOW,
        ..Default::default()
    }
}

/// A git index (version 2) listing `paths`, with the given extensions.
fn index(paths: &[String], exts: &[&[u8; 4]]) -> Vec<u8> {
    let mut b = b"DIRC".to_vec();
    b.extend(2u32.to_be_bytes());
    b.extend((paths.len() as u32).to_be_bytes());
    for path in paths {
        let start = b.len();
        let mut fixed = [0u8; 62];
        fixed[24..28].copy_from_slice(&0o100644u32.to_be_bytes());
        let flags = path.len().min(0xfff) as u16;
        fixed[60..62].copy_from_slice(&flags.to_be_bytes());
        b.extend(fixed);
        b.extend(path.as_bytes());
        let len = (62 + path.len() + 8) & !7;
        b.resize(start + len, 0);
    }
    for sig in exts {
        b.extend(*sig);
        b.extend(0u32.to_be_bytes());
    }
    b.extend([0u8; 20]);
    b
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_owned()).collect()
}

/// `n` files under `dir`, the first `md` of them notes.
fn files(mut f: FakeProbe, dir: &str, n: usize, md: usize) -> FakeProbe {
    for i in 0..n {
        let ext = if i < md { "md" } else { "txt" };
        f = f.file(format!("{dir}/f{i:04}.{ext}"), "");
    }
    f
}

fn eden(dev: u64) -> MountInfo {
    MountInfo {
        fs_type: "edenfs:".into(),
        from: "edenfs".into(),
        local: false,
        dev,
    }
}

fn local(dev: u64) -> MountInfo {
    MountInfo {
        fs_type: "apfs".into(),
        from: "/dev/disk".into(),
        local: true,
        dev,
    }
}

/// An EdenFS checkout at `/eden/repo` with `.eden/root` in `dirs`.
fn eden_repo(dirs: &[&str]) -> FakeProbe {
    let mut f = FakeProbe::new().home("/h").mount("/eden", eden(5));
    for d in dirs {
        f = f.symlink(format!("{d}/.eden/root"), "/eden/repo");
    }
    f.file("/eden/repo/docs/a.md", "")
}

/// Discover `file`, asserting no `read_dir` outside `allowed`.
fn run(
    probe: &Counting<FakeProbe>,
    reg: &mut MemRegistry,
    en: &dyn Enumerator,
    file: &str,
    allowed: &[&str],
) -> Decision {
    let allowed: Vec<PathBuf> = allowed.iter().map(|s| p(s)).collect();
    probe.forbid_read_dir_outside(&allowed);
    let d = discover(probe, reg, en, Path::new(file), &opts(), &Cancel::new());
    assert!(probe.violations().is_empty(), "{:?}", probe.violations());
    d
}

fn row(reg: &MemRegistry, path: &str) -> Option<RootRecord> {
    reg.all().into_iter().find(|r| r.path == Path::new(path))
}

/// A fake enumerator returning `paths` (or `None`), counting calls.
struct FakeEnum(Option<Vec<String>>, AtomicUsize);

impl Enumerator for FakeEnum {
    fn md_paths(&self, _: &Path, _: Duration, _: usize) -> Option<Vec<String>> {
        self.1.fetch_add(1, Ordering::Relaxed);
        self.0.clone()
    }
}

// --- stage-3 table rows -------------------------------------------------

#[test]
fn virtual_fs_is_lazy_with_zero_read_dir() {
    let probe = Counting::new(eden_repo(&["/eden/repo", "/eden/repo/docs"]));
    let mut reg = MemRegistry::new();
    let d = run(&probe, &mut reg, &NoEnumerator, "/eden/repo/docs/a.md", &[]);
    assert_eq!(d.mode, RootMode::Lazy);
    assert_eq!(d.root, Some(p("/eden/repo")));
    assert!(explain(&d).starts_with("lazy: statfs edenfs:, MNT_LOCAL unset"));
    assert_eq!(probe.read_dir_total(), 0);
    let r = row(&reg, "/eden/repo").unwrap();
    assert_eq!(r.verdict_source, VerdictSource::Budget);
    assert_eq!(r.marker.as_deref(), Some(".eden"));
}

#[test]
fn remote_fs_is_lazy_and_never_enumerated() {
    let nfs = MountInfo {
        fs_type: "nfs".into(),
        from: "server:/export".into(),
        local: false,
        dev: 7,
    };
    let f = FakeProbe::new()
        .home("/h")
        .mount("/net", nfs)
        .file("/net/share/a.md", "");
    let probe = Counting::new(f);
    let en = FakeEnum(Some(vec![]), AtomicUsize::new(0));
    let d = run(&probe, &mut MemRegistry::new(), &en, "/net/share/a.md", &[]);
    assert_eq!(d.mode, RootMode::Lazy);
    assert_eq!(d.root, None);
    assert_eq!(en.1.load(Ordering::Relaxed), 0);
}

#[test]
fn eden_enumeration_in_budget_is_vcs_enumerated() {
    let probe = Counting::new(eden_repo(&["/eden/repo/docs"]));
    let en = FakeEnum(Some(strs(&["docs/a.md", "b.md"])), AtomicUsize::new(0));
    let mut reg = MemRegistry::new();
    let d = run(&probe, &mut reg, &en, "/eden/repo/docs/a.md", &[]);
    assert_eq!(d.mode, RootMode::VcsEnumerated);
    assert_eq!(d.md, strs(&["b.md", "docs/a.md"]));
    assert!(explain(&d).starts_with("vcs-enumerated: 2 md"));
    assert_eq!(
        row(&reg, "/eden/repo").unwrap().mode,
        RootMode::VcsEnumerated
    );
}

#[test]
fn eden_monorepo_is_lazy_and_never_enumerated() {
    for marker in [".buckconfig", "WORKSPACE", "MODULE.bazel"] {
        let probe = Counting::new(
            eden_repo(&["/eden/repo", "/eden/repo/docs"]).file(format!("/eden/repo/{marker}"), ""),
        );
        let en = FakeEnum(Some(strs(&["docs/a.md"])), AtomicUsize::new(0));
        let d = run(
            &probe,
            &mut MemRegistry::new(),
            &en,
            "/eden/repo/docs/a.md",
            &[],
        );
        assert_eq!(d.mode, RootMode::Lazy, "{marker}");
        assert_eq!(
            en.1.load(Ordering::Relaxed),
            0,
            "{marker}: enumerator called"
        );
        assert!(
            explain(&d).contains(&format!("monorepo marker {marker}")),
            "{}",
            explain(&d)
        );
        assert_eq!(probe.read_dir_total(), 0);
    }
}

#[test]
fn eden_enumeration_over_budget_is_lazy_and_not_retried() {
    let probe = Counting::new(eden_repo(&["/eden/repo", "/eden/repo/docs"]));
    let en = FakeEnum(None, AtomicUsize::new(0));
    let mut reg = MemRegistry::new();
    let d = run(&probe, &mut reg, &en, "/eden/repo/docs/a.md", &[]);
    assert_eq!(d.mode, RootMode::Lazy);
    assert!(explain(&d).contains("enumeration over budget"));
    // A file elsewhere in the checkout: no second enumeration.
    let d = run(&probe, &mut reg, &en, "/eden/repo/a.md", &[]);
    assert_eq!(d.mode, RootMode::Lazy);
    assert_eq!(en.1.load(Ordering::Relaxed), 1);
    assert_eq!(probe.read_dir_total(), 0);
}

#[test]
fn local_mount_inside_virtual_repo_without_marker_is_single_file() {
    let f = eden_repo(&["/eden/repo"])
        .mount("/eden/repo/buck-out", local(6))
        .file("/eden/repo/buck-out/gen/a.md", "");
    let probe = Counting::new(f);
    let mut reg = MemRegistry::new();
    let d = run(
        &probe,
        &mut reg,
        &NoEnumerator,
        "/eden/repo/buck-out/gen/a.md",
        &[],
    );
    assert_eq!(d.mode, RootMode::SingleFile);
    assert_eq!(
        explain(&d),
        "single-file: inside a local mount in a virtual repo"
    );
    assert!(reg.all().is_empty());
}

#[test]
fn local_mount_inside_virtual_repo_with_marker_is_lazy() {
    let f = eden_repo(&["/eden/repo"])
        .mount("/eden/repo/buck-out", local(6))
        .file("/eden/repo/buck-out/gen/.mdroots", "")
        .file("/eden/repo/buck-out/gen/a.md", "");
    let probe = Counting::new(f);
    let d = run(
        &probe,
        &mut MemRegistry::new(),
        &NoEnumerator,
        "/eden/repo/buck-out/gen/a.md",
        &[],
    );
    assert_eq!(d.mode, RootMode::Lazy);
    assert_eq!(d.root, Some(p("/eden/repo/buck-out/gen")));
}

#[test]
fn monorepo_marker_is_lazy() {
    let f =
        files(FakeProbe::new().home("/h"), "/h/mono/src", 10, 5).file("/h/mono/.buckconfig", "");
    let probe = Counting::new(f);
    let d = run(
        &probe,
        &mut MemRegistry::new(),
        &NoEnumerator,
        "/h/mono/src/f0000.md",
        &[],
    );
    assert_eq!(d.mode, RootMode::Lazy);
    assert_eq!(d.root, Some(p("/h/mono")));
}

#[test]
fn git_at_home_is_tracked_only_with_zero_read_dir() {
    let idx = index(&strs(&["bashrc", "notes/a.md", "notes/gone.md"]), &[]);
    let mut f = FakeProbe::new()
        .home("/h")
        .file("/h/.git/index", idx)
        .file("/h/bashrc", "")
        .file("/h/notes/a.md", "");
    for i in 0..50 {
        f = files(f, &format!("/h/big/d{i:02}"), 40, 20);
    }
    let probe = Counting::new(f);
    let mut reg = MemRegistry::new();
    let d = run(&probe, &mut reg, &NoEnumerator, "/h/notes/a.md", &[]);
    assert_eq!(d.mode, RootMode::TrackedOnly);
    assert_eq!(d.root, Some(p("/h")));
    assert_eq!(d.md, strs(&["notes/a.md"]));
    assert_eq!(probe.read_dir_total(), 0);
    assert_eq!(row(&reg, "/h").unwrap().mode, RootMode::TrackedOnly);
}

#[test]
fn non_git_vcs_at_home_is_single_file() {
    let f = FakeProbe::new()
        .home("/h")
        .dir("/h/.hg")
        .file("/h/notes/a.md", "");
    let probe = Counting::new(f);
    let mut reg = MemRegistry::new();
    let d = run(&probe, &mut reg, &NoEnumerator, "/h/notes/a.md", &[]);
    assert_eq!(d.mode, RootMode::SingleFile);
    assert!(
        explain(&d).contains("tracked-only needs git"),
        "{}",
        explain(&d)
    );
    assert!(reg.all().is_empty());
}

#[test]
fn git_index_over_200k_entries_is_lazy() {
    let paths: Vec<String> = (0..200_001).map(|i| format!("f{i}")).collect();
    let f = FakeProbe::new()
        .home("/h")
        .file("/h/big/.git/index", index(&paths, &[]))
        .file("/h/big/a.md", "");
    let probe = Counting::new(f);
    let mut reg = MemRegistry::new();
    let d = run(&probe, &mut reg, &NoEnumerator, "/h/big/a.md", &[]);
    assert_eq!(d.mode, RootMode::Lazy);
    assert_eq!(explain(&d), "lazy: git index 200,001 entries (> 200,000)");
    assert_eq!(
        row(&reg, "/h/big").unwrap().verdict_source,
        VerdictSource::Budget
    );
    assert_eq!(probe.read_dir_total(), 0);
}

#[test]
fn git_index_over_32_mib_is_lazy() {
    let f = FakeProbe::new()
        .home("/h")
        .file("/h/big/.git/index", vec![0u8; (32 << 20) + 1])
        .file("/h/big/a.md", "");
    let probe = Counting::new(f);
    let d = run(
        &probe,
        &mut MemRegistry::new(),
        &NoEnumerator,
        "/h/big/a.md",
        &[],
    );
    assert_eq!(d.mode, RootMode::Lazy);
    assert_eq!(explain(&d), "lazy: git index over 32 MiB");
    assert_eq!(probe.read_dir_total(), 0);
}

#[test]
fn git_index_20k_to_200k_is_index_driven() {
    let mut paths = strs(&["a.md", "docs/b.md", "gone.md"]);
    paths.extend((3..48_213).map(|i| format!("src/f{i}.rs")));
    let f = FakeProbe::new()
        .home("/h")
        .file("/h/mid/.git/index", index(&paths, &[]))
        .file("/h/mid/a.md", "")
        .file("/h/mid/docs/b.md", "");
    let probe = Counting::new(f);
    let mut reg = MemRegistry::new();
    let d = run(&probe, &mut reg, &NoEnumerator, "/h/mid/a.md", &[]);
    assert_eq!(d.mode, RootMode::IndexDriven);
    assert_eq!(explain(&d), "index-driven: git index 48,213 entries");
    assert_eq!(d.md, strs(&["a.md", "docs/b.md"]));
    assert_eq!(probe.read_dir_total(), 0);
}

#[test]
fn sparse_git_index_is_index_driven() {
    let f = FakeProbe::new()
        .home("/h")
        .file(
            "/h/sp/.git/index",
            index(&strs(&["a.md", "x.rs"]), &[b"sdir"]),
        )
        .file("/h/sp/a.md", "");
    let probe = Counting::new(f);
    let d = run(
        &probe,
        &mut MemRegistry::new(),
        &NoEnumerator,
        "/h/sp/a.md",
        &[],
    );
    assert_eq!(d.mode, RootMode::IndexDriven);
    assert_eq!(d.md, strs(&["a.md"]));
    assert_eq!(probe.read_dir_total(), 0);
}

#[test]
fn split_git_index_is_lazy() {
    let f = FakeProbe::new()
        .home("/h")
        .file("/h/sp/.git/index", index(&strs(&["a.md"]), &[b"link"]))
        .file("/h/sp/a.md", "");
    let probe = Counting::new(f);
    let d = run(
        &probe,
        &mut MemRegistry::new(),
        &NoEnumerator,
        "/h/sp/a.md",
        &[],
    );
    assert_eq!(d.mode, RootMode::Lazy);
    assert_eq!(explain(&d), "lazy: git split index: entry count unknown");
}

#[test]
fn small_git_index_is_a_budgeted_walk_finding_untracked_md() {
    let f = FakeProbe::new()
        .home("/h")
        .file("/h/p/.git/index", index(&strs(&["README.md"]), &[]))
        .file("/h/p/README.md", "")
        .file("/h/p/notes/untracked.md", "");
    let probe = Counting::new(f);
    let mut reg = MemRegistry::new();
    let d = run(&probe, &mut reg, &NoEnumerator, "/h/p/README.md", &["/h/p"]);
    assert_eq!(d.mode, RootMode::Vcs);
    assert_eq!(d.md, strs(&["README.md", "notes/untracked.md"]));
    assert!(
        explain(&d).contains("git index 1 entries"),
        "{}",
        explain(&d)
    );
    let r = row(&reg, "/h/p").unwrap();
    assert_eq!(r.marker.as_deref(), Some(".git"));
    assert_eq!(r.stats.md, 2);
}

#[test]
fn git_worktree_file_points_at_its_index() {
    let f = FakeProbe::new()
        .home("/h")
        .file("/h/wt/.git", "gitdir: ../main/.git/worktrees/wt\n")
        .file(
            "/h/main/.git/worktrees/wt/index",
            index(&strs(&["a.md"]), &[b"link"]),
        )
        .file("/h/wt/a.md", "");
    let probe = Counting::new(f);
    let d = run(
        &probe,
        &mut MemRegistry::new(),
        &NoEnumerator,
        "/h/wt/a.md",
        &[],
    );
    // The split index proves the worktree's own index was read.
    assert_eq!(d.mode, RootMode::Lazy);
    assert_eq!(explain(&d), "lazy: git split index: entry count unknown");
}

#[test]
fn fresh_git_init_without_index_is_walked() {
    let f = FakeProbe::new()
        .home("/h")
        .dir("/h/q/.git")
        .file("/h/q/a.md", "");
    let probe = Counting::new(f);
    let d = run(
        &probe,
        &mut MemRegistry::new(),
        &NoEnumerator,
        "/h/q/a.md",
        &["/h/q"],
    );
    assert_eq!(d.mode, RootMode::Vcs);
    assert_eq!(d.md, strs(&["a.md"]));
}

#[test]
fn non_git_vcs_is_walked() {
    let f = FakeProbe::new()
        .home("/h")
        .dir("/h/hg/.hg")
        .file("/h/hg/a.md", "");
    let probe = Counting::new(f);
    let d = run(
        &probe,
        &mut MemRegistry::new(),
        &NoEnumerator,
        "/h/hg/a.md",
        &["/h/hg"],
    );
    assert_eq!(d.mode, RootMode::Vcs);
    assert_eq!(d.md, strs(&["a.md"]));
}

#[test]
fn notes_tool_marker_is_walked_and_reports_nested_roots() {
    let f = FakeProbe::new()
        .home("/h")
        .dir("/h/nb/.zk")
        .file("/h/nb/a.md", "")
        .dir("/h/nb/proj/.git")
        .file("/h/nb/proj/b.md", "");
    let probe = Counting::new(f);
    let d = run(
        &probe,
        &mut MemRegistry::new(),
        &NoEnumerator,
        "/h/nb/a.md",
        &["/h/nb"],
    );
    assert_eq!(d.mode, RootMode::Marker);
    assert_eq!(d.md, strs(&["a.md"]));
    assert_eq!(d.nested_roots, vec![p("/h/nb/proj")]);
    assert_eq!(probe.read_dir_count(Path::new("/h/nb/proj")), 0);
}

#[test]
fn walk_over_budget_is_lazy_budget() {
    // One directory chain 40 levels deep: over the depth budget of 32.
    let deep: String = (0..40).map(|i| format!("/{i}")).collect();
    let f = files(
        FakeProbe::new().home("/h").dir("/h/w/.zk"),
        "/h/w/d",
        10,
        10,
    )
    .dir(format!("/h/w/d{deep}"));
    let probe = Counting::new(f);
    let mut reg = MemRegistry::new();
    let d = run(
        &probe,
        &mut reg,
        &NoEnumerator,
        "/h/w/d/f0000.md",
        &["/h/w"],
    );
    assert_eq!(d.mode, RootMode::Lazy);
    assert!(
        explain(&d).starts_with("lazy: walk over budget (depth)"),
        "{}",
        explain(&d)
    );
    assert_eq!(
        row(&reg, "/h/w").unwrap().verdict_source,
        VerdictSource::Budget
    );
}

#[test]
fn no_marker_finds_a_loose_root() {
    let f = files(FakeProbe::new().home("/h"), "/h/notes", 25, 25);
    let probe = Counting::new(f);
    let mut reg = MemRegistry::new();
    let d = run(
        &probe,
        &mut reg,
        &NoEnumerator,
        "/h/notes/f0000.md",
        &["/h"],
    );
    assert_eq!(d.mode, RootMode::Loose);
    assert_eq!(d.root, Some(p("/h/notes")));
    assert_eq!(d.md.len(), 25);
    assert!(
        explain(&d).starts_with("loose root accepted at ~/notes"),
        "{}",
        explain(&d)
    );
    assert_eq!(row(&reg, "/h/notes").unwrap().mode, RootMode::Loose);
}

#[test]
fn denied_loose_start_is_single_file_and_unregistered() {
    let probe = Counting::new(FakeProbe::new().home("/h").file("/h/a.md", ""));
    let mut reg = MemRegistry::new();
    let d = run(&probe, &mut reg, &NoEnumerator, "/h/a.md", &[]);
    assert_eq!(d.mode, RootMode::SingleFile);
    assert_eq!(d.root, None);
    assert_eq!(explain(&d), "single-file: home directory");
    assert!(reg.all().is_empty());
}

#[test]
fn mdrootsignore_is_single_file() {
    let f = FakeProbe::new()
        .home("/h")
        .file("/h/p/.mdrootsignore", "")
        .file("/h/p/a.md", "");
    let probe = Counting::new(f);
    let d = run(
        &probe,
        &mut MemRegistry::new(),
        &NoEnumerator,
        "/h/p/a.md",
        &[],
    );
    assert_eq!(d.mode, RootMode::SingleFile);
}

// --- stage 1 ----------------------------------------------------------------

#[test]
fn new_local_mdrootsignore_below_registered_root_is_a_miss() {
    let mut reg = MemRegistry::new();
    let before = FakeProbe::new()
        .home("/h")
        .dir("/h/nb/.zk")
        .file("/h/nb/sub/a.md", "");
    run(
        &Counting::new(before),
        &mut reg,
        &NoEnumerator,
        "/h/nb/sub/a.md",
        &["/h/nb"],
    );

    let after = FakeProbe::new()
        .home("/h")
        .dir("/h/nb/.zk")
        .file("/h/nb/sub/.mdrootsignore", "")
        .file("/h/nb/sub/a.md", "");
    let d = run(
        &Counting::new(after),
        &mut reg,
        &NoEnumerator,
        "/h/nb/sub/a.md",
        &[],
    );
    assert_eq!(d.mode, RootMode::SingleFile);
}

#[test]
fn nonmatching_mdrootsignore_on_virtual_hit_does_not_ignore() {
    let mut reg = MemRegistry::new();
    let before = Counting::new(eden_repo(&["/eden/repo", "/eden/repo/sub"]));
    let first = run(&before, &mut reg, &NoEnumerator, "/eden/repo/sub/a.md", &[]);
    assert_eq!(first.root, Some(p("/eden/repo")));

    let after = eden_repo(&["/eden/repo", "/eden/repo/sub"])
        .file("/eden/repo/sub/.mdrootsignore", "other/**")
        .file("/eden/repo/sub/a.md", "");
    let d = run(
        &Counting::new(after),
        &mut reg,
        &NoEnumerator,
        "/eden/repo/sub/a.md",
        &[],
    );
    assert_eq!(d.mode, RootMode::Lazy);
    assert_eq!(d.root, Some(p("/eden/repo")));
}

#[test]
fn registry_hit_path_does_zero_read_dir() {
    let f = FakeProbe::new()
        .home("/h")
        .dir("/h/nb/.zk")
        .file("/h/nb/sub/a.md", "");
    let probe = Counting::new(f);
    let mut reg = MemRegistry::new();
    let first = run(
        &probe,
        &mut reg,
        &NoEnumerator,
        "/h/nb/sub/a.md",
        &["/h/nb"],
    );
    assert_eq!(first.mode, RootMode::Marker);
    let before = probe.read_dir_total();
    let hit = run(&probe, &mut reg, &NoEnumerator, "/h/nb/sub/a.md", &[]);
    assert_eq!(probe.read_dir_total(), before);
    assert_eq!(hit.root, first.root);
    assert_eq!(hit.mode, RootMode::Marker);
    assert_eq!(hit.reason, first.reason);
    assert_eq!(reg.all().len(), 1);
}

#[test]
fn new_git_below_registered_lazy_root_is_a_new_nested_root() {
    let mut reg = MemRegistry::new();
    let before = Counting::new(eden_repo(&["/eden/repo", "/eden/repo/docs"]));
    let d = run(
        &before,
        &mut reg,
        &NoEnumerator,
        "/eden/repo/docs/a.md",
        &[],
    );
    assert_eq!(d.root, Some(p("/eden/repo")));

    // `git clone` into the lazy root, then open a file in it.
    let after = eden_repo(&["/eden/repo", "/eden/repo/docs", "/eden/repo/sub"])
        .dir("/eden/repo/sub/.git")
        .file("/eden/repo/sub/x.md", "");
    let probe = Counting::new(after);
    let d = run(&probe, &mut reg, &NoEnumerator, "/eden/repo/sub/x.md", &[]);
    assert_eq!(d.root, Some(p("/eden/repo/sub")));
    assert_eq!(d.mode, RootMode::Lazy);
    assert_eq!(probe.read_dir_total(), 0);
    let r = row(&reg, "/eden/repo/sub").unwrap();
    assert_eq!(r.marker.as_deref(), Some(".git"));
    assert_eq!(reg.all().len(), 2);
}

#[test]
fn new_git_below_registered_loose_root_is_a_miss() {
    let mut reg = MemRegistry::new();
    let f = files(FakeProbe::new().home("/h"), "/h/notes", 25, 25).file("/h/notes/proj/a.md", "");
    let probe = Counting::new(f);
    run(
        &probe,
        &mut reg,
        &NoEnumerator,
        "/h/notes/f0000.md",
        &["/h"],
    );
    let f = files(FakeProbe::new().home("/h"), "/h/notes", 25, 25)
        .file("/h/notes/proj/a.md", "")
        .dir("/h/notes/proj/.git");
    let probe = Counting::new(f);
    let d = run(
        &probe,
        &mut reg,
        &NoEnumerator,
        "/h/notes/proj/a.md",
        &["/h/notes/proj"],
    );
    assert_eq!(d.root, Some(p("/h/notes/proj")));
    assert_eq!(d.mode, RootMode::Vcs);
    assert_eq!(reg.all().len(), 2);
}

#[test]
fn moved_root_is_rekeyed_without_a_walk() {
    let mut reg = MemRegistry::new();
    let f = FakeProbe::new()
        .home("/h")
        .dir("/h/notes/.zk")
        .file("/h/notes/a.md", "");
    run(
        &Counting::new(f),
        &mut reg,
        &NoEnumerator,
        "/h/notes/a.md",
        &["/h/notes"],
    );
    let id = row(&reg, "/h/notes").unwrap().root_id;

    let f = FakeProbe::new()
        .home("/h")
        .dir("/h/notes/.zk")
        .file("/h/notes/a.md", "")
        .rename("/h/notes", "/h/notes2");
    let probe = Counting::new(f);
    let d = run(&probe, &mut reg, &NoEnumerator, "/h/notes2/a.md", &[]);
    assert_eq!(d.root, Some(p("/h/notes2")));
    assert_eq!(probe.read_dir_total(), 0);
    let all = reg.all();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].root_id, id);
    assert_eq!(all[0].path, p("/h/notes2"));
}

// --- stage 4 rate flow ------------------------------------------------------

fn slow_notes() -> FakeProbe {
    let mut f = FakeProbe::new()
        .home("/h")
        .dir("/h/slow/.zk")
        .read_dir_cost("/h/slow", Duration::from_millis(10));
    // 12 dirs at 10 ms: the walk passes the 100 ms window, so it is rate-checked.
    for i in 0..11 {
        f = files(f, &format!("/h/slow/d{i}"), 3, 3);
    }
    f
}

#[test]
fn rate_verdict_needs_two_measurements() {
    let probe = Counting::new(slow_notes());
    let mut reg = MemRegistry::new();
    let file = "/h/slow/d0/f0000.md";

    let d = run(&probe, &mut reg, &NoEnumerator, file, &["/h/slow"]);
    assert_eq!(d.mode, RootMode::Lazy);
    assert_eq!(d.rate_confirmations, 1);
    assert_eq!(
        explain(&d),
        "lazy pending: rate 10.0 ms/dir, 1 of 2 measurements"
    );
    let r = row(&reg, "/h/slow").unwrap();
    assert_eq!(
        (r.verdict_source, r.rate_confirmations),
        (VerdictSource::Rate, 1)
    );

    // Pending is a stage-1 miss: measured again.
    let n = probe.read_dir_total();
    let d = run(&probe, &mut reg, &NoEnumerator, file, &["/h/slow"]);
    assert!(probe.read_dir_total() > n);
    assert_eq!(d.rate_confirmations, 2);
    assert_eq!(explain(&d), "lazy: rate 10.0 ms/dir, 2 of 2 measurements");
    let r2 = row(&reg, "/h/slow").unwrap();
    assert_eq!(r2.rate_confirmations, 2);
    assert_eq!(r2.root_id, r.root_id);
    assert_eq!(reg.all().len(), 1);

    // Final: a hit, no more walks.
    let n = probe.read_dir_total();
    let d = run(&probe, &mut reg, &NoEnumerator, file, &[]);
    assert_eq!(probe.read_dir_total(), n);
    assert_eq!(d.mode, RootMode::Lazy);
}

#[test]
fn completed_walk_replaces_a_pending_rate_record() {
    let mut reg = MemRegistry::new();
    let file = "/h/slow/d0/f0000.md";
    run(
        &Counting::new(slow_notes()),
        &mut reg,
        &NoEnumerator,
        file,
        &["/h/slow"],
    );
    let fast = slow_notes().read_dir_cost("/h/slow", Duration::from_micros(200));
    let d = run(
        &Counting::new(fast),
        &mut reg,
        &NoEnumerator,
        file,
        &["/h/slow"],
    );
    assert_eq!(d.mode, RootMode::Marker);
    let r = row(&reg, "/h/slow").unwrap();
    assert_eq!(
        (r.mode, r.verdict_source, r.rate_confirmations),
        (RootMode::Marker, VerdictSource::Fs, 0)
    );
}

// --- overlaps ---------------------------------------------------------------

fn stale(path: &str, mode: RootMode, marker: Option<&str>) -> RootRecord {
    RootRecord {
        root_id: "stale".into(),
        path: p(path),
        mode,
        marker: marker.map(str::to_owned),
        marker_ino: None,
        volume_id: "fake:1".into(),
        dev: 99, // fails lookup_valid: a stage-1 miss
        fs_type: "apfs".into(),
        stats: WalkStats::default(),
        verdict_source: VerdictSource::Fs,
        rate_confirmations: 0,
        decided_at_ms: 0,
        reason: "marker .zk at ~/n: recorded".into(),
        last_seen_ms: 0,
    }
}

#[test]
fn overlap_insert_returns_the_existing_containing_root() {
    // ~/n is registered but stale; the loose search picks ~/n/sub (~/n has
    // too few notes to grow into), and a loose root may not sit inside a
    // registered marker root.
    let f = files(FakeProbe::new().home("/h"), "/h/n/sub", 25, 25);
    let f = files(f, "/h/n", 100, 0);
    let probe = Counting::new(f);
    let mut reg = MemRegistry::new();
    reg.insert(stale("/h/n", RootMode::Marker, Some(".zk")))
        .unwrap();
    let d = run(
        &probe,
        &mut reg,
        &NoEnumerator,
        "/h/n/sub/f0000.md",
        &["/h"],
    );
    assert_eq!(d.root, Some(p("/h/n")));
    assert_eq!(d.mode, RootMode::Marker);
    assert_eq!(explain(&d), "marker .zk at ~/n: recorded");
    assert_eq!(reg.all().len(), 1);
}

#[test]
fn overlap_without_containing_root_returns_the_new_decision_unregistered() {
    // ~/a/b is a registered loose root; the loose search for ~/a/x.md
    // grows to ~/a, which would contain it.
    let f = files(FakeProbe::new().home("/h"), "/h/a", 25, 25);
    let f = files(f, "/h/a/b", 3, 3);
    let probe = Counting::new(f);
    let mut reg = MemRegistry::new();
    let mut existing = stale("/h/a/b", RootMode::Loose, None);
    existing.dev = 1;
    reg.insert(existing).unwrap();
    let d = run(&probe, &mut reg, &NoEnumerator, "/h/a/f0000.md", &["/h"]);
    assert_eq!(d.root, Some(p("/h/a")));
    assert!(
        explain(&d).ends_with("(not registered: overlaps ~/a/b)"),
        "{}",
        explain(&d)
    );
    assert_eq!(reg.all().len(), 1);
}

// --- list_root ----------------------------------------------------------------

/// Discover `file`, then re-list its root with `list_root` on the same
/// probe: the decision and the listing.
fn relist(
    probe: &Counting<FakeProbe>,
    en: &dyn Enumerator,
    file: &str,
    allowed: &[&str],
) -> (Decision, Option<Vec<String>>) {
    let d = run(probe, &mut MemRegistry::new(), en, file, allowed);
    let root = d.root.clone().expect("a root");
    let listed = list_root(probe, &root, d.mode, en, &Cancel::new());
    assert!(probe.violations().is_empty(), "{:?}", probe.violations());
    (d, listed)
}

#[test]
fn list_root_matches_discover_for_each_listing_mode() {
    let mut mid = strs(&["a.md", "docs/b.md", "gone.md"]);
    mid.extend((3..20_000).map(|i| format!("src/f{i}.rs")));
    let cases: Vec<(FakeProbe, &str, &[&str], RootMode)> = vec![
        (
            FakeProbe::new()
                .home("/h")
                .dir("/h/nb/.zk")
                .file("/h/nb/a.md", "")
                .file("/h/nb/x/b.md", ""),
            "/h/nb/a.md",
            &["/h/nb"],
            RootMode::Marker,
        ),
        (
            FakeProbe::new()
                .home("/h")
                .file("/h/p/.git/index", index(&strs(&["README.md"]), &[]))
                .file("/h/p/README.md", "")
                .file("/h/p/notes/untracked.md", ""),
            "/h/p/README.md",
            &["/h/p"],
            RootMode::Vcs,
        ),
        (
            files(FakeProbe::new().home("/h"), "/h/notes", 25, 25),
            "/h/notes/f0000.md",
            &["/h"],
            RootMode::Loose,
        ),
        (
            FakeProbe::new()
                .home("/h")
                .file("/h/mid/.git/index", index(&mid, &[]))
                .file("/h/mid/a.md", "")
                .file("/h/mid/docs/b.md", ""),
            "/h/mid/a.md",
            &[],
            RootMode::IndexDriven,
        ),
        (
            FakeProbe::new()
                .home("/h")
                .file(
                    "/h/.git/index",
                    index(&strs(&["bashrc", "notes/a.md", "notes/gone.md"]), &[]),
                )
                .file("/h/notes/a.md", ""),
            "/h/notes/a.md",
            &[],
            RootMode::TrackedOnly,
        ),
    ];
    for (f, file, allowed, mode) in cases {
        let probe = Counting::new(f);
        let (d, listed) = relist(&probe, &NoEnumerator, file, allowed);
        assert_eq!(d.mode, mode, "{}", explain(&d));
        assert!(!d.md.is_empty(), "{mode:?}");
        assert_eq!(listed.as_ref(), Some(&d.md), "{mode:?}");
    }
}

#[test]
fn list_root_matches_discover_when_vcs_enumerated() {
    let probe = Counting::new(eden_repo(&["/eden/repo/docs"]));
    let en = FakeEnum(Some(strs(&["docs/a.md", "b.md"])), AtomicUsize::new(0));
    let (d, listed) = relist(&probe, &en, "/eden/repo/docs/a.md", &[]);
    assert_eq!(d.mode, RootMode::VcsEnumerated);
    assert_eq!(listed, Some(strs(&["b.md", "docs/a.md"])));
    assert_eq!(en.1.load(Ordering::Relaxed), 2);
    assert_eq!(probe.read_dir_total(), 0);
}

#[test]
fn list_root_lazy_and_single_file_list_nothing() {
    let probe = Counting::new(
        FakeProbe::new()
            .home("/h")
            .dir("/h/nb/.zk")
            .file("/h/nb/a.md", ""),
    );
    let en = FakeEnum(Some(strs(&["a.md"])), AtomicUsize::new(0));
    for mode in [RootMode::Lazy, RootMode::SingleFile] {
        assert_eq!(
            list_root(&probe, Path::new("/h/nb"), mode, &en, &Cancel::new()),
            None
        );
    }
    assert_eq!(probe.read_dir_total(), 0);
    assert_eq!(en.1.load(Ordering::Relaxed), 0);
}

#[test]
fn list_root_sees_an_added_md_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canonicalize");
    std::fs::create_dir(root.join(".git")).expect("mkdir .git");
    std::fs::write(root.join("a.md"), "a\n").expect("write");
    let cancel = Cancel::new();
    let first = list_root(&StdProbe, &root, RootMode::Vcs, &NoEnumerator, &cancel);
    assert_eq!(first, Some(strs(&["a.md"])));
    std::fs::create_dir(root.join("sub")).expect("mkdir sub");
    std::fs::write(root.join("sub/b.md"), "b\n").expect("write");
    let second = list_root(&StdProbe, &root, RootMode::Vcs, &NoEnumerator, &cancel);
    assert_eq!(second, Some(strs(&["a.md", "sub/b.md"])));
}

#[test]
fn list_root_aborts_are_none() {
    // Walk over the depth budget.
    let deep: String = (0..40).map(|i| format!("/{i}")).collect();
    let f = FakeProbe::new()
        .home("/h")
        .dir("/h/w/.zk")
        .file("/h/w/a.md", "")
        .dir(format!("/h/w/d{deep}"));
    let probe = Counting::new(f);
    let c = Cancel::new();
    let w = Path::new("/h/w");
    assert_eq!(
        list_root(&probe, w, RootMode::Marker, &NoEnumerator, &c),
        None
    );

    // Cancelled before listing: no read_dir at all.
    let probe = Counting::new(
        FakeProbe::new()
            .home("/h")
            .dir("/h/nb/.zk")
            .file("/h/nb/a.md", ""),
    );
    let cancelled = Cancel::new();
    cancelled.cancel();
    let nb = Path::new("/h/nb");
    assert_eq!(
        list_root(&probe, nb, RootMode::Marker, &NoEnumerator, &cancelled),
        None
    );
    assert_eq!(probe.read_dir_total(), 0);

    // A split or missing git index; a failed enumeration.
    let probe = Counting::new(
        FakeProbe::new()
            .home("/h")
            .file("/h/sp/.git/index", index(&strs(&["a.md"]), &[b"link"]))
            .file("/h/sp/a.md", "")
            .dir("/h/none/.git"),
    );
    let sp = Path::new("/h/sp");
    assert_eq!(
        list_root(&probe, sp, RootMode::IndexDriven, &NoEnumerator, &c),
        None
    );
    let none = Path::new("/h/none");
    assert_eq!(
        list_root(&probe, none, RootMode::TrackedOnly, &NoEnumerator, &c),
        None
    );
    let en = FakeEnum(None, AtomicUsize::new(0));
    assert_eq!(
        list_root(&probe, sp, RootMode::VcsEnumerated, &en, &c),
        None
    );
    assert_eq!(probe.read_dir_total(), 0);
}
