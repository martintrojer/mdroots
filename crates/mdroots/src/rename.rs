//! [`Workspace::rename_note`]: the text edits that keep links working when
//! a note moves. Never writes files.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};

use mdroots_core::memstore::counts;
use mdroots_core::{Cancel, Error, ErrorKind};
use mdroots_resolve::ResolveStep;
use mdroots_resolve::ladder::LinkStatus;
use mdroots_resolve::normalize::percent_decode;
use mdroots_syntax::{Document, Link, LinkKind};

use crate::links::{encode_destination, relative_path};
use crate::workspace::{Workspace, is_note};

/// File changes for an editor to apply: create, then edit, then rename.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Default)]
pub struct WorkspaceEdit {
    /// Per file (absolute, sorted), edits sorted by range, not overlapping.
    pub edits: Vec<(PathBuf, Vec<TextEdit>)>,
    /// The file rename to perform after the edits: `(old, new)`.
    pub rename: Option<(PathBuf, PathBuf)>,
    /// Files to create (absolute, not existing) with their content, before
    /// the edits.
    pub create: Vec<(PathBuf, String)>,
}

/// Replace the bytes `range` of the file's current text with `new_text`.
#[derive(Debug, Clone, PartialEq)]
pub struct TextEdit {
    pub range: Range<usize>,
    pub new_text: String,
}

impl Workspace {
    /// The edits that rename the indexed note `old` to `new` (inside the
    /// root, a note extension, not on disk and not indexed). Links whose
    /// best target is `old` are rewritten in the style they were written
    /// in (file-relative, root-relative, site-rooted or by stem); links
    /// found by id, title, alias, a dialect transform or a partial match
    /// are left alone, as are links in code. When `old` moves to another
    /// directory its own file-relative links are rewritten too. Anchors are
    /// kept. Checks `cancel` per note.
    pub fn rename_note(
        &self,
        old: &Path,
        new: &Path,
        cancel: &Cancel,
    ) -> Result<WorkspaceEdit, Error> {
        cancel.check()?;
        let old_rel = self.rel(old)?;
        let new_rel = self.rel(new)?;
        let store = self.store();
        if store.document(&old_rel).is_none() {
            return Err(refuse(format!("{old_rel}: not an indexed note")));
        }
        let new_name = new_rel.rsplit('/').next().unwrap_or_default();
        if !is_note(new_name) {
            return Err(refuse(format!("{new_rel}: not a note extension")));
        }
        let new_abs = self.abs(&new_rel);
        if store.document(&new_rel).is_some() || self.fs().stat(&new_abs).is_ok() {
            return Err(refuse(format!("{new_rel}: already exists")));
        }
        let mut out = self.rename_notes(&[(old.to_owned(), new.to_owned())], cancel)?;
        out.rename = Some((self.abs(&old_rel), new_abs));
        Ok(out)
    }

