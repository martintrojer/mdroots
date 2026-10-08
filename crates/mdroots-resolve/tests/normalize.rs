//! Target normalisation and document keys.

use mdroots_resolve::{
    KeyKind, Normalized, doc_keys, normalize, normalize_str, percent_decode, scheme,
};
use mdroots_syntax::{Anchor, Dialect, Link, parse};

fn n(raw: &str) -> Normalized {
    normalize_str(raw, true)
}

fn one_link(src: &str) -> Link {
    let doc = parse(src, Dialect::Markdown);
    let links: Vec<&Link> = doc.links().collect();
    assert_eq!(links.len(), 1, "{src:?}: {links:#?}");
    links[0].clone()
}

#[test]
fn parsing_table() {
    // Cases taken from ramble d394783 tests/nav.rs `resolve_table`, as keys.
    let h = |s: &str| Some(Anchor::Heading(s.into()));
    let cases: &[(&str, &str, Option<Anchor>, Option<&str>)] = &[
        ("#section-two", "", h("section-two"), None),
        ("b.md", "b", None, None),
        ("b.md#intro", "b", h("intro"), None),
        ("b.md#", "b", None, None),
        ("100%.md", "100%", None, None),
        ("./b.md", "b", None, None),
        ("../up.md", "../up", None, None),
        ("my%20note.md", "my note", None, None),
        ("caf%C3%A9.md", "café", None, None),
        ("C:/dos.md", "C:/dos", None, None),
        ("https://example.com/a#b", "", None, Some("https")),
        ("http://x.org", "", None, Some("http")),
        ("mailto:me@x.org", "", None, Some("mailto")),
        ("obsidian+x://open", "", None, Some("obsidian+x")),
        ("file:///abs/f.md#h", "abs/f", h("h"), Some("file")),
        ("file://localhost/abs/f.md", "abs/f", None, Some("file")),
        ("file:/abs/f.md", "abs/f", None, Some("file")),
    ];
    for (raw, key, anchor, sch) in cases {
        let got = n(raw);
        assert_eq!(
            (got.key.as_str(), &got.anchor, got.scheme.as_deref()),
            (*key, anchor, *sch),
            "raw {raw:?}"
        );
    }
}

#[test]
fn external_has_empty_key() {
    let got = n("https://x");
    assert_eq!(got.scheme.as_deref(), Some("https"));
    assert_eq!(got.key, "");
    assert!(got.is_external());
    assert!(!n("file:///x.md").is_external());
    assert!(!n("x.md").is_external());
}

#[test]
fn scheme_and_percent_decode_helpers() {
    assert_eq!(scheme("https://x"), Some("https"));
    assert_eq!(scheme("obsidian+x://open"), Some("obsidian+x"));
    assert_eq!(scheme("C:/x"), None);
    assert_eq!(scheme("C:\\x"), None);
    assert_eq!(scheme("no-colon"), None);
    assert_eq!(scheme("1a:x"), None);
    assert_eq!(percent_decode("a%20b"), "a b");
    assert_eq!(percent_decode("100%"), "100%");
    assert_eq!(percent_decode("%4"), "%4");
    assert_eq!(percent_decode("%zz"), "%zz");
    assert_eq!(percent_decode("%ff"), "\u{fffd}");
}

#[test]
fn drive_letters_are_paths() {
    for raw in ["C:\\x", "C:/x"] {
        let got = n(raw);
        assert_eq!(got.scheme, None, "{raw}");
        assert_eq!(got.key, "C:/x", "{raw}");
    }
}

#[test]
fn nfc_and_nfd_give_the_same_key() {
    let nfc = "Notes/\u{dc}n\u{ef}code.md";
    let nfd = "Notes/U\u{308}ni\u{308}code.md";
    assert_ne!(nfc, nfd);
    assert_eq!(n(nfc).key, "Notes/\u{dc}n\u{ef}code");
    assert_eq!(n(nfc).key, n(nfd).key);
    assert_eq!(normalize_str(nfc, false).key, normalize_str(nfd, false).key);
}

