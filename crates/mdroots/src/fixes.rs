//! [`Workspace::fixes`]: quick fixes for a note's link diagnostics, link
//! conversion and link selection, as [`WorkspaceEdit`]s the caller applies
//! (D10). Never writes files.

use std::ops::Range;
use std::path::{Path, PathBuf};

use mdroots_core::fs::clean_path;
use mdroots_core::memstore::counts;
use mdroots_core::{Cancel, DiagCode, Diagnostic, Error, FileSystem, MemStore};
use mdroots_resolve::ResolveStep;
use mdroots_resolve::dialect::{LinkStyle, link_style};
use mdroots_resolve::ladder::LinkStatus;
use mdroots_resolve::normalize::percent_decode;
use mdroots_syntax::{Anchor, Context, Dialect, Document, Link, LinkKind, parse};

use crate::links::{link_text, wiki_target};
use crate::notes::score;
use crate::rename::{
    Rewrite, TextEdit, WorkspaceEdit, is_reference_usage, lands_after, link_path, parent,
    path_span, stem, text_close,
};
use crate::workspace::{Freshness, Workspace, is_note, slash, title};

/// Most candidates one trigger offers (notes, headings).
const MAX_CANDIDATES: usize = 3;

/// One proposed change: a title for the editor's menu and the edit.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct Fix {
    /// What the editor shows: `Change to <title>`, `Create note <path>`,
    /// `Change to #<slug>`, `Pick <path>`, `Convert to wiki link`,
    /// `Convert to Markdown link`, `Link to <title>`.
    pub title: String,
    pub kind: FixKind,
    /// The single candidate of its trigger: safe to apply with one key.
    pub preferred: bool,
    pub edit: WorkspaceEdit,
}

/// What a [`Fix`] answers.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum FixKind {
    /// Repairs this diagnostic (range and code included).
    QuickFix(Diagnostic),
    /// Rewrites a link in another syntax.
    ConvertLink,
    /// Turns a selection into a link.
    LinkSelection,
}

impl Workspace {
    /// The fixes for the links of the note at `path` selected by the byte
    /// `range`: a non-empty range selects the links overlapping it, an
    /// empty range at `off` the link whose range contains `off` or ends
    /// at it. Each selected link's diagnostic (computed now over the
    /// current store and overlay) is answered by:
    ///
    /// - broken link: `Change to <title>` for up to 3 notes whose title,
    ///   stem or alias matches the written stem exactly, as a prefix or as
    ///   a substring (only the path is replaced, in the form it was
    ///   written in), and `Create note <path>` for the path the link names;
    /// - broken anchor: `Change to #<slug>` for up to 3 headings of the
    ///   target matched the same way on slug or text;
    /// - ambiguous link: `Pick <path>` per candidate.
    ///
    /// Then each selected wiki or Markdown link that resolves to an indexed
    /// note gets `Convert to Markdown link` / `Convert to wiki link`
    /// ([`FixKind::ConvertLink`]), anchor and label kept. A non-empty range
    /// on one line that overlaps no link gets `Link to <title>`
    /// ([`FixKind::LinkSelection`]) for up to 3 notes whose title, stem or
    /// alias equals or starts with the trimmed selection: the selection
    /// becomes the label of a link to it in the root's style.
    ///
    /// Order: by link start, then the list above, then conversion, then
    /// candidate rank; link selection last. Links in code or comments,
    /// reference usages (`[x][r]`; their definition carries the fixes) and
    /// other diagnostics get none; Org notes get no anchor fix, conversion
    /// or link selection. Empty for a note that is not indexed.
    pub fn fixes(
        &self,
        path: &Path,
        range: Range<usize>,
        cancel: &Cancel,
    ) -> Result<Vec<Fix>, Error> {
        cancel.check()?;
        let rel = self.rel(path)?;
        let store = self.store();
        let Some(doc) = store.document(&rel) else {
            return Ok(Vec::new());
        };
        let diags = store
            .policy(self.freshness() == Freshness::Lazy)
            .diagnostics(&store, &rel);
        let mut links: Vec<Link> = doc
            .links()
            .filter(|l| counts(l) && !is_reference_usage(doc, l))
            .filter(|l| match range.is_empty() {
                true => l.range.start <= range.start && range.start <= l.range.end,
                false => l.range.start < range.end && range.start < l.range.end,
            })
            .cloned()
            .collect();
        links.sort_by_key(|l| (l.range.start, l.range.end));
        let ctx = Ctx {
            ws: self,
            store: &store,
            doc,
            from: &rel,
            root: self.root().path,
        };
        let mut out = Vec::new();
        for link in &links {
            cancel.check()?;
            for d in diags.iter().filter(|d| d.range == link.range) {
                out.extend(ctx.for_diagnostic(link, d));
            }
            out.extend(ctx.convert(link));
        }
        if !range.is_empty() {
            out.extend(ctx.link_selection(range));
        }
        Ok(out)
    }
}

