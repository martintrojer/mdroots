//! The markdown structure pass, through the public `parse`.

use mdroots_syntax::{
    Anchor, Confidence, Context, Dialect, Document, FrontmatterFormat, Link, LinkKind, parse,
};

fn md(src: &str) -> Document {
    parse(src, Dialect::Markdown)
}

fn links(doc: &Document) -> Vec<&Link> {
    doc.links().collect()
}

fn one_link(src: &str) -> Link {
    let doc = md(src);
    let ls = links(&doc);
    assert_eq!(ls.len(), 1, "{src:?}: {ls:#?}");
    ls[0].clone()
}

#[test]
fn headings_levels_text_slugs() {
    let doc = md("# Hello World\n\n## Hello World\n\n### A `code` $m$ bit\n\n# Custom {#my-id}\n");
    let got: Vec<(u8, &str, &str, Option<&str>)> = doc
        .headings()
        .map(|h| (h.level, h.text.as_str(), h.slug.as_str(), h.id.as_deref()))
        .collect();
    assert_eq!(
        got,
        vec![
            (1, "Hello World", "hello-world", None),
            (2, "Hello World", "hello-world-1", None),
            (3, "A code m bit", "a-code-m-bit", None),
            (1, "Custom", "custom", Some("my-id")),
        ]
    );
    let h = doc.headings().next().unwrap();
    assert_eq!(&doc.source()[h.range.clone()], "# Hello World\n");
}

#[test]
fn wiki_links() {
    let l = one_link("x [[a]] y");
    assert_eq!(l.kind, LinkKind::Wiki);
    assert_eq!(l.target.raw, "a");
    assert_eq!(l.target.path, "a");
    assert_eq!(l.label, None);
    assert_eq!(l.confidence, Confidence::Explicit);
    assert_eq!(l.context, Context::Prose);
    assert_eq!(l.range, 2..7);
    assert_eq!(l.text_range, 4..5);

    let l = one_link("[[a|b]]");
    assert_eq!(
        (l.target.raw.as_str(), l.label.as_deref()),
        ("a", Some("b"))
    );
    assert_eq!(l.text_range, 4..5);

    let l = one_link("[[a#h]]");
    assert_eq!(l.target.path, "a");
    assert_eq!(l.target.anchor, Some(Anchor::Heading("h".into())));

    let l = one_link("[[a#^blk]]");
    assert_eq!(l.target.path, "a");
    assert_eq!(l.target.anchor, Some(Anchor::Block("blk".into())));
}

#[test]
fn org_style_wiki_becomes_org_link() {
    let l = one_link("see [[t][d]] now");
    assert_eq!(l.kind, LinkKind::Org);
    assert_eq!(l.target.raw, "t");
    assert_eq!(l.label.as_deref(), Some("d"));
    assert_eq!(l.range, 4..12);
    assert_eq!(l.text_range, 9..10);
}

#[test]
fn multi_line_wiki_is_not_a_link() {
    assert!(links(&md("[[a\nb]]")).is_empty());
}

#[test]
fn wiki_embed() {
    let l = one_link("![[emb]]");
    assert_eq!(l.kind, LinkKind::WikiEmbed);
    assert_eq!(l.target.raw, "emb");
}

#[test]
fn link_kinds() {
    let doc = md("[a](x.md) [b][r] [r] [r][] ![i](p.png) <https://x> <a@b.c>\n\n[r]: ./r.md\n");
    let kinds: Vec<(LinkKind, &str)> = doc
        .links()
        .map(|l| (l.kind, l.target.raw.as_str()))
        .collect();
    assert_eq!(
        kinds,
        vec![
            (LinkKind::Markdown, "x.md"),
            (LinkKind::Reference, "./r.md"),
            (LinkKind::Reference, "./r.md"),
            (LinkKind::Reference, "./r.md"),
            (LinkKind::Image, "p.png"),
            (LinkKind::Autolink, "https://x"),
            (LinkKind::Autolink, "a@b.c"),
            (LinkKind::Reference, "./r.md"), // from the definition
        ]
    );
}

#[test]
fn confidence_by_scheme() {
    let l = one_link("<https://x>");
    assert_eq!(l.confidence, Confidence::External);
    let l = one_link("[t](file:///x)");
    assert_eq!(l.confidence, Confidence::Explicit);
    let l = one_link("[t](https://x.org/a)");
    assert_eq!(l.confidence, Confidence::External);
    let l = one_link("[t](c:/x)");
    assert_eq!(l.confidence, Confidence::Explicit);
}

