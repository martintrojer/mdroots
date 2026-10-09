//! The stage-4 budgeted walk (docs/specs/roots.md §1 stage 4).
//!
//! Breadth-first and sequential: the rate check needs per-directory timing,
//! and shallow files (the likeliest link targets) come first, so a partial
//! result after an abort still covers them. Every listing goes through
//! [`Probe::read_dir`]; pruned directories are never listed.
//!
//! Ignore files use [git](https://git-scm.com)'s gitignore syntax (matched with
//! the [`ignore`](https://crates.io/crates/ignore) crate's `gitignore` module), except `.hgignore`
//! ([Mercurial](https://www.mercurial-scm.org)), of which only glob lines are used.
//!
//! The wall budget and the cancel token are checked before every probe call,
//! so an abort lands at most one probe call past the budget.

use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Mutex;
use std::time::Duration;

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use mdroots_core::Cancel;

use crate::markers::{MarkerClass, markers_at};
use crate::probe::{FsStat, MountInfo, Probe};

/// Walk limits; exceeding one (a count going above it) aborts the walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// Directory entries listed.
    pub entries: usize,
    /// Note files found.
    pub md: usize,
    pub wall: Duration,
    /// Directory depth below the root (its children are depth 1).
    pub depth: usize,
}

impl Budget {
    /// For a root with a marker.
    pub fn marker() -> Self {
        Budget {
            entries: 200_000,
            md: 50_000,
            wall: Duration::from_millis(1500),
            depth: 32,
        }
    }

    /// For a loose root candidate (docs/specs/roots.md §2).
    pub fn loose() -> Self {
        Budget {
            entries: 10_000,
            md: 5_000,
            wall: Duration::from_millis(300),
            depth: 8,
        }
    }
}

/// Counts and timing of one walk.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct WalkStats {
    /// Every entry listed, pruned or not (except `skip` dirs).
    pub entries: usize,
    /// Note files found (not dataless).
    pub md: usize,
    /// Regular files kept after pruning, dataless ones included.
    pub files: usize,
    /// Directories listed, the root included.
    pub dirs: usize,
    /// Wall time of the walk by [`Probe::now`].
    pub ms: f64,
    /// Median `read_dir` time; `None` if no directory was listed.
    pub ms_per_dir: Option<f64>,
    /// Depth of the deepest directory listed (the root is 0).
    pub max_depth: usize,
}

/// Why a walk stopped early.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Abort {
    Entries,
    Md,
    Wall,
    Depth,
    /// The median `read_dir` time is above [`WalkOptions::rate_ms_per_dir`].
    Rate,
    Cancelled,
}

/// The result of [`walk`]. On abort, `md` holds what was found so far.
#[derive(Debug, Clone, PartialEq)]
pub struct WalkOutcome {
    /// Note files, root-relative, `/`-separated, sorted.
    pub md: Vec<String>,
    /// Dataless note files (contents not local), root-relative, sorted.
    pub dataless: Vec<String>,
    /// Subdirectories with a root marker, absolute, sorted; not descended.
    pub nested_roots: Vec<PathBuf>,
    pub stats: WalkStats,
    pub abort: Option<Abort>,
}

#[derive(Debug, Clone)]
pub struct WalkOptions {
    pub budget: Budget,
    /// Median ms per `read_dir` above which the filesystem counts as slow.
    pub rate_ms_per_dir: f64,
    /// Absolute directories never descended and not counted.
    pub skip: Vec<PathBuf>,
}

impl Default for WalkOptions {
    fn default() -> Self {
        WalkOptions {
            budget: Budget::marker(),
            rate_ms_per_dir: 5.0,
            skip: Vec::new(),
        }
    }
}

/// Directory names never descended. [Buck2](https://buck2.build) writes
/// `buck-out`, [Bazel](https://bazel.build) `bazel-out`.
const PRUNE_DIRS: &[&str] = &[
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
];

/// Whether the walk never descends a directory named `name` (the prune
/// list; hidden names are skipped separately).
pub fn pruned_dir(name: &str) -> bool {
    PRUNE_DIRS.contains(&name)
}

const NOTE_EXTS: &[&str] = &["md", "markdown", "org"];
const IGNORE_CAP: usize = 256 * 1024;
const MDROOTSIGNORE: &str = ".mdrootsignore";
/// Gitignore-syntax ignore files, in add order (later wins).
const IGNORE_FILES: &[&str] = &[".gitignore", ".ignore", MDROOTSIGNORE];
const RATE_DIRS: usize = 50;
/// Fewer timed listings than this are noise (one slow `readdir` on a
/// loaded machine): never rate-abort on them.
const RATE_MIN_DIRS: usize = 5;
const RATE_WINDOW: Duration = Duration::from_millis(100);

