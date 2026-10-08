//! The discovery orchestrator: stages 1–4 of docs/specs/roots.md §1 wired
//! together, the stage-3 mode table, and [`explain`].
//!
//! Callers hold the global [`crate::registry::DiscoverLock`] around
//! [`discover`] (spec §5: stages 2–4 run under it); this module does not take
//! it, so tests and single-process tools can run without a lock directory.
//!
//! Every filesystem access goes through the [`Probe`]; `read_dir` happens
//! only inside [`walk`] (directly or via [`find_loose_root`]), after the tree
//! is known to be local and bounded.
//!
//! Tools named here: [git](https://git-scm.com), [EdenFS](https://github.com/facebook/sapling)
//! (the virtual filesystem from the Sapling project), [Sapling](https://sapling-scm.com/).

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use mdroots_core::Cancel;

use crate::gitindex::{MAX_INDEX_BYTES, scan};
use crate::loose::{find_loose_root, is_denied};
use crate::markers::{Climb, FoundMarker, MarkerClass, StopReason, climb, markers_at};
use crate::probe::{FsClass, Probe, classify};
use crate::registry::{
    Registry, RootMode, RootRecord, VerdictSource, detect_move, lookup_valid, new_root_id,
};
use crate::walk::{Abort, Budget, WalkOptions, WalkStats, walk};

/// Index-driven above this many git index entries; budgeted walk below.
const INDEX_DRIVEN_MIN: u32 = 20_000;
/// Lazy above this many git index entries (also the scan cap).
const INDEX_CAP: usize = 200_000;
/// vcs-enumerated budget (spec §3).
const ENUM_BUDGET: Duration = Duration::from_millis(500);
const ENUM_CAP: usize = 20_000;
/// A Rate or Budget verdict blocks re-enumeration for this long.
const RETRY_MS: u64 = 7 * 24 * 60 * 60 * 1000;
/// Names stat'ed per level by the stage-1 probe on a non-local filesystem:
/// explicit and notes-tool markers, plus VCS markers for nested repos.
const NON_LOCAL_PROBE: &[&str] = &[
    ".mdroots",
    ".mdrootsignore",
    ".zk",
    ".obsidian",
    ".marksman.toml",
    ".iwe",
    ".foam",
    ".git",
    ".jj",
    ".hg",
    ".sl",
];

#[derive(Debug, Clone, Default)]
pub struct DiscoverOptions {
    /// LSP `workspaceFolders`, absolute.
    pub workspace_folders: Vec<PathBuf>,
    /// Directories of the buffer's relative link targets, absolute (§2).
    pub link_dirs: Vec<PathBuf>,
    /// Rate-check threshold in ms per `read_dir`; default 5.0.
    pub rate_ms_per_dir: Option<f64>,
    /// Wall-clock time for registry records (ms since the Unix epoch).
    pub now_ms: u64,
}

/// The outcome of [`discover`].
///
/// `md` (root-relative, `/`-separated) is filled when this call listed the
/// root (walk, git index, enumeration). A registry hit lists nothing: `md`
/// and `nested_roots` are empty and `stats` are the recorded ones.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    /// `None` in single-file mode (and for lazy mode on a virtual or remote
    /// filesystem without a root marker reachable without a climb).
    pub root: Option<PathBuf>,
    pub mode: RootMode,
    /// One line; see [`explain`].
    pub reason: String,
    pub stats: Option<WalkStats>,
    pub nested_roots: Vec<PathBuf>,
    pub md: Vec<String>,
    /// Agreeing rate-check measurements behind a Rate verdict (0 otherwise).
    pub rate_confirmations: u32,
}

/// Lists a vcs-enumerated root's markdown files from the VCS (spec §3).
pub trait Enumerator: Send + Sync {
    /// Root-relative md paths, or `None` on error, after `budget`, or with
    /// more than `cap` paths.
    fn md_paths(&self, root: &Path, budget: Duration, cap: usize) -> Option<Vec<String>>;
}

/// Never enumerates: vcs-enumerated mode is off.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoEnumerator;

impl Enumerator for NoEnumerator {
    fn md_paths(&self, _: &Path, _: Duration, _: usize) -> Option<Vec<String>> {
        None
    }
}

/// Runs `sl files 'glob:**/*.md'` ([Sapling](https://sapling-scm.com/)) in
/// the root of an [EdenFS](https://github.com/facebook/sapling) checkout,
/// killing it once the budget is spent.
#[derive(Debug, Clone, Copy, Default)]
pub struct SlFiles;

