//! Tests for the link-graph queries (`orphans`, `missing_backlinks`,
//! `related`, `links_to`, `links_from`) on in-memory roots. `related`
//! follows `zk list --related` of [zk](https://github.com/zk-org/zk).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use mdroots::{ErrorKind, IndexMode, NoEnumerator, Options, Workspace};
use mdroots_core::MemFs;
use mdroots_roots::probe::FakeProbe;

/// A marker root at `/v` holding `files` (paths relative to `/v`).
fn open(files: &[(&str, &str)]) -> Workspace {
    let mut fs = MemFs::new().with_file("/v/.mdroots", "");
    let mut probe = FakeProbe::new().home("/h").file("/v/.mdroots", "");
    for (rel, text) in files {
        let path = format!("/v/{rel}");
        fs = fs.with_file(&path, text);
        probe = probe.file(&path, text);
    }
    let opts = Options::default()
        .fs(Arc::new(fs))
        .probe(Arc::new(probe))
        .enumerator(Arc::new(NoEnumerator))
        .index(IndexMode::Memory);
    let first = format!("/v/{}", files[0].0);
    Workspace::open_for(Path::new(&first), opts).unwrap()
}

fn v(rel: &str) -> PathBuf {
    PathBuf::from(format!("/v/{rel}"))
}

fn orphans(w: &Workspace) -> Vec<String> {
    names(w.orphans().unwrap().into_iter().map(|n| n.path))
}

fn names(paths: impl IntoIterator<Item = PathBuf>) -> Vec<String> {
    paths
        .into_iter()
        .map(|p| p.strip_prefix("/v").unwrap().display().to_string())
        .collect()
}

fn pairs(w: &Workspace) -> Vec<(String, String)> {
    w.missing_backlinks()
        .unwrap()
        .into_iter()
        .map(|(a, b)| (names([a]).remove(0), names([b]).remove(0)))
        .collect()
}

#[test]
fn a_linked_note_is_not_an_orphan_and_the_index_note_is() {
    let w = open(&[
        ("index.md", "[[a]] and [b](b.md)\n"),
        ("a.md", "# A\n"),
        ("b.md", "back to [[a]]\n"),
    ]);
    // Nothing links to the index note: an orphan, to be excluded by path.
    assert_eq!(orphans(&w), ["index.md"]);
}

#[test]
fn a_self_link_does_not_count() {
    let w = open(&[("a.md", "[[a]] and [me](a.md)\n"), ("b.md", "")]);
    assert_eq!(orphans(&w), ["a.md", "b.md"]);
    assert!(pairs(&w).is_empty());
    assert!(w.links_to(&v("a.md")).unwrap().is_empty());
    assert!(w.links_from(&v("a.md")).unwrap().is_empty());
}

#[test]
fn links_in_code_do_not_count() {
    let w = open(&[
        ("a.md", "`[[b]]` and\n\n```\n[c](c.md)\n```\n"),
        ("b.md", ""),
        ("c.md", ""),
    ]);
    assert_eq!(orphans(&w), ["a.md", "b.md", "c.md"]);
    assert!(pairs(&w).is_empty());
}

#[test]
fn footnotes_do_not_count() {
    let w = open(&[("a.md", "Text[^b].\n\n[^b]: a footnote\n"), ("b.md", "")]);
    assert_eq!(orphans(&w), ["a.md", "b.md"]);
}

#[test]
fn a_frontmatter_link_counts() {
    let w = open(&[
        ("a.md", "---\nrelated: \"[[b]]\"\nsee: c.md\n---\n# A\n"),
        ("b.md", ""),
        ("c.md", ""),
    ]);
    assert_eq!(orphans(&w), ["a.md"]);
    assert_eq!(
        pairs(&w),
        [
            ("a.md".into(), "b.md".into()),
            ("a.md".into(), "c.md".into())
        ]
    );
}

#[test]
fn an_ambiguous_link_counts_for_every_candidate() {
    let w = open(&[
        ("a.md", "[[twin]]\n"),
        ("one/twin.md", ""),
        ("two/twin.md", ""),
    ]);
    assert_eq!(orphans(&w), ["a.md"]);
    assert_eq!(
        names(
            w.links_from(&v("a.md"))
                .unwrap()
                .into_iter()
                .map(|n| n.path)
        ),
        ["one/twin.md", "two/twin.md"]
    );
}

#[test]
fn a_broken_link_does_not_count() {
    let w = open(&[("a.md", "[[gone]] and [x](missing.md)\n"), ("b.md", "")]);
    assert_eq!(orphans(&w), ["a.md", "b.md"]);
    assert!(pairs(&w).is_empty());
}

#[test]
fn missing_backlinks_are_note_pairs_deduplicated_and_sorted() {
    let w = open(&[
        ("a.md", "[[b]] [[b]] [b](b.md) [[c]]\n"),
        ("b.md", "[[a]]\n"),
        ("c.md", "[[d]]\n"),
        ("d.md", ""),
    ]);
    assert_eq!(
        pairs(&w),
        [
            ("a.md".into(), "c.md".into()),
            ("c.md".into(), "d.md".into())
        ]
    );
}

