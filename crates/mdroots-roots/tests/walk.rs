//! Tests for the stage-4 budgeted walk. Tools named here: [git](https://git-scm.com),
//! [zk](https://github.com/zk-org/zk), [Buck2](https://buck2.build),
//! [Bazel](https://bazel.build), [Mercurial](https://www.mercurial-scm.org).
//! All trees are in-memory (FakeProbe).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use mdroots_core::Cancel;
use mdroots_roots::probe::{Counting, FakeProbe, FsStat, MountInfo, Probe};
use mdroots_roots::walk::{Abort, Budget, WalkOptions, WalkOutcome, walk};

const ROOT: &str = "/n";

fn run(fake: FakeProbe, opts: &WalkOptions) -> (WalkOutcome, Counting<FakeProbe>) {
    let probe = Counting::new(fake);
    probe.forbid_read_dir_outside(&[PathBuf::from(ROOT)]);
    let out = walk(&probe, Path::new(ROOT), opts, &Cancel::new());
    assert!(probe.violations().is_empty(), "{:?}", probe.violations());
    (out, probe)
}

fn run_default(fake: FakeProbe) -> (WalkOutcome, Counting<FakeProbe>) {
    run(fake, &WalkOptions::default())
}

fn p(s: &str) -> PathBuf {
    PathBuf::from(s)
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_owned()).collect()
}

/// `n` directories `/n/d<i>`, each holding one note.
fn wide(n: usize) -> FakeProbe {
    (0..n).fold(FakeProbe::new().dir(ROOT), |f, i| {
        f.file(format!("{ROOT}/d{i:03}/a.md"), "")
    })
}

#[test]
fn prunes_dirs_temp_files_and_counts_notes() {
    let mut fake = FakeProbe::new()
        .file("/n/a.md", "")
        .file("/n/B.MARKDOWN", "")
        .file("/n/c.org", "")
        .file("/n/readme.txt", "")
        .file("/n/sub/d.md", "");
    for d in [
        "node_modules",
        "target",
        ".venv",
        "venv",
        "__pycache__",
        "dist",
        "build",
        "buck-out",
        "bazel-out",
        ".direnv",
        ".cache",
        "Pods",
        "DerivedData",
        ".next",
        "vendor",
        ".hidden",
    ] {
        fake = fake.file(format!("/n/{d}/x.md"), "");
    }
    for f in [".h.md", "a.md~", "#a.md#", "a.md.swp", "4913", ".md"] {
        fake = fake.file(format!("/n/sub/{f}"), "");
    }
    let (out, probe) = run_default(fake);
    assert_eq!(out.md, strs(&["B.MARKDOWN", "a.md", "c.org", "sub/d.md"]));
    assert_eq!(out.abort, None);
    assert_eq!(out.stats.md, 4);
    assert_eq!(out.stats.dirs, 2);
    assert_eq!(out.stats.files, 5);
    // 5 + 16 pruned dirs at the root, 7 entries in sub.
    assert_eq!(out.stats.entries, 21 + 7);
    assert_eq!(probe.read_dir_total(), 2);
    assert_eq!(probe.read_dir_count(&p("/n/buck-out")), 0);
    assert_eq!(probe.read_dir_count(&p("/n/bazel-out")), 0);
}

#[test]
fn nested_roots_are_recorded_not_descended() {
    let fake = FakeProbe::new()
        .file("/n/a.md", "")
        .file("/n/repo/.git/HEAD", "")
        .file("/n/repo/r.md", "")
        .file("/n/wt/.git", "gitdir: x")
        .file("/n/zk/.zk/config.toml", "")
        .file("/n/zk/z.md", "")
        .file("/n/plain/p.md", "");
    let (out, probe) = run_default(fake);
    assert_eq!(out.md, strs(&["a.md", "plain/p.md"]));
    assert_eq!(out.nested_roots, vec![p("/n/repo"), p("/n/wt"), p("/n/zk")]);
    for d in ["/n/repo", "/n/wt", "/n/zk"] {
        assert_eq!(probe.read_dir_count(&p(d)), 0, "{d}");
    }
}