fn is_note(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(stem, ext)| {
        !stem.is_empty() && NOTE_EXTS.iter().any(|e| ext.eq_ignore_ascii_case(e))
    })
}

/// Hidden and editor temp files ([Vim](https://www.vim.org) writes `4913` to
/// test whether a directory is writable).
fn is_temp(name: &str) -> bool {
    name.starts_with('.')
        || name.ends_with('~')
        || (name.len() > 1 && name.starts_with('#') && name.ends_with('#'))
        || name.ends_with(".swp")
        || name == "4913"
}

fn median(v: &[f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    let n = s.len();
    Some(if n % 2 == 1 {
        s[n / 2]
    } else {
        (s[n / 2 - 1] + s[n / 2]) / 2.0
    })
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// `bytes` of an ignore file, or `None` if missing, unreadable or over the cap.
fn read_ignore(probe: &dyn Probe, p: &Path) -> Option<String> {
    probe
        .read_small(p, IGNORE_CAP)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// Whether `dir/.mdrootsignore` is an empty (or blank) file: never index here.
fn ignored_entirely(probe: &dyn Probe, dir: &Path) -> bool {
    let p = dir.join(MDROOTSIGNORE);
    probe.lstat(&p).is_ok_and(|s| s.is_file)
        && read_ignore(probe, &p).is_some_and(|t| t.trim().is_empty())
}

/// Pattern-kind prefixes Mercurial accepts on an `.hgignore` line; a line
/// starting with one of these (other than `glob:`) is not a glob pattern.
const HG_KINDS: &[&str] = &[
    "re:",
    "regexp:",
    "path:",
    "relpath:",
    "rootfilesin:",
    "filepath:",
    "relglob:",
    "rootglob:",
    "relre:",
    "include:",
    "subinclude:",
    "listfile:",
    "listfile0:",
    "set:",
];

/// The glob lines of an `.hgignore`: those in `syntax: glob` sections or
/// prefixed `glob:`. Regexp is Mercurial's default syntax and is skipped.
/// In a glob section only a leading kind prefix makes a line non-glob; other
/// colons are part of the pattern.
fn hg_globs(text: &str) -> Vec<String> {
    let mut glob = false;
    let mut out = Vec::new();
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        if let Some(s) = l.strip_prefix("syntax:") {
            glob = s.trim() == "glob";
        } else if let Some(p) = l.strip_prefix("glob:") {
            out.push(p.trim().to_owned());
        } else if glob && !HG_KINDS.iter().any(|k| l.starts_with(k)) {
            out.push(l.to_owned());
        }
    }
    out
}

/// Wraps the caller's probe: once the wall budget is spent or the walk is
/// cancelled, every I/O call fails without reaching the inner probe and the
/// reason is kept for [`Walker::checkpoint`].
struct Guard<'a> {
    inner: &'a dyn Probe,
    cancel: &'a Cancel,
    start: Duration,
    wall: Duration,
    tripped: Mutex<Option<Abort>>,
}

impl Guard<'_> {
    fn tripped(&self) -> Option<Abort> {
        *self.tripped.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn check(&self) -> io::Result<()> {
        let mut t = self.tripped.lock().unwrap_or_else(|e| e.into_inner());
        if t.is_none() {
            if self.cancel.is_cancelled() {
                *t = Some(Abort::Cancelled);
            } else if self.inner.now().saturating_sub(self.start) > self.wall {
                *t = Some(Abort::Wall);
            }
        }
        match *t {
            Some(_) => Err(io::Error::new(io::ErrorKind::Interrupted, "walk aborted")),
            None => Ok(()),
        }
    }
}

impl Probe for Guard<'_> {
    fn stat(&self, p: &Path) -> io::Result<FsStat> {
        self.check()?;
        self.inner.stat(p)
    }
    fn lstat(&self, p: &Path) -> io::Result<FsStat> {
        self.check()?;
        self.inner.lstat(p)
    }
    fn mount(&self, p: &Path) -> io::Result<MountInfo> {
        self.check()?;
        self.inner.mount(p)
    }
    fn read_dir(&self, p: &Path) -> io::Result<Vec<(String, FsStat)>> {
        self.check()?;
        self.inner.read_dir(p)
    }
    fn read_link(&self, p: &Path) -> io::Result<PathBuf> {
        self.check()?;
        self.inner.read_link(p)
    }
    fn read_small(&self, p: &Path, cap: usize) -> io::Result<Vec<u8>> {
        self.check()?;
        self.inner.read_small(p, cap)
    }
    fn read_prefix(&self, p: &Path, n: usize) -> io::Result<Vec<u8>> {
        self.check()?;
        self.inner.read_prefix(p, n)
    }
    fn volume_id(&self, p: &Path) -> io::Result<String> {
        self.check()?;
        self.inner.volume_id(p)
    }
    fn home(&self) -> Option<PathBuf> {
        self.inner.home()
    }
    fn now(&self) -> Duration {
        self.inner.now()
    }
}

