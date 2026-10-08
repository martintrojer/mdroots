//! Stage 2 of discovery: `statfs`, then the marker climb (docs/specs/roots.md
//! §1 stage 2, stage 3 item 2).
//!
//! Only `stat`/`lstat`, `statfs`, `readlink` and small reads of
//! `.mdrootsignore` files go through the [`Probe`]; this module never calls
//! `read_dir`.
//!
//! Tools whose markers are recognised: [zk](https://github.com/zk-org/zk),
//! [Obsidian](https://obsidian.md), [marksman](https://github.com/artempyanykh/marksman),
//! [git](https://git-scm.com), [jj](https://jj-vcs.github.io/jj/),
//! [Mercurial](https://www.mercurial-scm.org), [Sapling](https://sapling-scm.com/),
//! [EdenFS](https://github.com/facebook/sapling) (the virtual filesystem from the
//! Sapling project), [Buck2](https://buck2.build), [Bazel](https://bazel.build),
//! [iwe](https://github.com/iwe-org/iwe), [Foam](https://foambubble.github.io/foam/),
//! [MkDocs](https://www.mkdocs.org), [mdBook](https://rust-lang.github.io/mdBook/),
//! [Docusaurus](https://docusaurus.io), [Jekyll](https://jekyllrb.com),
//! [Hugo](https://gohugo.io) and [Sphinx](https://www.sphinx-doc.org).

use std::path::{Path, PathBuf};

use crate::probe::{FsClass, Probe, classify};

/// What kind of marker made a directory a root.
///
/// The derived order is not the priority order; see [`climb`].
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MarkerClass {
    /// `.mdroots`.
    Explicit,
    /// A notes tool's config: `.zk/`, `.obsidian/`, `.marksman.toml`, ...
    NotesTool,
    /// A docs generator's config: `mkdocs.yml`, `book.toml`, ...
    DocsTool,
    /// A VCS root: `.git` (dir or file), `.jj`, `.hg`, `.sl`.
    Vcs,
    /// A monorepo marker; the tree is huge.
    Monorepo,
    /// An editor workspace folder equal to the directory.
    Editor,
}

/// One marker found in `dir`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundMarker {
    pub dir: PathBuf,
    pub name: String,
    pub class: MarkerClass,
}

/// Why the climb stopped.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// A root marker, or a `.mdrootsignore` (then `root` is `None`).
    Marker,
    /// `st_dev` changed between a directory and its parent.
    MountBoundary,
    /// Reached `$HOME`, which is checked but never climbed past.
    Home,
    /// Reached `/`.
    FsRoot,
    /// The start directory is on a virtual or remote filesystem: no climb.
    Virtual,
}

/// The result of [`climb`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Climb {
    pub root: Option<PathBuf>,
    pub marker: Option<FoundMarker>,
    /// A monorepo marker sits at the root, or the root is a virtual checkout.
    pub huge: bool,
    pub stop: StopReason,
    /// A visited `.mdrootsignore` is empty or matches the start directory.
    pub ignore_here: bool,
    /// The start directory is on a local mount whose parent mount is
    /// virtual or remote, or has `.eden`.
    pub inside_virtual_mount: bool,
    /// `readlink(<start>/.eden/root)` when the start is virtual.
    pub virtual_root: Option<PathBuf>,
}

#[derive(Clone, Copy)]
enum Want {
    File,
    Dir,
    FileOrDir,
}

