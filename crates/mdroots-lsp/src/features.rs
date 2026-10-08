//! The request handlers: goto, references, hover, symbols, completion and
//! rename (docs/specs/library.md §3.6). Each works on one document's
//! current text (the overlay wins) and returns LSP types; the server
//! serializes them.

use std::collections::{BTreeSet, HashMap};
use std::ops::Range;
use std::path::{Component, Path, PathBuf};

use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionList, CompletionTextEdit,
    DocumentChangeOperation, DocumentChanges, DocumentSymbol, Hover, HoverContents, Location,
    MarkupContent, MarkupKind, OneOf, OptionalVersionedTextDocumentIdentifier, Position,
    PrepareRenameResponse, RenameFile, ResourceOp, SymbolInformation, SymbolKind, TextDocumentEdit,
};
use mdroots::syntax::{Heading, LineIndex, PositionEncoding};
use mdroots::{Cancel, LinkStatus, Workspace};

use crate::{position, uri};

/// Notes offered after `[[` (and the cap on `](` paths).
const NOTE_LIMIT: usize = 50;
/// Notes returned by `workspace/symbol`.
pub(crate) const SYMBOL_LIMIT: usize = 100;
/// Lines of a note shown on hover.
const PREVIEW_LINES: usize = 10;

/// Why a request failed.
pub(crate) enum Fail {
    Lib(mdroots::Error),
    /// Bad arguments from the client (`InvalidParams`).
    Params(String),
}

impl From<mdroots::Error> for Fail {
    fn from(e: mdroots::Error) -> Fail {
        Fail::Lib(e)
    }
}

/// One document a request is about.
pub(crate) struct Ctx<'a> {
    pub ws: &'a Workspace,
    pub path: PathBuf,
    pub text: String,
    pub index: LineIndex,
    pub enc: PositionEncoding,
}

impl<'a> Ctx<'a> {
    /// `None` when the note is not indexed in `ws`. `path` is made
    /// canonical, as the workspace's paths are.
    pub(crate) fn new(ws: &'a Workspace, path: PathBuf, enc: PositionEncoding) -> Option<Ctx<'a>> {
        let text = ws.text(&path).ok()?;
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        Some(Ctx {
            ws,
            index: LineIndex::new(&text),
            path,
            text,
            enc,
        })
    }

    pub(crate) fn offset(&self, pos: Position) -> usize {
        position::offset(&self.index, pos, self.enc)
    }

    fn range(&self, r: Range<usize>) -> lsp_types::Range {
        position::range(&self.index, r, self.enc)
    }
}

/// Line indexes of the files locations point into, read once each.
struct Texts<'a> {
    ws: &'a Workspace,
    enc: PositionEncoding,
    seen: HashMap<PathBuf, Option<LineIndex>>,
}

