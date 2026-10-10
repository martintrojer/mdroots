//! The liberal scan, through the public `parse`.

use mdroots_syntax::{
    Anchor, Confidence, Context, Dialect, Document, Link, LinkKind, ParseOptions, TagSyntax, parse,
    parse_with,
};

fn md(src: &str) -> Document {
    let doc = parse(src, Dialect::Markdown);
    check_ranges(&doc);
    doc
}

/// Link ranges are in bounds, on char boundaries and contain their text
/// range. (Regions are crate-private; that scanner ranges stay inside their
/// segment is checked by construction: each form slices one segment.)
fn check_ranges(doc: &Document) {
    let src = doc.source();
    for l in doc.links() {
        assert!(l.range.end <= src.len() && l.text_range.end <= src.len());
        assert!(src.is_char_boundary(l.range.start) && src.is_char_boundary(l.range.end));
        // Prose wiki/markdown links come from the structure pass; every
        // other context and kind is the scanner's.
        let scanner = l.context != Context::Prose && l.context != Context::Heading
            || matches!(
                l.kind,
                LinkKind::BarePath
                    | LinkKind::Url
                    | LinkKind::CodeMention
                    | LinkKind::Templating
                    | LinkKind::Footnote
            );
        if scanner {
            assert!(
                l.range.start <= l.text_range.start && l.text_range.end <= l.range.end,
                "{l:?}"
            );
        }
    }
}

fn of_kind(doc: &Document, kind: LinkKind) -> Vec<Link> {
    doc.links().filter(|l| l.kind == kind).cloned().collect()
}

fn one(src: &str, kind: LinkKind) -> Link {
    let doc = md(src);
    let ls = of_kind(&doc, kind);
    assert_eq!(ls.len(), 1, "{src:?}: {:#?}", doc.elements());
    ls[0].clone()
}

fn none(src: &str, kind: LinkKind) {
    let doc = md(src);
    let ls = of_kind(&doc, kind);
    assert!(ls.is_empty(), "{src:?}: {ls:#?}");
}

fn tags(doc: &Document) -> Vec<(String, TagSyntax)> {
    doc.tags().map(|t| (t.name.clone(), t.syntax)).collect()
}

/// The slice of the source a link covers.
fn at<'a>(doc: &'a Document, r: &std::ops::Range<usize>) -> &'a str {
    &doc.source()[r.clone()]
}

#[test]
fn wiki_in_fence_is_code_block_explicit() {
    let src = "```\nsee [[note#h]] and ![[img.png]]\n```\n";
    let doc = md(src);
    let w = of_kind(&doc, LinkKind::Wiki);
    assert_eq!(w.len(), 1);
    assert_eq!(w[0].context, Context::CodeBlock);
    assert_eq!(w[0].confidence, Confidence::Explicit);
    assert_eq!(w[0].target.path, "note");
    assert_eq!(w[0].target.anchor, Some(Anchor::Heading("h".into())));
    assert_eq!(at(&doc, &w[0].range), "[[note#h]]");
    assert_eq!(at(&doc, &w[0].text_range), "note#h");
    let e = of_kind(&doc, LinkKind::WikiEmbed);
    assert_eq!(e.len(), 1);
    assert_eq!(at(&doc, &e[0].range), "![[img.png]]");
}

#[test]
fn wiki_in_inline_code() {
    let l = one("a `[[x|lab]]` b", LinkKind::Wiki);
    assert_eq!(l.context, Context::InlineCode);
    assert_eq!(l.target.raw, "x");
    assert_eq!(l.label.as_deref(), Some("lab"));
    assert_eq!(l.text_range, 7..10);
}

#[test]
fn wiki_in_comment() {
    let l = one("a <!-- [[c]] --> b", LinkKind::Wiki);
    assert_eq!(l.context, Context::Comment);
}

#[test]
fn template_wiki_in_fence_is_still_emitted() {
    let l = one("```\n[[{{filename-stem}}]]\n```\n", LinkKind::Wiki);
    assert_eq!(l.context, Context::CodeBlock);
    assert_eq!(l.target.raw, "{{filename-stem}}");
}