/// The marker table. Every entry costs one `lstat` and is typed, so a
/// `workspace/` directory does not match `WORKSPACE` on a case-insensitive
/// volume. `conf.py` counts only together with an `index.md` file.
///
/// Tools: [zk](https://github.com/zk-org/zk), [Obsidian](https://obsidian.md),
/// [marksman](https://github.com/artempyanykh/marksman), [git](https://git-scm.com),
/// [jj](https://jj-vcs.github.io/jj/), [Mercurial](https://www.mercurial-scm.org),
/// [Sapling](https://sapling-scm.com/), [EdenFS](https://github.com/facebook/sapling)
/// (the virtual filesystem from the Sapling project), [Buck2](https://buck2.build),
/// [Bazel](https://bazel.build).
const MARKERS: &[(&str, Want, MarkerClass)] = &[
    (".mdroots", Want::File, MarkerClass::Explicit),
    (".zk", Want::Dir, MarkerClass::NotesTool),
    (".obsidian", Want::Dir, MarkerClass::NotesTool),
    (".marksman.toml", Want::File, MarkerClass::NotesTool),
    (".iwe", Want::Dir, MarkerClass::NotesTool),
    (".foam", Want::Dir, MarkerClass::NotesTool),
    ("mkdocs.yml", Want::File, MarkerClass::DocsTool),
    ("book.toml", Want::File, MarkerClass::DocsTool),
    ("docusaurus.config.js", Want::File, MarkerClass::DocsTool),
    ("docusaurus.config.ts", Want::File, MarkerClass::DocsTool),
    ("docusaurus.config.mjs", Want::File, MarkerClass::DocsTool),
    ("docusaurus.config.cjs", Want::File, MarkerClass::DocsTool),
    ("_config.yml", Want::File, MarkerClass::DocsTool),
    ("hugo.toml", Want::File, MarkerClass::DocsTool),
    ("conf.py", Want::File, MarkerClass::DocsTool),
    (".git", Want::FileOrDir, MarkerClass::Vcs),
    (".jj", Want::Dir, MarkerClass::Vcs),
    (".hg", Want::Dir, MarkerClass::Vcs),
    (".sl", Want::Dir, MarkerClass::Vcs),
    (".eden", Want::Dir, MarkerClass::Monorepo),
    (".buckconfig", Want::File, MarkerClass::Monorepo),
    ("WORKSPACE", Want::File, MarkerClass::Monorepo),
    ("MODULE.bazel", Want::File, MarkerClass::Monorepo),
];

const IGNORE_FILE: &str = ".mdrootsignore";
const IGNORE_CAP: usize = 256 * 1024;
/// `FoundMarker::name` for an editor workspace folder.
const EDITOR_NAME: &str = "workspaceFolders";

/// Priority within one directory, lower wins. Not the derived `Ord`.
fn rank(c: MarkerClass) -> u8 {
    match c {
        MarkerClass::Explicit => 0,
        MarkerClass::NotesTool => 1,
        MarkerClass::DocsTool => 2,
        MarkerClass::Editor => 3,
        MarkerClass::Vcs => 4,
        MarkerClass::Monorepo => 5,
    }
}

fn has(probe: &dyn Probe, p: &Path, want: Want) -> bool {
    probe.lstat(p).is_ok_and(|s| match want {
        Want::File => s.is_file,
        Want::Dir => s.is_dir,
        Want::FileOrDir => s.is_file || s.is_dir,
    })
}

/// The markers in `dir`, highest priority first (Explicit > NotesTool >
/// DocsTool > Vcs > Monorepo). Never includes `.mdrootsignore` (an ignore
/// file, not a marker) or editor folders. At most 24 `lstat`s.
pub fn markers_at(probe: &dyn Probe, dir: &Path) -> Vec<FoundMarker> {
    let mut out: Vec<FoundMarker> = MARKERS
        .iter()
        .filter(|(name, want, _)| has(probe, &dir.join(name), *want))
        .filter(|(name, _, _)| *name != "conf.py" || has(probe, &dir.join("index.md"), Want::File))
        .map(|(name, _, class)| FoundMarker {
            dir: dir.to_path_buf(),
            name: (*name).to_owned(),
            class: *class,
        })
        .collect();
    out.sort_by_key(|m| rank(m.class));
    out
}

/// Whether `<dir>/.mdrootsignore` exists, and if so whether it ignores
/// `start_dir`. An empty file ignores everything below `dir`; an unreadable
/// one (too large, I/O error, bad pattern) is treated as ignoring too.
fn ignore_file(probe: &dyn Probe, dir: &Path, start_dir: &Path) -> Option<bool> {
    let path = dir.join(IGNORE_FILE);
    if !has(probe, &path, Want::File) {
        return None;
    }
    let Ok(bytes) = probe.read_small(&path, IGNORE_CAP) else {
        return Some(true);
    };
    let text = String::from_utf8_lossy(&bytes);
    if text.trim().is_empty() {
        return Some(true);
    }
    let Ok(rel) = start_dir.strip_prefix(dir) else {
        return Some(false);
    };
    if rel.as_os_str().is_empty() {
        // Patterns are rooted at `dir` and never match `dir` itself.
        return Some(false);
    }
    let mut b = ignore::gitignore::GitignoreBuilder::new(dir);
    for line in text.lines() {
        if b.add_line(Some(path.clone()), line).is_err() {
            return Some(true);
        }
    }
    let Ok(gi) = b.build() else {
        return Some(true);
    };
    Some(gi.matched_path_or_any_parents(rel, true).is_ignore())
}