    /// The link edits for several moves planned together, so notes that
    /// move together keep correct links. Each pair is a note or a folder:
    /// a note moves to `new`; a folder maps every indexed note under it to
    /// the same sub-path under `new`. A pair whose old path is neither an
    /// indexed note nor a folder holding one, or whose new path is outside
    /// the root or not a note extension (for a note), is skipped. A note
    /// moved by two pairs is [`ErrorKind::Unsupported`]. Links are
    /// rewritten as for [`rename_note`](Self::rename_note), against the
    /// layout after all the moves. Edits only: `rename` is `None`, `create`
    /// is empty. Checks `cancel` per note.
    pub fn rename_notes(
        &self,
        moves: &[(PathBuf, PathBuf)],
        cancel: &Cancel,
    ) -> Result<WorkspaceEdit, Error> {
        cancel.check()?;
        let store = self.store();
        let mut map: HashMap<String, String> = HashMap::new();
        for (old, new) in moves {
            cancel.check()?;
            let (Ok(old_rel), Ok(new_rel)) = (self.rel(old), self.rel(new)) else {
                continue;
            };
            let mut pairs = Vec::new();
            if store.document(&old_rel).is_some() {
                if is_note(new_rel.rsplit('/').next().unwrap_or_default()) {
                    pairs.push((old_rel.clone(), new_rel));
                }
            } else {
                let prefix = format!("{old_rel}/");
                for f in store.files() {
                    if let Some(sub) = f.strip_prefix(&prefix) {
                        pairs.push((f.to_owned(), format!("{new_rel}/{sub}")));
                    }
                }
            }
            for (from, to) in pairs {
                if map.insert(from.clone(), to).is_some() {
                    return Err(refuse(format!("{from}: moved twice")));
                }
            }
        }
        let root = self.root().path;
        let docs_dir = store.conventions().docs_dir.clone();
        let mut out = WorkspaceEdit::default();
        // Stems of every note path after the moves, and before.
        let count_stems = |paths: Vec<&str>| {
            let mut m: HashMap<String, usize> = HashMap::new();
            for p in paths {
                *m.entry(stem(p).to_lowercase()).or_default() += 1;
            }
            m
        };
        let before = count_stems(store.files().collect());
        let after = count_stems(
            store
                .files()
                .map(|f| map.get(f).map_or(f, String::as_str))
                .collect(),
        );
        for from in store.files() {
            cancel.check()?;
            let Some(doc) = store.document(from) else {
                continue;
            };
            let from_after = map.get(from).map_or(from, String::as_str);
            let own_moved_dir = parent(from) != parent(from_after);
            let mut edits = Vec::new();
            for (link, r) in store.links(from) {
                if !counts(&link) || r.hint || link.target().path().is_empty() {
                    continue;
                }
                let Some(first) = r.targets.first() else {
                    continue;
                };
                let moved_to = map.get(first.as_str());
                let to_moved = moved_to.is_some()
                    && matches!(r.status, LinkStatus::Resolved | LinkStatus::Ambiguous);
                let own_relative = own_moved_dir
                    && r.step == Some(ResolveStep::FileRelative)
                    && matches!(
                        r.status,
                        LinkStatus::Resolved | LinkStatus::Ambiguous | LinkStatus::Unindexed
                    );
                if !to_moved && !own_relative {
                    continue;
                }
                if is_reference_usage(doc, &link) {
                    continue;
                }
                let Some(span) = path_span(doc.source(), &link) else {
                    continue;
                };
                let target_after = moved_to.map_or(first.as_str(), String::as_str);
                let site_key = to_moved
                    && store
                        .document(first)
                        .and_then(|d| d.frontmatter())
                        .and_then(|f| f.site_path())
                        .is_some();
                let ctx = Rewrite {
                    root: &root,
                    from_after,
                    target_after,
                    written: &doc.source()[span.clone()],
                    bracketed: doc.source()[..span.start].ends_with('<'),
                    kind: link.kind,
                    docs_dir: docs_dir.as_deref(),
                    site_key,
                };
                let new_stem = stem(target_after).to_lowercase();
                let taken_after = after.get(&new_stem).copied().unwrap_or(0) > 1;
                // A bare stem that other notes also carry after the moves
                // would find another note (or none): write the path. A name
                // that did not change and was shared before stays as is.
                let taken_before = before.get(&new_stem).copied().unwrap_or(0) > 1;
                let same_name = new_stem == stem(first).to_lowercase();
                let stem_taken = taken_after && !(same_name && taken_before);
                // A slash-less wiki target is written by name even when the
                // ladder found it by path (a note at the root): keep that
                // style unless the new name is taken.
                let step = match r.step {
                    Some(ResolveStep::FileRelative | ResolveStep::RootRelative)
                        if to_moved
                            && matches!(link.kind, LinkKind::Wiki | LinkKind::WikiEmbed)
                            && !ctx.written.contains('/')
                            && !stem_taken =>
                    {
                        Some(ResolveStep::Stem)
                    }
                    Some(ResolveStep::Stem) if to_moved && stem_taken => {
                        Some(ResolveStep::RootRelative)
                    }
                    s => s,
                };
                if let Some(new_text) = step.and_then(|s| ctx.text(s))
                    && new_text != ctx.written
                {
                    edits.push(TextEdit {
                        range: span,
                        new_text,
                    });
                }
            }
            edits.sort_by_key(|e| (e.range.start, e.range.end));
            edits.dedup_by(|a, b| a.range == b.range);
            if !edits.is_empty() {
                out.edits.push((self.abs(from), edits));
            }
        }
        out.edits.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }
}

fn refuse(msg: String) -> Error {
    Error::new(ErrorKind::Unsupported, format!("cannot rename: {msg}"))
}

/// A `[text][ref]` usage: its definition carries the destination.
pub(crate) fn is_reference_usage(doc: &Document, l: &Link) -> bool {
    l.kind == LinkKind::Reference && !doc.link_defs().any(|d| d.range == l.range)
}

/// The byte range of the link's target path (no anchor, no brackets) in
/// `src`, checked against `target.path`.
pub(crate) fn path_span(src: &str, l: &Link) -> Option<Range<usize>> {
    let slice = src.get(l.range.clone())?;
    let base = l.range.start;
    let (start, end) = match l.kind {
        LinkKind::Markdown | LinkKind::Image => {
            let from = l.text_range.end.saturating_sub(base).min(slice.len());
            let open = from + slice[from..].find("](")? + 2;
            dest(slice, open)
        }
        LinkKind::Reference => {
            let open = slice.find("]:")? + 2;
            dest(slice, open)
        }
        LinkKind::Wiki | LinkKind::WikiEmbed => {
            let open = slice.find("[[")? + 2;
            let len = slice[open..].find(['|', '#', ']'])?;
            (open, open + len)
        }
        LinkKind::Org => {
            let mut open = slice.find("[[")? + 2;
            if slice[open..].starts_with("file:") {
                open += "file:".len();
            }
            let rest = &slice[open..];
            let len = [rest.find(']'), rest.find("::"), rest.find('#')]
                .into_iter()
                .flatten()
                .min()?;
            (open, open + len)
        }
        _ => {
            let at = slice.find(l.target().path())?;
            (at, at + l.target().path().len())
        }
    };
    let text = slice.get(start..end)?;
    // Unexpected syntax (escapes, a target on the right of a pipe): no edit.
    (text == l.target().path() || text.split('#').next() == Some(l.target().path())).then(|| {
        let end = start + text.find('#').unwrap_or(text.len());
        base + start..base + end
    })
}

