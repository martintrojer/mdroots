//! Lowercase-hyphenated names for the facade's enums. All but `Severity`
//! are `#[non_exhaustive]`: a variant added later prints `unknown` until it
//! gets a name here.

use crate::{LinkStatus, ResolveStep, RootMode, Severity};

pub fn severity(s: Severity) -> &'static str {
    match s {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Info => "info",
        Severity::Hint => "hint",
    }
}

pub fn mode(m: RootMode) -> &'static str {
    match m {
        RootMode::Marker => "marker",
        RootMode::Vcs => "vcs",
        RootMode::TrackedOnly => "tracked-only",
        RootMode::IndexDriven => "index-driven",
        RootMode::Loose => "loose",
        RootMode::SingleFile => "single-file",
        RootMode::Lazy => "lazy",
        RootMode::VcsEnumerated => "vcs-enumerated",
        _ => "unknown",
    }
}

pub fn step(s: Option<ResolveStep>) -> &'static str {
    match s {
        None => "none",
        Some(ResolveStep::FileRelative) => "file-relative",
        Some(ResolveStep::RootRelative) => "root-relative",
        Some(ResolveStep::SiteRooted) => "site-rooted",
        Some(ResolveStep::Stem) => "stem",
        Some(ResolveStep::Id) => "id",
        Some(ResolveStep::Title) => "title",
        Some(ResolveStep::Alias) => "alias",
        Some(ResolveStep::DialectTransform) => "dialect-transform",
        Some(ResolveStep::Partial) => "partial",
        Some(_) => "unknown",
    }
}

pub fn status(s: LinkStatus) -> &'static str {
    match s {
        LinkStatus::Resolved => "resolved",
        LinkStatus::Ambiguous => "ambiguous",
        LinkStatus::Unindexed => "unindexed",
        LinkStatus::Broken => "broken",
        LinkStatus::External => "external",
        LinkStatus::Unchecked => "unchecked",
        _ => "unknown",
    }
}