#[test]
fn code_wiki_edge_cases() {
    none("```\n[[]] [[a\nb]]\n```\n", LinkKind::Wiki);
    let l = one("```\n[[t][d]]\n```\n", LinkKind::Org);
    assert_eq!(
        (l.target.raw.as_str(), l.label.as_deref()),
        ("t", Some("d"))
    );
    assert_eq!(l.context, Context::CodeBlock);
    // Markdown-style links inside code are not proposed.
    none("`[t](x.md)`", LinkKind::Markdown);
}

#[test]
fn code_mentions() {
    let l = one("open `src/main.rs:12` now", LinkKind::CodeMention);
    assert_eq!(l.target.path, "src/main.rs");
    assert_eq!(l.target.raw, "src/main.rs:12");
    assert_eq!(l.target.line, Some(12));
    assert_eq!(l.context, Context::InlineCode);
    assert_eq!(l.confidence, Confidence::Implicit);

    let l = one("`x.rs:3:1`", LinkKind::CodeMention);
    assert_eq!((l.target.path.as_str(), l.target.line), ("x.rs", Some(3)));

    let l = one("`~/notes/a`", LinkKind::CodeMention);
    assert_eq!(l.target.line, None);

    none("`foo bar`", LinkKind::CodeMention);
    none("`https://x.com/a`", LinkKind::CodeMention);
    none("`main`", LinkKind::CodeMention);
    // An overflowing line on a token that is not path-like: no link at all.
    assert_eq!(md("`x:99999999999`").links().count(), 0);
    let l = one("`a.rs:99999999999`", LinkKind::CodeMention);
    assert_eq!((l.target.path.as_str(), l.target.line), ("a.rs", None));
    // Inline code used as link text is the link, not a mention.
    none("[`a/b.rs`](x)", LinkKind::CodeMention);
}

#[test]
fn bare_paths() {
    let doc = md("see notes/foo.md.");
    let l = &of_kind(&doc, LinkKind::BarePath)[0];
    assert_eq!(l.target.path, "notes/foo.md");
    assert_eq!(l.confidence, Confidence::Implicit);
    assert_eq!(l.context, Context::Prose);
    assert_eq!(at(&doc, &l.range), "notes/foo.md");

    assert_eq!(one("(./a.png)", LinkKind::BarePath).target.raw, "./a.png");
    assert_eq!(one("x ../up/a b", LinkKind::BarePath).target.raw, "../up/a");
    assert_eq!(
        one("read me.md, ok", LinkKind::BarePath).target.raw,
        "me.md"
    );
    assert_eq!(
        one("# Head a/b\n", LinkKind::BarePath).context,
        Context::Heading
    );

    none("see host.com/path", LinkKind::BarePath);
    none("mode:mode/fast/x", LinkKind::BarePath);
    none("see https://x.com/a/b", LinkKind::BarePath);
    none("plain words and file.rs", LinkKind::BarePath);
    none("[see a/b.md](x.md)", LinkKind::BarePath);
    none("and/or", LinkKind::Url);
}

#[test]
fn bare_urls() {
    let doc = md("go to https://x.com/a?b=1. And (http://y.org/p_(q)).");
    let urls: Vec<String> = of_kind(&doc, LinkKind::Url)
        .iter()
        .map(|l| l.target.raw.clone())
        .collect();
    assert_eq!(urls, ["https://x.com/a?b=1", "http://y.org/p_(q)"]);
    assert!(
        of_kind(&doc, LinkKind::Url)
            .iter()
            .all(|l| l.confidence == Confidence::External)
    );
    none("[t](https://x.com)", LinkKind::Url);
    none("<https://x.com>", LinkKind::Url);
}

/// zkdiff unmatched-zk: zk (goldmark linkify) indexes bare URLs in
/// headings (`# loop: https://…`); a URL in a
/// heading is a URL, not a bare path.
#[test]
fn bare_urls_in_headings() {
    let doc = md("# loop: https://x.com/a?b=1\n\n## see https://y.org/p\n");
    let urls: Vec<(String, Context)> = of_kind(&doc, LinkKind::Url)
        .iter()
        .map(|l| (l.target.raw.clone(), l.context))
        .collect();
    assert_eq!(
        urls,
        [
            ("https://x.com/a?b=1".to_owned(), Context::Heading),
            ("https://y.org/p".to_owned(), Context::Heading),
        ]
    );
    assert!(of_kind(&doc, LinkKind::BarePath).is_empty());
}