/// A link destination starting at or after `open` (whitespace skipped):
/// inside `<…>`, or up to whitespace or the unbalanced `)`.
fn dest(slice: &str, open: usize) -> (usize, usize) {
    let rest = &slice[open..];
    let start = open + (rest.len() - rest.trim_start().len());
    let rest = &slice[start..];
    if let Some(inner) = rest.strip_prefix('<') {
        let len = inner.find('>').unwrap_or(inner.len());
        return (start + 1, start + 1 + len);
    }
    let mut depth = 0usize;
    for (i, c) in rest.char_indices() {
        match c {
            '(' => depth += 1,
            ')' if depth == 0 => return (start, start + i),
            ')' => depth -= 1,
            c if c.is_whitespace() => return (start, start + i),
            _ => {}
        }
    }
    (start, slice.len())
}

/// How to write one rewritten target.
pub(crate) struct Rewrite<'a> {
    pub(crate) root: &'a Path,
    /// Root-relative path of the linking note after the rename.
    pub(crate) from_after: &'a str,
    /// Root-relative path of the target after the rename.
    pub(crate) target_after: &'a str,
    /// The path as written (no anchor).
    pub(crate) written: &'a str,
    pub(crate) bracketed: bool,
    pub(crate) kind: LinkKind,
    pub(crate) docs_dir: Option<&'a str>,
    /// The target is found by its frontmatter site path: renaming the file
    /// does not change it.
    pub(crate) site_key: bool,
}

impl Rewrite<'_> {
    pub(crate) fn text(&self, step: ResolveStep) -> Option<String> {
        let decoded = percent_decode(self.written);
        let target = self.with_ext(self.target_after, &decoded);
        let out = match step {
            ResolveStep::FileRelative => {
                let from_dir = self.root.join(parent(self.from_after));
                let rel = relative_path(&from_dir, &self.root.join(&target));
                match decoded.starts_with("./") && !rel.starts_with("../") {
                    true => format!("./{rel}"),
                    false => rel,
                }
            }
            ResolveStep::RootRelative => {
                if decoded.starts_with('~') {
                    return None;
                }
                let root = self.root.to_string_lossy();
                if Path::new(&decoded).starts_with(self.root) {
                    format!("{}/{target}", root.trim_end_matches('/'))
                } else if decoded.starts_with('/') {
                    format!("/{target}")
                } else {
                    target
                }
            }
            ResolveStep::SiteRooted => {
                if self.site_key {
                    return None;
                }
                let under_docs = self
                    .docs_dir
                    .and_then(|d| target.strip_prefix(d)?.strip_prefix('/'));
                format!("/{}", under_docs.unwrap_or(&target))
            }
            ResolveStep::Stem => {
                let name = target.rsplit('/').next().unwrap_or(&target);
                name.to_owned()
            }
            _ => return None,
        };
        Some(self.encode(out))
    }

    /// `path` with its note extension kept iff `written` had one.
    fn with_ext(&self, path: &str, written: &str) -> String {
        let last = written.rsplit('/').next().unwrap_or(written);
        let name_at = path.rfind('/').map_or(0, |i| i + 1);
        if is_note(last) || !is_note(&path[name_at..]) {
            return path.to_owned();
        }
        match path[name_at..].rfind('.') {
            Some(dot) => path[..name_at + dot].to_owned(),
            None => path.to_owned(),
        }
    }

    /// Markdown destinations are encoded by
    /// [`encode_destination`](crate::links::encode_destination), in `<…>`
    /// when written so.
    fn encode(&self, s: String) -> String {
        let md = matches!(
            self.kind,
            LinkKind::Markdown | LinkKind::Image | LinkKind::Reference
        );
        match md {
            true => encode_destination(&s, self.bracketed),
            false => s,
        }
    }
}

/// The file name without its extension.
pub(crate) fn stem(rel: &str) -> &str {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    name.rsplit_once('.').map_or(name, |(s, _)| s)
}

/// The directory part of a root-relative path (`""` at the root).
pub(crate) fn parent(rel: &str) -> &str {
    rel.rsplit_once('/').map_or("", |(d, _)| d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dest_forms() {
        let s = "[t](ab.md \"x\")";
        assert_eq!(dest(s, 4), (4, 9));
        let s = "[t](<a b.md>)";
        assert_eq!(&s[dest(s, 4).0..dest(s, 4).1], "a b.md");
        let s = "[t](a(1).md)";
        assert_eq!(&s[dest(s, 4).0..dest(s, 4).1], "a(1).md");
    }
}