struct Item {
    abs: PathBuf,
    rel: String,
    depth: usize,
    /// Matchers of the ancestors, nearest last.
    matchers: Vec<Rc<Gitignore>>,
}

struct Walker<'a> {
    probe: &'a Guard<'a>,
    opts: &'a WalkOptions,
    root: &'a Path,
    root_dev: u64,
    start: Duration,
    out: WalkOutcome,
    times: Vec<f64>,
    rate_checked: bool,
}

/// Walk `root` within `opts.budget`, pruning as docs/specs/roots.md §1 stage 4
/// lists. Never follows symlinks or crosses mounts.
pub fn walk(probe: &dyn Probe, root: &Path, opts: &WalkOptions, cancel: &Cancel) -> WalkOutcome {
    let start = probe.now();
    let guard = Guard {
        inner: probe,
        cancel,
        start,
        wall: opts.budget.wall,
        tripped: Mutex::new(None),
    };
    let mut w = Walker {
        probe: &guard,
        opts,
        root,
        root_dev: 0,
        start,
        out: WalkOutcome {
            md: Vec::new(),
            dataless: Vec::new(),
            nested_roots: Vec::new(),
            stats: WalkStats::default(),
            abort: None,
        },
        times: Vec::new(),
        rate_checked: false,
    };
    // No rate check at the end: a walk that finished before the 50-dir /
    // 100 ms window is fast enough, whatever its median.
    let abort = w.run().err();
    w.finish(abort)
}