/// zkdiff unmatched-mdroots: an org link split across lines
/// (`[[https://x][` then the description on the next line) is not a link;
/// the bare URL inside it must not keep the trailing `][`.
#[test]
fn bare_url_drops_trailing_brackets() {
    let doc = md("[[https://x.com/a][\ndesc]]\n");
    let urls: Vec<String> = of_kind(&doc, LinkKind::Url)
        .iter()
        .map(|l| l.target.raw.clone())
        .collect();
    assert_eq!(urls, ["https://x.com/a"]);
}

#[test]
fn hash_tags() {
    let doc = md("# Heading\n\nsome #rust and #a/b, (#paren) x#no #1no\n");
    assert_eq!(
        tags(&doc),
        [
            ("rust".into(), TagSyntax::Hash),
            ("a/b".into(), TagSyntax::Hash),
            ("paren".into(), TagSyntax::Hash),
        ]
    );
    let t = doc.tags().next().unwrap();
    assert_eq!(&doc.source()[t.range.clone()], "#rust");

    assert_eq!(
        tags(&md("## Title #topic\n")),
        [("topic".into(), TagSyntax::Hash)]
    );
    assert!(tags(&md("```\n#include <x>\n```\n")).is_empty());
    assert!(tags(&md("`#include`")).is_empty());

    let mut opts = ParseOptions::new(Dialect::Markdown);
    opts.hashtags = false;
    assert!(parse_with("#rust", &opts).tags().next().is_none());
}

#[test]
fn tag_not_taken_from_link_destination() {
    let doc = md("[t](#anchor) #rust");
    assert_eq!(tags(&doc), [("rust".into(), TagSyntax::Hash)]);
}

#[test]
fn colon_tags_only_when_enabled() {
    let src = "x :a:b: y";
    assert!(tags(&md(src)).is_empty());
    let mut opts = ParseOptions::new(Dialect::Markdown);
    opts.colon_tags = true;
    let doc = parse_with(src, &opts);
    assert_eq!(
        tags(&doc),
        [
            ("a".into(), TagSyntax::Colon),
            ("b".into(), TagSyntax::Colon)
        ]
    );
    assert!(tags(&parse_with("x :a b: y", &opts)).is_empty());
    let mut org = ParseOptions::new(Dialect::Org);
    org.colon_tags = true;
    assert!(parse_with(src, &org).tags().next().is_none());
}

#[test]
fn multiword_tags_only_when_enabled() {
    let src = "x #multi word# y";
    assert!(
        !tags(&md(src))
            .iter()
            .any(|(_, s)| *s == TagSyntax::MultiWord)
    );
    let mut opts = ParseOptions::new(Dialect::Markdown);
    opts.multiword_tags = true;
    let doc = parse_with(src, &opts);
    assert_eq!(tags(&doc), [("multi word".into(), TagSyntax::MultiWord)]);
    let t = doc.tags().next().unwrap();
    assert_eq!(&src[t.range.clone()], "#multi word#");
}

#[test]
fn footnotes() {
    let doc = md("text[^1] more.\n\n[^1]: the note\n");
    let f = of_kind(&doc, LinkKind::Footnote);
    assert_eq!(f.len(), 1, "{f:#?}");
    assert_eq!(f[0].target.raw, "^1");
    assert_eq!(f[0].target.path, "^1");
    assert_eq!(f[0].confidence, Confidence::Explicit);
    assert_eq!(at(&doc, &f[0].range), "[^1]");
}

