//! Link insertion in the root's style (docs/specs/index.md §3.2): the
//! style comes from an existing tool config, else the convention vote.

use std::path::Path;

use mdroots_core::{Error, ErrorKind, MemStore};
use mdroots_resolve::dialect::{LinkStyle, link_style};
use mdroots_resolve::ladder::LinkStatus;
use mdroots_syntax::{Dialect, parse};

use crate::rename::relative;
use crate::workspace::{Workspace, is_note, title};

impl Workspace {
    /// How new links are written in this root: see
    /// [`link_style`](mdroots_resolve::dialect::link_style). The vote is
    /// cached until the next content change.
    pub fn link_style(&self) -> LinkStyle {
        let store = self.store();
        link_style(store.conventions(), &store.vote())
    }

    /// The link text to insert in `from` pointing at `target`, in the root's
    /// [`link_style`](Self::link_style). `target` need not exist yet.
    ///
    /// Without a label, wiki links are `[[target]]` and Markdown links use
    /// the target's title (the indexed note's title, else its file stem).
    /// `[[stem]]` becomes `[[dir/stem]]` when another note shares the stem
    /// (see [`wiki_target`](Self::wiki_target)). Markdown paths are written
    /// by [`markdown_destination`]. A root-relative path that would resolve
    /// to another note from `from` (a note at the same path under `from`'s
    /// directory wins) gets a leading `/`, else (or for wiki paths) is
    /// written relative to `from`.
    /// A wiki target containing `|`, `]` or a line break, or a label
    /// containing `]]` or a line break, is `Unsupported`, as are paths
    /// outside the root.
    pub fn link_to(
        &self,
        from: &Path,
        target: &Path,
        label: Option<&str>,
    ) -> Result<String, Error> {
        let from_rel = self.rel(from)?;
        let to_rel = self.rel(target)?;
        let style = self.link_style();
        let store = self.store();
        let wiki = |t: String| -> Result<String, Error> {
            if t.contains(['|', ']', '\n', '\r']) {
                return Err(unsupported(&t));
            }
            match label {
                None => Ok(format!("[[{t}]]")),
                Some(l) if l.contains("]]") || l.contains(['\n', '\r']) => Err(unsupported(l)),
                Some(l) => Ok(format!("[[{t}|{l}]]")),
            }
        };
        let markdown = |paths: Vec<String>, md_suffix: bool| -> String {
            let dests: Vec<String> = paths
                .into_iter()
                .map(|p| match md_suffix {
                    true => markdown_destination(&p),
                    false => markdown_destination(drop_ext(&p)),
                })
                .collect();
            let dest = first_landing(&store, &from_rel, &to_rel, dests, &|d| format!("[x]({d})"));
            let text = match label {
                Some(l) => l.to_owned(),
                None => store
                    .document(&to_rel)
                    .map(|d| title(&to_rel, d))
                    .unwrap_or_else(|| stem(&to_rel).to_owned()),
            };
            format!("[{}]({dest})", escape_label(&text))
        };
        let dir = Path::new(&from_rel).parent().unwrap_or(Path::new(""));
        let file_relative = relative(dir, Path::new(&to_rel));
        match style {
            LinkStyle::WikiStem => wiki(wiki_target(&store, &from_rel, &to_rel, true)),
            LinkStyle::MarkdownRelative { md_suffix } => {
                Ok(markdown(vec![file_relative], md_suffix))
            }
            LinkStyle::MarkdownRootRelative { md_suffix } => Ok(markdown(
                vec![to_rel.clone(), format!("/{to_rel}"), file_relative],
                md_suffix,
            )),
            // WikiPath, and any style added later.
            _ => wiki(wiki_target(&store, &from_rel, &to_rel, false)),
        }
    }

    /// The text inside `[[…]]` that links `from` to `target`, as
    /// [`link_to`](Self::link_to) writes it for [`LinkStyle::WikiStem`]:
    /// the stem unless another note shares it (compared case-insensitively),
    /// else the root-relative path without extension, else the path relative
    /// to `from`, else the root-relative path with a leading `/`; the first
    /// that resolves from `from` to `target`. Characters a wiki link cannot
    /// hold (`|`, `]`) are not checked. `target` need not exist yet.
    pub fn wiki_target(&self, from: &Path, target: &Path) -> Result<String, Error> {
        let from_rel = self.rel(from)?;
        let to_rel = self.rel(target)?;
        Ok(wiki_target(&self.store(), &from_rel, &to_rel, true))
    }
}

