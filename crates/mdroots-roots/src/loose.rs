//! Loose roots: the denylist and the upward climb for a file with no marker
//! or VCS above it (docs/specs/roots.md §2).
//!
//! The start directory (the file's directory, raised to cover every existing
//! link target directory) is accepted unless denied. Inside `$HOME` the climb
//! then grows one parent at a time, walking only what lies outside the child
//! and reusing the child's counts, and stops at the first parent that is
//! denied, overruns the loose budget or fails both acceptance rules.

use mdroots_core::Cancel;
use std::path::{Component, Path, PathBuf};

use crate::probe::{FsClass, Probe, classify};
use crate::walk::{Abort, Budget, WalkOptions, WalkStats, walk};

/// Rule (a): at least this many notes in the subtree...
const MIN_MD: usize = 20;
/// ...and at least this share of its files are notes.
const MIN_DENSITY: f64 = 0.30;

/// The result of [`find_loose_root`].
#[derive(Debug, Clone, PartialEq)]
pub struct LooseOutcome {
    /// The loose root; `None` means single-file mode.
    pub root: Option<PathBuf>,
    /// The last decision, one line (see [`find_loose_root`]).
    pub reason: String,
    /// Cumulative walk counts of the root (default in single-file mode).
    pub stats: WalkStats,
    /// Set when the start directory's own walk aborted; the climb then
    /// stops at the start directory with partial `stats`.
    pub abort: Option<Abort>,
}

const OBSIDIAN_DOCS: &str = "Library/Mobile Documents/iCloud~md~obsidian/Documents";

/// Why `dir` may not be a loose root, or `None` if it may.
///
/// Paths are compared literally (no symlink resolution): exact matches for
/// `/`, `$HOME`, `/tmp`, `/private`, `/private/tmp`, `/var`, `/private/var`,
/// a `/Volumes/<name>` mount root, `~/Downloads` and `~/Desktop`; anything at
/// or under `~/Library` except inside an Obsidian vault in iCloud. Also
/// denied: a virtual or remote filesystem, and a local mount whose parent
/// mount is virtual or remote (or has `.eden`). Temp dirs below `/var`, such
/// as `/var/folders/...`, are not denied.
pub fn is_denied(probe: &dyn Probe, dir: &Path) -> Option<&'static str> {
    const EXACT: &[(&str, &str)] = &[
        ("/", "filesystem root"),
        ("/tmp", "/tmp"),
        ("/private", "/private"),
        ("/private/tmp", "/private/tmp"),
        ("/var", "/var"),
        ("/private/var", "/private/var"),
    ];
    if let Some((_, why)) = EXACT.iter().find(|(p, _)| dir == Path::new(p)) {
        return Some(why);
    }
    if dir.starts_with("/Volumes") && dir.components().count() == 3 {
        return Some("volume root");
    }
    let home = probe.home();
    if let Some(home) = home.as_deref() {
        if dir == home {
            return Some("home directory");
        }
        if dir == home.join("Downloads") {
            return Some("Downloads");
        }
        if dir == home.join("Desktop") {
            return Some("Desktop");
        }
        let docs = home.join(OBSIDIAN_DOCS);
        let in_vault =
            dir.starts_with(&docs) && dir.components().count() > docs.components().count();
        if dir.starts_with(home.join("Library")) && !in_vault {
            return Some("under ~/Library");
        }
    }
    let Ok(mount) = probe.mount(dir) else {
        return Some("mount unknown");
    };
    match classify(&mount, dir, home.as_deref()) {
        FsClass::Virtual(_) => return Some("virtual filesystem"),
        FsClass::Remote(_) => return Some("remote filesystem"),
        _ => {}
    }
    if below_virtual_mount(probe, dir, mount.dev, home.as_deref()) {
        return Some("local mount inside a virtual repo");
    }
    None
}

/// Climb from `dir` (on `dev`) with one parent `stat` per level until
/// `st_dev` changes, `$HOME` or `/`. True if the change lands on a virtual
/// or remote parent, or one with `.eden`.
fn below_virtual_mount(probe: &dyn Probe, mut dir: &Path, dev: u64, home: Option<&Path>) -> bool {
    loop {
        if Some(dir) == home {
            return false;
        }
        let Some(parent) = dir.parent() else {
            return false;
        };
        match probe.stat(parent) {
            Ok(s) if s.dev == dev => dir = parent,
            Ok(_) => {
                let non_local = probe.mount(parent).is_ok_and(|m| {
                    matches!(
                        classify(&m, parent, home),
                        FsClass::Virtual(_) | FsClass::Remote(_)
                    )
                });
                return non_local || probe.lstat(&parent.join(".eden")).is_ok_and(|s| s.is_dir);
            }
            Err(_) => return false,
        }
    }
}

/// The deepest common ancestor of `a` and `b`, component-wise.
fn common_ancestor(a: &Path, b: &Path) -> PathBuf {
    a.components()
        .zip(b.components())
        .take_while(|(x, y)| x == y)
        .map(|(x, _)| x)
        .collect()
}

/// `dir` for messages: `~/...` under home, else absolute.
fn show(dir: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| dir.strip_prefix(h).ok()) {
        Some(rel) if rel.as_os_str().is_empty() => "~".into(),
        Some(rel) => format!("~/{}", rel.display()),
        None => dir.display().to_string(),
    }
}

fn abort_name(a: Abort) -> String {
    format!("{a:?}").to_lowercase()
}