impl<'a> Texts<'a> {
    fn new(ws: &'a Workspace, enc: PositionEncoding) -> Texts<'a> {
        Texts {
            ws,
            enc,
            seen: HashMap::new(),
        }
    }

    /// `r` in `path` as an LSP range; the file's start when it is not an
    /// indexed note.
    fn range(&mut self, path: &Path, r: Range<usize>) -> lsp_types::Range {
        let ws = self.ws;
        let idx = self
            .seen
            .entry(path.to_path_buf())
            .or_insert_with(|| ws.text(path).ok().map(|t| LineIndex::new(&t)));
        match idx {
            Some(i) => position::range(i, r, self.enc),
            None => lsp_types::Range::default(),
        }
    }
}

fn location(path: &Path, range: lsp_types::Range) -> Option<Location> {
    Some(Location {
        uri: uri::from_path(path)?,
        range,
    })
}

fn at_line(line: u32) -> lsp_types::Range {
    let p = Position { line, character: 0 };
    lsp_types::Range { start: p, end: p }
}

/// `r` without its trailing line break.
fn trim_line(text: &str, r: &Range<usize>) -> Range<usize> {
    let s = text.get(r.clone()).unwrap_or_default();
    r.start..r.start + s.trim_end_matches(['\n', '\r']).len()
}

pub(crate) fn definition(c: &Ctx, pos: Position) -> Result<Vec<Location>, Fail> {
    let Some(g) = c.ws.goto(&c.path, c.offset(pos))? else {
        return Ok(Vec::new());
    };
    let mut texts = Texts::new(c.ws, c.enc);
    let mut out = Vec::new();
    for (i, t) in g.targets.iter().enumerate() {
        let range = match (&g.heading, g.line) {
            (_, Some(n)) => at_line(n.saturating_sub(1)),
            (Some(h), None) if i == 0 => {
                let text = c.ws.text(t).unwrap_or_default();
                texts.range(t, trim_line(&text, h))
            }
            _ => at_line(0),
        };
        out.extend(location(t, range));
    }
    Ok(out)
}

/// Links to `note` as locations; `skip_self` drops links from the note
/// itself and keeps one per (file, line).
fn backlink_locations(c: &Ctx, note: &Path, skip_self: bool) -> Result<Vec<Location>, Fail> {
    let mut texts = Texts::new(c.ws, c.enc);
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for b in c.ws.backlinks(note)? {
        if skip_self && (b.from == note || !seen.insert((b.from.clone(), b.line))) {
            continue;
        }
        let range = texts.range(&b.from, b.range.clone());
        out.extend(location(&b.from, range));
    }
    Ok(out)
}

pub(crate) fn references(c: &Ctx, pos: Position) -> Result<Vec<Location>, Fail> {
    let off = c.offset(pos);
    let on_link =
        c.ws.document_links(&c.path)?
            .iter()
            .any(|l| l.range.contains(&off));
    let note = match c.ws.goto(&c.path, off)? {
        Some(g) => g.targets[0].clone(),
        // A link without a target (broken, external) has no references.
        None if on_link => return Ok(Vec::new()),
        None => c.path.clone(),
    };
    backlink_locations(c, &note, false)
}

/// `mdroots.backlinks`: links to the current note from other notes.
pub(crate) fn backlinks(c: &Ctx) -> Result<Vec<Location>, Fail> {
    backlink_locations(c, &c.path, true)
}

pub(crate) fn hover(c: &Ctx, pos: Position) -> Result<Option<Hover>, Fail> {
    let off = c.offset(pos);
    let Some(link) =
        c.ws.document_links(&c.path)?
            .into_iter()
            .find(|l| l.range.contains(&off))
    else {
        return Ok(None);
    };
    let value = match c.ws.goto(&c.path, off)? {
        Some(g) => match c.ws.preview(&g.targets[0], PREVIEW_LINES) {
            Ok(p) => format!("**{}**\n\n{}", p.title, p.excerpt),
            Err(_) => format!("`{}`", g.targets[0].display()),
        },
        None if link.status == LinkStatus::Broken => "broken link".to_owned(),
        None => return Ok(None),
    };
    Ok(Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value,
        }),
        range: Some(c.range(link.range)),
    }))
}

/// Headings nested by level; a heading's range runs to the next heading
/// of the same or a higher level, or the end of the text.
pub(crate) fn document_symbols(c: &Ctx) -> Result<Vec<DocumentSymbol>, Fail> {
    let hs = c.ws.outline(&c.path)?;
    let mut i = 0;
    Ok(nest(c, &hs, &mut i, 0))
}

/// Symbols for `hs[*i..]` while their level is deeper than `parent`.
fn nest(c: &Ctx, hs: &[Heading], i: &mut usize, parent: u8) -> Vec<DocumentSymbol> {
    let mut out = Vec::new();
    while let Some(h) = hs.get(*i) {
        if h.level <= parent {
            break;
        }
        *i += 1;
        let end = hs[*i..]
            .iter()
            .find(|n| n.level <= h.level)
            .map_or(c.text.len(), |n| n.range.start);
        let children = nest(c, hs, i, h.level);
        let name = match h.text.trim().is_empty() {
            true => "#".repeat(h.level as usize),
            false => h.text.clone(),
        };
        #[allow(deprecated)] // `deprecated` is a required field in lsp-types 0.97
        out.push(DocumentSymbol {
            name,
            detail: None,
            kind: SymbolKind::STRING,
            tags: None,
            deprecated: None,
            range: c.range(h.range.start..end),
            selection_range: c.range(trim_line(&c.text, &h.range)),
            children: (!children.is_empty()).then_some(children),
        });
    }
    out
}

