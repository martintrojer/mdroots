//! Tests for the loose-root denylist and climb (docs/specs/roots.md §2).
//! Trees are in-memory (FakeProbe) with a fake home `/h`. Repos are marked
//! with a `.git` dir ([git](https://git-scm.com)); virtual mounts use the
//! `edenfs` type name of [EdenFS](https://github.com/facebook/sapling) (the
//! virtual filesystem from the [Sapling](https://sapling-scm.com/) project).

use std::path::{Path, PathBuf};
use std::time::Duration;

use mdroots_core::Cancel;
use mdroots_roots::loose::{LooseOutcome, find_loose_root, is_denied};
use mdroots_roots::probe::{Counting, FakeProbe, MountInfo};
use mdroots_roots::walk::Abort;

fn p(s: &str) -> PathBuf {
    PathBuf::from(s)
}

/// `n` files under `dir`, the first `md` of them notes.
fn files(mut f: FakeProbe, dir: &str, n: usize, md: usize) -> FakeProbe {
    for i in 0..n {
        let ext = if i < md { "md" } else { "txt" };
        f = f.file(format!("{dir}/f{i:04}.{ext}"), "");
    }
    f
}

/// Run with `read_dir` allowed only under `allowed`; asserts no violation.
fn run(
    fake: FakeProbe,
    file: &str,
    links: &[&str],
    allowed: &[&str],
) -> (LooseOutcome, Counting<FakeProbe>) {
    let probe = Counting::new(fake);
    let allowed: Vec<PathBuf> = allowed.iter().map(|s| p(s)).collect();
    probe.forbid_read_dir_outside(&allowed);
    let links: Vec<PathBuf> = links.iter().map(|s| p(s)).collect();
    let out = find_loose_root(&probe, Path::new(file), &links, &Cancel::new());
    assert!(probe.violations().is_empty(), "{:?}", probe.violations());
    (out, probe)
}

fn local(dev: u64) -> MountInfo {
    MountInfo {
        fs_type: "apfs".into(),
        from: "/dev/disk".into(),
        local: true,
        dev,
    }
}

fn eden(dev: u64) -> MountInfo {
    MountInfo {
        fs_type: "edenfs".into(),
        from: "edenfs:".into(),
        local: true,
        dev,
    }
}

/// The spec's projects folder: ~40 repos, an optional tarball-like tree,
/// 6 loose files (1 md), and `scratch/` (10 files, 2 md, 6 nested repos).
fn projects(tarball: bool) -> FakeProbe {
    let mut f = FakeProbe::new().home("/h");
    for i in 0..40 {
        f = files(
            f.dir(format!("/h/projects/repo{i:02}/.git")),
            &format!("/h/projects/repo{i:02}"),
            30,
            10,
        );
    }
    for i in 0..6 {
        f = f.dir(format!("/h/projects/scratch/r{i}/.git"));
        f = files(f, &format!("/h/projects/scratch/r{i}"), 5, 5);
    }
    f = files(f, "/h/projects/scratch", 10, 2);
    f = files(f, "/h/projects", 6, 1);
    if tarball {
        f = files(f, "/h/projects/pkg-1.0", 490, 1);
    }
    f
}

#[test]
fn spec_example_scratch_not_grown() {
    let (out, probe) = run(
        projects(true),
        "/h/projects/scratch/f0000.md",
        &[],
        &["/h/projects"],
    );
    assert_eq!(out.root, Some(p("/h/projects/scratch")));
    assert_eq!(
        out.reason,
        "loose root rejected at ~/projects: 4 md (< 20), adds 2 md (<= 2 in child)"
    );
    assert_eq!((out.stats.md, out.stats.files), (2, 10));
    assert_eq!(out.abort, None);
    // Repos are pruned, never listed; scratch is listed once (not re-walked).
    assert_eq!(probe.read_dir_count(Path::new("/h/projects/repo00")), 0);
    assert_eq!(probe.read_dir_count(Path::new("/h/projects/scratch")), 1);
    assert_eq!(probe.read_dir_count(Path::new("/h")), 0);
}

#[test]
fn spec_example_without_tarball_still_rejected() {
    let (out, _) = run(
        projects(false),
        "/h/projects/scratch/f0000.md",
        &[],
        &["/h/projects"],
    );
    assert_eq!(out.root, Some(p("/h/projects/scratch")));
    assert_eq!(
        out.reason,
        "loose root rejected at ~/projects: 3 md (< 20), adds 1 md (<= 2 in child)"
    );
}

