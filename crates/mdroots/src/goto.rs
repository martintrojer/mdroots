//! [`Workspace::goto`]: the target of the link under a cursor.

use std::ops::Range;
use std::path::{Path, PathBuf};

use mdroots_core::Error;
use mdroots_resolve::ladder::LinkStatus;
use mdroots_resolve::normalize::percent_decode;
use mdroots_syntax::{Anchor, Document, slug};

use crate::workspace::Workspace;

/// Where a link leads, for goto definition.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct Goto {
    /// Absolute, best first (several when ambiguous); may lie outside the
    /// root or be a file that is not indexed.
    pub targets: Vec<PathBuf>,
    /// Byte range of the heading the anchor names, in `targets[0]`.
    pub heading: Option<Range<usize>>,
    /// 1-based line of a code mention `path:LINE`.
    pub line: Option<u32>,
}

impl Workspace {
    /// The link at byte `offset` of the note's current text (any context,
    /// code included), resolved with goto semantics (the Partial step is
    /// allowed). `None` when there is no link there or it has no target
    /// (broken, external). An anchor is matched like
    /// [`MemStore::check_anchor`](mdroots_core::MemStore::check_anchor):
    /// the first heading whose slug, org `:ID:` or `:CUSTOM_ID:` equals it.
    pub fn goto(&self, path: &Path, offset: usize) -> Result<Option<Goto>, Error> {
        let rel = self.rel(path)?;
        let store = self.store();
        let Some(doc) = store.document(&rel) else {
            return Ok(None);
        };
        let Some(link) = doc.links().find(|l| l.range.contains(&offset)) else {
            return Ok(None);
        };
        let r = store.resolve_link(&rel, link, true);
        if r.targets.is_empty()
            || !matches!(
                r.status,
                LinkStatus::Resolved | LinkStatus::Ambiguous | LinkStatus::Unindexed
            )
        {
            return Ok(None);
        }
        let heading = link
            .target
            .anchor
            .as_ref()
            .and_then(|a| store.document(&r.targets[0]).and_then(|d| heading_of(d, a)));
        Ok(Some(Goto {
            targets: r.targets.iter().map(|t| self.abs(t)).collect(),
            heading,
            line: link.target.line,
        }))
    }
}

/// The range of the first heading `anchor` names in `doc`.
fn heading_of(doc: &Document, anchor: &Anchor) -> Option<Range<usize>> {
    let (a, by_slug) = match anchor {
        Anchor::Heading(a) => (a, true),
        Anchor::CustomId(a) => (a, false),
        _ => return None,
    };
    let dec = percent_decode(a);
    let forms: Vec<&str> = match dec == *a {
        true => vec![a],
        false => vec![a, &dec],
    };
    doc.headings()
        .find(|h| {
            forms.iter().any(|s| {
                (by_slug && (h.slug == *s || slug::github(s) == h.slug))
                    || h.id.as_deref() == Some(s)
                    || h.custom_id.as_deref() == Some(s)
            })
        })
        .map(|h| h.range.clone())
}