impl Enumerator for SlFiles {
    fn md_paths(&self, root: &Path, budget: Duration, cap: usize) -> Option<Vec<String>> {
        let deadline = Instant::now() + budget;
        let mut child = Command::new("sl")
            .args(["files", "glob:**/*.md"])
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        };
        // Drain stdout on a thread so a full pipe never blocks the child.
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut out = Vec::new();
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(l) if out.len() < cap => out.push(l),
                    _ => {
                        let _ = tx.send(None);
                        return;
                    }
                }
            }
            let _ = tx.send(Some(out));
        });
        let mut lines: Option<Option<Vec<String>>> = None;
        let mut exited = false;
        loop {
            if lines.is_none()
                && let Ok(l) = rx.try_recv()
            {
                lines = Some(l);
            }
            if matches!(lines, Some(None)) {
                break; // read error or over cap
            }
            if !exited {
                match child.try_wait() {
                    Ok(Some(s)) if s.success() => exited = true,
                    Ok(None) => {}
                    _ => break,
                }
            }
            if exited && lines.is_some() {
                return lines.flatten();
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = child.kill();
        let _ = child.wait();
        None
    }
}

/// A decision plus what registering it needs.
struct Out {
    d: Decision,
    verdict: VerdictSource,
    marker: Option<String>,
    register: bool,
}

impl Out {
    fn new(root: Option<PathBuf>, mode: RootMode, reason: String, verdict: VerdictSource) -> Out {
        Out {
            d: Decision {
                root,
                mode,
                reason,
                stats: None,
                nested_roots: Vec::new(),
                md: Vec::new(),
                rate_confirmations: 0,
            },
            verdict,
            marker: None,
            register: true,
        }
    }

    fn single(reason: String) -> Out {
        let mut o = Out::new(None, RootMode::SingleFile, reason, VerdictSource::Fs);
        o.register = false;
        o
    }

    /// An already-registered decision.
    fn recorded(rec: &RootRecord) -> Out {
        let mut o = Out::new(None, rec.mode, String::new(), rec.verdict_source);
        o.d = from_record(rec);
        o.register = false;
        o
    }

    fn marker(mut self, m: Option<&FoundMarker>) -> Out {
        self.marker = m
            .filter(|m| m.class != MarkerClass::Editor)
            .map(|m| m.name.clone());
        self
    }

    fn stats(mut self, s: WalkStats) -> Out {
        self.d.stats = Some(s);
        self
    }
}

fn from_record(rec: &RootRecord) -> Decision {
    Decision {
        root: Some(rec.path.clone()),
        mode: rec.mode,
        reason: rec.reason.clone(),
        stats: Some(rec.stats),
        nested_roots: Vec::new(),
        md: Vec::new(),
        rate_confirmations: rec.rate_confirmations,
    }
}

/// Everything [`discover`] threads through the stages.
struct Ctx<'a> {
    probe: &'a dyn Probe,
    registry: &'a mut dyn Registry,
    file: &'a Path,
    dir: &'a Path,
    opts: &'a DiscoverOptions,
    cancel: &'a Cancel,
    home: Option<PathBuf>,
}

/// Decide the root of `file` (absolute, canonical) per docs/specs/roots.md
/// §1–§4 and register it (single-file decisions are never registered).
///
/// - Stage 1: a valid registry hit is used if no new marker sits between
///   `dir(file)` and the root (on a non-local filesystem, a marker there
///   becomes a new lazy nested root). A Rate verdict with fewer than two
///   confirmations is a miss.
/// - Stage 2: the marker climb. A virtual/remote start is lazy; an EdenFS
///   checkout is first offered to `enumerator` (vcs-enumerated). The
///   enumeration runs synchronously within its 500 ms budget; running it in
///   the background is left to the index layer.
/// - Stage 3: the mode table (monorepo → lazy; git at a denied location →
///   tracked-only; git index size → lazy / index-driven / walk).
/// - Stage 4: the budgeted walk. A rate abort is final only on the second
///   agreeing measurement; other aborts are lazy (Budget).
/// - No marker: the loose-root search (§2).
pub fn discover(
    probe: &dyn Probe,
    registry: &mut dyn Registry,
    enumerator: &dyn Enumerator,
    file: &Path,
    opts: &DiscoverOptions,
    cancel: &Cancel,
) -> Decision {
    let Some(dir) = file.parent().filter(|d| d.is_absolute()) else {
        return Out::single("single-file: no parent directory".into()).d;
    };
    let mut cx = Ctx {
        probe,
        registry,
        file,
        dir,
        opts,
        cancel,
        home: probe.home(),
    };
    if let Some(out) = stage1(&mut cx) {
        return cx.finish(out);
    }
    let c = climb(probe, dir, &opts.workspace_folders);
    let out = cx.decide(&c, enumerator);
    cx.finish(out)
}

