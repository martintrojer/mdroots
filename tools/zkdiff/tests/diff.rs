//! The matcher and classifier on hand-built rows, and on tests/corpus/zk-min
//! with the rows zk 0.15.6 produced for it (checked live in ramble's
//! lsp_zk_e2e). No access to real vaults.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use mdroots_core::{Cancel, MemStore, StdFs};
use mdroots_resolve::keys::ResolveStep;
use mdroots_resolve::ladder::LinkStatus;
use mdroots_syntax::{Confidence, Context, Link, LinkKind};
use zkdiff::{
    Category, MdRecord, ZkData, ZkRow, comparable_href, compared, diff, go_clean, md_key,
    md_records, normalize_href, zk_join,
};

fn zk(id: i64, source: &str, href: &str, target: Option<&str>) -> ZkRow {
    ZkRow {
        id,
        source: source.into(),
        href: href.into(),
        external: false,
        target: target.map(Into::into),
        snippet_start: 0,
        snippet_end: 0,
    }
}

fn ext(id: i64, source: &str, href: &str) -> ZkRow {
    ZkRow {
        external: true,
        ..zk(id, source, href, None)
    }
}

fn md(source: &str, href: &str, start: usize, status: LinkStatus, targets: &[&str]) -> MdRecord {
    MdRecord {
        source: source.into(),
        kind: LinkKind::Wiki,
        href: href.into(),
        range: start..start + 4,
        step: (!targets.is_empty()).then_some(ResolveStep::Stem),
        status,
        targets: targets.iter().map(|t| t.to_string()).collect(),
    }
}

fn data(notes: &[&str], rows: Vec<ZkRow>) -> ZkData {
    ZkData {
        notes: notes.iter().map(|n| n.to_string()).collect::<BTreeSet<_>>(),
        rows,
        orphans: 0,
        orphan_rows: vec![],
    }
}

fn cats(r: &zkdiff::Report) -> Vec<Category> {
    r.outcomes.iter().map(|o| o.category).collect()
}

#[test]
fn classifies_every_category() {
    use LinkStatus as S;
    let z = data(
        &["a.md", "b.md"],
        vec![
            zk(1, "a.md", "b", Some("b.md")),        // agree
            zk(2, "a.md", "gone", None),             // agree: both unresolved
            ext(3, "a.md", "https://x"),             // agree: both external
            zk(4, "a.md", "c", None),                // mdroots-only
            zk(5, "a.md", "d", Some("d.md")),        // zk-only
            zk(6, "a.md", "e", Some("e1.md")),       // disagree
            zk(7, "a.md", "f", Some("f.md")),        // disagree, unindexed
            zk(8, "a.md", "w", Some("x/w.md")),      // agree, ambiguous
            zk(9, "a.md", "v", Some("z/v.md")),      // disagree, ambiguous
            ext(10, "a.md", "file:~/x"),             // external-mismatch
            zk(11, "a.md", "only-zk", Some("b.md")), // unmatched-zk
            zk(12, "b.md", "a", Some("a.md")),       // agree
        ],
    );
    let m = vec![
        md("a.md", "b", 0, S::Resolved, &["b.md"]),
        md("a.md", "gone", 10, S::Broken, &[]),
        md("a.md", "https://x", 20, S::External, &[]),
        md("a.md", "c", 30, S::Unindexed, &["c.pdf"]),
        md("a.md", "d", 40, S::Broken, &[]),
        md("a.md", "e", 50, S::Resolved, &["e2.md"]),
        md("a.md", "f", 60, S::Unindexed, &["f.md"]),
        md("a.md", "w", 70, S::Ambiguous, &["y/w.md", "x/w.md"]),
        md("a.md", "v", 80, S::Ambiguous, &["y/v.md", "x/v.md"]),
        md("a.md", "file:~/x", 90, S::Resolved, &["x.md"]),
        md("a.md", "only-md", 100, S::Resolved, &["b.md"]),
        md("b.md", "a", 0, S::Resolved, &["a.md"]),
        md("x.org", "a.md", 0, S::Resolved, &["a.md"]),
    ];
    let r = diff(&z, &m);
    use Category as C;
    assert_eq!(
        cats(&r),
        [
            C::NotInZk,
            C::Agree,
            C::Agree,
            C::Agree,
            C::MdrootsOnly,
            C::ZkOnly,
            C::Disagree,
            C::Disagree,
            C::Agree,
            C::Disagree,
            C::ExternalMismatch,
            C::UnmatchedZk,
            C::Agree,
            C::UnmatchedMdroots,
        ]
    );
    let flags: Vec<(bool, bool)> = r
        .outcomes
        .iter()
        .map(|o| (o.ambiguous, o.unindexed))
        .collect();
    assert_eq!(flags[7], (false, true), "f: unindexed");
    assert_eq!(flags[8], (true, false), "w: ambiguous agree");
    assert_eq!(flags[9], (true, false), "v: ambiguous disagree");
    for c in Category::ALL {
        assert!(r.table("t", 3).contains(c.name()));
    }
    assert_eq!(r.to_json(&z, serde_json::Value::Null)["counts"]["agree"], 5);
}

