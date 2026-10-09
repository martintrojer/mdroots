//! [`Workspace::extract_note`]: move a selection into a new note and link
//! to it. Never writes files.

use std::ops::Range;
use std::path::Path;

use mdroots_core::{Cancel, Error, ErrorKind};
use mdroots_syntax::slug;

use crate::rename::{TextEdit, WorkspaceEdit};
use crate::workspace::Workspace;

/// Longest title taken from the first line of a selection, in characters.
const TITLE_CHARS: usize = 60;

impl Workspace {
    /// The edit that moves the bytes `range` of the indexed Markdown note
    /// `from` (current text, the overlay wins) into a new note next to it
    /// and replaces them with a [`link_to`](Self::link_to) the new note.
    ///
    /// The title is the heading's text when the selection starts at the
    /// start of a heading's line, else the first non-blank line (trimmed,
    /// at most 60 characters), else "Untitled". The new file is
    /// `<slug of the title>.<from's extension>` in `from`'s directory
    /// (`untitled` for an empty slug), with `-2`, `-3`, … appended while
    /// the name exists on disk or in the index; this is re-checked on every
    /// call, so a name that exists is never offered (some editors truncate
    /// an existing file when they apply a create). Its content is the
    /// selection, after a `# <title>` line unless the selection starts with
    /// a heading, ending with a line break.
    ///
    /// `Unsupported` for an empty or invalid range, a note that is not
    /// indexed and Org notes.
    pub fn extract_note(
        &self,
        from: &Path,
        range: Range<usize>,
        cancel: &Cancel,
    ) -> Result<WorkspaceEdit, Error> {
        cancel.check()?;
        let rel = self.rel(from)?;
        let ext = rel
            .rsplit_once('.')
            .map(|(_, e)| e.to_owned())
            .filter(|e| ["md", "markdown"].iter().any(|m| e.eq_ignore_ascii_case(m)))
            .ok_or_else(|| refuse(format!("{rel}: not a Markdown note")))?;
        let store = self.store();
        let doc = store
            .document(&rel)
            .ok_or_else(|| refuse(format!("{rel}: not indexed")))?;
        let src = doc.source();
        if range.start >= range.end || src.get(range.clone()).is_none() {
            return Err(refuse(format!("{range:?}: not a selection in {rel}")));
        }
        let selected = src[range.clone()].to_owned();
        let line_start = |off: usize| src[..off].rfind('\n').map_or(0, |i| i + 1);
        let heading = doc
            .headings()
            .find(|h| line_start(h.range.start) == range.start && h.range.start < range.end)
            .map(|h| h.text.trim().to_owned());
        let starts_with_heading = heading.is_some();
        let title = match heading {
            Some(t) => t,
            None => first_line_title(&selected),
        };
        let stem = match slug::github(&title) {
            s if s.is_empty() => "untitled".to_owned(),
            s => s,
        };
        let dir = rel.rsplit_once('/').map_or("", |(d, _)| d);
        let taken = |name: &str| {
            let r = match dir.is_empty() {
                true => name.to_owned(),
                false => format!("{dir}/{name}"),
            };
            store.document(&r).is_some() || self.fs().stat(&self.abs(&r)).is_ok()
        };
        let name = (1..)
            .map(|n| match n {
                1 => format!("{stem}.{ext}"),
                n => format!("{stem}-{n}.{ext}"),
            })
            .find(|n| !taken(n))
            .expect("unbounded counter");
        let new_rel = match dir.is_empty() {
            true => name,
            false => format!("{dir}/{name}"),
        };
        // link_to takes the store lock itself.
        drop(store);
        let new_abs = self.abs(&new_rel);
        let link = self.link_to(from, &new_abs, None)?;
        let mut content = match starts_with_heading {
            true => String::new(),
            false => format!("# {title}\n\n"),
        };
        content.push_str(&selected);
        if !content.ends_with('\n') {
            content.push('\n');
        }
        Ok(WorkspaceEdit {
            edits: vec![(
                self.abs(&rel),
                vec![TextEdit {
                    range,
                    new_text: link,
                }],
            )],
            rename: None,
            create: vec![(new_abs, content)],
        })
    }
}

/// The first non-blank line, trimmed and cut to [`TITLE_CHARS`]; else
/// "Untitled".
fn first_line_title(s: &str) -> String {
    match s.lines().map(str::trim).find(|l| !l.is_empty()) {
        Some(l) => l
            .chars()
            .take(TITLE_CHARS)
            .collect::<String>()
            .trim()
            .to_owned(),
        None => "Untitled".to_owned(),
    }
}

fn refuse(msg: String) -> Error {
    Error::new(ErrorKind::Unsupported, format!("cannot extract: {msg}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_from_the_first_line() {
        assert_eq!(first_line_title("\n  Some text  \nmore"), "Some text");
        assert_eq!(first_line_title(" \n\t\n"), "Untitled");
        let long = "é".repeat(70);
        assert_eq!(first_line_title(&long), "é".repeat(60));
    }
}