#[test]
fn mdrootsignore_is_not_a_marker_empty_one_prunes_dir() {
    let fake = FakeProbe::new()
        .file("/n/skip/.mdrootsignore", "")
        .file("/n/skip/s.md", "")
        .file("/n/part/.mdrootsignore", "drop.md\n")
        .file("/n/part/drop.md", "")
        .file("/n/part/keep.md", "");
    let (out, probe) = run_default(fake);
    assert_eq!(out.md, strs(&["part/keep.md"]));
    assert!(out.nested_roots.is_empty());
    assert_eq!(probe.read_dir_count(&p("/n/skip")), 0);
}

#[test]
fn other_mount_not_descended() {
    let fake = FakeProbe::new()
        .file("/n/a.md", "")
        .file("/n/mnt/m.md", "")
        .mount(
            "/n/mnt",
            MountInfo {
                fs_type: "apfs".into(),
                from: "/dev/other".into(),
                local: true,
                dev: 2,
            },
        );
    let (out, probe) = run_default(fake);
    assert_eq!(out.md, strs(&["a.md"]));
    assert_eq!(probe.read_dir_count(&p("/n/mnt")), 0);
}

#[test]
fn symlinks_not_followed() {
    let fake = FakeProbe::new()
        .file("/n/a.md", "")
        .file("/elsewhere/e.md", "")
        .symlink("/n/link", "/elsewhere")
        .symlink("/n/l.md", "/elsewhere/e.md");
    let (out, probe) = run_default(fake);
    assert_eq!(out.md, strs(&["a.md"]));
    assert_eq!(probe.read_dir_count(&p("/n/link")), 0);
    assert_eq!(probe.read_dir_count(&p("/elsewhere")), 0);
}

#[test]
fn dataless_file_recorded_dataless_dir_skipped() {
    let fake = FakeProbe::new()
        .file("/n/a.md", "")
        .dataless("/n/cloud.md")
        .dataless("/n/blob.bin")
        .dir("/n/cdir")
        .file("/n/cdir/c.md", "")
        .dataless("/n/cdir");
    let (out, probe) = run_default(fake);
    assert_eq!(out.md, strs(&["a.md"]));
    assert_eq!(out.dataless, strs(&["cloud.md"]));
    assert_eq!(out.stats.md, 1);
    assert_eq!(probe.read_dir_count(&p("/n/cdir")), 0);
}

#[test]
fn gitignore_with_dir_rule_and_negation() {
    let fake = FakeProbe::new()
        .file("/n/.gitignore", "cache/\n*.md\n!keep.md\n")
        .file("/n/a.md", "")
        .file("/n/keep.md", "")
        .file("/n/sub/keep.md", "")
        .file("/n/sub/b.md", "")
        .file("/n/cache/keep.md", "")
        .file("/n/cache/deep/c.md", "");
    let (out, probe) = run_default(fake);
    assert_eq!(out.md, strs(&["keep.md", "sub/keep.md"]));
    assert_eq!(probe.read_dir_count(&p("/n/cache")), 0);
    assert_eq!(probe.read_dir_count(&p("/n/cache/deep")), 0);
}

#[test]
fn nested_gitignore_scoped_to_its_dir() {
    let fake = FakeProbe::new()
        .file("/n/a.md", "")
        .file("/n/sub/.gitignore", "*.md\n!ok.md\n")
        .file("/n/sub/x.md", "")
        .file("/n/sub/ok.md", "")
        .file("/n/sub/deeper/y.md", "")
        .file("/n/other/z.md", "");
    let (out, _) = run_default(fake);
    assert_eq!(out.md, strs(&["a.md", "other/z.md", "sub/ok.md"]));
}