#[test]
fn repeated_hrefs_match_in_order_and_by_snippet() {
    use LinkStatus as S;
    let mut late = zk(1, "a.md", "b", Some("b2.md"));
    late.snippet_start = 100;
    late.snippet_end = 200;
    let z = data(
        &["a.md"],
        vec![
            late,
            zk(2, "a.md", "b", Some("b1.md")),
            zk(3, "a.md", "b", None),
        ],
    );
    let m = vec![
        md("a.md", "b", 10, S::Resolved, &["b1.md"]),
        md("a.md", "b", 150, S::Resolved, &["b2.md"]),
    ];
    let r = diff(&z, &m);
    // Row 1 takes the record inside its snippet, row 2 the first unused
    // one, row 3 is left over.
    assert_eq!(
        cats(&r),
        [Category::Agree, Category::Agree, Category::UnmatchedZk]
    );
    assert_eq!(r.outcomes[0].md.as_ref().unwrap().range.start, 150);
}

#[test]
fn hrefs_normalise_like_zk_stores_them() {
    assert_eq!(normalize_href("caf%C3%A9"), normalize_href("cafe\u{301}"));
    assert_ne!(normalize_href("A"), normalize_href("a"));

    assert_eq!(go_clean("a/./b//../c"), "a/c");
    assert_eq!(go_clean("../x"), "../x");
    assert_eq!(zk_join("references/x.md", "../cache/y.md"), "cache/y.md");
    assert_eq!(zk_join("x.md", "./a"), "a");
    assert_eq!(
        zk_join("notes/c.md", "file:///U/x.py"),
        "notes/file:/U/x.py"
    );
    assert_eq!(
        zk_join("t/h.md", "file://images/p.png"),
        "file://images/p.png"
    );
    assert_eq!(zk_join("t/h.md", "https://x/y"), "https://x/y");
    assert_eq!(zk_join("t/h.md", "mailto:a@b"), "mailto:a@b");

    let mut org = Link::new(
        LinkKind::Org,
        Context::Prose,
        Confidence::External,
        "https://x",
    );
    org.label = Some("desc".into());
    assert_eq!(comparable_href("a.md", &org), "https://x][desc");
    let mut wiki = Link::new(LinkKind::Wiki, Context::Prose, Confidence::Explicit, "t");
    wiki.label = Some("label".into());
    assert_eq!(comparable_href("d/a.md", &wiki), "t");
    let mdl = Link::new(
        LinkKind::Markdown,
        Context::Prose,
        Confidence::Explicit,
        "../b.md",
    );
    assert_eq!(comparable_href("d/e/a.md", &mdl), "d/b.md");

    assert!(compared(&mdl));
    let code = Link::new(
        LinkKind::Wiki,
        Context::InlineCode,
        Confidence::Explicit,
        "t",
    );
    assert!(!compared(&code));
    let img = Link::new(
        LinkKind::Image,
        Context::Prose,
        Confidence::Explicit,
        "../p.png",
    );
    assert!(compared(&img));
    assert_eq!(comparable_href("d/a.md", &img), "p.png");
    let bare = Link::new(
        LinkKind::BarePath,
        Context::Prose,
        Confidence::Implicit,
        "a/b",
    );
    assert!(!compared(&bare));
}

fn kind(mut m: MdRecord, k: LinkKind) -> MdRecord {
    m.kind = k;
    m
}