#[test]
fn case_folding() {
    assert_eq!(normalize_str("Notes/Ünïcode.md", true).key, "Notes/Ünïcode");
    assert_eq!(
        normalize_str("Notes/Ünïcode.md", false).key,
        "notes/ünïcode"
    );
    assert_eq!(
        normalize_str("A.MD", false).had_extension.as_deref(),
        Some("md")
    );
    assert_eq!(normalize_str("A.MD", true).key, "A");
}

#[test]
fn dot_segments_and_extension() {
    let got = n("./a/./b.md");
    assert_eq!(got.key, "a/b");
    assert_eq!(got.had_extension.as_deref(), Some("md"));
    assert_eq!(n("a/b.markdown").key, "a/b");
    assert_eq!(n("a/b.markdown").had_extension.as_deref(), Some("markdown"));
    assert_eq!(n("a.org").key, "a");
    assert_eq!(n("a/../b").key, "a/../b");
    assert_eq!(n("a\\b.md").key, "a/b");
    assert_eq!(n("plain").had_extension, None);
}

#[test]
fn other_extensions_stay_in_the_key() {
    let got = n("a.b.c");
    assert_eq!(got.key, "a.b.c");
    assert_eq!(got.had_extension.as_deref(), Some("c"));
    let got = n("x.PNG");
    assert_eq!(got.key, "x.PNG");
    assert_eq!(got.had_extension.as_deref(), Some("png"));
    assert_eq!(n("x.png").key, "x.png");
}

#[test]
fn rooted_and_home() {
    let got = n("/site/x");
    assert!(got.rooted && !got.home);
    assert_eq!(got.key, "site/x");

    let got = n("file:///abs/x.md");
    assert_eq!(got.scheme.as_deref(), Some("file"));
    assert!(got.rooted);
    assert_eq!(got.key, "abs/x");

    let got = n("~/n/a.md");
    assert!(got.home && !got.rooted);
    assert_eq!(got.key, "n/a");

    let got = n("~/x.md");
    assert!(got.home);
    assert_eq!(got.key, "x");

    assert!(!n("a/b").rooted);
}

#[test]
fn percent_decoding() {
    assert_eq!(n("a%20b.md").key, "a b");
}

#[test]
fn id_links_are_id_refs_not_external() {
    let got = n("id:UUID-1");
    assert_eq!(got.scheme, None);
    assert_eq!(got.key, "UUID-1");
    assert!(got.id_ref);
    assert!(!got.is_external());
    assert!(!n("x.md").id_ref);
}

#[test]
fn org_search_split_by_normalize_str() {
    let got = n("notes/a.org::*Intro");
    assert_eq!(got.key, "notes/a");
    assert_eq!(got.anchor, Some(Anchor::Heading("Intro".into())));
    assert_eq!(
        n("a.org::#cid").anchor,
        Some(Anchor::CustomId("cid".into()))
    );
    assert_eq!(
        n("file:a.org::some text").anchor,
        Some(Anchor::Search("some text".into()))
    );
    assert_eq!(n("a#^blk").anchor, Some(Anchor::Block("blk".into())));
}

#[test]
fn normalize_link_from_parse() {
    let got = normalize(&one_link("[a](b.md#h)"), true);
    assert_eq!(got.key, "b");
    assert_eq!(got.had_extension.as_deref(), Some("md"));
    assert_eq!(got.anchor, Some(Anchor::Heading("h".into())));
    assert_eq!(got.piped_alt, None);

    let got = normalize(&one_link("[[x|y]]"), true);
    assert_eq!(got.key, "x");
    assert_eq!(got.piped_alt.as_deref(), Some("y"));

    let got = normalize(&one_link("[[Notes/X|Some/Y.md]]"), false);
    assert_eq!(got.key, "notes/x");
    assert_eq!(got.piped_alt.as_deref(), Some("some/y"));

    let got = normalize(&one_link("[x](https://example.com/a#b)"), true);
    assert!(got.is_external());
    assert_eq!((got.key.as_str(), got.anchor), ("", None));

    let got = normalize(&one_link("[x](file:///abs/f.md#h)"), true);
    assert_eq!(got.key, "abs/f");
    assert!(got.rooted);
    assert_eq!(got.anchor, Some(Anchor::Heading("h".into())));
}