#[test]
fn text_ranges() {
    let l = one_link("[](x)");
    assert_eq!(l.text_range, 1..1);
    let src = "é日 [ab](x)";
    let l = one_link(src);
    let at = src.find('[').unwrap();
    assert_eq!(at, 6);
    assert_eq!(l.range, at..src.len());
    assert_eq!(l.text_range, at + 1..at + 3);
    let l = one_link("[a`b`c](d)");
    assert_eq!(l.text_range, 1..6);
}

#[test]
fn fenced_code_has_no_links() {
    let src = "```\n[x](y) [[w]]\n```\n";
    // The structure pass proposes nothing in code; the liberal scan adds
    // only the wiki form, tagged CodeBlock (tests/scan.rs).
    let doc = md(src);
    let ls = links(&doc);
    assert_eq!(ls.len(), 1, "{ls:#?}");
    assert_eq!(
        (ls[0].kind, ls[0].context),
        (LinkKind::Wiki, Context::CodeBlock)
    );
}

#[test]
fn heading_context() {
    let l = one_link("# see [x](y)\n");
    assert_eq!(l.context, Context::Heading);
}

#[test]
fn reference_definition() {
    let doc = md("[r]: ./a.md\n");
    let defs: Vec<_> = doc.link_defs().collect();
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].label, "r");
    assert_eq!(defs[0].dest, "./a.md");
    assert_eq!(defs[0].range, 0..11);
    let l = one_link("[r]: ./a.md\n");
    assert_eq!(l.kind, LinkKind::Reference);
    assert_eq!(l.context, Context::Prose);
    assert_eq!(l.target.raw, "./a.md");
    assert_eq!(l.range, 0..11);

    assert_eq!(md("> [q]: ./q.md").link_defs().count(), 1);
    assert_eq!(md("para\n[r]: ./a.md").link_defs().count(), 0);
}

#[test]
fn yaml_frontmatter_block() {
    let src = "---\nk: v\n---\nbody\n";
    let doc = md(src);
    let fm = doc.frontmatter().expect("frontmatter");
    assert_eq!(fm.format, FrontmatterFormat::Yaml);
    assert_eq!(fm.range, 0..12);
}

#[test]
fn toml_frontmatter_block() {
    let doc = md("+++\na = 1\n+++\n");
    let fm = doc.frontmatter().expect("frontmatter");
    assert_eq!(fm.format, FrontmatterFormat::Toml);
    assert_eq!(fm.range, 0..13);
}

#[test]
fn frontmatter_after_bom() {
    let doc = md("\u{feff}---\nk: v\n---\n");
    let fm = doc.frontmatter().expect("frontmatter");
    assert_eq!(fm.range, 3..15);
}

#[test]
fn mid_document_metadata_is_not_frontmatter() {
    assert_eq!(md("text\n\n---\nk: v\n---\n").frontmatter(), None);
}

#[test]
fn link_builder() {
    let l = Link::new(
        LinkKind::Wiki,
        Context::Prose,
        Confidence::Explicit,
        "a/b#^x",
    );
    assert_eq!(l.target.path, "a/b");
    assert_eq!(l.target.anchor, Some(Anchor::Block("x".into())));
    assert_eq!((l.range.clone(), l.text_range.clone()), (0..6, 0..6));
    let l = Link::new(
        LinkKind::Markdown,
        Context::Prose,
        Confidence::Explicit,
        "a#h",
    )
    .with_label("L")
    .with_line(3)
    .with_group(7)
    .with_anchor(Anchor::CustomId("c".into()));
    assert_eq!(l.target.anchor, Some(Anchor::CustomId("c".into())));
    assert_eq!(
        (l.label.as_deref(), l.target.line, l.group),
        (Some("L"), Some(3), Some(7))
    );
}

#[test]
fn parse_bytes_binary_and_lossy() {
    use mdroots_syntax::{ParseOptions, parse_bytes};
    let opts = ParseOptions::new(Dialect::Markdown);
    assert!(parse_bytes(b"a\0b", &opts).is_none());
    let doc = parse_bytes(b"ok [[a]]", &opts).unwrap();
    assert!(!doc.is_lossy());
    let doc = parse_bytes(b"bad \xff [[a]]", &opts).unwrap();
    assert!(doc.is_lossy());
    assert_eq!(doc.links().count(), 1);
}

