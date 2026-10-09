//! Link insertion in the root's style (docs/specs/index.md §3.2): the
//! style comes from an existing tool config, else the convention vote.

use std::path::Path;

use mdroots_core::{Error, ErrorKind};
use mdroots_resolve::dialect::{LinkStyle, link_style};

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
    /// Markdown paths have `%`, space, `(` and `)` percent-encoded. A wiki target
    /// or label that cannot be written (`|`, `]`, a line break) is
    /// `Unsupported`, as are paths outside the root.
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
        let markdown = |path: String, md_suffix: bool| -> String {
            let path = match md_suffix {
                true => path,
                false => drop_ext(&path).to_owned(),
            };
            let text = match label {
                Some(l) => l.to_owned(),
                None => store
                    .document(&to_rel)
                    .map(|d| title(&to_rel, d))
                    .unwrap_or_else(|| stem(&to_rel).to_owned()),
            };
            format!("[{}]({})", escape_label(&text), encode(&path))
        };
        match style {
            LinkStyle::WikiStem => {
                let s = stem(&to_rel);
                let shared = store
                    .files()
                    .any(|f| f != to_rel && stem(f).to_lowercase() == s.to_lowercase());
                match shared {
                    true => wiki(drop_ext(&to_rel).to_owned()),
                    false => wiki(s.to_owned()),
                }
            }
            LinkStyle::MarkdownRelative { md_suffix } => {
                let dir = Path::new(&from_rel).parent().unwrap_or(Path::new(""));
                Ok(markdown(relative(dir, Path::new(&to_rel)), md_suffix))
            }
            LinkStyle::MarkdownRootRelative { md_suffix } => {
                Ok(markdown(to_rel.clone(), md_suffix))
            }
            // WikiPath, and any style added later.
            _ => wiki(drop_ext(&to_rel).to_owned()),
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

fn encode(path: &str) -> String {
    path.replace('%', "%25")
        .replace(' ', "%20")
        .replace('(', "%28")
        .replace(')', "%29")
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