/// Notes matching `query` in every open workspace, as file symbols.
pub(crate) fn workspace_symbols(wss: &[Workspace], query: &str) -> Vec<SymbolInformation> {
    let mut out = Vec::new();
    for ws in wss {
        for n in ws.search_notes(query, SYMBOL_LIMIT) {
            let Some(uri) = uri::from_path(&n.path) else {
                continue;
            };
            #[allow(deprecated)] // `deprecated` is a required field in lsp-types 0.97
            out.push(SymbolInformation {
                name: n.title,
                kind: SymbolKind::FILE,
                tags: None,
                deprecated: None,
                location: Location {
                    uri,
                    range: at_line(0),
                },
                container_name: None,
            });
        }
    }
    out.truncate(SYMBOL_LIMIT);
    out
}

/// What the text before the cursor asks to complete.
#[derive(Debug, PartialEq)]
enum Want<'a> {
    /// `[[typed`: notes.
    Notes(&'a str),
    /// `[[note#typed` (`note` empty: this note): headings.
    Headings(&'a str),
    /// `](typed`: relative paths.
    Paths,
    /// `#typed` after a blank: tags.
    Tags,
    Nothing,
}

/// The completion context of `line` (the text from line start to cursor)
/// and the byte offset in `line` where the typed part starts.
fn want(line: &str) -> (Want<'_>, usize) {
    if let Some(open) = line.rfind("[[") {
        let inner = &line[open + 2..];
        if !inner.contains("]]") {
            if inner.contains('|') {
                return (Want::Nothing, line.len());
            }
            return match inner.find('#') {
                Some(h) => (Want::Headings(&inner[..h]), open + 2 + h + 1),
                None => (Want::Notes(inner), open + 2),
            };
        }
    }
    if let Some(open) = line.rfind("](") {
        let inner = &line[open + 2..];
        if !inner.contains([')', ' ', '#']) {
            return (Want::Paths, open + 2);
        }
    }
    let typed = line
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphanumeric() || matches!(c, '_' | '-' | '/'))
        .last()
        .map_or(line.len(), |(i, _)| i);
    if let Some(before) = line[..typed].strip_suffix('#') {
        // A `#` first on the line starts a heading (spec §3.6): no popup.
        // Same tag-start rule as the parser: after whitespace or `(`.
        let tag_pos =
            before.ends_with(|c: char| c.is_whitespace() || c == '(') && !before.trim().is_empty();
        if tag_pos {
            return (Want::Tags, typed);
        }
    }
    (Want::Nothing, line.len())
}

