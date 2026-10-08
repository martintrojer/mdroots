//! Tests for [`Workspace::goto`] and [`mdroots::names`], on in-memory trees.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mdroots::{NoEnumerator, Options, RootMode, Workspace, names};
use mdroots_core::MemFs;
use mdroots_roots::probe::FakeProbe;

fn open(files: &[(&str, &str)], at: &str) -> Workspace {
    let mut fs = MemFs::new();
    let mut probe = FakeProbe::new().home("/h");
    for (path, text) in files {
        fs = fs.with_file(path, text);
        probe = probe.file(path, text);
    }
    let opts = Options::default()
        .fs(Arc::new(fs))
        .probe(Arc::new(probe))
        .enumerator(Arc::new(NoEnumerator));
    Workspace::open_for(Path::new(at), opts).unwrap()
}

const NOTE: &str = "\
---
related: loose.md
---
# Note

See [the setup](#setup), [[#Usage]], [x][ref] and [[other#Part Two]].
Code: `other.md:3` and [[oth]] and [gone](missing.md) and [[twin]].

## Setup

## Usage

[ref]: other.md
";

fn ws() -> Workspace {
    open(
        &[
            ("/v/.mdroots", ""),
            ("/v/note.md", NOTE),
            ("/v/other.md", "# Other\n\n## Part Two\n"),
            ("/v/loose.md", "# Loose\n"),
            ("/v/a/twin.md", ""),
            ("/v/b/twin.md", ""),
        ],
        "/v/note.md",
    )
}

/// `goto` at the first byte of `needle` in NOTE.
fn at(ws: &Workspace, needle: &str) -> Option<mdroots::Goto> {
    let off = NOTE.find(needle).unwrap();
    ws.goto(Path::new("/v/note.md"), off).unwrap()
}

fn heading_text(r: std::ops::Range<usize>, text: &str) -> String {
    text[r].to_owned()
}

#[test]
fn in_document_anchors_go_to_the_heading() {
    let ws = ws();
    let g = at(&ws, "[the setup]").unwrap();
    assert_eq!(g.targets, [PathBuf::from("/v/note.md")]);
    assert_eq!(heading_text(g.heading.unwrap(), NOTE), "## Setup\n");
    let g = at(&ws, "[[#Usage]]").unwrap();
    assert_eq!(heading_text(g.heading.unwrap(), NOTE), "## Usage\n");
}

#[test]
fn a_wiki_anchor_in_another_note_matches_by_github_slug() {
    let ws = ws();
    let g = at(&ws, "[[other#").unwrap();
    assert_eq!(g.targets, [PathBuf::from("/v/other.md")]);
    let other = "# Other\n\n## Part Two\n";
    assert_eq!(heading_text(g.heading.unwrap(), other), "## Part Two\n");
}

#[test]
fn reference_and_frontmatter_links_resolve() {
    let ws = ws();
    let g = at(&ws, "[x][ref]").unwrap();
    assert_eq!(g.targets, [PathBuf::from("/v/other.md")]);
    assert_eq!(g.heading, None);
    let g = at(&ws, "loose.md").unwrap();
    assert_eq!(g.targets, [PathBuf::from("/v/loose.md")]);
}

#[test]
fn a_code_mention_carries_its_line() {
    let ws = ws();
    let g = at(&ws, "other.md:3").unwrap();
    assert_eq!(g.targets, [PathBuf::from("/v/other.md")]);
    assert_eq!(g.line, Some(3));
}

#[test]
fn partial_ambiguous_broken_and_no_link() {
    let ws = ws();
    // Partial step: "oth" is part of the stem "other".
    assert_eq!(
        at(&ws, "[[oth]]").unwrap().targets,
        [PathBuf::from("/v/other.md")]
    );
    let g = at(&ws, "[[twin]]").unwrap();
    assert_eq!(g.targets.len(), 2, "{g:?}");
    assert_eq!(at(&ws, "[gone]"), None);
    assert_eq!(at(&ws, "Code:"), None);
}

#[test]
fn names_are_lowercase_hyphenated() {
    assert_eq!(names::mode(RootMode::VcsEnumerated), "vcs-enumerated");
    assert_eq!(names::severity(mdroots::Severity::Warning), "warning");
    assert_eq!(names::step(None), "none");
    assert_eq!(names::status(mdroots::LinkStatus::Broken), "broken");
}