#[test]
fn templating() {
    let l = one(r#"see {{< ref "x.md" >}} and"#, LinkKind::Templating);
    assert_eq!(l.target.raw, "x.md");
    assert_eq!(l.confidence, Confidence::Explicit);
    assert_eq!(
        one(r#"{{< relref "y" >}}"#, LinkKind::Templating)
            .target
            .raw,
        "y"
    );
    assert_eq!(
        one("{% link z.md %}", LinkKind::Templating).target.raw,
        "z.md"
    );
    none("{{< figure src=\"a.png\" >}}", LinkKind::Templating);
}

#[test]
fn templating_anchor_is_split_off() {
    for src in [
        r#"see {{< relref "posts/b.md#sec" >}} x"#,
        "{% link posts/b.md#sec %}",
    ] {
        let l = one(src, LinkKind::Templating);
        assert_eq!(l.target.raw, "posts/b.md#sec");
        assert_eq!(l.target.path, "posts/b.md");
        assert_eq!(l.target.anchor, Some(Anchor::Heading("sec".into())));
    }
    let l = one(r##"{{< ref "#here" >}}"##, LinkKind::Templating);
    assert_eq!(
        (l.target.path.as_str(), l.target.anchor),
        ("", Some(Anchor::Heading("here".into())))
    );
}

#[test]
fn bare_path_and_html_anchor_is_split_off() {
    let l = one("see notes/a.md#sec now", LinkKind::BarePath);
    assert_eq!(l.target.raw, "notes/a.md#sec");
    assert_eq!(l.target.path, "notes/a.md");
    assert_eq!(l.target.anchor, Some(Anchor::Heading("sec".into())));
    let l = one("<a href=\"b.md#^blk\">b</a>\n", LinkKind::Html);
    assert_eq!(l.target.path, "b.md");
    assert_eq!(l.target.anchor, Some(Anchor::Block("blk".into())));
    // URLs are external: left as written.
    let l = one("go https://x.com/a#frag now", LinkKind::Url);
    assert_eq!(
        (l.target.path.as_str(), l.target.anchor),
        ("https://x.com/a#frag", None)
    );
}

#[test]
fn html_links() {
    let doc = md("<a href=\"./a.md\">x</a> and <img alt=\"\" src='https://x/p.png'>\n");
    let h = of_kind(&doc, LinkKind::Html);
    let got: Vec<(&str, Confidence, Context)> = h
        .iter()
        .map(|l| (l.target.raw.as_str(), l.confidence, l.context))
        .collect();
    assert_eq!(
        got,
        [
            ("./a.md", Confidence::Explicit, Context::Html),
            ("https://x/p.png", Confidence::External, Context::Html),
        ]
    );
    let l = one("<div>\n<a href=b.md>b</a>\n</div>\n", LinkKind::Html);
    assert_eq!(l.target.raw, "b.md");
}

#[test]
fn multibyte_neighbours() {
    // Ranges stay on char boundaries next to multibyte text.
    let doc = md("é#tag 😀 notes/é.md. `é/ü.rs:2` https://é.x/ü");
    check_ranges(&doc);
    assert_eq!(
        of_kind(&doc, LinkKind::BarePath)[0].target.raw,
        "notes/é.md"
    );
    assert!(doc.tags().next().is_none());
}

mod fuzz {
    use super::*;
    use proptest::prelude::*;

    fn fragments() -> impl Strategy<Value = String> {
        let frag = prop_oneof![
            Just("[["),
            Just("]]"),
            Just("]["),
            Just("|"),
            Just("![["),
            Just("[^"),
            Just("]"),
            Just(":"),
            Just("#"),
            Just("#a"),
            Just("/"),
            Just("./"),
            Just(".md"),
            Just("`"),
            Just("```\n"),
            Just("<!--"),
            Just("-->"),
            Just("{{< ref "),
            Just(" >}}"),
            Just("{% link "),
            Just(" %}"),
            Just("\""),
            Just("<a href="),
            Just("<img src="),
            Just(">"),
            Just("https://"),
            Just("("),
            Just(")"),
            Just("\n"),
            Just(" "),
            Just("é"),
            Just("😀"),
            Just("a"),
            Just("1"),
        ];
        proptest::collection::vec(frag, 0..50).prop_map(|v| v.concat())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]
        #[test]
        fn scan_fragments_are_total(src in fragments()) {
            for opts in [
                ParseOptions::new(Dialect::Markdown),
                {
                    let mut o = ParseOptions::new(Dialect::Markdown);
                    o.colon_tags = true;
                    o.multiword_tags = true;
                    o
                },
            ] {
                let doc = parse_with(&src, &opts);
                check_ranges(&doc);
                for t in doc.tags() {
                    prop_assert!(src.get(t.range.clone()).is_some());
                }
            }
        }
    }
}

#[test]
fn wiki_in_inline_code_is_not_also_a_code_mention() {
    let doc = md("`[[x/y]]`");
    let kinds: Vec<LinkKind> = doc.links().map(|l| l.kind).collect();
    assert_eq!(kinds, [LinkKind::Wiki]);
    let doc = md("`[[t][d/e.md]]`");
    let kinds: Vec<LinkKind> = doc.links().map(|l| l.kind).collect();
    assert_eq!(kinds, [LinkKind::Org]);
}