/// One line describing `d`, as `mdroots roots` and `window/logMessage`
/// print it, e.g. `lazy: statfs edenfs:, MNT_LOCAL unset`.
pub fn explain(d: &Decision) -> String {
    d.reason.replace(['\n', '\r'], " ")
}

fn non_local(probe: &dyn Probe, dir: &Path, home: Option<&Path>) -> bool {
    probe.mount(dir).is_ok_and(|m| {
        matches!(
            classify(&m, dir, home),
            FsClass::Virtual(_) | FsClass::Remote(_)
        )
    })
}

/// Stage 1. `None` is a miss.
fn stage1(cx: &mut Ctx<'_>) -> Option<Out> {
    let rec = lookup_valid(cx.registry, cx.probe, cx.file)?;
    if rec.verdict_source == VerdictSource::Rate && rec.rate_confirmations < 2 {
        return None;
    }
    // Budget verdicts are retried at most once per 7 days (spec §1 stage 4).
    if rec.verdict_source == VerdictSource::Budget
        && cx.opts.now_ms.saturating_sub(rec.decided_at_ms) >= RETRY_MS
    {
        return None;
    }
    let remote = non_local(cx.probe, cx.dir, cx.home.as_deref());
    for d in cx.dir.ancestors().take_while(|d| *d != rec.path) {
        if remote {
            let hit = NON_LOCAL_PROBE
                .iter()
                .find(|n| cx.probe.lstat(&d.join(n)).is_ok());
            if let Some(&".mdrootsignore") = hit {
                return Some(Out::single(format!(
                    "single-file: .mdrootsignore at {}",
                    cx.show(d)
                )));
            }
            if let Some(name) = hit {
                let reason = format!(
                    "lazy: new marker {name} at {} inside {} on a non-local filesystem",
                    cx.show(d),
                    cx.show(&rec.path)
                );
                let mut o = Out::new(
                    Some(d.to_path_buf()),
                    RootMode::Lazy,
                    reason,
                    VerdictSource::Fs,
                );
                o.marker = Some((*name).to_owned());
                return Some(o);
            }
        } else if !markers_at(cx.probe, d).is_empty() {
            return None;
        }
    }
    Some(Out::recorded(&rec))
}

fn keep_md(p: &str) -> bool {
    p.rsplit_once('.').is_some_and(|(stem, ext)| {
        !stem.is_empty()
            && !stem.ends_with('/')
            && ["md", "markdown", "org"]
                .iter()
                .any(|e| ext.eq_ignore_ascii_case(e))
    })
}

/// `n` with `,` thousands separators.
fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn abort_name(a: Abort) -> String {
    format!("{a:?}").to_lowercase()
}

/// The git index of the repo at `root`: `.git/index`, or `<gitdir>/index`
/// for a `.git` file (`gitdir: <p>`, relative to `root`).
fn git_index_path(probe: &dyn Probe, root: &Path) -> Option<PathBuf> {
    let dotgit = root.join(".git");
    if probe.stat(&dotgit).ok()?.is_dir {
        return Some(dotgit.join("index"));
    }
    let bytes = probe.read_small(&dotgit, 4096).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let p = text.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim();
    (!p.is_empty()).then(|| root.join(p).join("index"))
}