impl Walker<'_> {
    fn run(&mut self) -> Result<(), Abort> {
        self.checkpoint()?;
        let st = self.probe.stat(self.root);
        self.checkpoint()?;
        let Ok(st) = st else {
            return Ok(());
        };
        if !st.is_dir {
            return Ok(());
        }
        let ignored = ignored_entirely(self.probe, self.root);
        self.checkpoint()?;
        if ignored {
            return Ok(());
        }
        self.root_dev = st.dev;
        let mut queue = std::collections::VecDeque::from([Item {
            abs: self.root.to_path_buf(),
            rel: String::new(),
            depth: 0,
            matchers: Vec::new(),
        }]);
        while let Some(item) = queue.pop_front() {
            self.checkpoint()?;
            self.visit(item, &mut queue)?;
        }
        Ok(())
    }

    /// List one directory, count its entries and queue its subdirectories.
    fn visit(
        &mut self,
        item: Item,
        queue: &mut std::collections::VecDeque<Item>,
    ) -> Result<(), Abort> {
        let t0 = self.probe.now();
        let listing = self.probe.read_dir(&item.abs);
        let t1 = self.probe.now();
        // A refused listing did no I/O; keep it out of the rate sample.
        self.checkpoint()?;
        self.times.push(ms(t1.saturating_sub(t0)));
        let Ok(listing) = listing else {
            return self.timing_checks();
        };
        self.out.stats.dirs += 1;
        self.out.stats.max_depth = self.out.stats.max_depth.max(item.depth);

        let mut matchers = item.matchers;
        let gi = self.matcher(&item.abs, item.depth == 0, &listing);
        self.checkpoint()?;
        if let Some(gi) = gi {
            matchers.push(Rc::new(gi));
        }

        let counted = listing
            .iter()
            .filter(|(name, _)| !self.opts.skip.contains(&item.abs.join(name)))
            .count();
        self.out.stats.entries += counted;
        if self.out.stats.entries > self.opts.budget.entries {
            return Err(Abort::Entries);
        }
        self.timing_checks()?;

        for (name, st) in &listing {
            let abs = item.abs.join(name);
            let rel = match item.rel.is_empty() {
                true => name.clone(),
                false => format!("{}/{name}", item.rel),
            };
            if !self.keep(name, st, &abs, &matchers) {
                continue;
            }
            if st.is_file {
                self.file(name, st, rel)?;
            } else if st.is_dir {
                // An empty .mdrootsignore wins over any marker: never index here.
                if st.dataless || ignored_entirely(self.probe, &abs) {
                    self.checkpoint()?;
                    continue;
                }
                if self.nested_root(&abs)? {
                    continue;
                }
                if item.depth + 1 > self.opts.budget.depth {
                    return Err(Abort::Depth);
                }
                queue.push_back(Item {
                    abs,
                    rel,
                    depth: item.depth + 1,
                    matchers: matchers.clone(),
                });
            }
        }
        Ok(())
    }

    /// Prune rules that need no extra I/O: skip list, symlinks, other
    /// mounts, hidden and temp names, pruned dir names, ignore files.
    fn keep(&self, name: &str, st: &FsStat, abs: &Path, matchers: &[Rc<Gitignore>]) -> bool {
        if st.is_symlink || st.dev != self.root_dev || self.opts.skip.iter().any(|s| s == abs) {
            return false;
        }
        if st.is_dir && (name.starts_with('.') || PRUNE_DIRS.contains(&name)) {
            return false;
        }
        if st.is_file && is_temp(name) {
            return false;
        }
        // Nearest matcher with an opinion wins, as in git.
        let verdict = matchers
            .iter()
            .rev()
            .map(|m| m.matched(abs, st.is_dir))
            .find(|m| !m.is_none());
        !matches!(verdict, Some(Match::Ignore(_)))
    }

    fn file(&mut self, name: &str, st: &FsStat, rel: String) -> Result<(), Abort> {
        self.out.stats.files += 1;
        if !is_note(name) {
            return Ok(());
        }
        if st.dataless {
            self.out.dataless.push(rel);
            return Ok(());
        }
        self.out.md.push(rel);
        self.out.stats.md += 1;
        if self.out.stats.md > self.opts.budget.md {
            return Err(Abort::Md);
        }
        Ok(())
    }

    /// Records `dir` as a nested root if it has a root marker.
    fn nested_root(&mut self, dir: &Path) -> Result<bool, Abort> {
        let nested = markers_at(self.probe, dir).iter().any(|m| {
            matches!(
                m.class,
                MarkerClass::Explicit
                    | MarkerClass::NotesTool
                    | MarkerClass::DocsTool
                    | MarkerClass::Vcs
                    | MarkerClass::Monorepo
            )
        });
        // An aborted probe may have hidden a marker; do not record then.
        self.checkpoint()?;
        if nested {
            self.out.nested_roots.push(dir.to_path_buf());
        }
        Ok(nested)
    }

    /// The matcher for the ignore files in `dir`'s listing, if any.
    fn matcher(
        &self,
        dir: &Path,
        is_root: bool,
        listing: &[(String, FsStat)],
    ) -> Option<Gitignore> {
        let present = |n: &str| listing.iter().any(|(name, st)| name == n && st.is_file);
        let mut b = GitignoreBuilder::new(dir);
        let mut any = false;
        let mut add = |from: PathBuf, lines: Vec<String>| {
            for l in lines {
                // A bad pattern drops that line only.
                let _ = b.add_line(Some(from.clone()), &l);
                any = true;
            }
        };
        if is_root && present(".hgignore") {
            let p = dir.join(".hgignore");
            if let Some(t) = read_ignore(self.probe, &p) {
                add(p, hg_globs(&t));
            }
        }
        for n in IGNORE_FILES.iter().filter(|n| present(n)) {
            let p = dir.join(n);
            if let Some(t) = read_ignore(self.probe, &p) {
                add(p, t.lines().map(str::to_owned).collect());
            }
        }
        if !any {
            return None;
        }
        b.build().ok().filter(|g| !g.is_empty())
    }

    fn elapsed(&self) -> Duration {
        self.probe.now().saturating_sub(self.start)
    }

    /// Abort if a guarded probe call was refused, the walk is cancelled or
    /// the wall budget is spent.
    fn checkpoint(&self) -> Result<(), Abort> {
        match self.probe.check() {
            Ok(()) => Ok(()),
            Err(_) => Err(self.probe.tripped().unwrap_or(Abort::Cancelled)),
        }
    }

    fn timing_checks(&mut self) -> Result<(), Abort> {
        self.checkpoint()?;
        let elapsed = self.elapsed();
        if !self.rate_checked && (self.times.len() >= RATE_DIRS || elapsed >= RATE_WINDOW) {
            self.rate_check()?;
        }
        Ok(())
    }

    /// The one rate check, after the first 50 dirs or 100 ms. Walks that end
    /// sooner, or that timed fewer than 5 listings, are never rate-aborted.
    fn rate_check(&mut self) -> Result<(), Abort> {
        if self.rate_checked {
            return Ok(());
        }
        self.rate_checked = true;
        if self.times.len() < RATE_MIN_DIRS {
            return Ok(());
        }
        match median(&self.times) {
            Some(m) if m > self.opts.rate_ms_per_dir => Err(Abort::Rate),
            _ => Ok(()),
        }
    }

    fn finish(mut self, abort: Option<Abort>) -> WalkOutcome {
        self.out.stats.ms = ms(self.elapsed());
        self.out.stats.ms_per_dir = median(&self.times);
        self.out.md.sort();
        self.out.dataless.sort();
        self.out.nested_roots.sort();
        self.out.abort = abort;
        self.out
    }
}
