//! One diagnostics policy over a [`MemStore`] (docs/specs/index.md §3.3), so
//! every front end (CLI, language server) reports the same thing.
//!
//! Diagnosed: broken explicit links and their missing anchors, ambiguous
//! links (info, with every candidate as related), and frontmatter that does
//! not parse (info). Never diagnosed: links in code or comments, implicit
//! links, external links, targets on disk but not indexed, and hint
//! resolutions.

use std::ops::Range;

use mdroots_resolve::dialect::{Severity, VoteResult, link_severity};
use mdroots_resolve::ladder::{LinkStatus, Resolution};
use mdroots_syntax::{Anchor, Confidence, Link, LinkKind};

use crate::memstore::{AnchorStatus, MemStore, counts};

/// What a [`Diagnostic`] reports.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagCode {
    /// An explicit link whose target is neither indexed nor on disk.
    BrokenLink,
    /// A link to one note in which its anchor does not exist.
    BrokenAnchor,
    /// A link with several equally good targets.
    AmbiguousLink,
    /// Lazy roots: an unresolved link that only a full index could check.
    NotInWorkingSet,
    /// Frontmatter that does not parse; the rest of the note is indexed.
    InvalidFrontmatter,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// Byte range in the note's source.
    pub range: Range<usize>,
    pub severity: Severity,
    pub code: DiagCode,
    pub message: String,
    /// Root-relative paths of related notes (ambiguous candidates).
    pub related: Vec<String>,
}

/// The per-root decisions behind [`diagnostics`](Self::diagnostics).
#[derive(Debug, Clone, PartialEq)]
pub struct DiagnosticPolicy {
    /// Severity of broken links and anchors; `None` = not reported.
    pub broken: Option<Severity>,
    /// Share of explicit links in referencing contexts that resolve
    /// (indexed, ambiguous or on disk); `None` when there are none.
    pub resolved_share: Option<f64>,
    /// The store holds a working set, not the whole root: only links whose
    /// target can be checked with a `stat` are reported broken.
    pub lazy: bool,
}

impl DiagnosticPolicy {
    /// Decide the broken-link severity for the store's root: the root's
    /// config wins (zk `dead-link`), else the share of resolving links. In
    /// lazy mode the share counts only `stat`-checkable links.
    pub fn for_store(store: &MemStore, lazy: bool) -> DiagnosticPolicy {
        let (mut total, mut resolved) = (0u32, 0u32);
        for rel in store.files() {
            for (l, r) in store.links(rel) {
                if !diagnosable(&l) || r.status == LinkStatus::External {
                    continue;
                }
                if lazy && !stat_checkable(&l) {
                    continue;
                }
                total += 1;
                if matches!(
                    r.status,
                    LinkStatus::Resolved | LinkStatus::Ambiguous | LinkStatus::Unindexed
                ) {
                    resolved += 1;
                }
            }
        }
        let resolved_share = (total > 0).then(|| f64::from(resolved) / f64::from(total));
        let conv = store.conventions();
        let broken = match conv.dead_link_off {
            true => None,
            false => {
                let mut vote = VoteResult::default();
                vote.explicit_links = total;
                vote.resolved_share = resolved_share.unwrap_or(1.0) as f32;
                Some(link_severity(conv, &vote))
            }
        };
        DiagnosticPolicy {
            broken,
            resolved_share,
            lazy,
        }
    }

    /// Diagnostics for one indexed note, sorted by start offset (then by
    /// code). Empty for a note that is not indexed.
    pub fn diagnostics(&self, store: &MemStore, root_rel: &str) -> Vec<Diagnostic> {
        let Some(doc) = store.document(root_rel) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if let Some(fm) = doc.frontmatter()
            && let Some(err) = &fm.error
        {
            out.push(Diagnostic {
                range: fm.range.clone(),
                severity: Severity::Info,
                code: DiagCode::InvalidFrontmatter,
                message: format!("frontmatter: {err}"),
                related: Vec::new(),
            });
        }
        for (l, r) in store.links(root_rel) {
            if diagnosable(&l)
                && let Some(d) = self.link_diagnostic(store, &l, r)
            {
                out.push(d);
            }
        }
        out.sort_by_key(|d| (d.range.start, rank(d.code)));
        out
    }

    fn link_diagnostic(&self, store: &MemStore, l: &Link, r: Resolution) -> Option<Diagnostic> {
        let diag = |severity, code, message, related| Diagnostic {
            range: l.range.clone(),
            severity,
            code,
            message,
            related,
        };
        let raw = &l.target.raw;
        match r.status {
            _ if r.hint => None,
            LinkStatus::Broken if self.lazy && !stat_checkable(l) => Some(diag(
                Severity::Hint,
                DiagCode::NotInWorkingSet,
                format!("not in indexed set: {raw}"),
                Vec::new(),
            )),
            LinkStatus::Broken => Some(diag(
                self.broken?,
                DiagCode::BrokenLink,
                format!("broken link: {raw}"),
                Vec::new(),
            )),
            LinkStatus::Ambiguous => Some(diag(
                Severity::Info,
                DiagCode::AmbiguousLink,
                format!("ambiguous link: {raw}"),
                r.targets,
            )),
            LinkStatus::Resolved => {
                let anchor = l.target.anchor.as_ref()?;
                let [target] = r.targets.as_slice() else {
                    return None;
                };
                if store.check_anchor(target, anchor) != AnchorStatus::Missing {
                    return None;
                }
                let frag = match anchor {
                    Anchor::Block(b) => format!("^{b}"),
                    Anchor::Heading(a) | Anchor::CustomId(a) => a.clone(),
                    _ => return None,
                };
                Some(diag(
                    self.broken?,
                    DiagCode::BrokenAnchor,
                    format!("missing anchor: #{frag}"),
                    Vec::new(),
                ))
            }
            _ => None,
        }
    }
}

/// Explicit links in referencing contexts: the set `MemStore::broken` checks.
fn diagnosable(l: &Link) -> bool {
    l.confidence == Confidence::Explicit && counts(l)
}

/// Whether a `stat` can tell if the target exists without a full index:
/// path link forms, or any target containing `/`. A bare `[[stem]]` needs
/// the index, whatever its apparent extension.
fn stat_checkable(l: &Link) -> bool {
    matches!(
        l.kind,
        LinkKind::Markdown | LinkKind::Reference | LinkKind::Image | LinkKind::Html | LinkKind::Org
    ) || l.target.path.contains('/')
}

/// Order of diagnostics that start at the same offset.
fn rank(c: DiagCode) -> u8 {
    match c {
        DiagCode::InvalidFrontmatter => 0,
        DiagCode::BrokenLink => 1,
        DiagCode::BrokenAnchor => 2,
        DiagCode::AmbiguousLink => 3,
        DiagCode::NotInWorkingSet => 4,
    }
}