#[test]
fn notes_folder_grows_by_density() {
    let f = files(FakeProbe::new().home("/h"), "/h/notes/sub", 10, 5);
    let f = files(f, "/h/notes", 30, 25);
    let f = files(f, "/h/notes/more", 20, 15);
    let (out, probe) = run(f, "/h/notes/sub/f0000.md", &[], &["/h/notes"]);
    assert_eq!(out.root, Some(p("/h/notes")));
    assert_eq!((out.stats.md, out.stats.files), (45, 60));
    assert_eq!(
        out.reason,
        "loose root accepted at ~/notes: 45 md / 60 files"
    );
    assert_eq!(probe.read_dir_count(Path::new("/h/notes/sub")), 1);
    assert_eq!(probe.read_dir_count(Path::new("/h")), 0);
}

#[test]
fn rule_b_parent_adds_more_than_child() {
    // Child: 3 md / 30 files. Parent adds 5 md / 100 files: (a) fails, (b) holds.
    let f = files(FakeProbe::new().home("/h"), "/h/x/a/b", 30, 3);
    let f = files(f, "/h/x/a/c", 100, 5);
    let f = files(f, "/h/x", 4, 0);
    let (out, _) = run(f, "/h/x/a/b/f0000.md", &[], &["/h/x"]);
    assert_eq!(out.root, Some(p("/h/x/a")));
    assert_eq!((out.stats.md, out.stats.files), (8, 130));
    assert_eq!(
        out.reason,
        "loose root rejected at ~/x: 8 md (< 20), adds 0 md (<= 8 in child)"
    );
}

#[test]
fn rule_a_density_rejection_names_density() {
    let f = files(FakeProbe::new().home("/h"), "/h/x/a", 30, 25);
    let f = files(f, "/h/x/big", 400, 10);
    let (out, _) = run(f, "/h/x/a/f0000.md", &[], &["/h/x"]);
    assert_eq!(out.root, Some(p("/h/x/a")));
    assert_eq!(
        out.reason,
        "loose root rejected at ~/x: 35 md / 430 files (< 30%), adds 10 md (<= 25 in child)"
    );
}

#[test]
fn denied_starts_are_single_file_with_no_read_dir() {
    let f = || {
        FakeProbe::new()
            .home("/h")
            .file("/h/Downloads/a.md", "")
            .file("/h/Desktop/a.md", "")
            .file("/h/a.md", "")
            .file("/h/Library/foo/a.md", "")
            .file("/tmp/a.md", "")
    };
    for (file, why) in [
        ("/h/Downloads/a.md", "single-file: Downloads"),
        ("/h/Desktop/a.md", "single-file: Desktop"),
        ("/h/a.md", "single-file: home directory"),
        ("/h/Library/foo/a.md", "single-file: under ~/Library"),
        ("/tmp/a.md", "single-file: /tmp"),
    ] {
        let (out, probe) = run(f(), file, &[], &[]);
        assert_eq!(out.root, None, "{file}");
        assert_eq!(out.reason, why, "{file}");
        assert_eq!(probe.read_dir_total(), 0, "{file}");
    }
}

#[test]
fn denylist_exact_paths() {
    let f = [
        "/Volumes/Disk/sub",
        "/var/folders/x/T",
        "/private/var/folders/x",
        "/tmp/x",
        "/h/Downloads/x",
        "/h/Desktop",
        "/h/notes",
        "/h/Library/Mobile Documents/iCloud~md~obsidian/Documents/v/sub",
    ]
    .iter()
    .fold(FakeProbe::new().home("/h"), |f, d| f.dir(d));
    for d in [
        "/",
        "/h",
        "/tmp",
        "/private",
        "/private/tmp",
        "/var",
        "/private/var",
        "/Volumes/Disk",
        "/h/Downloads",
        "/h/Desktop",
        "/h/Library",
        "/h/Library/Mobile Documents/iCloud~md~obsidian/Documents",
    ] {
        assert!(is_denied(&f, Path::new(d)).is_some(), "{d}");
    }
    for d in [
        "/var/folders/x/T",
        "/private/var/folders/x",
        "/tmp/x",
        "/Volumes/Disk/sub",
        "/h/Downloads/x",
        "/h/notes",
        "/h/Library/Mobile Documents/iCloud~md~obsidian/Documents/v",
        "/h/Library/Mobile Documents/iCloud~md~obsidian/Documents/v/sub",
    ] {
        assert_eq!(is_denied(&f, Path::new(d)), None, "{d}");
    }
}

#[test]
fn obsidian_vault_in_library_is_allowed() {
    let vault = "/h/Library/Mobile Documents/iCloud~md~obsidian/Documents/v";
    let f = files(FakeProbe::new().home("/h"), vault, 4, 3);
    let (out, _) = run(f, &format!("{vault}/f0000.md"), &[], &[vault]);
    assert_eq!(out.root, Some(p(vault)));
    assert_eq!(
        out.reason,
        "stopped at ~/Library/Mobile Documents/iCloud~md~obsidian/Documents: denied (under ~/Library)"
    );
}

