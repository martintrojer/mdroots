//! Read-only note queries for editors: outline, fuzzy note search, preview
//! and current text.

use std::collections::BTreeMap;
use std::ops::Range;
use std::path::Path;

use mdroots_core::{Error, ErrorKind};
use mdroots_syntax::{Document, Heading, Value};

use crate::goto::heading_index;
use crate::workspace::{Backlink, NoteSummary, Workspace, title};

/// What a hover or picker shows for a note.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct Preview {
    /// Frontmatter title, else the first level-1 heading, else the file stem.
    pub title: String,
    /// Frontmatter entries in document order; lists joined with `", "`,
    /// null as `""`.
    pub frontmatter: Vec<(String, String)>,
    /// The first lines after the frontmatter, leading blank lines dropped.
    pub excerpt: String,
}

impl Workspace {
    /// The note's headings in source order (overlay wins). Empty for a path
    /// inside the root that is not indexed.
    pub fn outline(&self, path: &Path) -> Result<Vec<Heading>, Error> {
        let rel = self.rel(path)?;
        let store = self.store();
        Ok(store
            .document(&rel)
            .map(|d| d.headings().cloned().collect())
            .unwrap_or_default())
    }

    /// Notes whose title, file stem or a frontmatter alias contains `query`
    /// as a case-insensitive subsequence, best first: an exact match, then a
    /// prefix, a substring, and a subsequence with fewer gaps; ties by path.
    /// An empty query lists every note by path. At most `limit` results.
    pub fn search_notes(&self, query: &str, limit: usize) -> Vec<NoteSummary> {
        let notes = self.notes();
        let q: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
        if q.is_empty() {
            return notes.into_iter().take(limit).collect();
        }
        let store = self.store();
        let mut scored: Vec<((u8, usize), NoteSummary)> = notes
            .into_iter()
            .filter_map(|n| {
                let rel = self.rel(&n.path).ok()?;
                let doc = store.document(&rel)?;
                let score = candidates(&rel, doc, &n.title)
                    .iter()
                    .filter_map(|c| score(&q, c))
                    .min()?;
                Some((score, n))
            })
            .collect();
        scored.sort_by(|(a, x), (b, y)| a.cmp(b).then_with(|| x.path.cmp(&y.path)));
        scored.into_iter().take(limit).map(|(_, n)| n).collect()
    }

    /// Title, frontmatter and the first `max_lines` lines of the note's
    /// current text. `Unsupported` for a note that is not indexed.
    pub fn preview(&self, path: &Path, max_lines: usize) -> Result<Preview, Error> {
        let rel = self.rel(path)?;
        let store = self.store();
        let doc = store.document(&rel).ok_or_else(|| not_indexed(&rel))?;
        let (frontmatter, body_start) = match doc.frontmatter() {
            Some(f) => (
                f.entries()
                    .iter()
                    .map(|(k, v)| (k.clone(), display(v)))
                    .collect(),
                f.range.end,
            ),
            None => (Vec::new(), 0),
        };
        let body = doc.source().get(body_start..).unwrap_or_default();
        let excerpt = body
            .lines()
            .skip_while(|l| l.trim().is_empty())
            .take(max_lines)
            .collect::<Vec<_>>()
            .join("\n");
        Ok(Preview {
            title: title(&rel, doc),
            frontmatter,
            excerpt,
        })
    }

    /// Byte range of the note's frontmatter block, from its opening
    /// delimiter to the end of its closing one (overlay wins). `None` when
    /// it has none or is not indexed.
    pub fn frontmatter_range(&self, path: &Path) -> Result<Option<Range<usize>>, Error> {
        let rel = self.rel(path)?;
        let store = self.store();
        Ok(store
            .document(&rel)
            .and_then(|d| d.frontmatter())
            .map(|f| f.range.clone()))
    }