/// One `fixes` call: the note, under one read of the store.
struct Ctx<'a> {
    ws: &'a Workspace,
    store: &'a MemStore,
    doc: &'a Document,
    /// Root-relative path of the note.
    from: &'a str,
    root: std::path::PathBuf,
}

/// A fix's title and its edit, before its trigger decides `preferred`.
type Candidate = (String, WorkspaceEdit);

impl Ctx<'_> {
    fn for_diagnostic(&self, link: &Link, d: &Diagnostic) -> Vec<Fix> {
        let fix = |(title, edit): Candidate, preferred| Fix {
            title,
            kind: FixKind::QuickFix(d.clone()),
            preferred,
            edit,
        };
        // One candidate exactly: preferred.
        let single = |c: Vec<Candidate>| -> Vec<Fix> {
            let one = c.len() == 1;
            c.into_iter().map(|c| fix(c, one)).collect()
        };
        match d.code {
            DiagCode::BrokenLink => {
                let mut v = single(self.change_to(link));
                v.extend(self.create_note(link).map(|c| fix(c, false)));
                v
            }
            DiagCode::BrokenAnchor if self.doc.dialect() != Dialect::Org => {
                single(self.change_anchor(link))
            }
            DiagCode::AmbiguousLink => d
                .related
                .iter()
                .filter_map(|t| {
                    let text = self.path_text(link, t)?;
                    Some(fix((format!("Pick {t}"), text), false))
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    /// `Change to <title>`: notes matching the written stem (tiers 0–2).
    fn change_to(&self, link: &Link) -> Vec<Candidate> {
        let written = percent_decode(link_path(link));
        let name = written.rsplit('/').next().unwrap_or(&written);
        let query = stem(name);
        if query.is_empty() {
            return Vec::new();
        }
        self.ws
            .search_scored(self.store, query)
            .into_iter()
            .filter(|(rel, (tier, _))| *tier <= 2 && rel != self.from)
            .take(MAX_CANDIDATES)
            .filter_map(|(rel, _)| {
                let edit = self.path_text(link, &rel)?;
                let title = title(&rel, self.store.document(&rel)?);
                Some((format!("Change to {title}"), edit))
            })
            .collect()
    }

    /// The edit putting `target` (root-relative) in place of the link's
    /// path, in the form the path was written in (extension kept iff it
    /// had one); else, for a form that does not land on `target` alone,
    /// the first fallback that does: wiki links the stem, the
    /// root-relative, file-relative and `/`-rooted paths; other links the
    /// file-relative and `/`-rooted paths. `None` if none lands.
    fn path_text(&self, link: &Link, target: &str) -> Option<WorkspaceEdit> {
        let src = self.doc.source();
        let span = path_span(src, link)?;
        let written = &src[span.clone()];
        let rw = Rewrite {
            root: &self.root,
            from_after: self.from,
            target_after: target,
            written,
            bracketed: src[..span.start].ends_with('<'),
            kind: link.kind,
            docs_dir: self.store.conventions().docs_dir.as_deref(),
            site_key: false,
        };
        let wiki = matches!(link.kind, LinkKind::Wiki | LinkKind::WikiEmbed);
        let decoded = percent_decode(written);
        let step = match (wiki, decoded.starts_with('/'), decoded.contains('/')) {
            (_, true, _) => ResolveStep::RootRelative,
            (true, _, false) => ResolveStep::Stem,
            (true, _, true) => ResolveStep::RootRelative,
            (false, _, _) => ResolveStep::FileRelative,
        };
        let mut tries: Vec<String> = rw.text(step).into_iter().collect();
        if wiki {
            tries.extend(rw.text(ResolveStep::Stem));
            tries.push(rw.root_relative());
        }
        tries.extend([rw.file_relative(), rw.slash_rooted()]);
        let text = tries.into_iter().find(|t| {
            let spliced = format!(
                "{}{t}{}",
                &src[link.range.start..span.start],
                &src[span.end..link.range.end]
            );
            lands_after(self.store, self.from, &spliced, t, target)
        })?;
        Some(self.one_edit(span, text))
    }

    /// `Create note <path>`: the note the link names, if it is a note
    /// path inside the root that is neither on disk nor indexed.
    fn create_note(&self, link: &Link) -> Option<Candidate> {
        let written = percent_decode(link_path(link));
        let name = written.rsplit('/').next().unwrap_or(&written);
        if name.is_empty() || (has_ext(name) && !is_note(name)) || written.starts_with('~') {
            return None;
        }
        let wiki = matches!(link.kind, LinkKind::Wiki | LinkKind::WikiEmbed);
        let rel = if let Some(inside) = Path::new(&written)
            .strip_prefix(&self.root)
            .ok()
            .filter(|_| written.starts_with('/'))
        {
            slash(inside)
        } else if let Some(r) = written.strip_prefix('/') {
            r.to_owned()
        } else if wiki && !written.contains('/') {
            join(parent(self.from), &written)
        } else if wiki {
            written.clone()
        } else {
            join(parent(self.from), &written)
        };
        let rel = match is_note(rel.rsplit('/').next().unwrap_or(&rel)) {
            true => rel,
            false => {
                let ext = self.from.rsplit_once('.').map_or("md", |(_, e)| e);
                format!("{rel}.{ext}")
            }
        };
        // Through the filesystem, as `Workspace::rel` maps public paths: a
        // symlinked folder inside the root may lead out of it.
        let abs = canonical(self.ws.fs(), &clean_path(&self.root.join(&rel)))?;
        let inside = abs.strip_prefix(&self.root).ok()?;
        if inside.as_os_str().is_empty() {
            return None;
        }
        let rel = slash(inside);
        if self.store.document(&rel).is_some() || self.ws.fs().stat(&abs).is_ok() {
            return None;
        }
        let content = format!("# {}\n", stem(&rel));
        let edit = WorkspaceEdit {
            edits: Vec::new(),
            rename: None,
            create: vec![(abs, content)],
        };
        Some((format!("Create note {rel}"), edit))
    }

    /// `Change to #<slug>`: headings of the link's one target whose slug
    /// or text matches the written heading anchor (tiers 0–2), by score
    /// then document order. Block and custom-id anchors: none.
    fn change_anchor(&self, link: &Link) -> Vec<Candidate> {
        let Some(Anchor::Heading(_)) = link.target().anchor() else {
            return Vec::new();
        };
        let src = self.doc.source();
        let Some(written) = anchor_span(src, link) else {
            return Vec::new();
        };
        let r = self.store.resolve_link(self.from, link, false);
        let [target] = r.targets.as_slice() else {
            return Vec::new();
        };
        let Some(tdoc) = self.store.document(target) else {
            return Vec::new();
        };
        let q: Vec<char> = percent_decode(&src[written.clone()])
            .chars()
            .flat_map(char::to_lowercase)
            .collect();
        if q.is_empty() {
            return Vec::new();
        }
        let mut scored: Vec<((u8, usize), usize, &str)> = tdoc
            .headings()
            .enumerate()
            .filter_map(|(i, h)| {
                let s = [score(&q, &h.slug), score(&q, &h.text)]
                    .into_iter()
                    .flatten()
                    .min()?;
                (s.0 <= 2 && !h.slug.is_empty()).then_some((s, i, &*h.slug))
            })
            .collect();
        scored.sort();
        scored
            .into_iter()
            .take(MAX_CANDIDATES)
            .map(|(_, _, slug)| {
                let edit = self.one_edit(written.clone(), slug.to_owned());
                (format!("Change to #{slug}"), edit)
            })
            .collect()
    }

    /// The root's style for inserted links, from the store already held.
    fn style(&self) -> LinkStyle {
        link_style(self.store.conventions(), &self.store.vote())
    }

    /// `Convert to Markdown link` for a wiki link, `Convert to wiki link`
    /// for a Markdown link: the whole link rewritten by `link_text`, anchor
    /// (as written) and label kept. Only links outside frontmatter (where
    /// `[` would start a YAML list) in a Markdown note that resolve to one
    /// indexed note, and only when the new link resolves to that note too.
    /// A Markdown label holding `|` (dialects disagree on the pipe order)
    /// and in-note anchor links (`[x](#h)`, `[[#h]]`) are not converted.
    fn convert(&self, link: &Link) -> Option<Fix> {
        if self.doc.dialect() != Dialect::Markdown
            || link.context == Context::Frontmatter
            || link_path(link).is_empty()
        {
            return None;
        }
        let r = self.store.resolve_link(self.from, link, false);
        let [target] = r.targets.as_slice() else {
            return None;
        };
        if r.status != LinkStatus::Resolved {
            return None;
        }
        let src = self.doc.source();
        let anchor = match link.target().anchor() {
            None => None,
            Some(_) => Some(&src[anchor_span(src, link)?]).filter(|a| !a.is_empty()),
        };
        let (title, text) = match link.kind {
            LinkKind::Wiki => {
                let style = match self.style() {
                    s @ (LinkStyle::MarkdownRelative { .. }
                    | LinkStyle::MarkdownRootRelative { .. }) => s,
                    _ => LinkStyle::MarkdownRelative { md_suffix: true },
                };
                let text = link_text(self.store, self.from, target, style, anchor, link.label());
                ("Convert to Markdown link", text.ok()?)
            }
            LinkKind::Markdown => {
                // The whole source between `[` and its matching `](`:
                // `text_range` covers text events only, not formatting at
                // the edges or a nested image's destination.
                let slice = src.get(link.range.clone())?;
                let label = &slice[1..text_close(slice)?];
                if label.contains('|') {
                    return None;
                }
                let generated = wiki_target(self.store, self.from, target, true);
                let label = (!label.is_empty() && label != generated).then_some(label);
                let style = LinkStyle::WikiStem;
                let text = link_text(self.store, self.from, target, style, anchor, label);
                ("Convert to wiki link", text.ok()?)
            }
            _ => return None,
        };
        let doc = parse(&text, Dialect::Markdown);
        let new = doc.links().next()?;
        let landed = self.store.resolve_link(self.from, new, false);
        if landed.status != LinkStatus::Resolved || landed.targets.first() != Some(target) {
            return None;
        }
        Some(Fix {
            title: title.to_owned(),
            kind: FixKind::ConvertLink,
            preferred: false,
            edit: self.one_edit(link.range.clone(), text),
        })
    }

    /// `Link to <title>`: the trimmed selection, on one line and
    /// overlapping no link, becomes the label of a link (root style) to
    /// each of up to 3 notes, not this one, matching it exactly or as a
    /// prefix (tiers 0–1). Markdown notes only; a selection in code, a
    /// comment or frontmatter gets none.
    fn link_selection(&self, range: Range<usize>) -> Vec<Fix> {
        let src = self.doc.source();
        let Some(sel) = src.get(range.clone()) else {
            return Vec::new();
        };
        let trimmed = sel.trim();
        if self.doc.dialect() != Dialect::Markdown
            || trimmed.is_empty()
            || sel.contains(['\n', '\r'])
            || self
                .doc
                .links()
                .any(|l| l.range.start < range.end && range.start < l.range.end)
        {
            return Vec::new();
        }
        let start = range.start + (sel.len() - sel.trim_start().len());
        let span = start..start + trimmed.len();
        let style = self.style();
        let wiki = !matches!(
            style,
            LinkStyle::MarkdownRelative { .. } | LinkStyle::MarkdownRootRelative { .. }
        );
        let found: Vec<Candidate> = self
            .ws
            .search_scored(self.store, trimmed)
            .into_iter()
            .filter(|(rel, (tier, _))| *tier <= 1 && rel != self.from)
            .take(MAX_CANDIDATES)
            .filter_map(|(rel, _)| {
                let plain = link_text(self.store, self.from, &rel, style, None, None).ok();
                // `[[b]]`, not `[[b|b]]`.
                let text = match plain {
                    Some(p) if wiki && p == format!("[[{trimmed}]]") => p,
                    _ => link_text(self.store, self.from, &rel, style, None, Some(trimmed)).ok()?,
                };
                // In place, the new link must be one in prose or a heading
                // (not code, a comment or frontmatter) that reaches `rel`.
                let spliced = format!("{}{text}{}", &src[..span.start], &src[span.end..]);
                let doc = parse(&spliced, Dialect::Markdown);
                let new = doc
                    .links()
                    .find(|l| l.range == (span.start..span.start + text.len()))?;
                let r = self.store.resolve_link(self.from, new, false);
                if !matches!(new.context, Context::Prose | Context::Heading)
                    || r.status != LinkStatus::Resolved
                    || r.targets.first() != Some(&rel)
                {
                    return None;
                }
                let title = title(&rel, self.store.document(&rel)?);
                Some((
                    format!("Link to {title}"),
                    self.one_edit(span.clone(), text),
                ))
            })
            .collect();
        let one = found.len() == 1;
        found
            .into_iter()
            .map(|(title, edit)| Fix {
                title,
                kind: FixKind::LinkSelection,
                preferred: one,
                edit,
            })
            .collect()
    }

    fn one_edit(&self, range: Range<usize>, new_text: String) -> WorkspaceEdit {
        WorkspaceEdit {
            edits: vec![(self.ws.abs(self.from), vec![TextEdit { range, new_text }])],
            rename: None,
            create: Vec::new(),
        }
    }
}

/// The byte range of the link's written anchor (after the `#`) in `src`.
/// The parser's raw target is the destination as written, already bounded
/// by its own syntax (`<…>`, balanced parentheses, an attribute's quotes, a
/// wiki `|` or `]]`), so the anchor is the part of it after the path, found
/// where the path starts in the source. `None` when the source does not
/// hold the raw target there (escapes, entities) or it has no `#`.
fn anchor_span(src: &str, link: &Link) -> Option<Range<usize>> {
    let t = link.target();
    let anchor = t.raw().strip_prefix(t.path())?.strip_prefix('#')?;
    let start = path_span(src, link)?.start;
    let end = start + t.raw().len();
    (end <= link.range.end && src.get(start..end)? == t.raw()).then(|| end - anchor.len()..end)
}

/// `p` through the filesystem's `canonicalize`; when that fails (a missing
/// path on a filesystem that needs it to exist), the longest ancestor it
/// canonicalizes with the rest appended.
fn canonical(fs: &dyn FileSystem, p: &Path) -> Option<PathBuf> {
    let mut tail = Vec::new();
    let mut cur = p;
    loop {
        if let Ok(mut c) = fs.canonicalize(cur) {
            c.extend(tail.iter().rev());
            return Some(c);
        }
        tail.push(cur.file_name()?);
        cur = cur.parent()?;
    }
}

/// Whether the file name has an extension (`a.b`, not `.b` or `a.`).
fn has_ext(name: &str) -> bool {
    name.rfind('.').is_some_and(|d| d > 0 && d + 1 < name.len())
}

/// `rel` joined under the directory `dir` (root-relative, `""` at the
/// root); `..` is folded later.
fn join(dir: &str, rel: &str) -> String {
    match dir.is_empty() {
        true => rel.to_owned(),
        false => format!("{dir}/{rel}"),
    }
}