#[test]
fn links_to_and_from_are_deduplicated_notes() {
    let w = open(&[
        ("a.md", "[[b]] [[b]] [[c]]\n"),
        ("b.md", "[[a]]\n"),
        ("c.md", "[[b]]\n"),
    ]);
    let paths = |v: Vec<mdroots::NoteSummary>| names(v.into_iter().map(|n| n.path));
    assert_eq!(paths(w.links_from(&v("a.md")).unwrap()), ["b.md", "c.md"]);
    assert_eq!(paths(w.links_to(&v("b.md")).unwrap()), ["a.md", "c.md"]);
}

/// ```text
/// a → b, c → a, b → d, c → d, c → e, e → f, g → b, a → h, h → a, h → d
/// ```
/// N(a) = {b, c, h}. Two hops: via b {d, g}, via c {d, e}, via h {d}; a's
/// own neighbours are dropped. Scores: d 3, e 1, g 1. f is three hops away.
#[test]
fn related_is_distance_two_scored_by_shared_neighbours() {
    let w = open(&[
        ("a.md", "[[b]] [[h]]\n"),
        ("b.md", "[[d]]\n"),
        ("c.md", "[[a]] [[d]] [[e]]\n"),
        ("d.md", ""),
        ("e.md", "[[f]]\n"),
        ("f.md", ""),
        ("g.md", "[[b]]\n"),
        ("h.md", "[[a]] [[d]]\n"),
    ]);
    let got: Vec<(String, usize)> = w
        .related(&v("a.md"))
        .unwrap()
        .into_iter()
        .map(|(n, s)| (names([n.path]).remove(0), s))
        .collect();
    assert_eq!(
        got,
        [("d.md".into(), 3), ("e.md".into(), 1), ("g.md".into(), 1)]
    );
    // A directly linked note is never related, even when it is also two
    // hops away (c is a neighbour of a and of h).
    assert!(got.iter().all(|(n, _)| n != "c.md"));
}

#[test]
fn related_of_an_unlinked_note_is_empty() {
    let w = open(&[("a.md", ""), ("b.md", "[[c]]\n"), ("c.md", "")]);
    assert!(w.related(&v("a.md")).unwrap().is_empty());
}

#[test]
fn a_path_outside_the_root_is_unsupported() {
    let w = open(&[("a.md", "")]);
    let e = w.related(Path::new("/elsewhere/x.md")).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Unsupported);
}

#[test]
fn graph_queries_on_a_lazy_root_are_unsupported() {
    // A single-file workspace holds one note: lazy.
    let fs = MemFs::new()
        .with_file("/d/a.md", "[[b]]\n")
        .with_file("/d/b.md", "");
    let probe = FakeProbe::new()
        .home("/h")
        .file("/d/a.md", "[[b]]\n")
        .file("/d/b.md", "");
    let opts = Options::default()
        .fs(Arc::new(fs))
        .probe(Arc::new(probe))
        .enumerator(Arc::new(NoEnumerator))
        .index(IndexMode::Memory);
    let w = Workspace::open_single(Path::new("/d/a.md"), opts).unwrap();
    assert_eq!(w.freshness(), mdroots::Freshness::Lazy);
    assert_eq!(w.orphans().unwrap_err().kind(), ErrorKind::Unsupported);
    assert_eq!(
        w.missing_backlinks().unwrap_err().kind(),
        ErrorKind::Unsupported
    );
    let a = Path::new("/d/a.md");
    assert_eq!(w.related(a).unwrap_err().kind(), ErrorKind::Unsupported);
    assert_eq!(w.links_to(a).unwrap_err().kind(), ErrorKind::Unsupported);
    assert_eq!(w.links_from(a).unwrap_err().kind(), ErrorKind::Unsupported);
}

/// 3,000 notes, each linking to the next three: every query builds the
/// graph once, so they stay linear in notes + links.
#[test]
fn graph_queries_stay_fast_on_3000_notes() {
    const N: usize = 3_000;
    let texts: Vec<(String, String)> = (0..N)
        .map(|i| {
            let links: String = (1..=3).map(|d| format!("[[n{}]] ", (i + d) % N)).collect();
            (format!("n{i}.md"), format!("# Note {i}\n\n{links}\n"))
        })
        .collect();
    let files: Vec<(&str, &str)> = texts
        .iter()
        .map(|(p, t)| (p.as_str(), t.as_str()))
        .collect();
    let w = open(&files);
    let t = Instant::now();
    assert!(w.orphans().unwrap().is_empty());
    assert_eq!(w.missing_backlinks().unwrap().len(), 3 * N);
    let rel = w.related(&v("n0.md")).unwrap();
    // N(n0) = n1..n3 and n2997..n2999; two hops reach n4..n6, n2994..n2996.
    assert_eq!(rel.len(), 6);
    let took = t.elapsed();
    eprintln!("graph queries on {N} notes: {took:?}");
    assert!(took.as_secs() < 10, "took {took:?}");
}
