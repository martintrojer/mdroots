//! End-to-end discovery fixtures 1–11 of docs/specs/roots.md §7.
//!
//! Fixtures 1–4 and 9–11 build real trees in a temp dir and run [`discover`]
//! over [`StdProbe`], with the probe's home set to the temp dir (so loose
//! growth rules apply and no climb leaves the temp dir) and every `read_dir`
//! checked by [`Counting`]. The probe's clock is fake (frozen unless a
//! fixture makes it tick), so the walks' wall and rate budgets never depend
//! on how loaded the machine is. Repos are made with [git](https://git-scm.com);
//! a fixture that needs git prints a note and passes when git is missing.
//!
//! Fixtures 5–8 need a virtual filesystem or a cloud folder, so they run on
//! [`FakeProbe`]: an [EdenFS](https://github.com/facebook/sapling) checkout
//! (the virtual filesystem from the [Sapling](https://sapling-scm.com/)
//! project) and a dataless cloud file. No test touches a real virtual FS,
//! cloud folder or home directory.
//!
//! Markers used: `.zk` ([zk](https://github.com/zk-org/zk)), `.obsidian`
//! ([Obsidian](https://obsidian.md)), `.buckconfig` and `buck-out`
//! ([Buck2](https://buck2.build)).
//!
//! Each fixture prints its decision ([`explain`]) and the wall time of the
//! [`discover`] call, which must stay under one second.
#![cfg(unix)]

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use mdroots_core::Cancel;
use mdroots_roots::{
    Counting, Decision, DiscoverOptions, Enumerator, FakeProbe, FsStat, MemRegistry, MountInfo,
    NoEnumerator, Probe, Registry, RootMode, RootRecord, StdProbe, VerdictSource, discover,
    explain,
};

/// Serialises the real-filesystem fixtures: parallel tree building would
/// skew the one-second check on each [`discover`] call.
static FS_LOCK: Mutex<()> = Mutex::new(());

const NOW: u64 = 1_000_000_000;

fn opts() -> DiscoverOptions {
    DiscoverOptions {
        now_ms: NOW,
        ..Default::default()
    }
}

/// [`StdProbe`] with `home()` pinned to a fixture's temp dir and a fake
/// clock; filesystem calls delegate. (Setting `$HOME` would need
/// `std::env::set_var`, which is unsafe.)
///
/// `now()` advances by `tick` per `read_dir` and by nothing else, so the
/// walks' wall and rate budgets see the same time on every run however
/// loaded the machine is. A zero tick freezes the clock.
struct HomeProbe {
    inner: StdProbe,
    home: PathBuf,
    tick: Duration,
    now: Mutex<Duration>,
}