pub(crate) fn completion(c: &Ctx, pos: Position) -> Result<CompletionList, Fail> {
    let off = c.offset(pos);
    let line_start = c.text[..off].rfind('\n').map_or(0, |i| i + 1);
    let line = &c.text[line_start..off];
    let (w, at) = want(line);
    let edit_range = c.range(line_start + at..off);
    let item = |label: String, new_text: String, kind, detail: Option<String>| CompletionItem {
        label,
        kind: Some(kind),
        detail,
        text_edit: Some(CompletionTextEdit::Edit(lsp_types::TextEdit {
            range: edit_range,
            new_text,
        })),
        ..Default::default()
    };
    let mut incomplete = false;
    let items = match w {
        Want::Notes(typed) => {
            let notes = c.ws.search_notes(typed, NOTE_LIMIT);
            incomplete = notes.len() == NOTE_LIMIT;
            notes
                .into_iter()
                .map(|n| {
                    let stem = file_stem(&n.path);
                    item(stem.clone(), stem, CompletionItemKind::FILE, Some(n.title))
                })
                .collect()
        }
        Want::Headings(note) => {
            let target = match note.trim().is_empty() {
                true => Some(c.path.clone()),
                false => {
                    c.ws.resolve(&c.path, &format!("[[{note}]]"))
                        .ok()
                        .and_then(|r| r.targets.into_iter().next())
                }
            };
            let hs = match target {
                Some(t) => c.ws.outline(&t).unwrap_or_default(),
                None => Vec::new(),
            };
            // The first H1 is the note's title, not a section.
            let title = hs.iter().position(|h| h.level == 1);
            hs.into_iter()
                .enumerate()
                .filter(|(i, _)| Some(*i) != title)
                .map(|(_, h)| {
                    let t = h.text.clone();
                    item(t.clone(), t, CompletionItemKind::REFERENCE, None)
                })
                .collect()
        }
        Want::Paths => {
            let typed = line[at..].to_lowercase();
            let dir = c.path.parent().unwrap_or(Path::new("/")).to_path_buf();
            let mut paths: Vec<(String, String)> =
                c.ws.notes()
                    .into_iter()
                    .filter(|n| n.path != c.path)
                    .map(|n| (relative(&dir, &n.path), n.title))
                    .filter(|(p, _)| p.to_lowercase().contains(&typed))
                    .collect();
            incomplete = paths.len() > NOTE_LIMIT;
            paths.truncate(NOTE_LIMIT);
            paths
                .into_iter()
                .map(|(p, title)| item(p.clone(), p, CompletionItemKind::FILE, Some(title)))
                .collect()
        }
        // The partial tag being typed is a tag of the overlay too: drop it
        // when this note is its only carrier.
        Want::Tags => {
            c.ws.tags()
                .into_iter()
                .filter(|(t, n)| !(*n == 1 && *t == line[at..]))
                .map(|(t, n)| {
                    let detail = format!("{n} note{}", if n == 1 { "" } else { "s" });
                    item(t.clone(), t, CompletionItemKind::KEYWORD, Some(detail))
                })
                .collect()
        }
        Want::Nothing => Vec::new(),
    };
    Ok(CompletionList {
        is_incomplete: incomplete,
        items,
    })
}