#[test]
fn ignore_files_add_order_and_cap() {
    // `.ignore` re-includes what `.gitignore` drops; `.mdrootsignore` wins last.
    let big = "#".repeat(256 * 1024 + 1);
    let fake = FakeProbe::new()
        .file("/n/.gitignore", "*.md\n")
        .file("/n/.ignore", "!*.md\n")
        .file("/n/.mdrootsignore", "drop.md\n")
        .file("/n/a.md", "")
        .file("/n/drop.md", "")
        .file("/n/big/.gitignore", big)
        .file("/n/big/b.md", "");
    let (out, _) = run_default(fake);
    assert_eq!(out.md, strs(&["a.md", "big/b.md"]));
    assert_eq!(out.abort, None);
}

#[test]
fn hgignore_globs_only_at_root() {
    let fake = FakeProbe::new()
        .file(
            "/n/.hgignore",
            "re\\.md$\nsyntax: glob\ng*.md\nsyntax: regexp\nx.*\nglob:pre*.md\n",
        )
        .file("/n/re.md", "")
        .file("/n/g1.md", "")
        .file("/n/x1.md", "")
        .file("/n/pre1.md", "")
        .file("/n/sub/.hgignore", "syntax: glob\n*.md\n")
        .file("/n/sub/s.md", "");
    let (out, _) = run_default(fake);
    assert_eq!(out.md, strs(&["re.md", "sub/s.md", "x1.md"]));
}

#[test]
fn entries_budget_aborts() {
    let opts = WalkOptions {
        budget: Budget {
            entries: 5,
            ..Budget::marker()
        },
        ..WalkOptions::default()
    };
    let (out, _) = run(wide(10), &opts);
    assert_eq!(out.abort, Some(Abort::Entries));
    // Exactly at the limit is fine.
    let opts = WalkOptions {
        budget: Budget {
            entries: 4 + 4,
            ..Budget::marker()
        },
        ..WalkOptions::default()
    };
    let (out, _) = run(wide(4), &opts);
    assert_eq!(out.abort, None);
}

#[test]
fn md_budget_aborts_with_partial_sorted_result() {
    let opts = WalkOptions {
        budget: Budget {
            md: 3,
            ..Budget::marker()
        },
        ..WalkOptions::default()
    };
    let (out, _) = run(wide(10), &opts);
    assert_eq!(out.abort, Some(Abort::Md));
    assert_eq!(
        out.md,
        strs(&["d000/a.md", "d001/a.md", "d002/a.md", "d003/a.md"])
    );
    let (out, _) = run(wide(3), &opts);
    assert_eq!(out.abort, None);
}

#[test]
fn depth_budget_aborts() {
    let opts = WalkOptions {
        budget: Budget {
            depth: 2,
            ..Budget::marker()
        },
        ..WalkOptions::default()
    };
    let (out, _) = run(FakeProbe::new().file("/n/a/b/c/x.md", ""), &opts);
    assert_eq!(out.abort, Some(Abort::Depth));
    let (out, _) = run(FakeProbe::new().file("/n/a/b/x.md", ""), &opts);
    assert_eq!(out.abort, None);
    assert_eq!(out.md, strs(&["a/b/x.md"]));
    assert_eq!(out.stats.max_depth, 2);
}

#[test]
fn wall_budget_aborts() {
    let opts = WalkOptions {
        budget: Budget {
            wall: Duration::from_millis(10),
            ..Budget::marker()
        },
        rate_ms_per_dir: 5.0,
        ..WalkOptions::default()
    };
    let fake = wide(30).read_dir_cost(ROOT, Duration::from_millis(1));
    let (out, probe) = run(fake, &opts);
    assert_eq!(out.abort, Some(Abort::Wall));
    assert_eq!(probe.read_dir_total(), 11);
}

#[test]
fn rate_check_aborts_slow_fs_only() {
    let slow = wide(20).read_dir_cost(ROOT, Duration::from_millis(8));
    let (out, probe) = run_default(slow);
    assert_eq!(out.abort, Some(Abort::Rate));
    // Decided once 100 ms had passed: 13 dirs at 8 ms.
    assert_eq!(probe.read_dir_total(), 13);
    assert_eq!(out.stats.ms_per_dir, Some(8.0));

    let fast = wide(20).read_dir_cost(ROOT, Duration::from_micros(200));
    let (out, _) = run_default(fast);
    assert_eq!(out.abort, None);
    assert_eq!(out.stats.dirs, 21);
    let m = out.stats.ms_per_dir.unwrap();
    assert!((m - 0.2).abs() < 1e-9, "{m}");
}

