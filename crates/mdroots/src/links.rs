//! Link insertion in the root's style (docs/specs/index.md §3.2): the
//! style comes from an existing tool config, else the convention vote.

use std::path::Path;

use mdroots_core::{Error, ErrorKind};
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
    /// `[[stem]]` becomes `[[dir/stem]]` when another note shares the stem.
    /// Markdown paths are written by [`markdown_destination`]. A
    /// root-relative path that would resolve to another note from `from`
    /// (a note at the same path under `from`'s directory wins) gets a
    /// leading `/`, else (or for wiki paths) is written relative to `from`.
    /// A wiki target or label that cannot be written (`|`, `]`, a line break)
    /// is `Unsupported`, as are paths outside the root.
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
        // Whether `link` (written in `from`) leads to `target`, or to nothing
        // when `target` is not indexed. Text that does not parse as a link
        // in `from`'s dialect cannot be checked: it passes.
        let lands = |link: &str| -> bool {
            let doc = parse(link, Dialect::detect_from_path(Path::new(&from_rel)));
            let Some(l) = doc.links().next() else {
                return true;
            };
            let r = store.resolve_link(&from_rel, l, false);
            match r.targets.first() {
                Some(t) => *t == to_rel && r.status != LinkStatus::Ambiguous,
                None => store.document(&to_rel).is_none(),
            }
        };
        // The first candidate that lands, else the first.
        let pick = |candidates: Vec<String>, as_link: &dyn Fn(&str) -> String| -> String {
            let first = candidates[0].clone();
            candidates
                .into_iter()
                .find(|c| lands(&as_link(c)))
                .unwrap_or(first)
        };
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
            let dest = pick(dests, &|d| format!("[x]({d})"));
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
        let path = drop_ext(&to_rel).to_owned();
        let wiki_relative = drop_ext(&file_relative).to_owned();
        let as_wiki = |t: &str| format!("[[{t}]]");
        match style {
            LinkStyle::WikiStem => {
                let s = stem(&to_rel);
                let shared = store
                    .files()
                    .any(|f| f != to_rel && stem(f).to_lowercase() == s.to_lowercase());
                let mut c = Vec::new();
                if !shared {
                    c.push(s.to_owned());
                }
                c.extend([path.clone(), wiki_relative, format!("/{path}")]);
                wiki(pick(c, &as_wiki))
            }
            LinkStyle::MarkdownRelative { md_suffix } => {
                Ok(markdown(vec![file_relative], md_suffix))
            }
            LinkStyle::MarkdownRootRelative { md_suffix } => Ok(markdown(
                vec![to_rel.clone(), format!("/{to_rel}"), file_relative],
                md_suffix,
            )),
            // WikiPath, and any style added later.
            _ => wiki(pick(
                vec![path.clone(), wiki_relative, format!("/{path}")],
                &as_wiki,
            )),
        }
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
/// space, ASCII control characters (tab, line breaks), `(`, `)`, `#`, `<`
/// and `>` are percent-encoded, so the destination parses whole, keeps no
/// anchor it did not have, and decodes back to `path`.
pub fn markdown_destination(path: &str) -> String {
    encode_destination(path, false)
}

/// [`markdown_destination`], or inside `<…>` (`bracketed`), where space,
/// `(` and `)` stay as they are.
pub(crate) fn encode_destination(path: &str, bracketed: bool) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        let encode = match c {
            '%' | '#' | '<' | '>' => true,
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