fn file_stem(p: &Path) -> String {
    p.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `to` relative to the directory `from`, `/`-separated, both absolute.
fn relative(from: &Path, to: &Path) -> String {
    let parts = |p: &Path| -> Vec<String> {
        p.components()
            .filter_map(|c| match c {
                Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect()
    };
    let (f, t) = (parts(from), parts(to));
    let common = f.iter().zip(&t).take_while(|(a, b)| a == b).count();
    let mut out = vec!["..".to_owned(); f.len() - common];
    out.extend(t[common..].iter().cloned());
    out.join("/")
}

/// The note a rename at `pos` renames and the range it covers: a link to
/// another (or this) indexed note, or this note's first level-1 heading.
fn rename_target(c: &Ctx, pos: Position) -> Result<Option<(PathBuf, Range<usize>)>, Fail> {
    let off = c.offset(pos);
    if let Some(link) =
        c.ws.document_links(&c.path)?
            .into_iter()
            .find(|l| l.range.contains(&off))
    {
        let Some(g) = c.ws.goto(&c.path, off)? else {
            return Ok(None);
        };
        let t = g.targets[0].clone();
        // An in-document anchor names a heading, not the note.
        if (t == c.path && g.heading.is_some()) || c.ws.text(&t).is_err() {
            return Ok(None);
        }
        return Ok(Some((t, link.range)));
    }
    let h1 = c.ws.outline(&c.path)?.into_iter().find(|h| h.level == 1);
    Ok(h1.and_then(|h| {
        let r = trim_line(&c.text, &h.range);
        (r.start <= off && off <= r.end).then(|| (c.path.clone(), r))
    }))
}

pub(crate) fn prepare_rename(
    c: &Ctx,
    pos: Position,
) -> Result<Option<PrepareRenameResponse>, Fail> {
    Ok(
        rename_target(c, pos)?.map(|(t, r)| PrepareRenameResponse::RangeWithPlaceholder {
            range: c.range(r),
            placeholder: file_stem(&t),
        }),
    )
}

pub(crate) fn rename(
    c: &Ctx,
    pos: Position,
    new_name: &str,
    cancel: &Cancel,
) -> Result<Option<lsp_types::WorkspaceEdit>, Fail> {
    let bad = new_name.trim().is_empty()
        || new_name.contains(['/', '\\'])
        || new_name
            .rsplit_once('.')
            .is_some_and(|(s, e)| !s.is_empty() && !e.is_empty());
    if bad {
        return Err(Fail::Params(format!(
            "new name must be a file stem without directory or extension: {new_name:?}"
        )));
    }
    let Some((old, _)) = rename_target(c, pos)? else {
        return Ok(None);
    };
    let mut name = new_name.to_owned();
    if let Some(ext) = old.extension() {
        name.push('.');
        name.push_str(&ext.to_string_lossy());
    }
    let new = old.with_file_name(name);
    rename_file(c.ws, &old, &new, c.enc, cancel).map(Some)
}

/// The LSP edit renaming `old` to `new`: a text edit per linking file
/// (unversioned), then the file rename.
pub(crate) fn rename_file(
    ws: &Workspace,
    old: &Path,
    new: &Path,
    enc: PositionEncoding,
    cancel: &Cancel,
) -> Result<lsp_types::WorkspaceEdit, Fail> {
    let e = ws.rename_note(old, new, cancel)?;
    let mut texts = Texts::new(ws, enc);
    let mut ops = Vec::new();
    for (file, edits) in &e.edits {
        let Some(uri) = uri::from_path(file) else {
            continue;
        };
        let edits = edits
            .iter()
            .map(|t| {
                OneOf::Left(lsp_types::TextEdit {
                    range: texts.range(file, t.range.clone()),
                    new_text: t.new_text.clone(),
                })
            })
            .collect();
        ops.push(DocumentChangeOperation::Edit(TextDocumentEdit {
            text_document: OptionalVersionedTextDocumentIdentifier { uri, version: None },
            edits,
        }));
    }
    if let Some((from, to)) = &e.rename
        && let (Some(old_uri), Some(new_uri)) = (uri::from_path(from), uri::from_path(to))
    {
        ops.push(DocumentChangeOperation::Op(ResourceOp::Rename(
            RenameFile {
                old_uri,
                new_uri,
                options: None,
                annotation_id: None,
            },
        )));
    }
    Ok(lsp_types::WorkspaceEdit {
        document_changes: Some(DocumentChanges::Operations(ops)),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_contexts() {
        assert_eq!(want("see [[no"), (Want::Notes("no"), 6));
        assert_eq!(want("see [[#Us"), (Want::Headings(""), 7));
        assert_eq!(want("[[note#"), (Want::Headings("note"), 7));
        assert_eq!(want("[[a|b").0, Want::Nothing);
        assert_eq!(want("[[a]] x").0, Want::Nothing);
        assert_eq!(want("[t](../a"), (Want::Paths, 4));
        assert_eq!(want("[t](a.md) x").0, Want::Nothing);
        assert_eq!(want("text #pro"), (Want::Tags, 6));
        assert_eq!(want("text #"), (Want::Tags, 6));
        assert_eq!(want("Try (#pr"), (Want::Tags, 6));
        // A `#` first on the line (after blanks) is a heading.
        assert_eq!(want("#").0, Want::Nothing);
        assert_eq!(want("  #pro").0, Want::Nothing);
        assert_eq!(want("## x").0, Want::Nothing);
        assert_eq!(want("a#b").0, Want::Nothing);
    }

    #[test]
    fn relative_paths() {
        let r = |a: &str, b: &str| relative(Path::new(a), Path::new(b));
        assert_eq!(r("/v/notes", "/v/concepts/b.md"), "../concepts/b.md");
        assert_eq!(r("/v", "/v/a.md"), "a.md");
    }
}