#[test]
fn small_fast_walk_is_never_rate_aborted() {
    // 4 dirs at 8 ms finish in 32 ms, before the 50-dir / 100 ms window:
    // a walk that short is fast enough whatever its median, so one slow
    // listing (cold cache, loaded machine) does not make a small repo lazy.
    let slow = wide(3).read_dir_cost(ROOT, Duration::from_millis(8));
    let (out, _) = run_default(slow);
    assert_eq!(out.abort, None);
    assert_eq!(out.md.len(), 3);
    assert_eq!(out.stats.ms_per_dir, Some(8.0));
}

#[test]
fn one_read_dir_per_visited_dir() {
    let fake = FakeProbe::new()
        .file("/n/a/x.md", "")
        .file("/n/a/b/y.md", "")
        .file("/n/c/z.md", "")
        .file("/n/node_modules/m/q.md", "");
    let (out, probe) = run_default(fake);
    assert_eq!(out.stats.dirs, 4);
    assert_eq!(probe.read_dir_total(), 4);
    for d in ["/n", "/n/a", "/n/a/b", "/n/c"] {
        assert_eq!(probe.read_dir_count(&p(d)), 1, "{d}");
    }
    assert_eq!(probe.read_dir_count(&p("/n/node_modules")), 0);
}

#[test]
fn skip_dirs_not_descended_or_counted() {
    let opts = WalkOptions {
        skip: vec![p("/n/child")],
        ..WalkOptions::default()
    };
    let fake = FakeProbe::new()
        .file("/n/a.md", "")
        .file("/n/child/c.md", "");
    let (out, probe) = run(fake, &opts);
    assert_eq!(out.md, strs(&["a.md"]));
    assert_eq!(out.stats.entries, 1);
    assert_eq!(probe.read_dir_count(&p("/n/child")), 0);
}

#[test]
fn cancelled_before_start() {
    let probe = Counting::new(FakeProbe::new().file("/n/a.md", ""));
    let cancel = Cancel::new();
    cancel.cancel();
    let out = walk(&probe, Path::new(ROOT), &WalkOptions::default(), &cancel);
    assert_eq!(out.abort, Some(Abort::Cancelled));
    assert_eq!(probe.read_dir_total(), 0);
}

#[test]
fn budget_presets() {
    assert_eq!(
        Budget::marker(),
        Budget {
            entries: 200_000,
            md: 50_000,
            wall: Duration::from_millis(1500),
            depth: 32
        }
    );
    assert_eq!(
        Budget::loose(),
        Budget {
            entries: 10_000,
            md: 5_000,
            wall: Duration::from_millis(300),
            depth: 8
        }
    );
    let d = WalkOptions::default();
    assert_eq!(
        (d.budget, d.rate_ms_per_dir, d.skip.len()),
        (Budget::marker(), 5.0, 0)
    );
}

/// A FakeProbe whose `stat`/`lstat` each advance the clock by `cost`, and
/// which cancels `cancel` after `cancel_after` of them.
struct SlowStat {
    inner: FakeProbe,
    cost: Duration,
    extra: Mutex<Duration>,
    stats: AtomicUsize,
    cancel_after: Option<(usize, Cancel)>,
}

impl SlowStat {
    fn new(inner: FakeProbe, cost: Duration) -> Self {
        SlowStat {
            inner,
            cost,
            extra: Mutex::default(),
            stats: AtomicUsize::new(0),
            cancel_after: None,
        }
    }

    fn tick(&self) {
        *self.extra.lock().unwrap() += self.cost;
        let n = self.stats.fetch_add(1, Ordering::Relaxed) + 1;
        if let Some((after, c)) = &self.cancel_after
            && n >= *after
        {
            c.cancel();
        }
    }
}