    /// For each heading of the note (by index in [`outline`](Self::outline))
    /// that links from other notes name by anchor, the number of such
    /// links; headings without any are left out. A link names the first
    /// heading its anchor matches, as in [`goto`](Self::goto).
    pub fn anchor_backlinks(&self, path: &Path) -> Result<Vec<(usize, usize)>, Error> {
        let mut counts: BTreeMap<usize, usize> = BTreeMap::new();
        for (i, _) in self.anchored(path)? {
            *counts.entry(i).or_default() += 1;
        }
        Ok(counts.into_iter().collect())
    }

    /// The links from other notes naming heading `heading` (an index in
    /// [`outline`](Self::outline)) by anchor, sorted by source path.
    pub fn heading_backlinks(&self, path: &Path, heading: usize) -> Result<Vec<Backlink>, Error> {
        Ok(self
            .anchored(path)?
            .into_iter()
            .filter(|(i, _)| *i == heading)
            .map(|(_, b)| b)
            .collect())
    }

    /// Backlinks from other notes with an anchor naming a heading of the
    /// note, with that heading's index.
    fn anchored(&self, path: &Path) -> Result<Vec<(usize, Backlink)>, Error> {
        let rel = self.rel(path)?;
        let store = self.store();
        let Some(doc) = store.document(&rel) else {
            return Ok(Vec::new());
        };
        Ok(store
            .backlinks(&rel)
            .into_iter()
            .filter(|(from, _)| *from != rel)
            .filter_map(|(from, l)| {
                let i = heading_index(doc, l.target.anchor.as_ref()?)?;
                Some((i, self.backlink(&store, from, &l)?))
            })
            .collect())
    }

    /// The note's current text (the overlay wins). `Unsupported` for a note
    /// that is not indexed.
    pub fn text(&self, path: &Path) -> Result<String, Error> {
        let rel = self.rel(path)?;
        let store = self.store();
        let doc = store.document(&rel).ok_or_else(|| not_indexed(&rel))?;
        Ok(doc.source().to_owned())
    }
}

fn not_indexed(rel: &str) -> Error {
    Error::new(ErrorKind::Unsupported, format!("{rel}: not indexed"))
}

fn display(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        Value::List(items) => items.join(", "),
        _ => String::new(),
    }
}

/// The names a note is searched by: title, file stem and aliases.
fn candidates(rel: &str, doc: &Document, title: &str) -> Vec<String> {
    let stem = Path::new(rel)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut v = vec![title.to_owned(), stem];
    if let Some(f) = doc.frontmatter() {
        v.extend(f.aliases().into_iter().map(str::to_owned));
    }
    v
}

/// Lower is better: (0 exact | 1 prefix | 2 substring | 3 subsequence,
/// gaps). `None` when `q` (lowercase) is not a subsequence of `name`.
fn score(q: &[char], name: &str) -> Option<(u8, usize)> {
    let n: Vec<char> = name.chars().flat_map(char::to_lowercase).collect();
    if n == q {
        return Some((0, 0));
    }
    if n.starts_with(q) {
        return Some((1, 0));
    }
    if n.windows(q.len()).any(|w| w == q) {
        return Some((2, 0));
    }
    // Greedy subsequence match; a gap is a run of skipped chars between
    // two matched ones.
    let mut gaps = 0;
    let mut qi = 0;
    let mut last: Option<usize> = None;
    for (i, c) in n.iter().enumerate() {
        if qi < q.len() && *c == q[qi] {
            if last.is_some_and(|l| i > l + 1) {
                gaps += 1;
            }
            last = Some(i);
            qi += 1;
        }
    }
    (qi == q.len()).then_some((3, gaps))
}

#[cfg(test)]
mod tests {
    use super::score;

    fn s(q: &str, n: &str) -> Option<(u8, usize)> {
        score(&q.chars().collect::<Vec<_>>(), n)
    }

    #[test]
    fn score_tiers() {
        assert_eq!(s("ab", "AB"), Some((0, 0)));
        assert_eq!(s("ab", "abc"), Some((1, 0)));
        assert_eq!(s("bc", "abcd"), Some((2, 0)));
        assert_eq!(s("ac", "abc"), Some((3, 1)));
        assert_eq!(s("ace", "abcde"), Some((3, 2)));
        assert_eq!(s("x", "abc"), None);
    }
}