#[test]
fn normalize_link_built_by_hand() {
    use mdroots_syntax::{Confidence, Context, LinkKind};
    let link = Link::new(
        LinkKind::Org,
        Context::Prose,
        Confidence::Explicit,
        "id:ABC",
    );
    let got = normalize(&link, true);
    assert!(got.id_ref);
    assert_eq!(got.scheme, None);
    assert_eq!(got.key, "ABC");

    // Org links whose path syntax already split: raw keeps `file:` and `::`.
    let mut link = Link::new(
        LinkKind::Org,
        Context::Prose,
        Confidence::Explicit,
        "file:notes/a.org",
    )
    .with_anchor(Anchor::Heading("Intro".into()));
    link.target.raw = "file:notes/a.org::*Intro".into();
    link.target.path = "notes/a.org".into();
    let got = normalize(&link, true);
    assert_eq!(got.scheme.as_deref(), Some("file"));
    assert_eq!(got.key, "notes/a");
    assert_eq!(got.anchor, Some(Anchor::Heading("Intro".into())));
}

fn keys_of(path: &str, src: &str, cs: bool) -> Vec<(KeyKind, String)> {
    doc_keys(path, &parse(src, Dialect::Markdown), cs)
}

fn has(keys: &[(KeyKind, String)], kind: KeyKind, key: &str) -> bool {
    keys.iter().any(|(k, v)| *k == kind && v == key)
}

#[test]
fn doc_keys_from_path_h1_and_id_prefix() {
    let keys = keys_of(
        "Zk/202101011200 My Note.md",
        "# Hello World\n\n# Second\n",
        true,
    );
    assert_eq!(
        keys,
        vec![
            (KeyKind::Path, "Zk/202101011200 My Note".into()),
            (KeyKind::Stem, "202101011200 My Note".into()),
            (KeyKind::Id, "202101011200".into()),
            (KeyKind::TitleSlug, "hello-world".into()),
        ]
    );
    let keys = keys_of("Zk/A.md", "", false);
    assert_eq!(
        keys,
        vec![(KeyKind::Path, "zk/a".into()), (KeyKind::Stem, "a".into())]
    );
    // 11 or 15 leading digits are not an id prefix.
    assert!(
        !keys_of("12345678901 x.md", "", true)
            .iter()
            .any(|(k, _)| *k == KeyKind::Id)
    );
    assert!(
        !keys_of("123456789012345.md", "", true)
            .iter()
            .any(|(k, _)| *k == KeyKind::Id)
    );
    assert!(has(
        &keys_of("20210101120000.md", "", true),
        KeyKind::Id,
        "20210101120000"
    ));
}

/// Every key kind from a parsed document, through the syntax_frontmatter
/// accessors (`id`, `title`, `aliases`, `slug`) and the first H1.
#[test]
fn doc_keys_from_frontmatter() {
    let src = "---\nid: Abc-1\ntitle: My Title\naliases: [Other Name, Caf%C3%A9]\nslug: /posts/my-post/\n---\n\n# Heading One\n";
    let keys = keys_of("notes/202101011200 x.md", src, false);
    for (kind, key) in [
        (KeyKind::Path, "notes/202101011200 x"),
        (KeyKind::Stem, "202101011200 x"),
        (KeyKind::Id, "abc-1"),
        (KeyKind::Id, "202101011200"),
        (KeyKind::TitleSlug, "my-title"),
        (KeyKind::TitleSlug, "heading-one"),
        (KeyKind::Alias, "other name"),
        (KeyKind::Alias, "café"),
        (KeyKind::SitePath, "posts/my-post"),
    ] {
        assert!(
            has(&keys, kind, key),
            "missing {kind:?} {key:?} in {keys:?}"
        );
    }
}