/// [`Workspace::wiki_target`] on root-relative paths; without
/// `stem_first` the stem is not tried.
fn wiki_target(store: &MemStore, from_rel: &str, to_rel: &str, stem_first: bool) -> String {
    let s = stem(to_rel);
    let shared = || {
        let s = s.to_lowercase();
        store
            .files()
            .any(|f| f != to_rel && stem(f).to_lowercase() == s)
    };
    let dir = Path::new(from_rel).parent().unwrap_or(Path::new(""));
    let path = drop_ext(to_rel).to_owned();
    let mut c = Vec::new();
    if stem_first && !shared() {
        c.push(s.to_owned());
    }
    c.extend([
        path.clone(),
        drop_ext(&relative(dir, Path::new(to_rel))).to_owned(),
        format!("/{path}"),
    ]);
    first_landing(store, from_rel, to_rel, c, &|t| format!("[[{t}]]"))
}

/// The first of `candidates` that, written by `as_link` in `from_rel`,
/// [`lands`] on `to_rel`; else the first.
fn first_landing(
    store: &MemStore,
    from_rel: &str,
    to_rel: &str,
    candidates: Vec<String>,
    as_link: &dyn Fn(&str) -> String,
) -> String {
    let first = candidates[0].clone();
    candidates
        .into_iter()
        .find(|c| lands(store, from_rel, to_rel, &as_link(c)))
        .unwrap_or(first)
}

/// Whether `link` (written in `from_rel`) leads to `to_rel`, or to nothing
/// when `to_rel` is not indexed. Text that does not parse as a link in
/// `from_rel`'s dialect cannot be checked: it passes.
fn lands(store: &MemStore, from_rel: &str, to_rel: &str, link: &str) -> bool {
    let doc = parse(link, Dialect::detect_from_path(Path::new(from_rel)));
    let Some(l) = doc.links().next() else {
        return true;
    };
    let r = store.resolve_link(from_rel, l, false);
    match r.targets.first() {
        Some(t) => t == to_rel && r.status != LinkStatus::Ambiguous,
        None => store.document(to_rel).is_none(),
    }
}

fn unsupported(s: &str) -> Error {
    Error::new(
        ErrorKind::Unsupported,
        format!("{s:?}: not writable as a link"),
    )
}

/// The file name without its extension.
fn stem(rel: &str) -> &str {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    name.rsplit_once('.').map_or(name, |(s, _)| s)
}

/// `path` without its note extension (`.md`, `.markdown`, `.org`).
fn drop_ext(path: &str) -> &str {
    let name_at = path.rfind('/').map_or(0, |i| i + 1);
    match is_note(&path[name_at..]) {
        true => path.rsplit_once('.').map_or(path, |(s, _)| s),
        false => path,
    }
}

/// `path` written as a Markdown link destination (not in `<…>`): `%`,
/// space, ASCII control characters (tab, line breaks), `(`, `)`, `#`, `<`,
/// `>` and `&` are percent-encoded, so the destination parses whole, keeps
/// no anchor it did not have, holds no character reference (`&copy;`), and
/// decodes back to `path`.
pub fn markdown_destination(path: &str) -> String {
    encode_destination(path, false)
}

/// [`markdown_destination`], or inside `<…>` (`bracketed`), where space,
/// `(` and `)` stay as they are.
pub(crate) fn encode_destination(path: &str, bracketed: bool) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        let encode = match c {
            '%' | '#' | '<' | '>' | '&' => true,
            ' ' | '(' | ')' => !bracketed,
            c => c.is_ascii_control(),
        };
        match encode {
            true => out.push_str(&format!("%{:02X}", c as u32)),
            false => out.push(c),
        }
    }
    out
}

fn escape_label(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '[' | ']') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}
