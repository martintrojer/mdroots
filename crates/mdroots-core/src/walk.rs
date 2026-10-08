//! The M1 markdown walk (docs/specs/roots.md Stage 4, minus budgets and ignore
//! files): sequential, sorted, no symlink following.

use std::path::Path;

use crate::cancel::Cancel;
use crate::error::Error;
use crate::fs::FileSystem;

/// Directories never descended (besides every dot dir): dependency and
/// build-output dirs of common toolchains, e.g. `buck-out` from
/// [Buck2](https://buck2.build) and `bazel-out` from [Bazel](https://bazel.build).
const PRUNED: &[&str] = &[
    "node_modules",
    "target",
    "buck-out",
    "bazel-out",
    "__pycache__",
    "dist",
    "build",
    "venv",
    "Pods",
    "DerivedData",
    "vendor",
];

/// Root-relative paths of `*.md`, `*.markdown` and `*.org` files under
/// `root`, sorted. Skips hidden files and dirs (leading `.`, which covers
/// `.git`, `.jj`, `.hg`, `.sl`, `.venv`), editor temp files (`*~`, `.#*`,
/// `#*#`, `.*.swp`, `.m-reflow-*`), the dirs in the prune list, and entries
/// that are neither regular files nor dirs (symlinks are not followed).
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
            if is_hidden_or_temp(&name) {
                continue;
            }
            let path = match rel.is_empty() {
                true => name.clone(),
                false => format!("{rel}/{name}"),
            };
            if meta.is_dir {
                if !PRUNED.contains(&name.as_str()) {
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

fn is_hidden_or_temp(name: &str) -> bool {
    name.starts_with('.')
        || name.ends_with('~')
        || (name.len() > 1 && name.starts_with('#') && name.ends_with('#'))
}

fn is_note(name: &str) -> bool {
    let Some((stem, ext)) = name.rsplit_once('.') else {
        return false;
    };
    !stem.is_empty()
        && ["md", "markdown", "org"]
            .iter()
            .any(|e| ext.eq_ignore_ascii_case(e))
}
