//! The M1 markdown walk (docs/specs/roots.md Stage 4, minus budgets and ignore
//! files): sequential, sorted, no symlink following.

use std::path::Path;

use crate::cancel::Cancel;
use crate::error::Error;
use crate::fs::FileSystem;

/// Directory names never descended: dependency and build-output dirs of
/// common toolchains, e.g. `buck-out` from [Buck2](https://buck2.build) and
/// `bazel-out` from [Bazel](https://bazel.build). The dot dirs listed are
/// skipped as hidden anyway by every walk.
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

/// Whether a walk never descends a directory named `name` (the prune
/// list; hidden names are skipped separately).
pub fn pruned_dir(name: &str) -> bool {
    PRUNE_DIRS.contains(&name)
}

/// Root-relative paths of `*.md`, `*.markdown` and `*.org` files under
/// `root`, sorted. Skips hidden files and dirs (leading `.`, which covers
/// `.git`, `.jj`, `.hg`, `.sl`, `.venv`), editor temp files (`*~`, `.#*`,
/// `#*#`, `.*.swp`, `.m-reflow-*`), the dirs in the prune list, and entries
/// that are neither regular files nor dirs (symlinks are not followed), and
/// dataless entries (cloud placeholders: such dirs are not descended, such
/// files not listed, since touching them would download them).
/// `.gitignore` is not honoured in M1 (deferred to mdroots-roots).
/// Checks `cancel` before each directory; unreadable subdirectories are skipped,
/// an unreadable root is an error.
pub fn walk_md(fs: &dyn FileSystem, root: &Path, cancel: &Cancel) -> Result<Vec<String>, Error> {
    let mut out = Vec::new();
    let mut stack: Vec<String> = vec![String::new()];
    while let Some(rel) = stack.pop() {
        cancel.check()?;
        let entries = match fs.read_dir(&root.join(&rel)) {
            Ok(e) => e,
            Err(e) if rel.is_empty() => return Err(e.into()),
            Err(_) => continue,
        };
        for (name, meta) in entries {
            if meta.dataless || is_hidden_or_temp(&name) {
                continue;
            }
            let path = match rel.is_empty() {
                true => name.clone(),
                false => format!("{rel}/{name}"),
            };
            if meta.is_dir {
                if !pruned_dir(&name) {
                    stack.push(path);
                }
            } else if meta.is_file && is_note(&name) {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Hidden (leading `.`) or an editor temp name (`*~`, `#*#`).
pub fn is_hidden_or_temp(name: &str) -> bool {
    name.starts_with('.')
        || name.ends_with('~')
        || (name.len() > 1 && name.starts_with('#') && name.ends_with('#'))
}

/// Whether the file name `name` is a note: a non-empty stem and a `md`,
/// `markdown` or `org` extension (any case).
pub fn is_note(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(stem, ext)| {
        !stem.is_empty()
            && ["md", "markdown", "org"]
                .iter()
                .any(|e| ext.eq_ignore_ascii_case(e))
    })
}