#[test]
fn dialect_from_path() {
    use std::path::Path;
    assert_eq!(
        Dialect::detect_from_path(Path::new("a/b.org")),
        Dialect::Org
    );
    assert_eq!(
        Dialect::detect_from_path(Path::new("a/b.md")),
        Dialect::Markdown
    );
    assert_eq!(
        Dialect::detect_from_path(Path::new("README")),
        Dialect::Markdown
    );
}

#[test]
fn empty_frontmatter_blocks() {
    let cases: &[(&str, FrontmatterFormat, std::ops::Range<usize>)] = &[
        ("---\n---\n", FrontmatterFormat::Yaml, 0..7),
        ("---\n\n---\n# H\n", FrontmatterFormat::Yaml, 0..8),
        ("---\n  \n\n---", FrontmatterFormat::Yaml, 0..11),
        ("+++\n+++\n", FrontmatterFormat::Toml, 0..7),
        ("\u{feff}---\n---\n", FrontmatterFormat::Yaml, 3..10),
        ("\u{feff}+++\n\n+++\n", FrontmatterFormat::Toml, 3..11),
    ];
    for (src, format, range) in cases {
        let doc = md(src);
        let fm = doc.frontmatter().unwrap_or_else(|| panic!("{src:?}"));
        assert_eq!(fm.format, *format, "{src:?}");
        assert_eq!(fm.range, *range, "{src:?}");
        assert!(fm.entries().is_empty(), "{src:?}");
    }
    // The body after an empty block still parses: no rule eats the heading.
    let doc = md("---\n\n---\n# H\n");
    assert_eq!(
        doc.headings().map(|h| h.text.as_str()).collect::<Vec<_>>(),
        ["H"]
    );
}

#[test]
fn lone_rule_is_not_frontmatter() {
    assert_eq!(md("---\n").frontmatter(), None);
    assert_eq!(md("---\n\ntext\n").frontmatter(), None);
    assert_eq!(md("+++\n").frontmatter(), None);
    assert_eq!(md("text\n\n---\n---\n").frontmatter(), None);
}

mod drawn_text {
    use super::*;
    use proptest::prelude::*;

    fn word() -> impl Strategy<Value = String> {
        "[a-zA-Zé😀日]{1,6}"
    }

    /// A source piece and, for links, the text it draws.
    fn piece() -> impl Strategy<Value = (String, Option<String>)> {
        prop_oneof![
            (word(), "[a-z]{1,5}(\\.md)?(#[a-z]{1,4})?")
                .prop_map(|(w, d)| (format!("[{w}]({d})"), Some(w))),
            word().prop_map(|w| (format!("[[{w}]]"), Some(w))),
            (word(), word()).prop_map(|(t, l)| (format!("[[{t}|{l}]]"), Some(l))),
            word().prop_map(|w| (w, None)),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        // Ported from ramble tests/doc.rs link_text_range_is_drawn_text.
        #[test]
        fn link_text_range_is_drawn_text(pieces in prop::collection::vec(piece(), 1..8)) {
            let src = pieces.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>().join(" ");
            let doc = md(&src);
            let expected: Vec<&String> = pieces.iter().filter_map(|(_, t)| t.as_ref()).collect();
            let got: Vec<&Link> = doc.links().collect();
            prop_assert_eq!(got.len(), expected.len(), "{}", src);
            for (link, text) in got.iter().zip(expected) {
                prop_assert!(
                    link.range.start <= link.text_range.start && link.text_range.end <= link.range.end
                );
                prop_assert_eq!(&src[link.text_range.clone()], text.as_str(), "{}", src);
            }
        }
    }
}

#[test]
fn explicit_id_keeps_text_slug() {
    let doc = md("# Visible {#custom}\n\n## Visible\n\n## Other {#custom}\n");
    let got: Vec<(&str, Option<&str>)> = doc
        .headings()
        .map(|h| (h.slug.as_str(), h.id.as_deref()))
        .collect();
    // ids are kept as written, even when repeated; slugs come from the text.
    assert_eq!(
        got,
        vec![
            ("visible", Some("custom")),
            ("visible-1", None),
            ("other", Some("custom")),
        ]
    );
}

#[test]
fn empty_image_text_range_is_just_inside_the_bracket() {
    let src = "a ![](x.png) b [](y.md)";
    let doc = parse(src, Dialect::Markdown);
    let ranges: Vec<_> = doc
        .links()
        .map(|l| (l.kind, l.text_range.clone()))
        .collect();
    assert_eq!(ranges.len(), 2);
    assert_eq!(ranges[0].1, 4..4, "image: after `![` (bytes 2-3)");
    assert_eq!(ranges[1].1, 16..16, "link: after `[` (byte 15)");
}
