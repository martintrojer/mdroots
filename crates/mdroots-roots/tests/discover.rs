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
    run_with(probe, reg, en, file, allowed, &opts())
}

/// [`run`] with explicit options.
fn run_with(
    probe: &Counting<FakeProbe>,
    reg: &mut MemRegistry,
    en: &dyn Enumerator,
    file: &str,
    allowed: &[&str],
    o: &DiscoverOptions,
) -> Decision {
    let allowed: Vec<PathBuf> = allowed.iter().map(|s| p(s)).collect();
    probe.forbid_read_dir_outside(&allowed);
    let d = discover(probe, reg, en, Path::new(file), o, &Cancel::new());
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
fn marker_added_at_a_markerless_lazy_root_is_a_miss() {
    // 5001 notes: over the loose md budget, so the loose root is lazy
    // (verdict budget, no marker), normally a hit for 7 days.
    let mut reg = MemRegistry::new();
    let tree = || files(FakeProbe::new().home("/h"), "/h/big", 5001, 5001);
    let d = run(
        &Counting::new(tree()),
        &mut reg,
        &NoEnumerator,
        "/h/big/f0000.md",
        &["/h"],
    );
    assert_eq!(d.mode, RootMode::Lazy, "{}", explain(&d));
    assert_eq!(row(&reg, "/h/big").unwrap().marker, None);

    // An `.mdroots` at the root itself re-decides at once.
    let probe = Counting::new(tree().file("/h/big/.mdroots", ""));
    let d = run(
        &probe,
        &mut reg,
        &NoEnumerator,
        "/h/big/f0000.md",
        &["/h/big"],
    );
    assert_eq!(d.mode, RootMode::Marker, "{}", explain(&d));
    assert_eq!(d.root, Some(p("/h/big")));
    let r = row(&reg, "/h/big").unwrap();
    assert_eq!(r.marker.as_deref(), Some(".mdroots"));
    assert_eq!(reg.all().len(), 1);
}

#[test]
fn deleted_nested_git_goes_to_the_parent_without_rewalks() {
    let mut reg = MemRegistry::new();
    let tree = || {
        FakeProbe::new()
            .home("/h")
            .dir("/h/nb/.zk")
            .file("/h/nb/a.md", "")
            .file("/h/nb/proj/b.md", "")
    };
    let with_git = Counting::new(tree().dir("/h/nb/proj/.git"));
    run(&with_git, &mut reg, &NoEnumerator, "/h/nb/a.md", &["/h/nb"]);
    let d = run(
        &with_git,
        &mut reg,
        &NoEnumerator,
        "/h/nb/proj/b.md",
        &["/h/nb/proj"],
    );
    assert_eq!(d.root, Some(p("/h/nb/proj")));
    assert_eq!(reg.all().len(), 2);

    // `rm -rf proj/.git`: the nested row is skipped for its parent, a hit.
    let probe = Counting::new(tree());
    let mut counts = Vec::new();
    for _ in 0..3 {
        let d = run(&probe, &mut reg, &NoEnumerator, "/h/nb/proj/b.md", &[]);
        assert_eq!(d.root, Some(p("/h/nb")));
        assert_eq!(d.mode, RootMode::Marker);
        counts.push(probe.read_dir_total());
    }
    assert_eq!(counts, [0, 0, 0]);
}

#[test]
fn workspace_folder_root_is_not_reused_without_the_folder() {
    let mut reg = MemRegistry::new();
    let probe = Counting::new(
        FakeProbe::new()
            .home("/h")
            .dir("/h/repo/.git")
            .file("/h/repo/ws/a.md", ""),
    );
    let file = "/h/repo/ws/a.md";
    let folders = DiscoverOptions {
        workspace_folders: vec![p("/h/repo/ws")],
        ..opts()
    };
    let d = run_with(
        &probe,
        &mut reg,
        &NoEnumerator,
        file,
        &["/h/repo"],
        &folders,
    );
    assert_eq!(d.root, Some(p("/h/repo/ws")));
    assert!(explain(&d).starts_with("workspace folder ~/repo/ws"));

    // A later session without that workspace folder: the git root, which
    // registers despite the folder's row inside it.
    let d = run(&probe, &mut reg, &NoEnumerator, file, &["/h/repo"]);
    assert_eq!(d.root, Some(p("/h/repo")), "{}", explain(&d));
    assert_eq!(d.mode, RootMode::Vcs);
    assert_eq!(row(&reg, "/h/repo").unwrap().mode, RootMode::Vcs);

    // Both rows are hits from now on, each in its own kind of session.
    let n = probe.read_dir_total();
    let d = run_with(&probe, &mut reg, &NoEnumerator, file, &[], &folders);
    assert_eq!(d.root, Some(p("/h/repo/ws")));
    let d = run(&probe, &mut reg, &NoEnumerator, file, &[]);
    assert_eq!(d.root, Some(p("/h/repo")));
    assert_eq!(probe.read_dir_total(), n);
    assert_eq!(reg.all().len(), 2);
}

#[test]
fn new_workspace_folder_below_a_registered_root_is_a_miss() {
    // As the climb ranks a workspace folder at a level as a marker, so does
    // the stage-1 probe: the folder becomes the root whatever was
    // registered first.
    let mut reg = MemRegistry::new();
    let probe = Counting::new(
        FakeProbe::new()
            .home("/h")
            .dir("/h/repo/.git")
            .file("/h/repo/ws/a.md", ""),
    );
    let file = "/h/repo/ws/a.md";
    let d = run(&probe, &mut reg, &NoEnumerator, file, &["/h/repo"]);
    assert_eq!(d.root, Some(p("/h/repo")));
    let folders = DiscoverOptions {
        workspace_folders: vec![p("/h/repo/ws")],
        ..opts()
    };
    let d = run_with(
        &probe,
        &mut reg,
        &NoEnumerator,
        file,
        &["/h/repo"],
        &folders,
    );
    assert_eq!(d.root, Some(p("/h/repo/ws")), "{}", explain(&d));
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
    // 61 dirs at 10 ms: big enough to be rate-checked, and the walk passes
    // the 100 ms window, so it is.
    for i in 0..60 {
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
    // ~/n is a valid loose root whose budget verdict is due for a retry
    // (a stage-1 miss); the loose search picks ~/n/sub (~/n has too few
    // notes to grow into), and loose roots never nest.
    let f = files(FakeProbe::new().home("/h"), "/h/n/sub", 25, 25);
    let f = files(f, "/h/n", 100, 0);
    let probe = Counting::new(f);
    let mut reg = MemRegistry::new();
    let mut existing = stale("/h/n", RootMode::Loose, None);
    existing.dev = 1;
    existing.verdict_source = VerdictSource::Budget;
    existing.reason = "loose root accepted at ~/n: recorded".into();
    reg.insert(existing).unwrap();
    let d = run(
        &probe,
        &mut reg,
        &NoEnumerator,
        "/h/n/sub/f0000.md",
        &["/h"],
    );
    assert_eq!(d.root, Some(p("/h/n")));
    assert_eq!(d.mode, RootMode::Loose);
    assert_eq!(explain(&d), "loose root accepted at ~/n: recorded");
    assert_eq!(reg.all().len(), 1);
}

#[test]
fn overlap_with_a_row_whose_marker_is_gone_replaces_it() {
    // ~/n was a .zk root; .zk is gone. Its row must not be served, and it
    // must not keep the new loose root ~/n/sub from registering.
    let f = files(FakeProbe::new().home("/h"), "/h/n/sub", 25, 25);
    let f = files(f, "/h/n", 100, 0);
    let probe = Counting::new(f);
    let mut reg = MemRegistry::new();
    let mut gone = stale("/h/n", RootMode::Marker, Some(".zk"));
    gone.dev = 1;
    reg.insert(gone).unwrap();
    let file = "/h/n/sub/f0000.md";
    let d = run(&probe, &mut reg, &NoEnumerator, file, &["/h"]);
    assert_eq!(d.root, Some(p("/h/n/sub")), "{}", explain(&d));
    assert_eq!(d.mode, RootMode::Loose);
    let paths: Vec<_> = reg.all().into_iter().map(|r| r.path).collect();
    assert_eq!(paths, [p("/h/n/sub")]);

    let n = probe.read_dir_total();
    let d = run(&probe, &mut reg, &NoEnumerator, file, &[]);
    assert_eq!(d.root, Some(p("/h/n/sub")));
    assert_eq!(probe.read_dir_total(), n);
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

// --- registry round 2: device check, stale editor rows, legacy rows ----------

fn nfs(dev: u64) -> MountInfo {
    MountInfo {
        fs_type: "nfs".into(),
        from: "server:/export".into(),
        local: false,
        dev,
    }
}

/// The new mounts of the device-check tests: local, remote, virtual.
fn new_mounts() -> [MountInfo; 3] {
    [local(7), nfs(7), eden(7)]
}

/// `(root, mode)` of `file` with `reg` and with an empty registry.
fn cached_and_fresh(
    probe: &FakeProbe,
    reg: &mut MemRegistry,
    file: &str,
) -> ((Option<PathBuf>, RootMode), (Option<PathBuf>, RootMode)) {
    let d = discover(
        probe,
        reg,
        &NoEnumerator,
        Path::new(file),
        &opts(),
        &Cancel::new(),
    );
    let mut empty = MemRegistry::new();
    let f = discover(
        probe,
        &mut empty,
        &NoEnumerator,
        Path::new(file),
        &opts(),
        &Cancel::new(),
    );
    ((d.root, d.mode), (f.root, f.mode))
}

#[test]
fn deleted_nested_git_at_a_new_mount_is_decided_afresh() {
    let tree = || {
        FakeProbe::new()
            .home("/h")
            .dir("/h/nb/.zk")
            .file("/h/nb/a.md", "")
            .file("/h/nb/proj/b.md", "")
    };
    for m in new_mounts() {
        let mut reg = MemRegistry::new();
        let with_git = Counting::new(tree().dir("/h/nb/proj/.git"));
        run(&with_git, &mut reg, &NoEnumerator, "/h/nb/a.md", &["/h/nb"]);
        run(
            &with_git,
            &mut reg,
            &NoEnumerator,
            "/h/nb/proj/b.md",
            &["/h/nb/proj"],
        );
        assert_eq!(reg.all().len(), 2);

        // `rm -rf proj/.git` and a new filesystem mounted at proj.
        let probe = tree().mount("/h/nb/proj", m.clone());
        let (cached, fresh) = cached_and_fresh(&probe, &mut reg, "/h/nb/proj/b.md");
        assert_ne!(cached.0, Some(p("/h/nb")), "{}", m.fs_type);
        assert_eq!(cached, fresh, "{}", m.fs_type);
    }
}

#[test]
fn a_new_mount_without_a_row_under_a_registered_root_is_a_miss() {
    for m in new_mounts() {
        let tree = FakeProbe::new()
            .home("/h")
            .dir("/h/nb/.zk")
            .file("/h/nb/a.md", "");
        let mut reg = MemRegistry::new();
        run(
            &Counting::new(tree),
            &mut reg,
            &NoEnumerator,
            "/h/nb/a.md",
            &["/h/nb"],
        );

        let probe = FakeProbe::new()
            .home("/h")
            .dir("/h/nb/.zk")
            .file("/h/nb/a.md", "")
            .file("/h/nb/other/c.md", "")
            .mount("/h/nb/other", m.clone());
        let (cached, fresh) = cached_and_fresh(&probe, &mut reg, "/h/nb/other/c.md");
        assert_ne!(cached.0, Some(p("/h/nb")), "{}", m.fs_type);
        assert_eq!(cached, fresh, "{}", m.fs_type);
    }
}

#[test]
fn a_former_workspace_folder_does_not_block_an_enclosing_loose_root() {
    let mut f = FakeProbe::new().home("/h").file("/h/notes/ws/a.md", "");
    for i in 0..30 {
        f = f.file(format!("/h/notes/{i}.md"), "");
    }
    let probe = Counting::new(f);
    let mut reg = MemRegistry::new();
    let file = "/h/notes/ws/a.md";
    let folders = DiscoverOptions {
        workspace_folders: vec![p("/h/notes/ws")],
        ..opts()
    };
    let d = run_with(&probe, &mut reg, &NoEnumerator, file, &["/h"], &folders);
    assert_eq!(d.root, Some(p("/h/notes/ws")));

    // A later session without the folder: the loose root registers.
    let d = run(&probe, &mut reg, &NoEnumerator, file, &["/h"]);
    assert_eq!(d.root, Some(p("/h/notes")), "{}", explain(&d));
    assert_eq!(d.mode, RootMode::Loose);
    assert_eq!(row(&reg, "/h/notes").map(|r| r.mode), Some(RootMode::Loose));

    // And is a hit from then on.
    let n = probe.read_dir_total();
    let d = run(&probe, &mut reg, &NoEnumerator, file, &[]);
    assert_eq!(d.root, Some(p("/h/notes")));
    assert_eq!(probe.read_dir_total(), n);
}

#[test]
fn a_legacy_lazy_workspace_folder_row_gives_way_to_the_git_root() {
    let probe = Counting::new(
        FakeProbe::new()
            .home("/h")
            .dir("/h/repo/.git")
            .file("/h/repo/ws/a.md", "")
            .read_dir_cost("/h/repo/ws", Duration::from_millis(1600)),
    );
    let mut reg = MemRegistry::new();
    let file = "/h/repo/ws/a.md";
    let folders = DiscoverOptions {
        workspace_folders: vec![p("/h/repo/ws")],
        ..opts()
    };
    let d = run_with(
        &probe,
        &mut reg,
        &NoEnumerator,
        file,
        &["/h/repo"],
        &folders,
    );
    assert_eq!((d.root, d.mode), (Some(p("/h/repo/ws")), RootMode::Lazy));
    // The shape 0.2.8 wrote for an aborted workspace-folder walk.
    let mut old = row(&reg, "/h/repo/ws").unwrap();
    old.marker = None;
    old.marker_ino = None;
    reg.update(old);

    // Same time (age 0), no folders: the git root, registered.
    let d = run(&probe, &mut reg, &NoEnumerator, file, &["/h/repo"]);
    assert_eq!(d.root, Some(p("/h/repo")), "{}", explain(&d));
    let r = row(&reg, "/h/repo").expect("git root registered");
    assert_eq!(r.marker.as_deref(), Some(".git"));
    assert!(row(&reg, "/h/repo/ws").is_none());
}

/// A legacy `(Lazy, None)` workspace-folder row below an enclosing git root
/// that is already registered: the update of the git root removes it, so
/// the next open is a stage 1 hit with no walk.
#[test]
fn a_legacy_row_below_a_registered_git_root_is_removed_on_update() {
    let probe = Counting::new(
        FakeProbe::new()
            .home("/h")
            .dir("/h/repo/.git")
            .file("/h/repo/top.md", "")
            .file("/h/repo/ws/a.md", ""),
    );
    let mut reg = MemRegistry::new();
    let d = run(
        &probe,
        &mut reg,
        &NoEnumerator,
        "/h/repo/top.md",
        &["/h/repo"],
    );
    assert_eq!(d.root, Some(p("/h/repo")));
    let mut legacy = row(&reg, "/h/repo").unwrap();
    legacy.root_id = "legacy".into();
    legacy.path = p("/h/repo/ws");
    legacy.mode = RootMode::Lazy;
    legacy.marker = None;
    legacy.marker_ino = None;
    // Lazy rows do not nest, so plant the row directly (`update` with a
    // new id adds it), as an old registry would hold it.
    reg.update(legacy);

    let file = "/h/repo/ws/a.md";
    let d = run(
        &probe,
        &mut reg,
        &NoEnumerator,
        file,
        &["/h/repo", "/h/repo/ws"],
    );
    assert_eq!(d.root, Some(p("/h/repo")), "{}", explain(&d));
    assert!(row(&reg, "/h/repo/ws").is_none(), "legacy row removed");
    let before = probe.read_dir_total();
    let d = run(
        &probe,
        &mut reg,
        &NoEnumerator,
        file,
        &["/h/repo", "/h/repo/ws"],
    );
    assert_eq!(d.root, Some(p("/h/repo")));
    assert_eq!(probe.read_dir_total(), before, "reopen is a stage 1 hit");
}

/// A legacy `(Marker, None)` workspace-folder row nests, so registering the
/// enclosing git root succeeds without an overlap; the row is still removed
/// (no workspace folders now).
#[test]
fn a_new_git_root_removes_a_markerless_editor_row_inside_it() {
    let probe = Counting::new(
        FakeProbe::new()
            .home("/h")
            .dir("/h/repo/.git")
            .file("/h/repo/ws/a.md", ""),
    );
    let mut reg = MemRegistry::new();
    let file = "/h/repo/ws/a.md";
    let folders = DiscoverOptions {
        workspace_folders: vec![p("/h/repo/ws")],
        ..opts()
    };
    run_with(
        &probe,
        &mut reg,
        &NoEnumerator,
        file,
        &["/h/repo", "/h/repo/ws"],
        &folders,
    );
    let mut old = row(&reg, "/h/repo/ws").unwrap();
    old.marker = None;
    old.marker_ino = None;
    reg.update(old);

    let d = run(
        &probe,
        &mut reg,
        &NoEnumerator,
        file,
        &["/h/repo", "/h/repo/ws"],
    );
    assert_eq!(d.root, Some(p("/h/repo")), "{}", explain(&d));
    assert!(row(&reg, "/h/repo").is_some());
    assert!(
        row(&reg, "/h/repo/ws").is_none(),
        "markerless child removed"
    );
}

/// The sweep after registering a marker root keeps a markerless root
/// below a mount (the climb from it stops at the mount, so the outer
/// marker does not govern it).
#[test]
fn a_registered_marker_root_keeps_a_markerless_root_below_a_mount() {
    let mut reg = MemRegistry::new();
    let mut probe = FakeProbe::new()
        .home("/h")
        .file("/h/repo/.mdroots", "")
        .file("/h/repo/top.md", "")
        .mount(
            "/h/repo/mnt",
            MountInfo {
                fs_type: "apfs".into(),
                from: "disk7".into(),
                local: true,
                dev: 7,
            },
        );
    for i in 0..30 {
        probe = probe.file(format!("/h/repo/mnt/notes/n{i}.md"), "");
    }
    let o = opts();
    let d = discover(
        &probe,
        &mut reg,
        &NoEnumerator,
        Path::new("/h/repo/mnt/notes/n0.md"),
        &o,
        &Cancel::new(),
    );
    // Loose growth stops at the mount's root.
    assert_eq!(d.root, Some(p("/h/repo/mnt")), "{}", explain(&d));
    let child = row(&reg, "/h/repo/mnt").expect("loose row below the mount");
    assert_eq!((child.marker.clone(), child.dev), (None, 7));

    // Registering (and later updating) the outer marker root keeps it.
    for _ in 0..2 {
        let d = discover(
            &probe,
            &mut reg,
            &NoEnumerator,
            Path::new("/h/repo/top.md"),
            &o,
            &Cancel::new(),
        );
        assert_eq!(d.root, Some(p("/h/repo")), "{}", explain(&d));
        assert!(
            row(&reg, "/h/repo/mnt").is_some(),
            "root below the mount kept: {:?}",
            reg.all()
                .iter()
                .map(|r| (&r.path, r.mode, r.dev))
                .collect::<Vec<_>>()
        );
    }
}

/// A nested marker root that moved keeps its row until the destination is
/// opened, so registering the old enclosing root does not lose the move.
#[test]
fn registering_an_enclosing_root_keeps_a_moved_roots_row() {
    let mut reg = MemRegistry::new();
    let tree = || {
        FakeProbe::new()
            .home("/h")
            .file("/h/repo/.mdroots", "")
            .dir("/h/repo/child/.zk")
            .file("/h/repo/child/a.md", "")
            .file("/h/repo/top.md", "")
    };
    let o = opts();
    let probe = tree();
    discover(
        &probe,
        &mut reg,
        &NoEnumerator,
        Path::new("/h/repo/child/a.md"),
        &o,
        &Cancel::new(),
    );
    let id = row(&reg, "/h/repo/child").expect("child row").root_id;

    let probe = tree().rename("/h/repo/child", "/h/moved");
    discover(
        &probe,
        &mut reg,
        &NoEnumerator,
        Path::new("/h/repo/top.md"),
        &o,
        &Cancel::new(),
    );
    discover(
        &probe,
        &mut reg,
        &NoEnumerator,
        Path::new("/h/moved/a.md"),
        &o,
        &Cancel::new(),
    );
    let moved = row(&reg, "/h/moved").expect("moved root registered");
    assert_eq!(moved.root_id, id, "root_id kept across the move");
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

#[test]
fn list_root_drops_a_removed_md_file() {
    let cancel = Cancel::new();

    // Walk mode: the file is gone from disk.
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canonicalize");
    std::fs::create_dir(root.join(".git")).expect("mkdir .git");
    std::fs::write(root.join("a.md"), "a\n").expect("write");
    std::fs::write(root.join("b.md"), "b\n").expect("write");
    let first = list_root(&StdProbe, &root, RootMode::Vcs, &NoEnumerator, &cancel);
    assert_eq!(first, Some(strs(&["a.md", "b.md"])));
    std::fs::remove_file(root.join("b.md")).expect("rm");
    let second = list_root(&StdProbe, &root, RootMode::Vcs, &NoEnumerator, &cancel);
    assert_eq!(second, Some(strs(&["a.md"])));

    // Git index modes: deleted from disk (still in the index), then
    // dropped from the index.
    for mode in [RootMode::IndexDriven, RootMode::TrackedOnly] {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonicalize");
        std::fs::create_dir(root.join(".git")).expect("mkdir .git");
        let all = strs(&["a.md", "b.md", "c.md"]);
        std::fs::write(root.join(".git/index"), index(&all, &[])).expect("write");
        for f in &all {
            std::fs::write(root.join(f), "x\n").expect("write");
        }
        let first = list_root(&StdProbe, &root, mode, &NoEnumerator, &cancel);
        assert_eq!(first, Some(all.clone()), "{mode:?}");
        std::fs::remove_file(root.join("b.md")).expect("rm");
        let second = list_root(&StdProbe, &root, mode, &NoEnumerator, &cancel);
        assert_eq!(second, Some(strs(&["a.md", "c.md"])), "{mode:?}");
        let kept = strs(&["a.md"]);
        std::fs::write(root.join(".git/index"), index(&kept, &[])).expect("write");
        let third = list_root(&StdProbe, &root, mode, &NoEnumerator, &cancel);
        assert_eq!(third, Some(kept), "{mode:?}");
    }
}

#[test]
fn review_tracked_only_over_index_cap_returns_none() {
    // A partial listing would make the reconciler drop valid rows, so a
    // truncated or split index lists nothing in either git index mode.
    let mut paths: Vec<String> = (0..200_000).map(|i| format!("f{i:06}")).collect();
    paths.push("z.md".into());
    let probe = Counting::new(
        FakeProbe::new()
            .home("/h")
            .file("/h/.git/index", index(&paths, &[]))
            .file("/h/z.md", "")
            .file("/h/sp/.git/index", index(&strs(&["a.md"]), &[b"link"]))
            .file("/h/sp/a.md", ""),
    );
    let c = Cancel::new();
    for mode in [RootMode::TrackedOnly, RootMode::IndexDriven] {
        for root in ["/h", "/h/sp"] {
            assert_eq!(
                list_root(&probe, Path::new(root), mode, &NoEnumerator, &c),
                None,
                "{mode:?} {root}"
            );
        }
    }
    assert_eq!(probe.read_dir_total(), 0);
}