impl Probe for SlowStat {
    fn stat(&self, p: &Path) -> io::Result<FsStat> {
        self.tick();
        self.inner.stat(p)
    }
    fn lstat(&self, p: &Path) -> io::Result<FsStat> {
        self.tick();
        self.inner.lstat(p)
    }
    fn mount(&self, p: &Path) -> io::Result<MountInfo> {
        Probe::mount(&self.inner, p)
    }
    fn read_dir(&self, p: &Path) -> io::Result<Vec<(String, FsStat)>> {
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
        Probe::home(&self.inner)
    }
    fn now(&self) -> Duration {
        self.inner.now() + *self.extra.lock().unwrap()
    }
}

#[test]
fn wall_budget_checked_during_marker_probes() {
    let opts = WalkOptions {
        budget: Budget {
            wall: Duration::from_millis(10),
            ..Budget::marker()
        },
        ..WalkOptions::default()
    };
    // One listing (0.2 ms) then 1 ms per lstat of the marker probes.
    let probe = Counting::new(SlowStat::new(wide(30), Duration::from_millis(1)));
    let out = walk(&probe, Path::new(ROOT), &opts, &Cancel::new());
    assert_eq!(out.abort, Some(Abort::Wall));
    assert!(out.stats.ms <= 11.0 + 0.2 + 1e-9, "{}", out.stats.ms);
    assert_eq!(probe.read_dir_total(), 1);
}

#[test]
fn cancel_checked_during_marker_probes() {
    let cancel = Cancel::new();
    let mut slow = SlowStat::new(wide(30), Duration::ZERO);
    slow.cancel_after = Some((5, cancel.clone()));
    let probe = Counting::new(slow);
    let out = walk(&probe, Path::new(ROOT), &WalkOptions::default(), &cancel);
    assert_eq!(out.abort, Some(Abort::Cancelled));
    assert_eq!(probe.read_dir_total(), 1);
    assert!(probe.inner().stats.load(Ordering::Relaxed) <= 5);
}

#[test]
fn empty_mdrootsignore_wins_over_marker() {
    let fake = FakeProbe::new()
        .file("/n/a.md", "")
        .file("/n/sub/.git/HEAD", "")
        .file("/n/sub/.mdrootsignore", "")
        .file("/n/sub/s.md", "");
    let (out, probe) = run_default(fake);
    assert_eq!(out.md, strs(&["a.md"]));
    assert!(out.nested_roots.is_empty(), "{:?}", out.nested_roots);
    assert_eq!(probe.read_dir_count(&p("/n/sub")), 0);
}

#[test]
fn hgignore_glob_section_keeps_lines_with_colons() {
    let fake = FakeProbe::new()
        .file(
            "/n/.hgignore",
            "syntax: glob\nfoo:bar.md\nre:^r.*\\.md$\npath:p.md\nglob:g.md\n",
        )
        .file("/n/foo:bar.md", "")
        .file("/n/r1.md", "")
        .file("/n/p.md", "")
        .file("/n/g.md", "")
        .file("/n/k.md", "");
    let (out, _) = run_default(fake);
    assert_eq!(out.md, strs(&["k.md", "p.md", "r1.md"]));
}

#[test]
fn pruned_dir_matches_the_prune_list() {
    use mdroots_roots::pruned_dir;
    for name in ["node_modules", "target", "buck-out", ".venv", "vendor"] {
        assert!(pruned_dir(name), "{name}");
    }
    for name in ["notes", "src", "Target", ""] {
        assert!(!pruned_dir(name), "{name}");
    }
}

#[test]
fn one_slow_listing_never_rate_aborts() {
    // A single listing slower than the whole rate window (a loaded machine)
    // is not evidence of a slow filesystem.
    let slow = wide(2).read_dir_cost(ROOT, Duration::from_millis(150));
    let (out, _) = run_default(slow);
    assert_eq!(out.abort, None);
}
