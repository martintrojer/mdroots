//! Lowercase-hyphenated names for the facade's enums. All but `Severity`
//! are `#[non_exhaustive]`: a variant added later prints `unknown` until it
//! gets a name here.

use crate::{DialectMarker, LinkStatus, ResolveStep, RootMode, Severity, Source};

pub fn severity(s: Severity) -> &'static str {
    mdroots_resolve::dialect::severity_name(s)
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

/// Where a setting came from, as `roots` and the editor info command print it: `zk .zk/config.toml
/// link-format`, `zk default`, `vote` or `default`.
pub fn source(s: &Source) -> String {
    match s {
        Source::Config { tool, file, key } => format!("{tool} {file} {key}"),
        Source::Marker(m) => format!("{} default", marker(*m)),
        Source::Vote => "vote".to_owned(),
        Source::Default => "default".to_owned(),
        _ => "unknown".to_owned(),
    }
}

pub fn marker(m: DialectMarker) -> &'static str {
    match m {
        DialectMarker::Zk => "zk",
        DialectMarker::Obsidian => "obsidian",
        DialectMarker::Marksman => "marksman",
        DialectMarker::Foam => "foam",
        DialectMarker::Dendron => "dendron",
        DialectMarker::Logseq => "logseq",
        DialectMarker::OrgRoam => "org-roam",
        DialectMarker::Gollum => "gollum",
        DialectMarker::Mkdocs => "mkdocs",
        DialectMarker::Docusaurus => "docusaurus",
        DialectMarker::Hugo => "hugo",
        DialectMarker::Jekyll => "jekyll",
        DialectMarker::MdBook => "mdbook",
        DialectMarker::Zettlr => "zettlr",
        _ => "unknown",
    }
}