#[test]
fn virtual_mount_is_single_file_with_zero_read_dir() {
    let f = files(
        FakeProbe::new().home("/h").mount("/h/repo", eden(2)),
        "/h/repo/docs",
        5,
        5,
    );
    let (out, probe) = run(f, "/h/repo/docs/f0000.md", &[], &[]);
    assert_eq!(out.root, None);
    assert_eq!(out.reason, "single-file: virtual filesystem");
    assert_eq!(probe.read_dir_total(), 0);
}

#[test]
fn local_mount_inside_virtual_repo_is_single_file() {
    let f = FakeProbe::new()
        .home("/h")
        .mount("/h/repo", eden(2))
        .mount("/h/repo/out", local(3))
        .file("/h/repo/out/gen/a.md", "");
    let (out, probe) = run(f, "/h/repo/out/gen/a.md", &[], &[]);
    assert_eq!(out.root, None);
    assert_eq!(out.reason, "single-file: local mount inside a virtual repo");
    assert_eq!(probe.read_dir_total(), 0);
}

#[test]
fn link_lower_bound_raises_start() {
    let f = FakeProbe::new()
        .home("/h")
        .file("/h/w/a/x.md", "")
        .file("/h/w/b/y.md", "")
        .file("/h/w/z.txt", "");
    // A missing link dir and a relative one are ignored.
    let (out, probe) = run(
        f,
        "/h/w/a/x.md",
        &["/h/w/b", "/h/gone", "../elsewhere"],
        &["/h/w"],
    );
    assert_eq!(out.root, Some(p("/h/w")));
    assert_eq!((out.stats.md, out.stats.files), (2, 3));
    assert_eq!(probe.read_dir_count(Path::new("/h/w")), 1);
}

#[test]
fn link_lower_bound_into_denied_dir_is_single_file() {
    let f = FakeProbe::new()
        .home("/h")
        .file("/h/w/a/x.md", "")
        .file("/h/v/y.md", "");
    let (out, probe) = run(f, "/h/w/a/x.md", &["/h/v"], &[]);
    assert_eq!(out.root, None);
    assert_eq!(out.reason, "single-file: home directory");
    assert_eq!(probe.read_dir_total(), 0);
}

#[test]
fn never_walks_home_or_above() {
    // Every level up to /h/a is accepted; /h is never listed.
    let f = files(FakeProbe::new().home("/h"), "/h/a/b/c", 30, 30);
    let f = files(f, "/h/a/b", 10, 10);
    let f = files(f, "/h/a", 10, 10);
    let f = files(f, "/h", 100, 0);
    let (out, probe) = run(f, "/h/a/b/c/f0000.md", &[], &["/h/a"]);
    assert_eq!(out.root, Some(p("/h/a")));
    assert_eq!(out.reason, "loose root accepted at ~/a: 50 md / 50 files");
    assert_eq!(probe.read_dir_count(Path::new("/h")), 0);
    assert_eq!(probe.read_dir_count(Path::new("/")), 0);
}

#[test]
fn temp_dir_outside_home_never_grows() {
    let f = files(FakeProbe::new().home("/h"), "/var/folders/ab/T/n", 3, 2);
    let f = files(f, "/var/folders/ab/T/big", 2000, 1000);
    let (out, probe) = run(
        f,
        "/var/folders/ab/T/n/f0000.md",
        &[],
        &["/var/folders/ab/T/n"],
    );
    assert_eq!(out.root, Some(p("/var/folders/ab/T/n")));
    assert_eq!(
        out.reason,
        "loose root at /var/folders/ab/T/n: outside home, no growth"
    );
    assert_eq!(probe.read_dir_count(Path::new("/var/folders/ab/T")), 0);
}

#[test]
fn var_folders_not_denied() {
    let f = FakeProbe::new()
        .home("/h")
        .file("/var/folders/x/T/n/a.md", "");
    let (out, _) = run(f, "/var/folders/x/T/n/a.md", &[], &["/var/folders/x/T/n"]);
    assert_eq!(out.root, Some(p("/var/folders/x/T/n")));
    assert_eq!(out.abort, None);
}

#[test]
fn no_home_never_grows() {
    let f = files(FakeProbe::new(), "/w/n", 2, 2);
    let f = files(f, "/w", 100, 100);
    let (out, _) = run(f, "/w/n/f0000.md", &[], &["/w/n"]);
    assert_eq!(out.root, Some(p("/w/n")));
    assert_eq!(out.reason, "loose root at /w/n: outside home, no growth");
}

