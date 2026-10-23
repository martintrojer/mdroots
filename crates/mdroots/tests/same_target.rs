//! Tests for [`Workspace::same_target_links`] and [`Workspace::tag_at`], on
//! in-memory trees.

use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use mdroots::{NoEnumerator, Options, Workspace};
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
tags: [Alpha, beta]
---
# Note

First [[other]], then [again](other.md) and [[other|label]].
Parts: [[other#Part]], [p](other.md#Part), [[other#Part]] and [[#Usage]].
Broken [[gone]] and [[gone]] and <https://example.com> and <https://example.com>.
Code `[[other]]` and #rust and #rust again.

## Usage

Back to [the usage](#usage) and [[twin]].
";

fn ws() -> Workspace {
    open(
        &[
            ("/v/.mdroots", ""),
            ("/v/note.md", NOTE),
            ("/v/other.md", "# Other\n\n## Part\n"),
            ("/v/a/twin.md", ""),
            ("/v/b/twin.md", ""),
        ],
        "/v/note.md",
    )
}

/// The text of each range, in order.
fn texts(rs: &[Range<usize>]) -> Vec<&str> {
    rs.iter().map(|r| &NOTE[r.clone()]).collect()
}

/// `same_target_links` at the `n`th (0-based) occurrence of `needle`.
fn same(needle: &str, n: usize) -> Vec<Range<usize>> {
    let off = NOTE.match_indices(needle).nth(n).unwrap().0;
    ws().same_target_links(Path::new("/v/note.md"), off)
        .unwrap()
}

#[test]
fn links_to_one_note_without_an_anchor_group_in_source_order() {
    let want = ["[[other]]", "[again](other.md)", "[[other|label]]"];
    for needle in ["[[other]],", "[again]", "[[other|label"] {
        assert_eq!(texts(&same(needle, 0)), want, "from {needle}");
    }
}

#[test]
fn the_anchor_must_match_too() {
    let want = ["[[other#Part]]", "[p](other.md#Part)", "[[other#Part]]"];
    assert_eq!(texts(&same("[[other#Part]]", 0)), want);
    assert_eq!(texts(&same("[p]", 0)), want);
    assert_eq!(texts(&same("[[other#Part]]", 1)), want);
}

#[test]
fn a_link_in_code_groups_with_nothing() {
    // Offset 1 into the code span: no counting link there.
    assert!(same("[[other]]`", 0).is_empty());
}

#[test]
fn broken_and_external_links_group_with_nothing() {
    assert!(same("[[gone]]", 0).is_empty());
    assert!(same("[[gone]]", 1).is_empty());
    assert!(same("<https://example.com>", 0).is_empty());
}

#[test]
fn an_ambiguous_link_groups_by_its_best_target() {
    let r = same("[[twin]]", 0);
    assert_eq!(texts(&r), ["[[twin]]"]);
}

#[test]
fn a_heading_gives_the_in_note_links_naming_it() {
    // `[[#Usage]]` matches by slug text, `[the usage](#usage)` by slug.
    let want = ["[[#Usage]]", "[the usage](#usage)"];
    assert_eq!(texts(&same("## Usage", 0)), want);
    // On a link the anchor must be equal as written.
    assert_eq!(texts(&same("[[#Usage]]", 0)), ["[[#Usage]]"]);
}

#[test]
fn a_heading_without_links_and_plain_text_give_nothing() {
    assert!(same("# Note", 0).is_empty());
    assert!(same("First", 0).is_empty());
    let ws = ws();
    assert!(
        ws.same_target_links(Path::new("/v/other.md"), 3)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn tag_at_finds_body_and_frontmatter_tags_as_written() {
    let ws = ws();
    let at = |needle: &str, n: usize| {
        let off = NOTE.match_indices(needle).nth(n).unwrap().0;
        ws.tag_at(Path::new("/v/note.md"), off).unwrap()
    };
    let (name, r) = at("Alpha", 0).unwrap();
    assert_eq!((name.as_str(), &NOTE[r]), ("Alpha", "Alpha"));
    assert_eq!(at("beta", 0).unwrap().0, "beta");
    let (name, r) = at("#rust", 1).unwrap();
    assert_eq!(name, "rust");
    assert!(NOTE[r].contains("rust"));
    assert!(at("First", 0).is_none());
    assert!(at("tags:", 0).is_none());
}