/// Whether the mount holding `parent` is virtual or remote, or `parent` has
/// `.eden`: a local mount below it is inside a virtual repo.
fn parent_is_virtual(probe: &dyn Probe, parent: &Path, home: Option<&Path>) -> bool {
    let non_local = probe.mount(parent).is_ok_and(|m| {
        matches!(
            classify(&m, parent, home),
            FsClass::Virtual(_) | FsClass::Remote(_)
        )
    });
    non_local || has(probe, &parent.join(".eden"), Want::Dir)
}

/// Climb from `dir` (whose `st_dev` is `dev`) with one parent `stat` per
/// level until `st_dev` changes, `$HOME` or `/`. True if the change lands in
/// a virtual parent.
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
            Ok(_) => return parent_is_virtual(probe, parent, home),
            Err(_) => return false,
        }
    }
}

/// Find the root for files in `start_dir` (absolute, canonical).
///
/// 1. `statfs(start_dir)` first. Virtual or remote: no climb; the root is
///    `readlink(<start_dir>/.eden/root)` if that resolves (huge), else none.
/// 2. Otherwise climb from `start_dir` and stop at the nearest directory with
///    a marker, which is the root. Within one directory the reported marker
///    is the highest of Explicit > NotesTool > DocsTool > Editor (a
///    `workspace_folders` entry equal to the directory) > Vcs > Monorepo;
///    any Monorepo marker there sets `huge`. After a marker stop, parent
///    `stat`s continue up to the next `st_dev` change to detect a local
///    mount inside a virtual repo.
/// 3. The climb also stops at a `.mdrootsignore` (no root), at an `st_dev`
///    change (`MountBoundary`), at `$HOME` (checked, never passed) and at `/`.
pub fn climb(probe: &dyn Probe, start_dir: &Path, workspace_folders: &[PathBuf]) -> Climb {
    let home = probe.home();
    let home = home.as_deref();
    let mut out = Climb {
        root: None,
        marker: None,
        huge: false,
        stop: StopReason::FsRoot,
        ignore_here: false,
        inside_virtual_mount: false,
        virtual_root: None,
    };

    let mount = probe.mount(start_dir).ok();
    let class = mount.as_ref().map(|m| classify(m, start_dir, home));
    if let Some(FsClass::Virtual(_) | FsClass::Remote(_)) = class {
        out.stop = StopReason::Virtual;
        let eden = start_dir.join(".eden");
        if let Ok(target) = probe.read_link(&eden.join("root")) {
            let target = eden.join(target); // no-op when absolute
            out.marker = Some(FoundMarker {
                dir: target.clone(),
                name: ".eden".into(),
                class: MarkerClass::Monorepo,
            });
            out.root = Some(target.clone());
            out.virtual_root = Some(target);
            out.huge = true;
        }
        return out;
    }

    let dev = match mount {
        Some(m) => m.dev,
        None => match probe.stat(start_dir) {
            Ok(s) => s.dev,
            Err(_) => return out,
        },
    };
    let mut dir = start_dir;
    loop {
        let ignore = ignore_file(probe, dir, start_dir);
        out.ignore_here |= ignore == Some(true);

        let mut found = markers_at(probe, dir);
        if workspace_folders.iter().any(|w| w == dir) {
            found.push(FoundMarker {
                dir: dir.to_path_buf(),
                name: EDITOR_NAME.into(),
                class: MarkerClass::Editor,
            });
            found.sort_by_key(|m| rank(m.class));
        }
        if let Some(best) = found.first() {
            out.huge = found.iter().any(|m| m.class == MarkerClass::Monorepo);
            out.root = Some(dir.to_path_buf());
            out.marker = Some(best.clone());
            out.stop = StopReason::Marker;
            out.inside_virtual_mount = below_virtual_mount(probe, dir, dev, home);
            return out;
        }
        if ignore.is_some() {
            out.stop = StopReason::Marker;
            return out;
        }
        if Some(dir) == home {
            out.stop = StopReason::Home;
            return out;
        }
        let Some(parent) = dir.parent() else {
            out.stop = StopReason::FsRoot;
            return out;
        };
        match probe.stat(parent) {
            Ok(s) if s.dev == dev => dir = parent,
            Ok(_) => {
                out.stop = StopReason::MountBoundary;
                out.inside_virtual_mount = parent_is_virtual(probe, parent, home);
                return out;
            }
            Err(_) => {
                out.stop = StopReason::FsRoot;
                return out;
            }
        }
    }
}
