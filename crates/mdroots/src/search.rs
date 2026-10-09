//! Full-text search: [`Workspace::full_text`].
//!
//! A reconciler answers from the [FTS5](https://sqlite.org/fts5.html) table
//! of its DB (docs/specs/index.md §1.2). A peer (whose DB may lag behind the
//! text it serves), notes with an editor overlay, and memory mode use a
//! naive scan of the current text with the same rules: lowercase, tokens
//! are runs of letters and digits, every query term must appear (a term of
//! several tokens as a phrase), and the last term also matches as a prefix.
//! Both fold diacritics (`cafe` finds `café`), as FTS5's `unicode61
//! remove_diacritics 2` tokenizer does.

use std::collections::BTreeSet;
use std::path::PathBuf;

use mdroots_core::{Cancel, Error};
use mdroots_index::Role;

use crate::workspace::Workspace;

/// Most characters of a [`Hit::snippet`].
const SNIPPET_CHARS: usize = 120;

/// One note matching a [`Workspace::full_text`] query.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// Absolute.
    pub path: PathBuf,
    /// 0-based line of the first line containing the first query term in
    /// the note's current text; 0 if none does.
    pub line: u32,
    /// Matching text on one line: whitespace runs collapsed to one space,
    /// at most 120 characters.
    pub snippet: String,
}

impl Workspace {
    /// Notes containing every term of `query` (plain text, split on
    /// whitespace; terms without a letter or digit are dropped; the last
    /// term also matches as a prefix), case-insensitively. At most `limit`
    /// hits: the DB's, ranked, first; then the naive scan's by path (see
    /// the module docs). A query without terms finds nothing.
    pub fn full_text(&self, query: &str, limit: usize, cancel: &Cancel) -> Result<Vec<Hit>, Error> {
        cancel.check()?;
        let terms = Terms::new(query);
        if terms.0.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        // Lock order: the index guard is dropped before overlays and the
        // store are taken (overlays, then the store).
        let overlay_count = self.overlays().len();
        let db_hits = {
            let index = self.index();
            match index.as_ref() {
                Some(ix) if ix.locks.role() == Role::Reconciler => {
                    Some(ix.db.search(query, limit.saturating_add(overlay_count))?)
                }
                _ => None,
            }
        };
        let overlays = self.overlays();
        let store = self.store();
        let mut hits = Vec::new();
        let scan: Vec<&str> = match db_hits {
            Some(db_hits) => {
                let files: BTreeSet<&str> = store.files().collect();
                for (rel, snippet) in db_hits {
                    if overlays.contains_key(&rel) || !files.contains(rel.as_str()) {
                        continue;
                    }
                    let line = store
                        .document(&rel)
                        .map_or(0, |d| terms.first_line(d.source()));
                    hits.push(Hit {
                        path: self.abs(&rel),
                        line,
                        snippet: one_line(&snippet),
                    });
                }
                overlays.keys().map(String::as_str).collect()
            }
            // A peer or memory mode: every note.
            None => store.files().collect(),
        };
        let mut naive = Vec::new();
        for rel in scan {
            cancel.check()?;
            let Some(doc) = store.document(rel) else {
                continue;
            };
            let text = doc.source();
            if !terms.matches(&tokens(text)) {
                continue;
            }
            let line = terms.first_line(text);
            let snippet = text.lines().nth(line as usize).unwrap_or_default();
            naive.push(Hit {
                path: self.abs(rel),
                line,
                snippet: one_line(snippet),
            });
        }
        naive.sort_by(|a, b| a.path.cmp(&b.path));
        hits.extend(naive);
        hits.truncate(limit);
        Ok(hits)
    }
}

/// The query's terms, each as its lowercase tokens.
struct Terms(Vec<Vec<String>>);

impl Terms {
    fn new(query: &str) -> Terms {
        Terms(
            query
                .split_whitespace()
                .map(tokens)
                .filter(|t| !t.is_empty())
                .collect(),
        )
    }

    /// Every term appears in `toks`.
    fn matches(&self, toks: &[String]) -> bool {
        let last = self.0.len() - 1;
        self.0
            .iter()
            .enumerate()
            .all(|(i, t)| has_phrase(toks, t, i == last))
    }

    /// The first line containing the first term, else 0.
    fn first_line(&self, text: &str) -> u32 {
        let first = &self.0[0];
        let prefix = self.0.len() == 1;
        text.lines()
            .position(|l| has_phrase(&tokens(l), first, prefix))
            .map_or(0, |n| u32::try_from(n).unwrap_or(u32::MAX))
    }
}

/// `phrase` occurs in `toks` as consecutive tokens; with `prefix`, its last
/// token may be a prefix of the token it meets.
fn has_phrase(toks: &[String], phrase: &[String], prefix: bool) -> bool {
    let n = phrase.len();
    toks.windows(n).any(|w| {
        w.iter()
            .zip(phrase)
            .enumerate()
            .all(|(i, (tok, p))| match prefix && i == n - 1 {
                true => tok.starts_with(p.as_str()),
                false => tok == p,
            })
    })
}

/// Runs of letters and digits, lowercase, diacritics removed (NFD, then
/// combining marks dropped).
fn tokens(s: &str) -> Vec<String> {
    use unicode_normalization::UnicodeNormalization;
    use unicode_normalization::char::is_combining_mark;
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| {
            t.nfd()
                .filter(|c| !is_combining_mark(*c))
                .collect::<String>()
                .to_lowercase()
        })
        .collect()
}

/// `s` trimmed with whitespace runs collapsed to one space, at most
/// [`SNIPPET_CHARS`] characters.
fn one_line(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(SNIPPET_CHARS)
        .collect()
}