impl Probe for HomeProbe {
    fn stat(&self, p: &Path) -> io::Result<FsStat> {
        self.inner.stat(p)
    }
    fn lstat(&self, p: &Path) -> io::Result<FsStat> {
        self.inner.lstat(p)
    }
    fn mount(&self, p: &Path) -> io::Result<MountInfo> {
        self.inner.mount(p)
    }
    fn read_dir(&self, p: &Path) -> io::Result<Vec<(String, FsStat)>> {
        *self.now.lock().unwrap_or_else(|e| e.into_inner()) += self.tick;
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
        Some(self.home.clone())
    }
    fn now(&self) -> Duration {
        *self.now.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A temp dir (canonical path) that is also the probe's home.
struct Fx {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    probe: Counting<HomeProbe>,
}

impl Fx {
    /// A fixture whose probe clock is frozen.
    fn new() -> Fx {
        Fx::ticking(Duration::ZERO)
    }

    /// A fixture whose probe clock advances `tick` per `read_dir`.
    fn ticking(tick: Duration) -> Fx {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().canonicalize().expect("canonicalize tempdir");
        let probe = Counting::new(HomeProbe {
            inner: StdProbe,
            home: home.clone(),
            tick,
            now: Mutex::new(Duration::ZERO),
        });
        Fx {
            _tmp: tmp,
            home,
            probe,
        }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.home.join(rel)
    }

    /// Write `rel` (parents created).
    fn write(&self, rel: &str, body: &str) {
        let p = self.path(rel);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        std::fs::write(&p, body).expect("write");
    }

    /// `n` files directly in `dir`, the first `md` of them notes.
    fn files(&self, dir: &str, n: usize, md: usize) {
        for i in 0..n {
            let ext = if i < md { "md" } else { "txt" };
            self.write(&format!("{dir}/f{i:04}.{ext}"), "x\n");
        }
    }

    /// Discover `rel`, timing only the call and checking that every
    /// `read_dir` stayed inside the temp dir.
    fn run(&self, name: &str, reg: &mut MemRegistry, rel: &str) -> Decision {
        let home = [self.home.clone()];
        timed(
            name,
            &self.probe,
            &home,
            reg,
            &NoEnumerator,
            &self.path(rel),
        )
    }
}

/// Run [`discover`] for `file` with `read_dir` allowed only under `allowed`;
/// print the decision and time, assert under one second and no violations.
fn timed<P: Probe>(
    name: &str,
    probe: &Counting<P>,
    allowed: &[PathBuf],
    reg: &mut MemRegistry,
    en: &dyn Enumerator,
    file: &Path,
) -> Decision {
    probe.forbid_read_dir_outside(allowed);
    let before = probe.read_dir_total();
    let t = Instant::now();
    let d = discover(probe, reg, en, file, &opts(), &Cancel::new());
    let el = t.elapsed();
    println!(
        "fixture {name}: {} [{} read_dir, {:.1} ms]",
        explain(&d),
        probe.read_dir_total() - before,
        el.as_secs_f64() * 1000.0
    );
    assert!(el < Duration::from_secs(1), "fixture {name} took {el:?}");
    assert!(
        probe.violations().is_empty(),
        "read_dir outside {allowed:?}: {:?}",
        probe.violations()
    );
    d
}

/// Whether git runs here; prints a skip note if not.
fn have_git(fixture: &str) -> bool {
    let ok = Command::new("git")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        println!("fixture {fixture}: skipped, git not found");
    }
    ok
}

/// Run git in `dir` with the user's and system config ignored.
fn git(dir: &Path, args: &[&str]) {
    let st = Command::new("git")
        .args(["-c", "init.defaultBranch=main"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run git");
    assert!(st.success(), "git {args:?} in {}", dir.display());
}

fn git_init(dir: &Path) {
    std::fs::create_dir_all(dir).expect("mkdir");
    git(dir, &["init", "-q"]);
}

fn row(reg: &MemRegistry, path: &Path) -> Option<RootRecord> {
    reg.all().into_iter().find(|r| r.path == path)
}

// --- real temp dirs ---------------------------------------------------------

/// A projects folder: 40 small repos (`git init` only, 6 of them inside
/// `scratch/`), `scratch/` (10 files, 2 md), 6 loose files (1 md) and,
/// unless `tarball` is false, an unpacked tarball (490 files, 1 md).
fn projects(fx: &Fx, tarball: bool) {
    for i in 0..40 {
        let repo = match i < 6 {
            true => format!("projects/scratch/repo{i:02}"),
            false => format!("projects/repo{i:02}"),
        };
        git_init(&fx.path(&repo));
        fx.write(&format!("{repo}/README.md"), "# repo\n");
        fx.write(&format!("{repo}/main.c"), "int main(void) { return 0; }\n");
    }
    fx.files("projects/scratch", 10, 2);
    fx.files("projects", 6, 1);
    if tarball {
        for d in 0..7 {
            fx.files(&format!("projects/lib-1.0/src{d}"), 70, usize::from(d == 0));
        }
    }
}

#[test]
fn fixture_01_scratch_dir_in_projects_folder_is_its_own_loose_root() {
    let _l = FS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !have_git("1") {
        return;
    }
    for (tarball, rejected) in [
        (
            true,
            "loose root rejected at ~/projects: 4 md (< 20), adds 2 md (<= 2 in child)",
        ),
        (
            false,
            "loose root rejected at ~/projects: 3 md (< 20), adds 1 md (<= 2 in child)",
        ),
    ] {
        let fx = Fx::new();
        projects(&fx, tarball);
        let mut reg = MemRegistry::new();
        let d = fx.run("1", &mut reg, "projects/scratch/f0000.md");
        assert_eq!(d.mode, RootMode::Loose);
        assert_eq!(d.root, Some(fx.path("projects/scratch")));
        assert_eq!(explain(&d), rejected);
        let s = d.stats.expect("stats");
        assert_eq!((s.files, s.md), (10, 2));
        assert_eq!(d.md, ["f0000.md", "f0001.md"]);
        assert_eq!(d.nested_roots.len(), 6);
        // Nothing outside projects/ was listed.
        assert_eq!(fx.probe.read_dir_count(&fx.home), 0);

        // A file inside any repo gets that repo (no index: budgeted walk).
        for repo in ["projects/repo07", "projects/scratch/repo03"] {
            let mut reg = MemRegistry::new();
            let d = fx.run("1", &mut reg, &format!("{repo}/README.md"));
            assert_eq!(d.mode, RootMode::Vcs);
            assert_eq!(d.root, Some(fx.path(repo)));
            assert!(explain(&d).contains("(no git index)"), "{}", explain(&d));
            assert_eq!(d.md, ["README.md"]);
        }
    }
}

#[test]
fn fixture_02_mid_size_git_checkout_is_a_budgeted_walk() {
    let _l = FS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !have_git("2") {
        return;
    }
    let fx = Fx::new();
    let repo = fx.path("projects/editor");
    git_init(&repo);
    // 3,800 files in 38 directories, 13 of them notes.
    for i in 0..3800 {
        let ext = if i % 300 == 0 { "md" } else { "c" };
        fx.write(
            &format!("projects/editor/src/m{:02}/f{i:04}.{ext}", i / 100),
            "x\n",
        );
    }
    git(&repo, &["add", "-A"]);
    let d = fx.run(
        "2",
        &mut MemRegistry::new(),
        "projects/editor/src/m00/f0000.md",
    );
    assert_eq!(d.mode, RootMode::Vcs);
    assert_eq!(d.root, Some(repo.clone()));
    assert_eq!(d.md.len(), 13);
    assert!(
        explain(&d).contains("(git index 3,800 entries): 13 md"),
        "{}",
        explain(&d)
    );
    assert_eq!(fx.probe.violations(), Vec::<PathBuf>::new());
    assert_eq!(fx.probe.read_dir_count(&fx.path("projects")), 0);
}

#[test]
fn fixture_03_dense_notes_folder_is_accepted_as_a_loose_root() {
    let _l = FS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fx::new();
    // 4,000 md among 6,000 files: inside the loose budget (10k entries, 5k md).
    for d in 0..40 {
        fx.files(&format!("notes/d{d:02}"), 150, 100);
    }
    let mut reg = MemRegistry::new();
    let d = fx.run("3", &mut reg, "notes/d00/f0000.md");
    assert_eq!(d.mode, RootMode::Loose);
    assert_eq!(d.root, Some(fx.path("notes")));
    assert_eq!(
        explain(&d),
        "loose root accepted at ~/notes: 4000 md / 6000 files"
    );
    // Rule (a): at least 20 notes and at least 30% of files.
    let s = d.stats.expect("stats");
    assert_eq!((s.md, s.files), (4000, 6000));
    assert!(s.md >= 20 && s.md * 10 >= s.files * 3);
    assert_eq!(d.md.len(), 4000);
    assert_eq!(fx.probe.read_dir_count(&fx.home), 0);
}

#[test]
fn fixture_03_notes_folder_over_the_loose_md_budget_is_lazy() {
    let _l = FS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fx::new();
    // 6,000 md below the start dir: accepted, but its walk aborts on md.
    for d in 0..40 {
        fx.files(&format!("notes/d{d:02}"), 150, 150);
    }
    fx.write("notes/today.md", "# today\n");
    let mut reg = MemRegistry::new();
    let d = fx.run("3 (6k md)", &mut reg, "notes/today.md");
    assert_eq!(d.mode, RootMode::Lazy);
    assert_eq!(d.root, Some(fx.path("notes")));
    assert!(
        explain(&d).starts_with("lazy: walk over budget (md) at ~/notes"),
        "{}",
        explain(&d)
    );
    let r = row(&reg, &fx.path("notes")).expect("registered");
    assert_eq!(r.verdict_source, VerdictSource::Budget);
    assert_eq!(r.mode, RootMode::Lazy);
}

#[test]
fn fixture_03_slow_listings_abort_the_loose_walk_on_wall_time() {
    let _l = FS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // The dense folder of the first fixture 3, but every listing takes 20 ms
    // of probe time: 41 dirs need 820 ms, over the 300 ms loose wall budget.
    // (Under 50 dirs, so the rate check never runs.)
    let fx = Fx::ticking(Duration::from_millis(20));
    for d in 0..40 {
        fx.files(&format!("notes/d{d:02}"), 150, 100);
    }
    let mut reg = MemRegistry::new();
    let d = fx.run("3 (slow)", &mut reg, "notes/d00/f0000.md");
    // The start dir's own walk fits; growing to notes/ runs out of wall time,
    // so the start dir stays the root.
    assert_eq!(d.mode, RootMode::Loose);
    assert_eq!(d.root, Some(fx.path("notes/d00")));
    assert_eq!(
        explain(&d),
        "loose root rejected at ~/notes: walk aborted (wall)"
    );
    assert_eq!(d.md.len(), 100);
}

#[test]
fn fixture_04_zk_notebook_inside_a_git_repo() {
    let _l = FS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !have_git("4") {
        return;
    }
    let fx = Fx::new();
    let repo = fx.path("projects/app");
    git_init(&repo);
    fx.write("projects/app/README.md", "# app\n");
    fx.write("projects/app/src/main.rs", "fn main() {}\n");
    std::fs::create_dir_all(fx.path("projects/app/notebook/.zk")).expect("mkdir");
    fx.files("projects/app/notebook", 5, 5);

    // The nearer notes-tool marker beats the farther git root.
    let mut reg = MemRegistry::new();
    let d = fx.run("4", &mut reg, "projects/app/notebook/f0000.md");
    assert_eq!(d.mode, RootMode::Marker);
    assert_eq!(d.root, Some(fx.path("projects/app/notebook")));
    assert!(explain(&d).starts_with("marker .zk at ~/projects/app/notebook: 5 md"));
    assert_eq!(d.md.len(), 5);
    assert_eq!(fx.probe.read_dir_count(&repo), 0);

    // A file elsewhere in the repo gets the repo (`git init` only: walked),
    // with the notebook as a nested root.
    let d = fx.run("4", &mut reg, "projects/app/README.md");
    assert_eq!(d.mode, RootMode::Vcs);
    assert_eq!(d.root, Some(repo.clone()));
    assert!(explain(&d).contains("(no git index)"), "{}", explain(&d));
    assert_eq!(d.md, ["README.md"]);
    assert_eq!(d.nested_roots, [fx.path("projects/app/notebook")]);
    assert_eq!(reg.all().len(), 2);
}

#[test]
fn fixture_09_dotfiles_repo_at_home_is_tracked_only() {
    let _l = FS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !have_git("9") {
        return;
    }
    let fx = Fx::new();
    git_init(&fx.home);
    fx.write(".bashrc", "export EDITOR=vi\n");
    fx.write("notes.md", "# notes\n");
    fx.write("docs/setup.md", "# setup\n");
    git(&fx.home, &["add", ".bashrc", "notes.md", "docs/setup.md"]);
    // A large untracked tree that must not be walked.
    for d in 0..20 {
        fx.files(&format!("big/d{d:02}"), 100, 50);
    }
    for file in ["notes.md", "docs/setup.md"] {
        let mut reg = MemRegistry::new();
        let d = timed("9", &fx.probe, &[], &mut reg, &NoEnumerator, &fx.path(file));
        assert_eq!(d.mode, RootMode::TrackedOnly);
        assert_eq!(d.root, Some(fx.home.clone()));
        assert_eq!(d.md, ["docs/setup.md", "notes.md"]);
        assert_eq!(
            explain(&d),
            "tracked-only: vcs at ~ (home directory): 2 tracked md"
        );
    }
    assert_eq!(fx.probe.read_dir_total(), 0);
}

#[test]
fn fixture_10_new_git_repo_inside_a_lazy_root_becomes_a_nested_root() {
    let _l = FS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !have_git("10") {
        return;
    }
    let fx = Fx::new();
    let mono = fx.path("mono");
    fx.write("mono/.buckconfig", "");
    fx.files("mono/src", 50, 10);
    let mut reg = MemRegistry::new();
    let d = timed(
        "10",
        &fx.probe,
        &[],
        &mut reg,
        &NoEnumerator,
        &fx.path("mono/src/f0000.md"),
    );
    assert_eq!(d.mode, RootMode::Lazy);
    assert_eq!(d.root, Some(mono.clone()));
    assert_eq!(explain(&d), "lazy: monorepo marker at ~/mono");

    // `git init` + `git add` a subdir, then open a file in it.
    let sub = fx.path("mono/tools/notes");
    git_init(&sub);
    fx.files("mono/tools/notes", 3, 3);
    git(&sub, &["add", "-A"]);
    let d = timed(
        "10",
        &fx.probe,
        std::slice::from_ref(&sub),
        &mut reg,
        &NoEnumerator,
        &sub.join("f0000.md"),
    );
    assert_eq!(d.root, Some(sub.clone()));
    assert_eq!(d.mode, RootMode::Vcs);
    assert_eq!(d.md.len(), 3);
    let all: Vec<PathBuf> = reg.all().into_iter().map(|r| r.path).collect();
    assert_eq!(all, [mono, sub]);
}

#[test]
fn fixture_11_moved_root_is_rekeyed_without_a_rebuild() {
    let _l = FS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fx::new();
    std::fs::create_dir_all(fx.path("notes/.zk")).expect("mkdir");
    fx.files("notes", 5, 5);
    let mut reg = MemRegistry::new();
    let d = fx.run("11", &mut reg, "notes/f0000.md");
    assert_eq!(d.mode, RootMode::Marker);
    let id = row(&reg, &fx.path("notes")).expect("registered").root_id;

    std::fs::rename(fx.path("notes"), fx.path("notes2")).expect("rename");
    let before = fx.probe.read_dir_total();
    let d = fx.run("11", &mut reg, "notes2/f0001.md");
    assert_eq!(fx.probe.read_dir_total(), before, "no cold rebuild");
    assert_eq!(d.root, Some(fx.path("notes2")));
    assert_eq!(d.mode, RootMode::Marker);
    let all = reg.all();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].root_id, id);
    assert_eq!(all[0].path, fx.path("notes2"));
}

// --- fake virtual filesystems ----------------------------------------------

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

/// An EdenFS checkout at `root` on mount `/eden`: `.eden/root` in each of
/// `dirs`, plus `n` notes in each.
fn eden_checkout(root: &str, dirs: &[&str], n: usize) -> FakeProbe {
    let mut f = FakeProbe::new().home("/h").mount("/eden", eden(5));
    for d in dirs {
        f = f.symlink(format!("{d}/.eden/root"), root);
        for i in 0..n {
            f = f.file(format!("{d}/n{i:03}.md"), "");
        }
    }
    f
}

/// A large checkout: a monorepo marker and many dirs that must stay unlisted.
fn large_checkout() -> FakeProbe {
    let dirs: Vec<String> = (0..50)
        .map(|i| format!("/eden/repo/proj{i:02}/docs"))
        .chain(["/eden/repo".to_owned()])
        .collect();
    let dirs: Vec<&str> = dirs.iter().map(String::as_str).collect();
    eden_checkout("/eden/repo", &dirs, 20).file("/eden/repo/.buckconfig", "")
}

#[test]
fn fixture_05_file_in_a_large_eden_checkout_is_lazy_with_zero_read_dir() {
    let probe = Counting::new(large_checkout());
    let file = Path::new("/eden/repo/proj07/docs/n000.md");
    let mut reg = MemRegistry::new();
    let allowed = [PathBuf::from("/eden/repo/proj07/docs")];
    let d = timed("5", &probe, &allowed, &mut reg, &NoEnumerator, file);
    assert_eq!(d.mode, RootMode::Lazy);
    assert_eq!(d.root, Some(PathBuf::from("/eden/repo")));
    assert!(explain(&d).starts_with("lazy: statfs edenfs:, MNT_LOCAL unset"));
    assert!(d.md.is_empty() && d.stats.is_none());
    assert_eq!(probe.read_dir_total(), 0);
}

#[test]
fn fixture_06_build_output_mount_inside_the_checkout_is_never_walked() {
    // No marker below the mount: single-file, nothing registered.
    let f = large_checkout()
        .mount("/eden/repo/buck-out", local(6))
        .file("/eden/repo/buck-out/gen/a.md", "")
        .file("/eden/repo/buck-out/gen/b.md", "");
    let probe = Counting::new(f);
    let mut reg = MemRegistry::new();
    let allowed = [PathBuf::from("/eden/repo/buck-out/gen")];
    let file = Path::new("/eden/repo/buck-out/gen/a.md");
    let d = timed("6", &probe, &allowed, &mut reg, &NoEnumerator, file);
    assert_eq!(d.mode, RootMode::SingleFile);
    assert_eq!(d.root, None);
    assert!(reg.all().is_empty());

    // A marker below the mount: lazy there, still no row at the mount.
    let f = large_checkout()
        .mount("/eden/repo/buck-out", local(6))
        .file("/eden/repo/buck-out/gen/docs/.mdroots", "")
        .file("/eden/repo/buck-out/gen/docs/a.md", "");
    let probe = Counting::new(f);
    let mut reg = MemRegistry::new();
    let allowed = [PathBuf::from("/eden/repo/buck-out/gen/docs")];
    let file = Path::new("/eden/repo/buck-out/gen/docs/a.md");
    let d = timed(
        "6 (marker)",
        &probe,
        &allowed,
        &mut reg,
        &NoEnumerator,
        file,
    );
    assert_eq!(d.mode, RootMode::Lazy);
    assert_eq!(d.root, Some(PathBuf::from("/eden/repo/buck-out/gen/docs")));
    assert!(row(&reg, Path::new("/eden/repo/buck-out")).is_none());
    assert_eq!(probe.read_dir_total(), 0);
}

/// Returns a fixed md list at once, like a VCS file listing would.
struct FakeEnumerator(Vec<String>);

impl Enumerator for FakeEnumerator {
    fn md_paths(&self, _: &Path, _: Duration, cap: usize) -> Option<Vec<String>> {
        (self.0.len() <= cap).then(|| self.0.clone())
    }
}

#[test]
fn fixture_07_small_notes_repo_on_eden_is_vcs_enumerated() {
    let probe = Counting::new(eden_checkout(
        "/eden/notes",
        &["/eden/notes", "/eden/notes/daily", "/eden/notes/topics"],
        10,
    ));
    let mut md: Vec<String> = ["daily", "topics"]
        .iter()
        .flat_map(|d| (0..10).map(move |i| format!("{d}/n{i:03}.md")))
        .chain((0..10).map(|i| format!("n{i:03}.md")))
        .collect();
    md.reverse(); // unsorted, as a VCS may list them
    let en = FakeEnumerator(md.clone());
    let mut reg = MemRegistry::new();
    let file = Path::new("/eden/notes/daily/n003.md");
    let t = Instant::now();
    let d = timed("7", &probe, &[], &mut reg, &en, file);
    assert!(t.elapsed() < Duration::from_millis(500));
    assert_eq!(d.mode, RootMode::VcsEnumerated);
    assert_eq!(d.root, Some(PathBuf::from("/eden/notes")));
    md.sort();
    assert_eq!(d.md, md);
    assert!(explain(&d).starts_with("vcs-enumerated: 30 md from the VCS"));
    assert_eq!(probe.read_dir_total(), 0);
    // The spec's "[[stem]] resolves" part needs the index; it is checked
    // in M3.
}

#[test]
fn fixture_08_dataless_cloud_file_is_listed_by_stat_only() {
    let vault = "/h/Library/Mobile Documents/iCloud~md~obsidian/Documents/vault";
    let mut f = FakeProbe::new()
        .home("/h")
        .dir(format!("{vault}/.obsidian"))
        .file(format!("{vault}/today.md"), "see [[linked]]\n")
        .dataless(format!("{vault}/linked.md"))
        .file(format!("{vault}/archive/old.md"), "")
        .dataless(format!("{vault}/archive"));
    for i in 0..20 {
        f = f.file(format!("{vault}/topics/t{i:02}.md"), "");
    }
    let probe = Counting::new(f);
    let allowed = [PathBuf::from(vault)];
    let linked = PathBuf::from(format!("{vault}/linked.md"));

    for file in [format!("{vault}/today.md"), linked.display().to_string()] {
        let mut reg = MemRegistry::new();
        let d = timed(
            "8",
            &probe,
            &allowed,
            &mut reg,
            &NoEnumerator,
            Path::new(&file),
        );
        assert_eq!(d.mode, RootMode::Marker);
        assert_eq!(d.root, Some(PathBuf::from(vault)));
        // The dataless note is not among the walked notes...
        assert_eq!(d.md.len(), 21);
        assert!(!d.md.iter().any(|m| m == "linked.md"));
        assert_eq!(d.stats.expect("stats").md, 21);
    }
    // ...but a link to it still resolves by stat, and it stays dataless.
    let st = probe.stat(&linked).expect("stat");
    assert!(st.is_file && st.dataless);
    // The dataless directory was never listed.
    assert_eq!(probe.read_dir_count(&Path::new(vault).join("archive")), 0);
}