#[test]
fn start_walk_md_abort_keeps_start_and_stops() {
    let f = files(FakeProbe::new().home("/h"), "/h/x/big", 5001, 5001);
    let f = files(f, "/h/x", 100, 100);
    let (out, probe) = run(f, "/h/x/big/f0000.md", &[], &["/h/x/big"]);
    assert_eq!(out.root, Some(p("/h/x/big")));
    assert_eq!(out.abort, Some(Abort::Md));
    assert_eq!(
        out.reason,
        "loose root at ~/x/big: walk aborted (md), no growth"
    );
    assert!(out.stats.md > 0);
    assert_eq!(probe.read_dir_count(Path::new("/h/x")), 0);
}

#[test]
fn start_walk_rate_abort() {
    let mut f = FakeProbe::new()
        .home("/h")
        .read_dir_cost("/h/slow", Duration::from_millis(10));
    // The start /h/slow/d0 has 60 subdirs at 10 ms each: big enough to be
    // rate-checked; the walk passes the 100 ms window and the check aborts it.
    for i in 0..60 {
        f = f.file(format!("/h/slow/d0/s{i}/a.md"), "");
    }
    let (out, _) = run(f, "/h/slow/d0/a.md", &[], &["/h/slow/d0", "/h/slow"]);
    assert_eq!(out.root, Some(p("/h/slow/d0")));
    assert_eq!(out.abort, Some(Abort::Rate));
}

#[test]
fn short_slow_start_walk_is_not_rate_aborted() {
    let f = FakeProbe::new()
        .home("/h")
        .read_dir_cost("/h/slow", Duration::from_millis(10))
        .file("/h/slow/d0/a.md", "");
    let (out, _) = run(f, "/h/slow/d0/a.md", &[], &["/h/slow/d0", "/h/slow"]);
    assert_eq!(out.abort, None);
}

#[test]
fn parent_walk_abort_rejects_parent() {
    let f = files(FakeProbe::new().home("/h"), "/h/x/a", 5, 5);
    let f = files(f, "/h/x/big", 10_001, 0);
    let (out, _) = run(f, "/h/x/a/f0000.md", &[], &["/h/x"]);
    assert_eq!(out.root, Some(p("/h/x/a")));
    assert_eq!(out.abort, None);
    assert_eq!(
        out.reason,
        "loose root rejected at ~/x: walk aborted (entries)"
    );
}

#[test]
fn parent_components_are_cleaned_before_the_denylist() {
    let f = FakeProbe::new()
        .home("/h")
        .file("/h/notes/a.md", "")
        .file("/h/Downloads/x.md", "");
    let (out, probe) = run(f, "/h/notes/../Downloads/./x.md", &[], &[]);
    assert_eq!(out.root, None);
    assert_eq!(out.reason, "single-file: Downloads");
    assert_eq!(probe.read_dir_total(), 0);
}

#[test]
fn dotdot_above_root_stays_at_root() {
    let f = FakeProbe::new().home("/h").file("/x.md", "");
    let (out, probe) = run(f, "/../../x.md", &[], &[]);
    assert_eq!(out.root, None);
    assert_eq!(out.reason, "single-file: filesystem root");
    assert_eq!(probe.read_dir_total(), 0);
}

#[test]
fn link_dir_with_dotdot_contributes_its_cleaned_dir() {
    let f = FakeProbe::new()
        .home("/h")
        .file("/h/w/a/x.md", "")
        .file("/h/w/b/y.md", "")
        .file("/h/w/z.txt", "");
    let (out, _) = run(f, "/h/w/a/x.md", &["/h/w/a/../b"], &["/h/w"]);
    assert_eq!(out.root, Some(p("/h/w")));
    assert_eq!((out.stats.md, out.stats.files), (2, 3));
}

#[test]
fn climb_measures_depth_from_the_candidate_root() {
    // The start /h/x/s holds notes 8 levels down (its own walk fits the
    // loose depth budget); /h/x would put them at depth 9.
    let deep = "/h/x/s/1/2/3/4/5/6/7/8";
    let f = files(FakeProbe::new().home("/h"), deep, 30, 30);
    let f = files(f, "/h/x", 30, 30);
    let (out, probe) = run(f, "/h/x/s/f.md", &[], &["/h/x/s"]);
    assert_eq!(out.root, Some(p("/h/x/s")));
    assert_eq!(out.abort, None);
    assert_eq!(out.stats.max_depth, 8);
    assert_eq!(out.reason, "loose root rejected at ~/x: depth budget");
    assert_eq!(probe.read_dir_count(Path::new("/h/x")), 0);
}

#[test]
fn climb_accepts_parent_when_depth_fits() {
    let deep = "/h/x/s/1/2/3/4/5/6/7";
    let f = files(FakeProbe::new().home("/h"), deep, 30, 30);
    let f = files(f, "/h/x", 30, 30);
    let (out, _) = run(f, "/h/x/s/f.md", &[], &["/h/x"]);
    assert_eq!(out.root, Some(p("/h/x")));
    assert_eq!(out.stats.max_depth, 8);
}