/// `a` plus `b`; `ms_per_dir` is the dir-weighted mean of both medians.
fn add(a: &WalkStats, b: &WalkStats) -> WalkStats {
    let ms_per_dir = match (a.ms_per_dir, b.ms_per_dir) {
        (Some(x), Some(y)) => {
            let (wa, wb) = (a.dirs.max(1) as f64, b.dirs.max(1) as f64);
            Some((x * wa + y * wb) / (wa + wb))
        }
        (x, y) => x.or(y),
    };
    WalkStats {
        entries: a.entries + b.entries,
        md: a.md + b.md,
        files: a.files + b.files,
        dirs: a.dirs + b.dirs,
        ms: a.ms + b.ms,
        ms_per_dir,
    }
}

fn rule_a(s: &WalkStats) -> bool {
    s.md >= MIN_MD && s.files > 0 && s.md as f64 / s.files as f64 >= MIN_DENSITY
}

/// The loose root for `file` (absolute) per docs/specs/roots.md §2.
///
/// `link_dirs` are the directories of the buffer's relative link targets,
/// absolute; relative or missing ones are ignored. The start directory is
/// the deepest common ancestor of `dir(file)` and the existing link dirs.
///
/// - Start denied: `root: None`, reason `single-file: <why>`, no `read_dir`.
/// - Otherwise the start is walked with [`Budget::loose`] and accepted. If
///   that walk aborts, `abort` is set and there is no growth.
/// - A start outside `$HOME` (or with no `$HOME`) never grows.
/// - Each parent below `$HOME` is walked with the child skipped, within the
///   loose budget left after the child's counts (wall time counted from the
///   start of this call), and accepted iff (a) the cumulative subtree has at
///   least 20 notes and a note density of at least 30%, or (b) it adds more
///   notes outside the child than the child holds. The climb stops at the
///   first denied or rejected parent; an aborted parent walk rejects it.
///
/// `reason` is the last decision: the rejection line, `stopped at <dir>:
/// denied (<why>)`, `loose root accepted at <top>: <md> md / <files> files`,
/// `loose root at <dir>: outside home, no growth` or `loose root at <dir>:
/// walk aborted (<abort>), no growth`.
pub fn find_loose_root(
    probe: &dyn Probe,
    file: &Path,
    link_dirs: &[PathBuf],
    cancel: &Cancel,
) -> LooseOutcome {
    let home = probe.home();
    let home = home.as_deref();
    let single = |why: &str| LooseOutcome {
        root: None,
        reason: format!("single-file: {why}"),
        stats: WalkStats::default(),
        abort: None,
    };
    let Some(dir) = file.parent().filter(|d| d.is_absolute()) else {
        return single("no parent directory");
    };
    let mut start = dir.to_path_buf();
    for l in link_dirs {
        let lexical = !l.components().any(|c| c == Component::ParentDir);
        if l.is_absolute() && lexical && probe.stat(l).is_ok_and(|s| s.is_dir) {
            start = common_ancestor(&start, l);
        }
    }
    if let Some(why) = is_denied(probe, &start) {
        return single(why);
    }

    let t0 = probe.now();
    let loose = Budget::loose();
    let first = walk(
        probe,
        &start,
        &WalkOptions {
            budget: loose,
            ..Default::default()
        },
        cancel,
    );
    let mut stats = first.stats;
    let at = |dir: &Path, rest: &str| format!("loose root at {}: {rest}", show(dir, home));
    if let Some(a) = first.abort {
        return LooseOutcome {
            reason: at(
                &start,
                &format!("walk aborted ({}), no growth", abort_name(a)),
            ),
            root: Some(start),
            stats,
            abort: Some(a),
        };
    }
    let Some(home) = home.filter(|h| start.starts_with(h)) else {
        return LooseOutcome {
            reason: at(&start, "outside home, no growth"),
            root: Some(start),
            stats,
            abort: None,
        };
    };

    let accepted = |dir: &Path, s: &WalkStats| {
        format!(
            "loose root accepted at {}: {} md / {} files",
            show(dir, Some(home)),
            s.md,
            s.files
        )
    };
    let mut child = start;
    let mut reason = accepted(&child, &stats);
    while let Some(parent) = child.parent().filter(|p| *p != home) {
        let parent = parent.to_path_buf();
        let shown = show(&parent, Some(home));
        if let Some(why) = is_denied(probe, &parent) {
            reason = format!("stopped at {shown}: denied ({why})");
            break;
        }
        let budget = Budget {
            entries: loose.entries.saturating_sub(stats.entries),
            md: loose.md.saturating_sub(stats.md),
            wall: loose.wall.saturating_sub(probe.now().saturating_sub(t0)),
            depth: loose.depth,
        };
        let out = walk(
            probe,
            &parent,
            &WalkOptions {
                budget,
                skip: vec![child.clone()],
                ..Default::default()
            },
            cancel,
        );
        if let Some(a) = out.abort {
            reason = format!(
                "loose root rejected at {shown}: walk aborted ({})",
                abort_name(a)
            );
            break;
        }
        let added = out.stats;
        let total = add(&stats, &added);
        if rule_a(&total) || added.md > stats.md {
            stats = total;
            child = parent;
            reason = accepted(&child, &stats);
            continue;
        }
        let a = match total.md < MIN_MD {
            true => format!("{} md (< {MIN_MD})", total.md),
            false => format!(
                "{} md / {} files (< {:.0}%)",
                total.md,
                total.files,
                MIN_DENSITY * 100.0
            ),
        };
        reason = format!(
            "loose root rejected at {shown}: {a}, adds {} md (<= {} in child)",
            added.md, stats.md
        );
        break;
    }
    LooseOutcome {
        root: Some(child),
        reason,
        stats,
        abort: None,
    }
}