/// zk's wikilink parser drops backslashes, so wiki and org-in-md hrefs
/// match without them; markdown destinations keep theirs, so a markdown
/// href with a backslash does not match a zk row without it.
#[test]
fn backslashes_are_dropped_for_wikilink_kinds_only() {
    use LinkStatus as S;
    let wiki = kind(md("a.md", r"x/\[1\]", 0, S::External, &[]), LinkKind::Wiki);
    let org = kind(md("a.md", r"u\n][d", 10, S::External, &[]), LinkKind::Org);
    let mdl = kind(
        md("a.md", r"y/\[2\]", 20, S::Broken, &[]),
        LinkKind::Markdown,
    );
    assert_eq!(md_key(&wiki), "x/[1]");
    assert_eq!(md_key(&org), "un][d");
    assert_eq!(md_key(&mdl), r"y/\[2\]");
    assert_eq!(normalize_href(r"x/\[a\]"), r"x/\[a\]");
    let z = data(
        &["a.md"],
        vec![
            ext(1, "a.md", "x/[1]"),
            ext(2, "a.md", "un][d"),
            zk(3, "a.md", "y/[2]", None),
        ],
    );
    let r = diff(&z, &[wiki, org, mdl]);
    use Category as C;
    assert_eq!(
        cats(&r),
        [C::Agree, C::Agree, C::UnmatchedZk, C::UnmatchedMdroots]
    );
}

/// Markdown images are compared; one with no zk row is image-not-in-zk,
/// not unmatched-mdroots, and one with a row is classified as usual.
#[test]
fn images_without_zk_rows_are_counted_apart() {
    use LinkStatus as S;
    let z = data(&["a.md"], vec![zk(1, "a.md", "img/p.png", None)]);
    let m = vec![
        kind(
            md("a.md", "img/p.png", 0, S::Unindexed, &["img/p.png"]),
            LinkKind::Image,
        ),
        kind(
            md("a.md", "img/q.png", 10, S::Unindexed, &["img/q.png"]),
            LinkKind::Image,
        ),
        kind(
            md("a.md", "https://x/r.jpg", 20, S::External, &[]),
            LinkKind::Image,
        ),
        md("a.md", "w", 30, S::Broken, &[]),
    ];
    let r = diff(&z, &m);
    use Category as C;
    assert_eq!(r.count(C::MdrootsOnly), 1);
    assert_eq!(r.count(C::ImageNotInZk), 2);
    assert_eq!(r.count(C::UnmatchedMdroots), 1);
    assert_eq!(
        r.to_json(&z, serde_json::Value::Null)["counts"]["image-not-in-zk"],
        2
    );
    assert!(r.table("t", 0).contains("image-not-in-zk"));
}

/// tests/corpus/zk-min: zk resolves extensionless markdown links `a`/`b`
/// to `a.md`/`b.md` and leaves `missing-note` unresolved. mdroots agrees
/// on every row.
#[test]
fn zk_min_corpus_agrees() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus/zk-min");
    let store = MemStore::open(Arc::new(StdFs), root, &Cancel::new()).unwrap();
    let notes = [
        "README.md",
        "a.md",
        "b.md",
        "broken.md",
        "emoji.md",
        "tagged.md",
    ];
    let z = data(
        &notes,
        vec![
            zk(1, "a.md", "b", Some("b.md")),
            zk(2, "b.md", "a", Some("a.md")),
            zk(3, "broken.md", "missing-note", None),
            zk(4, "emoji.md", "b", Some("b.md")),
            zk(5, "tagged.md", "a", Some("a.md")),
        ],
    );
    let records: Vec<MdRecord> = md_records(&store)
        .into_iter()
        .filter(|m| m.source != "README.md")
        .collect();
    let r = diff(&z, &records);
    let bad: Vec<String> = r
        .outcomes
        .iter()
        .filter(|o| o.category != Category::Agree)
        .map(|o| o.describe())
        .collect();
    assert!(bad.is_empty(), "{bad:#?}");
    assert_eq!(r.count(Category::Agree), 5);
}

#[test]
fn json_lists_orphan_rows_and_meta() {
    let mut z = data(&["a.md"], vec![]);
    z.orphan_rows = vec![zkdiff::OrphanRow {
        id: 9,
        source_id: 273,
        href: "references/gone".into(),
        target: Some("references/gone.md".into()),
    }];
    z.orphans = 1;
    let r = diff(&z, &[]);
    let j = r.to_json(&z, serde_json::json!({ "vault": "/v" }));
    assert_eq!(j["orphan_count"], 1);
    assert_eq!(j["orphans"][0]["source_id"], 273);
    assert_eq!(j["orphans"][0]["href"], "references/gone");
    assert_eq!(j["meta"]["vault"], "/v");
}
