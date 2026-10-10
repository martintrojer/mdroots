//! Note queries with combinable filters, in the shape of `zk list`: tag
//! expressions, dates, link-graph filters, sort and limit. The CLI's
//! `mdroots notes` is a thin layer over [`NoteQuery`].

use std::path::PathBuf;
use std::time::SystemTime;

use crate::{Error, NoteSummary, Workspace};

/// A parsed tag expression in zk's syntax: `a,b` (and), `a OR b` / `a|b`,
/// `NOT a` / `-a`, globs (`year/201*`); names compare case-insensitively.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagExpr {
    _private: (),
}

impl TagExpr {
    /// Parse `s`; the error names the problem and its byte column.
    pub fn parse(s: &str) -> Result<TagExpr, String> {
        let _ = s;
        todo!("U2")
    }

    /// Whether a note with `tags` matches.
    pub fn matches(&self, tags: &[String]) -> bool {
        let _ = tags;
        todo!("U2")
    }
}

/// A sort key and direction (`KEY[+|-]`, zk's defaults: dates newest first,
/// path and title A–Z).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SortKey {
    Title,
    Path,
    Created,
    Modified,
}

/// What [`Workspace::query`] returns notes for. All filters combine (and).
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct NoteQuery {
    /// Only notes under these paths (files or directories).
    pub paths: Vec<PathBuf>,
    /// Never notes under these paths.
    pub exclude: Vec<PathBuf>,
    pub tag: Vec<TagExpr>,
    pub tagless: bool,
    /// Full-text match (all words, as `full_text`).
    pub matching: Option<String>,
    pub created_after: Option<SystemTime>,
    pub created_before: Option<SystemTime>,
    pub modified_after: Option<SystemTime>,
    pub modified_before: Option<SystemTime>,
    pub orphan: bool,
    pub missing_backlink: bool,
    /// Notes linking to any of these.
    pub link_to: Vec<PathBuf>,
    /// Notes linked from any of these.
    pub linked_by: Vec<PathBuf>,
    pub related: Vec<PathBuf>,
    /// `None`: title A–Z. `bool`: ascending.
    pub sort: Option<(SortKey, bool)>,
    pub limit: Option<usize>,
}

/// Parse a date filter value: `YYYY-MM-DD`, an RFC 3339 time, or zk's
/// relative forms (`today`, `yesterday`, `last week`, `2 days ago`, …).
pub fn parse_date(s: &str, now: SystemTime) -> Result<SystemTime, String> {
    let _ = (s, now);
    todo!("U2")
}

impl Workspace {
    /// The notes matching `q`, sorted and limited as it says. Graph filters
    /// on a lazy root are an error (the answer would be partial).
    pub fn query(&self, q: &NoteQuery) -> Result<Vec<NoteSummary>, Error> {
        let _ = (self, q);
        todo!("U2")
    }
}