impl Ctx<'_> {
    /// `dir` for messages: `~/...` under home, else absolute.
    fn show(&self, dir: &Path) -> String {
        match self.home.as_deref().and_then(|h| dir.strip_prefix(h).ok()) {
            Some(rel) if rel.as_os_str().is_empty() => "~".into(),
            Some(rel) => format!("~/{}", rel.display()),
            None => dir.display().to_string(),
        }
    }

    /// Stages 2–4 after a stage-1 miss.
    fn decide(&mut self, c: &Climb, enumerator: &dyn Enumerator) -> Out {
        if c.ignore_here {
            return Out::single(format!(
                "single-file: .mdrootsignore covers {}",
                self.show(self.dir)
            ));
        }
        if c.stop == StopReason::Virtual {
            return self.virtual_start(c, enumerator);
        }
        if c.inside_virtual_mount {
            return match (&c.root, &c.marker) {
                (Some(root), Some(m)) => {
                    let reason = format!(
                        "lazy: marker {} at {} in a local mount inside a virtual repo",
                        m.name,
                        self.show(root)
                    );
                    Out::new(
                        Some(root.clone()),
                        RootMode::Lazy,
                        reason,
                        VerdictSource::Fs,
                    )
                    .marker(Some(m))
                }
                _ => Out::single("single-file: inside a local mount in a virtual repo".into()),
            };
        }
        let (Some(root), Some(m)) = (c.root.clone(), c.marker.clone()) else {
            return self.loose();
        };
        if m.class != MarkerClass::Editor
            && self.registry.lookup(&root).is_none_or(|r| r.path != root)
            && let Some(rec) = detect_move(self.registry, self.probe, &root, &m.name)
        {
            self.registry.update(rec.clone());
            return Out::recorded(&rec);
        }
        self.stage3(&root, &m, c.huge)
    }

    /// A virtual or remote start: lazy, or vcs-enumerated for EdenFS.
    fn virtual_start(&mut self, c: &Climb, enumerator: &dyn Enumerator) -> Out {
        let mount = self.probe.mount(self.dir).ok();
        let why = match &mount {
            Some(m) if !m.local => format!("statfs {}, MNT_LOCAL unset", m.fs_type),
            Some(m) => format!("statfs {}, virtual by name", m.fs_type),
            None => "statfs failed".into(),
        };
        let class = mount
            .as_ref()
            .map(|m| classify(m, self.dir, self.home.as_deref()));
        let Some(vroot) = c.virtual_root.clone() else {
            let mut o = Out::new(
                None,
                RootMode::Lazy,
                format!("lazy: {why}, no root marker without a climb"),
                VerdictSource::Fs,
            );
            o.register = false;
            return o;
        };
        let marker = c.marker.as_ref();
        if !matches!(class, Some(FsClass::Virtual(_))) {
            return Out::new(
                Some(vroot),
                RootMode::Lazy,
                format!("lazy: {why}"),
                VerdictSource::Eden,
            )
            .marker(marker);
        }
        let recent = self.registry.lookup(&vroot).filter(|r| {
            r.path == vroot
                && matches!(
                    r.verdict_source,
                    VerdictSource::Rate | VerdictSource::Budget
                )
                && self.opts.now_ms.saturating_sub(r.decided_at_ms) < RETRY_MS
        });
        if let Some(rec) = recent {
            return Out::recorded(&rec);
        }
        match enumerator.md_paths(&vroot, ENUM_BUDGET, ENUM_CAP) {
            Some(mut md) => {
                md.sort();
                let reason = format!("vcs-enumerated: {} md from the VCS ({why})", md.len());
                let mut o = Out::new(
                    Some(vroot),
                    RootMode::VcsEnumerated,
                    reason,
                    VerdictSource::Eden,
                )
                .marker(marker);
                o.d.md = md;
                o
            }
            None => Out::new(
                Some(vroot),
                RootMode::Lazy,
                format!("lazy: {why}; enumeration over budget"),
                VerdictSource::Budget,
            )
            .marker(marker),
        }
    }

    /// Stage 3 for a marker root.
    fn stage3(&mut self, root: &Path, m: &FoundMarker, huge: bool) -> Out {
        if huge {
            let reason = format!("lazy: monorepo marker at {}", self.show(root));
            return Out::new(
                Some(root.to_path_buf()),
                RootMode::Lazy,
                reason,
                VerdictSource::Fs,
            )
            .marker(Some(m));
        }
        let (mode, label) = match m.class {
            MarkerClass::Vcs => (
                RootMode::Vcs,
                format!("vcs {} at {}", m.name, self.show(root)),
            ),
            MarkerClass::Editor => (
                RootMode::Marker,
                format!("workspace folder {}", self.show(root)),
            ),
            _ => (
                RootMode::Marker,
                format!("marker {} at {}", m.name, self.show(root)),
            ),
        };
        if m.class == MarkerClass::Vcs && m.name == ".git" {
            return self.git(root, m, label);
        }
        if m.class == MarkerClass::Vcs
            && let Some(why) = is_denied(self.probe, root)
        {
            return Out::single(format!(
                "single-file: vcs at {} ({why}): tracked-only needs git",
                self.show(root)
            ));
        }
        self.walk_root(root, Some(m), Budget::marker(), mode, label)
    }

    /// The git rows of the stage-3 table.
    fn git(&mut self, root: &Path, m: &FoundMarker, label: String) -> Out {
        let lazy = |reason: String| {
            Out::new(
                Some(root.to_path_buf()),
                RootMode::Lazy,
                reason,
                VerdictSource::Budget,
            )
            .marker(Some(m))
        };
        let index = git_index_path(self.probe, root).filter(|i| self.probe.stat(i).is_ok());
        if let Some(why) = is_denied(self.probe, root) {
            return self.tracked_only(root, m, index, why);
        }
        let Some(index) = index else {
            let label = format!("{label} (no git index)");
            return self.walk_root(root, Some(m), Budget::marker(), RootMode::Vcs, label);
        };
        let s = match scan(self.probe, &index, &keep_md, INDEX_CAP) {
            Ok(s) => s,
            // gitindex errors are prefixed "git index:"; any other
            // InvalidData is the probe's read cap (the index is too large).
            Err(e)
                if e.kind() == std::io::ErrorKind::InvalidData
                    && !e.to_string().starts_with("git index:") =>
            {
                return lazy(format!(
                    "lazy: git index over {} MiB",
                    MAX_INDEX_BYTES >> 20
                ));
            }
            Err(e) => {
                let label = format!("{label} (git index unreadable: {e})");
                return self.walk_root(root, Some(m), Budget::marker(), RootMode::Vcs, label);
            }
        };
        let n = thousands(u64::from(s.entries));
        if s.split {
            return lazy("lazy: git split index: entry count unknown".into());
        }
        if s.truncated {
            return lazy(format!(
                "lazy: git index {n} entries (> {})",
                thousands(INDEX_CAP as u64)
            ));
        }
        if s.entries >= INDEX_DRIVEN_MIN || s.sparse {
            let sparse = if s.sparse { ", sparse" } else { "" };
            let mut o = Out::new(
                Some(root.to_path_buf()),
                RootMode::IndexDriven,
                format!("index-driven: git index {n} entries{sparse}"),
                VerdictSource::Fs,
            )
            .marker(Some(m));
            o.d.md = self.existing(root, s.paths);
            return o;
        }
        let label = format!("{label} (git index {n} entries)");
        self.walk_root(root, Some(m), Budget::marker(), RootMode::Vcs, label)
    }

    /// A git root at a denied location: the tracked md that exist, no walk.
    fn tracked_only(
        &mut self,
        root: &Path,
        m: &FoundMarker,
        index: Option<PathBuf>,
        why: &str,
    ) -> Out {
        let shown = self.show(root);
        let scanned = index.and_then(|i| scan(self.probe, &i, &keep_md, INDEX_CAP).ok());
        let Some(s) = scanned else {
            return Out::single(format!(
                "single-file: vcs at {shown} ({why}): git index unreadable"
            ));
        };
        let md = self.existing(root, s.paths);
        let reason = format!(
            "tracked-only: vcs at {shown} ({why}): {} tracked md",
            md.len()
        );
        let mut o = Out::new(
            Some(root.to_path_buf()),
            RootMode::TrackedOnly,
            reason,
            VerdictSource::Fs,
        )
        .marker(Some(m));
        o.d.md = md;
        o
    }

    /// The listed paths that exist as files (the index stat data is a
    /// snapshot).
    fn existing(&self, root: &Path, paths: Vec<String>) -> Vec<String> {
        paths
            .into_iter()
            .filter(|p| self.probe.stat(&root.join(p)).is_ok_and(|s| s.is_file))
            .collect()
    }

    /// Stage 4: walk `root` and map the outcome to a decision.
    fn walk_root(
        &mut self,
        root: &Path,
        m: Option<&FoundMarker>,
        budget: Budget,
        mode: RootMode,
        label: String,
    ) -> Out {
        let opts = WalkOptions {
            budget,
            rate_ms_per_dir: self.opts.rate_ms_per_dir.unwrap_or(5.0),
            skip: Vec::new(),
        };
        let o = walk(self.probe, root, &opts, self.cancel);
        let s = o.stats;
        match o.abort {
            None => {
                let reason = format!(
                    "{label}: {} md, {} entries, {} dirs in {:.0} ms",
                    s.md, s.entries, s.dirs, s.ms
                );
                let mut out = Out::new(Some(root.to_path_buf()), mode, reason, VerdictSource::Fs)
                    .marker(m)
                    .stats(s);
                out.d.md = o.md;
                out.d.nested_roots = o.nested_roots;
                out
            }
            Some(a) => self.aborted(root, m, a, s),
        }
    }

    /// A walk of `root` aborted with `a`.
    fn aborted(&mut self, root: &Path, m: Option<&FoundMarker>, a: Abort, s: WalkStats) -> Out {
        let lazy = |reason: String, verdict| {
            Out::new(Some(root.to_path_buf()), RootMode::Lazy, reason, verdict)
                .marker(m)
                .stats(s)
        };
        match a {
            Abort::Rate => {
                let prev = self.registry.lookup(root).filter(|r| {
                    r.path == root
                        && r.verdict_source == VerdictSource::Rate
                        && r.rate_confirmations >= 1
                });
                let ms = s.ms_per_dir.unwrap_or(0.0);
                let (n, reason) = match prev {
                    Some(_) => (2, format!("lazy: rate {ms:.1} ms/dir, 2 of 2 measurements")),
                    None => (
                        1,
                        format!("lazy pending: rate {ms:.1} ms/dir, 1 of 2 measurements"),
                    ),
                };
                let mut o = lazy(reason, VerdictSource::Rate);
                o.d.rate_confirmations = n;
                o
            }
            Abort::Cancelled => {
                let mut o = lazy("lazy: discovery cancelled".into(), VerdictSource::Budget);
                o.register = false;
                o
            }
            a => lazy(
                format!(
                    "lazy: walk over budget ({}) at {}: {} entries, {} md",
                    abort_name(a),
                    self.show(root),
                    s.entries,
                    s.md
                ),
                VerdictSource::Budget,
            ),
        }
    }

    /// No marker: the loose-root search, then a walk of the accepted root
    /// for its md list.
    fn loose(&mut self) -> Out {
        let lo = find_loose_root(self.probe, self.file, &self.opts.link_dirs, self.cancel);
        let Some(root) = lo.root else {
            return Out::single(lo.reason);
        };
        if let Some(a) = lo.abort {
            return self.aborted(&root, None, a, lo.stats);
        }
        let mut o = self.walk_root(&root, None, Budget::loose(), RootMode::Loose, String::new());
        if o.d.mode == RootMode::Loose {
            o.d.reason = lo.reason;
        }
        o
    }

    /// Register `out` (unless single-file or unregistrable) and return its
    /// decision. A row at the same path is replaced keeping its `root_id`.
    /// On an overlap, the nearest registered root containing the file wins;
    /// with none, the decision is returned unregistered.
    fn finish(&mut self, out: Out) -> Decision {
        let mut d = out.d;
        let Some(root) = d.root.clone() else {
            return d;
        };
        if !out.register || d.mode == RootMode::SingleFile {
            return d;
        }
        let now = self.opts.now_ms;
        let marker_ino = out
            .marker
            .as_ref()
            .and_then(|m| self.probe.lstat(&root.join(m)).ok())
            .map(|s| s.ino);
        let mut rec = RootRecord {
            root_id: new_root_id(&root, now),
            path: root.clone(),
            mode: d.mode,
            marker: out.marker,
            marker_ino,
            volume_id: self.probe.volume_id(&root).unwrap_or_default(),
            dev: self.probe.stat(&root).map(|s| s.dev).unwrap_or_default(),
            fs_type: self
                .probe
                .mount(&root)
                .map(|m| m.fs_type)
                .unwrap_or_default(),
            stats: d.stats.unwrap_or_default(),
            verdict_source: out.verdict,
            rate_confirmations: d.rate_confirmations,
            decided_at_ms: now,
            reason: d.reason.clone(),
            last_seen_ms: now,
        };
        if let Some(e) = self.registry.lookup(&root).filter(|e| e.path == root) {
            rec.root_id = e.root_id;
            self.registry.update(rec);
            return d;
        }
        match self.registry.insert(rec) {
            Ok(()) => d,
            Err(o) => match self.registry.lookup(self.file) {
                Some(r) => from_record(&r),
                None => {
                    d.reason = format!(
                        "{} (not registered: overlaps {})",
                        d.reason,
                        self.show(&o.existing)
                    );
                    d
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_groups() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(48_213), "48,213");
        assert_eq!(thousands(1_200_000), "1,200,000");
    }

    #[test]
    fn keep_md_matches_note_extensions() {
        assert!(keep_md("a.md") && keep_md("x/b.Markdown") && keep_md("t.org"));
        assert!(!keep_md(".md") && !keep_md("x/.md") && !keep_md("a.txt"));
    }
}
