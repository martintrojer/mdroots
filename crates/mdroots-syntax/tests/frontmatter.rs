//! Frontmatter parsing, links and tags from values, and the standard-key
//! accessors, through the public `parse`.

use std::ops::Range;

use mdroots_syntax::{
    Confidence, Context, Dialect, Document, Field, FieldValue, Frontmatter, FrontmatterFormat,
    Link, LinkKind, ParseOptions, Tag, TagSyntax, Value, parse, parse_with,
};

fn md(src: &str) -> Document {
    parse(src, Dialect::Markdown)
}

fn fm(doc: &Document) -> &Frontmatter {
    doc.frontmatter().expect("frontmatter")
}

fn str_(s: &str) -> Value {
    Value::Str(s.into())
}

fn list(v: &[&str]) -> Value {
    Value::List(v.iter().map(|s| s.to_string()).collect())
}

fn fm_links(doc: &Document) -> Vec<&Link> {
    doc.links()
        .filter(|l| l.context == Context::Frontmatter)
        .collect()
}

fn fm_tags(doc: &Document) -> Vec<&Tag> {
    doc.tags()
        .filter(|t| t.syntax == TagSyntax::Frontmatter)
        .collect()
}

fn tag_names(doc: &Document) -> Vec<String> {
    fm_tags(doc).iter().map(|t| t.name.clone()).collect()
}

#[test]
fn yaml_scalars_lists_null_and_nested_maps_flatten() {
    let src = "---\ntitle: Hello\ncount: 007\nflag: True\ntags: [a, b]\n\
               authors:\n  - Ann\n  - Bob\nnothing: ~\nalso: null\nempty:\n\
               meta:\n  a: 1\n  inner:\n    b: x\n---\nbody\n";
    let doc = md(src);
    let f = fm(&doc);
    assert_eq!(f.format, FrontmatterFormat::Yaml);
    assert_eq!(f.error, None);
    assert_eq!(
        f.entries(),
        [
            ("title".into(), str_("Hello")),
            ("count".into(), str_("007")),
            ("flag".into(), str_("True")),
            ("tags".into(), list(&["a", "b"])),
            ("authors".into(), list(&["Ann", "Bob"])),
            ("nothing".into(), Value::Null),
            ("also".into(), Value::Null),
            ("empty".into(), Value::Null),
            ("meta.a".into(), str_("1")),
            ("meta.inner.b".into(), str_("x")),
        ]
    );
    assert_eq!(f.get("meta.inner.b"), Some(&str_("x")));
}

#[test]
fn toml_block() {
    let src = "+++\ntitle = \"T\"\ndate = 2024-01-05\ntags = [\"x\", \"y\"]\n\
               [extra]\nk = 1\n+++\n\n# Body\n";
    let doc = md(src);
    let f = fm(&doc);
    assert_eq!(f.format, FrontmatterFormat::Toml);
    assert_eq!(f.error, None);
    assert_eq!(
        f.entries(),
        [
            ("title".into(), str_("T")),
            ("date".into(), str_("2024-01-05")),
            ("tags".into(), list(&["x", "y"])),
            ("extra.k".into(), str_("1")),
        ]
    );
    assert_eq!(f.title(), Some("T"));
    assert_eq!(f.dates(), [("date", "2024-01-05")]);
    assert_eq!(tag_names(&doc), ["x", "y"]);
    assert_eq!(doc.headings().count(), 1);
}

#[test]
fn logseq_properties() {
    let src = "title:: X\nalias:: [[y]]\n\nbody\n";
    let doc = md(src);
    let f = fm(&doc);
    assert_eq!(f.format, FrontmatterFormat::Logseq);
    assert_eq!(f.title(), Some("X"));
    assert_eq!(&src[f.range.clone()], "title:: X\nalias:: [[y]]");
    // Exactly one link: the structure pass's own wikilink is dropped.
    let links: Vec<&Link> = doc.links().collect();
    assert_eq!(links.len(), 1, "{links:#?}");
    assert_eq!(links[0].kind, LinkKind::Wiki);
    assert_eq!(links[0].context, Context::Frontmatter);
    assert_eq!(links[0].target.raw, "y");
    assert_eq!(&src[links[0].range.clone()], "[[y]]");
}

#[test]
fn multimarkdown_header() {
    let src = "Title: My Note\nAuthor: Ann\nDate: 2024-01-05\n\nBody text.\n";
    let doc = md(src);
    let f = fm(&doc);
    assert_eq!(f.format, FrontmatterFormat::MultiMarkdown);
    assert_eq!(f.title(), Some("My Note"));
    assert_eq!(f.dates(), [("Date", "2024-01-05")]);
    assert_eq!(f.entries().len(), 3);
}

#[test]
fn prose_with_a_colon_is_not_multimarkdown() {
    assert_eq!(md("Note: this is prose\nmore text\n").frontmatter(), None);
    assert_eq!(md("Note: this is prose\n").frontmatter(), None);
}

#[test]
fn json_object() {
    let src = "{\n  \"title\": \"J\",\n  \"tags\": [\"a\", \"b\"],\n  \"n\": 3,\n  \
               \"x\": {\"y\": null}\n}\n\n# Body\n";
    let doc = md(src);
    let f = fm(&doc);
    assert_eq!(f.format, FrontmatterFormat::Json);
    assert_eq!(f.error, None);
    assert_eq!(
        f.entries(),
        [
            ("title".into(), str_("J")),
            ("tags".into(), list(&["a", "b"])),
            ("n".into(), str_("3")),
            ("x.y".into(), Value::Null),
        ]
    );
    assert_eq!(tag_names(&doc), ["a", "b"]);
    assert!(src[f.range.clone()].ends_with('}'));
}

#[test]
fn invalid_yaml_sets_error_and_the_rest_still_parses() {
    let src = "---\ntitle: X\n author: [\n\tbad: tab\nid: z1\n---\n\n# After\n\n[[later]]\n";
    let doc = md(src);
    let f = fm(&doc);
    assert!(f.error.is_some());
    assert_eq!(f.id(), Some("z1"));
    assert_eq!(
        doc.headings().map(|h| h.text.as_str()).collect::<Vec<_>>(),
        ["After"]
    );
    assert!(
        doc.links()
            .any(|l| l.target.raw == "later" && l.context == Context::Prose)
    );
}

#[test]
fn title_collisions() {
    let doc = md("---\nTitle: Upper\ntitle: lower\n---\n");
    assert_eq!(fm(&doc).title(), Some("lower"));
    let doc = md("---\nlinkTitle: Link\ntitle: Real\n---\n");
    assert_eq!(fm(&doc).title(), Some("Real"));
    // No exact name: first in document order.
    let doc = md("---\nTITLE: A\nLinkTitle: B\n---\n");
    assert_eq!(fm(&doc).title(), Some("A"));
    // Placeholders count as absent.
    let doc = md("---\ntitle: \"—\"\nlinkTitle: L\n---\n");
    assert_eq!(fm(&doc).title(), Some("L"));
}

#[test]
fn placeholder_values_are_stored_but_not_links() {
    let src = "---\nsource-url: \"—\"\nsource-local: n/a\nsee: TBD\n---\n";
    let doc = md(src);
    assert_eq!(fm(&doc).get("source-url"), Some(&str_("—")));
    assert!(fm_links(&doc).is_empty());
}

#[test]
fn comma_joined_paths_share_a_group() {
    let src = "---\ntitle: é😀\nsource-cache: \"cache/a.html, cache/a.txt\"\n---\n";
    let doc = md(src);
    let links = fm_links(&doc);
    assert_eq!(links.len(), 2, "{links:#?}");
    for (l, want) in links.iter().zip(["cache/a.html", "cache/a.txt"]) {
        assert_eq!(l.kind, LinkKind::BarePath);
        assert_eq!(l.confidence, Confidence::Implicit);
        assert_eq!(&src[l.range.clone()], want);
        assert_eq!(l.target.raw, want);
    }
    assert!(links[0].group.is_some());
    assert_eq!(links[0].group, links[1].group);
}

#[test]
fn single_path_has_no_group_and_groups_differ_per_value() {
    let src = "---\nsource-local: notes/x.md\na: \"p/1.md, p/2.md\"\nb: \"q/1.md, —\"\n---\n";
    let doc = md(src);
    let links = fm_links(&doc);
    let got: Vec<(&str, Option<u32>)> = links
        .iter()
        .map(|l| (&src[l.range.clone()], l.group))
        .collect();
    assert_eq!(got.len(), 4, "{got:?}");
    assert_eq!(got[0], ("notes/x.md", None));
    assert_eq!(got[1].1, got[2].1);
    assert_ne!(got[1].1, got[3].1);
    assert_eq!(got[3].0, "q/1.md");
}

#[test]
fn non_path_strings_are_not_links() {
    let doc = md("---\ntitle: A plain title\nstage: reading\nversion: 1.2\n---\n");
    assert!(fm_links(&doc).is_empty());
}

#[test]
fn urls_are_external_links() {
    let src = "---\nsource-url: \"https://example.com/a/b\"\nmail: mailto:a@b.c\n---\n";
    let doc = md(src);
    let links = fm_links(&doc);
    assert_eq!(links.len(), 2);
    for l in links {
        assert_eq!(l.kind, LinkKind::Url);
        assert_eq!(l.confidence, Confidence::External);
    }
    assert!(doc.links().all(|l| l.kind != LinkKind::BarePath));
}

#[test]
fn wiki_value_is_an_explicit_frontmatter_link() {
    let src = "---\nrelated: \"[[foo]]\"\nup: [\"[[a|A]]\", \"[[b]]\"]\n---\n";
    let doc = md(src);
    let links = fm_links(&doc);
    let got: Vec<(&str, &str)> = links
        .iter()
        .map(|l| (l.target.raw.as_str(), &src[l.range.clone()]))
        .collect();
    assert_eq!(got, [("foo", "[[foo]]"), ("a", "[[a|A]]"), ("b", "[[b]]")]);
    assert!(
        links
            .iter()
            .all(|l| l.kind == LinkKind::Wiki && l.confidence == Confidence::Explicit)
    );
    assert_eq!(links[1].label.as_deref(), Some("A"));
    assert_eq!(&src[links[1].text_range.clone()], "A");
}

#[test]
fn tag_forms() {
    for src in [
        "---\ntags: [a, b]\n---\n",
        "---\ntags: a, b\n---\n",
        "---\ntags: \"#a #b\"\n---\n",
        "---\ntags: [#a, #b]\n---\n",
        "---\ntags:\n  - a\n  - b\n---\n",
        "---\nkeywords: a b\n---\n",
    ] {
        let doc = md(src);
        assert_eq!(tag_names(&doc), ["a", "b"], "{src:?}");
        assert_eq!(fm(&doc).tags(), ["a", "b"], "{src:?}");
        for t in fm_tags(&doc) {
            assert_eq!(&src[t.range.clone()], t.name, "{src:?}");
        }
    }
    // The stored value keeps `#` as written.
    let doc = md("---\ntags: [#a, #b]\n---\n");
    assert_eq!(fm(&doc).get("tags"), Some(&list(&["#a", "#b"])));
    // Tag values never become path links.
    assert!(fm_links(&md("---\ntags: a/b, c.md\n---\n")).is_empty());
}

#[test]
fn accessors() {
    let src = "---\nTitle: T\naliases: x, y\nuid: 42\ntag: [t]\nexcerpt: Short\n\
               created: 2024-01-01\nlastmod: 2024-02-02\npermalink: /blog/t\n---\n";
    let doc = md(src);
    let f = fm(&doc);
    assert_eq!(f.title(), Some("T"));
    assert_eq!(f.aliases(), ["x", "y"]);
    assert_eq!(f.id(), Some("42"));
    assert_eq!(f.tags(), ["t"]);
    assert_eq!(f.summary(), Some("Short"));
    assert_eq!(
        f.dates(),
        [("created", "2024-01-01"), ("lastmod", "2024-02-02")]
    );
    assert_eq!(f.site_path(), Some("/blog/t"));
    let doc = md("---\nalias: [p, q]\nzettel-id: \"—\"\nid: real\n---\n");
    assert_eq!(fm(&doc).aliases(), ["p", "q"]);
    assert_eq!(fm(&doc).id(), Some("real"));
}

#[test]
fn org_keyword_frontmatter_maps_title_and_filetags() {
    let f = Frontmatter::from_entries(
        FrontmatterFormat::OrgKeywords,
        0..0,
        vec![
            ("title".into(), str_("T")),
            ("filetags".into(), str_(":a:b:")),
        ],
        None,
    );
    assert_eq!(f.title(), Some("T"));
    assert_eq!(f.tags(), ["a", "b"]);
}

#[test]
fn empty_blocks_are_frontmatter_with_no_entries() {
    for src in ["---\n---\n", "+++\n+++\n", "---\n\n---\n# H\n"] {
        let doc = md(src);
        let f = fm(&doc);
        assert!(f.entries().is_empty(), "{src:?}");
        assert_eq!(f.error, None, "{src:?}");
    }
}

#[test]
fn bom_before_the_block() {
    let src = "\u{feff}---\ntitle: T\nsee: a/b.md\n---\n";
    let doc = md(src);
    assert_eq!(fm(&doc).title(), Some("T"));
    let l = fm_links(&doc);
    assert_eq!(&src[l[0].range.clone()], "a/b.md");
}

#[test]
fn duplicate_keys_kept_in_order_first_wins_for_accessors() {
    let doc = md("---\nid: one\nb: 2\nid: two\n---\n");
    let keys: Vec<&str> = fm(&doc).entries().iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, ["id", "b", "id"]);
    assert_eq!(fm(&doc).id(), Some("one"));
}

/// A research-notes shaped example (written by hand).
#[test]
fn fair_style_note() {
    let src = "---\n\
type: reference\n\
id: ref-12\n\
title: \"Scoping AI verification\"\n\
date: 2025-11-03\n\
tags: [ai, verification]\n\
stage: reading\n\
source-url: \"https://example.org/paper\"\n\
source-local: \"—\"\n\
source-cache: \"cache/12_scoping.html, cache/12_scoping.txt\"\n\
---\n\
\n\
# Scoping AI verification\n\
\n\
See [[project-scoping-2026]].\n";
    let doc = md(src);
    let f = fm(&doc);
    assert_eq!(f.error, None);
    assert_eq!(f.id(), Some("ref-12"));
    assert_eq!(f.title(), Some("Scoping AI verification"));
    assert_eq!(f.dates(), [("date", "2025-11-03")]);
    assert_eq!(f.tags(), ["ai", "verification"]);
    assert_eq!(f.get("stage"), Some(&str_("reading")));
    assert_eq!(f.get("source-local"), Some(&str_("—")));
    let links = fm_links(&doc);
    let got: Vec<(LinkKind, &str)> = links
        .iter()
        .map(|l| (l.kind, &src[l.range.clone()]))
        .collect();
    assert_eq!(
        got,
        [
            (LinkKind::Url, "https://example.org/paper"),
            (LinkKind::BarePath, "cache/12_scoping.html"),
            (LinkKind::BarePath, "cache/12_scoping.txt"),
        ]
    );
    assert_eq!(tag_names(&doc), ["ai", "verification"]);
    assert!(
        doc.links()
            .any(|l| l.context == Context::Prose && l.target.raw == "project-scoping-2026")
    );
    assert_eq!(doc.headings().count(), 1);
}

mod totality {
    use mdroots_syntax::{Dialect, Element, parse};
    use proptest::prelude::*;

    fn frags() -> impl Strategy<Value = String> {
        let frag = prop_oneof![
            Just("---\n"),
            Just("+++\n"),
            Just("{\n"),
            Just("}\n"),
            Just("key: "),
            Just("k:: "),
            Just("Title: "),
            Just("k = "),
            Just("tags: "),
            Just("[[x]]"),
            Just("[#a, "),
            Just("]"),
            Just("\"a/b.md, c.txt\""),
            Just("\""),
            Just("'"),
            Just("  - "),
            Just("é😀"),
            Just("—"),
            Just("https://x.y"),
            Just("\n"),
            Just("\r\n"),
            Just("\t"),
            Just("#"),
            Just(","),
            Just("\u{feff}"),
        ];
        proptest::collection::vec(frag, 0..40).prop_map(|v| v.concat())
    }

    proptest! {
        #[test]
        fn ranges_are_valid(src in frags()) {
            let doc = parse(&src, Dialect::Markdown);
            let ok = |r: &std::ops::Range<usize>| r.start <= r.end
                && r.end <= src.len()
                && src.is_char_boundary(r.start)
                && src.is_char_boundary(r.end);
            for el in doc.elements() {
                prop_assert!(ok(&el.range()), "{:?} in {:?}", el, src);
                if let Element::Link(l) = el {
                    prop_assert!(ok(&l.text_range), "{:?} in {:?}", l, src);
                }
            }
            if let Some(f) = doc.frontmatter() {
                prop_assert!(ok(&f.range));
                let _ = (f.title(), f.aliases(), f.id(), f.tags(), f.summary(), f.dates(), f.site_path());
            }
        }
    }
}

/// Each link's range slices its own value: (key path, range).
fn nested_link_ranges(src: &str) -> Vec<std::ops::Range<usize>> {
    let doc = md(src);
    let links = fm_links(&doc);
    for l in &links {
        assert_eq!(&src[l.range.clone()], l.target.raw, "{src:?}");
    }
    links.iter().map(|l| l.range.clone()).collect()
}

#[test]
fn nested_map_links_point_at_their_own_bytes() {
    for (src, child) in [
        ("---\nfirst: x.md\nmeta: {path: x.md}\n---\n", "path: x.md"),
        ("---\nfirst: x.md\nmeta:\n  path: x.md\n---\n", "path: x.md"),
        (
            "+++\nfirst = \"x.md\"\n[meta]\npath = \"x.md\"\n+++\n",
            "path = \"x.md\"",
        ),
        (
            "+++\nfirst = \"x.md\"\nmeta.path = \"x.md\"\n+++\n",
            "path = \"x.md\"",
        ),
    ] {
        let ranges = nested_link_ranges(src);
        assert_eq!(ranges.len(), 2, "{src:?}: {ranges:?}");
        assert_ne!(ranges[0], ranges[1], "{src:?}");
        let at = src.find(child).unwrap();
        assert!(ranges[1].start > at, "{src:?}: {ranges:?}");
    }
    // A nested tag-free list in a flow map: each item at its own bytes.
    let src = "---\nfirst: a/b.md\nmeta: {see: \"a/b.md, c/d.md\"}\n---\n";
    let ranges = nested_link_ranges(src);
    assert_eq!(ranges.len(), 3, "{ranges:?}");
    assert!(ranges[1].start > src.find("see").unwrap());
}

#[test]
fn unlocatable_nested_value_gives_no_link() {
    // The parser's text (escape decoded) is not in the source: no link
    // rather than one pointing at the wrong bytes.
    let src = "---\nfirst: x.md\nmeta: {path: \"\\x78.md\"}\n---\n";
    let doc = md(src);
    let links = fm_links(&doc);
    for l in &links {
        assert_eq!(&src[l.range.clone()], l.target.raw);
    }
    assert_eq!(links.len(), 1, "{links:#?}");
}

#[test]
fn dates_include_week_and_year() {
    let doc = md("---\ndate: 2024-01-05\nweek: 2024-W01\nyear: 2024\ntitle: T\n---\n");
    assert_eq!(
        fm(&doc).dates(),
        [
            ("date", "2024-01-05"),
            ("week", "2024-W01"),
            ("year", "2024")
        ]
    );
}

#[test]
fn quoted_toml_table_key_keeps_its_dot() {
    let src = "+++\n[\"meta.data\"]\npath = \"x.md\"\n+++\n";
    let doc = parse(src, Dialect::Markdown);
    let links: Vec<_> = doc.links().map(|l| &src[l.range.clone()]).collect();
    assert_eq!(links, ["x.md"]);
}

/// The entries of the frontmatter `parse_bytes` reads from `src`.
fn entries_of(src: &str) -> Vec<(String, Value)> {
    let doc = mdroots_syntax::parse_bytes(
        src.as_bytes(),
        &mdroots_syntax::ParseOptions::new(Dialect::Markdown),
    )
    .expect("text");
    fm(&doc).entries().to_vec()
}

#[test]
fn block_scalars_are_one_line_of_text() {
    let src = "---\ndesc: |\n  a: b\n  c\nlist: >\n  - a\n  - b\nz: 1\n---\n\nbody\n";
    assert_eq!(
        entries_of(src),
        [
            ("desc".into(), str_("a: b c")),
            ("list".into(), str_("- a - b")),
            ("z".into(), str_("1")),
        ]
    );
}

#[test]
fn nested_list_items_are_the_parser_items() {
    let src =
        "---\nresources:\n- src: a.jpg\n  title: A\nl:\n  - a\n  - - b\n    - c\n---\n\nbody\n";
    assert_eq!(
        entries_of(src),
        [
            ("resources".into(), list(&["{…}"])),
            ("l".into(), list(&["a", "b, c"])),
        ]
    );
}

#[test]
fn map_list_items_are_placeholders() {
    let doc = md("---\ntags:\n- name: x\n- b\naliases:\n- k: v\n- A\n---\n");
    let f = fm(&doc);
    assert_eq!(f.tags(), ["b"]);
    assert_eq!(f.aliases(), ["A"]);
    assert_eq!(tag_names(&doc), ["b"]);
    // Only map items: no value at all.
    let doc = md("---\ntitle:\n- k: v\nfiles:\n- p: a.md\n---\n");
    assert_eq!(fm(&doc).title(), None);
    assert!(fm_links(&doc).is_empty());
    // A nested list item is a normal item.
    let doc = md("---\ntags:\n- - b\n  - c\n- d\n---\n");
    assert_eq!(fm(&doc).tags(), ["b, c", "d"]);
}

#[test]
fn block_scalar_list_items_are_their_text() {
    let src = "---\nl:\n  - |\n    a: b\n  - c\n---\n\nbody\n";
    assert_eq!(entries_of(src), [("l".into(), list(&["a: b", "c"]))]);
}

// ---------------------------------------------------------------------------
// Fields: top-level entries as written, with their ranges.

/// The byte range of `needle` in `src` (first occurrence).
fn at(src: &str, needle: &str) -> Range<usize> {
    let i = src.find(needle).expect(needle);
    i..i + needle.len()
}

/// `(key, display, text of the range)` per field.
fn shown<'a>(src: &'a str, fields: &'a [Field]) -> Vec<(&'a str, String, &'a str)> {
    fields
        .iter()
        .map(|f| (f.key.as_str(), f.value.display(), &src[f.range.clone()]))
        .collect()
}

fn children(f: &Field) -> &[Field] {
    match &f.value {
        FieldValue::Map(c) => c,
        v => panic!("not a map: {v:?}"),
    }
}

#[test]
fn yaml_fields_in_order_with_their_lines() {
    let src = "---\ntitle: T\nmeta:\n  a: 1\n  b: x\nflow: {k: v}\n\
               tags:\n  - a\n  - b\ndesc: |\n  one\n  two\nnothing: null\n---\n\nbody\n";
    let doc = md(src);
    let f = fm(&doc);
    assert!(f.parsed());
    assert_eq!(
        shown(src, f.fields()),
        [
            ("title", "T".into(), "title: T"),
            ("meta", "{…}".into(), "meta:\n  a: 1\n  b: x"),
            ("flow", "{…}".into(), "flow: {k: v}"),
            ("tags", "a, b".into(), "tags:\n  - a\n  - b"),
            ("desc", "one two".into(), "desc: |\n  one\n  two"),
            ("nothing", "null".into(), "nothing: null"),
        ]
    );
    let fields = f.fields();
    assert_eq!(
        fields[3].value,
        FieldValue::List(vec!["a".into(), "b".into()])
    );
    assert_eq!(fields[5].value, FieldValue::Scalar("null".into()));
    // entries() keeps flattening and the null mapping.
    assert_eq!(f.get("meta.a"), Some(&str_("1")));
    assert_eq!(f.get("nothing"), Some(&Value::Null));
}

#[test]
fn nested_map_children_have_their_ranges() {
    let src = "---\nm: {a: 1, b: {c: 2}}\nn:\n  x: 1\n  y:\n    z: 2\n---\n";
    let doc = md(src);
    let f = fm(&doc);
    let inner = f.inner();
    let [m, n] = f.fields() else {
        panic!("{:?}", f.fields())
    };
    // A flow map: the parser alone knows the children; they take the
    // parent's range.
    assert_eq!(m.range, at(src, "m: {a: 1, b: {c: 2}}"));
    let mc = children(m);
    assert_eq!(
        mc.iter().map(|c| c.key.as_str()).collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert!(mc.iter().all(|c| c.range == m.range));
    assert_eq!(children(&mc[1])[0].key, "c");
    assert_eq!(children(&mc[1])[0].range, m.range);
    // A block map: the scan located every child (whole lines, indentation
    // included).
    assert_eq!(n.range, at(src, "n:\n  x: 1\n  y:\n    z: 2"));
    let [x, y] = children(n) else { panic!() };
    assert_eq!(x.range, at(src, "  x: 1"));
    assert_eq!(y.range, at(src, "  y:\n    z: 2"));
    let [z] = children(y) else { panic!() };
    assert_eq!(
        (z.key.as_str(), z.range.clone()),
        ("z", at(src, "    z: 2"))
    );
    assert_eq!(z.value, FieldValue::Scalar("2".into()));
    assert!(inner.start <= m.range.start && n.range.end <= inner.end);
}

#[test]
fn map_display_is_an_ellipsis() {
    assert_eq!(FieldValue::Map(Vec::new()).display(), "{…}");
    assert_eq!(
        FieldValue::List(vec!["a".into(), "b".into()]).display(),
        "a, b"
    );
    assert_eq!(FieldValue::Scalar("x".into()).display(), "x");
}

#[test]
fn toml_fields_table_and_dotted_key() {
    // Keys the parser alone found: as it reports them, over the inner block.
    let src = "+++\na.b = 1\n[t]\nk = 2\n+++\n";
    let doc = md(src);
    let f = fm(&doc);
    let inner = f.inner();
    assert_eq!(&src[inner.clone()], "a.b = 1\n[t]\nk = 2");
    assert_eq!(
        shown(src, f.fields()),
        [
            ("a", "{…}".into(), &src[inner.clone()]),
            ("t", "{…}".into(), &src[inner.clone()]),
        ]
    );
    let t = &f.fields()[1];
    assert_eq!(children(t)[0].key, "k");
    assert_eq!(children(t)[0].range, inner);
    // A table next to scanned keys: the scan's lines where a key matches.
    let src = "+++\ntitle = \"T\"\n[extra]\nk = 1\n+++\n";
    let doc = md(src);
    let f = fm(&doc);
    assert_eq!(f.fields()[0].key, "title");
    assert_eq!(f.fields()[0].range, at(src, "title = \"T\""));
    assert_eq!(f.fields()[1].key, "extra");
    assert_eq!(f.fields()[1].range, f.inner());
}

#[test]
fn toml_text_as_written() {
    // Parser-only path (a table forces it): floats keep `.0`, a nested
    // array is one item written as TOML.
    let src = "+++\nx = [[1, 2], 3]\ny = 1.0\n[t]\nk = 2.0\n+++\n";
    let doc = md(src);
    let f = fm(&doc);
    let shown: Vec<_> = f
        .fields()
        .iter()
        .map(|f| (f.key.as_str(), f.value.clone()))
        .collect();
    assert_eq!(
        shown[..2],
        [
            ("x", FieldValue::List(vec!["[1, 2]".into(), "3".into()])),
            ("y", FieldValue::Scalar("1.0".into())),
        ]
    );
    assert_eq!(children(&f.fields()[2])[0].value.display(), "2.0");
    assert_eq!(f.get("x"), Some(&list(&["[1, 2]", "3"])));
    assert_eq!(f.get("t.k"), Some(&str_("2.0")));
    // Scanned path: the source text.
    let src = "+++\ny = 1.0\nx = [[1, 2]]\n+++\n";
    let doc = md(src);
    let f = fm(&doc);
    assert_eq!(f.fields()[0].value, FieldValue::Scalar("1.0".into()));
    assert_eq!(f.fields()[1].value, FieldValue::List(vec!["[1, 2]".into()]));
}

#[test]
fn unparsed_and_blank_blocks() {
    // Not blank, no key: a comment counts as text.
    for src in [
        "---\njust some text\nmore text\n---\n",
        "---\n# only a comment\n---\n",
    ] {
        let doc = md(src);
        let f = fm(&doc);
        assert!(!f.parsed(), "{src:?}");
        assert!(f.fields().is_empty(), "{src:?}");
    }
    for src in ["---\n---\n", "---\n\n  \n---\n", "+++\n+++\n"] {
        let doc = md(src);
        let f = fm(&doc);
        assert!(f.parsed(), "{src:?}");
        assert!(f.fields().is_empty(), "{src:?}");
    }
}

#[test]
fn inner_is_between_the_fences() {
    let src = "---\na: 1\n---\n\n# H\n";
    assert_eq!(&src[fm(&md(src)).inner()], "a: 1");
    let src = "+++\na = 1\n+++\n";
    assert_eq!(&src[fm(&md(src)).inner()], "a = 1");
    let src = "---\n---\n";
    assert_eq!(fm(&md(src)).inner(), 4..4);
    let src = "---\r\na: 1\r\n---\r\n\r\nbody\r\n";
    let doc = md(src);
    let f = fm(&doc);
    assert_eq!(&src[f.inner()], "a: 1\r");
    assert_eq!(f.fields()[0].range, at(src, "a: 1"));
    // Unfenced: the whole range.
    let src = "title:: X\n\nbody\n";
    let doc = md(src);
    assert_eq!(fm(&doc).inner(), fm(&doc).range);
}

#[test]
fn logseq_fields_are_the_lines() {
    let src = "title:: X\nalias:: [[y]], [[z]]\nempty:: \n\nbody\n";
    let doc = md(src);
    let f = fm(&doc);
    assert!(f.parsed());
    assert_eq!(
        shown(src, f.fields()),
        [
            ("title", "X".into(), "title:: X"),
            ("alias", "[[y]], [[z]]".into(), "alias:: [[y]], [[z]]"),
            ("empty", "".into(), "empty:: "),
        ]
    );
}

#[test]
fn json_fields_keep_null_as_written() {
    let src = "{\n  \"title\": \"J\",\n  \"x\": {\"y\": null}\n}\n\n# Body\n";
    let doc = md(src);
    let f = fm(&doc);
    assert!(f.parsed());
    let keys: Vec<_> = f
        .fields()
        .iter()
        .map(|f| (f.key.as_str(), f.value.clone()))
        .collect();
    assert_eq!(
        keys,
        [
            ("title", FieldValue::Scalar("J".into())),
            ("x.y", FieldValue::Scalar("null".into())),
        ]
    );
    assert_eq!(f.get("x.y"), Some(&Value::Null));
}

#[test]
fn org_fields_from_entries() {
    let src = "#+TITLE: T\n#+FILETAGS: :a:b:\n\n* H\n";
    let doc = parse(src, Dialect::Org);
    let f = fm(&doc);
    assert_eq!(
        f.fields()
            .iter()
            .map(|f| (f.key.as_str(), f.value.display(), f.range.clone()))
            .collect::<Vec<_>>(),
        [
            ("title", "T".into(), f.range.clone()),
            ("filetags", "a, b".into(), f.range.clone()),
        ]
    );
}

#[test]
fn unfenced_frontmatter_can_be_turned_off() {
    let src = "title:: X\nalias:: [[y]]\n\nbody [[z]]\n";
    let mut opts = ParseOptions::new(Dialect::Markdown);
    assert!(opts.unfenced_frontmatter);
    let on = parse_with(src, &opts);
    assert_eq!(fm(&on).format, FrontmatterFormat::Logseq);
    assert_eq!(on, md(src));
    opts.unfenced_frontmatter = false;
    let off = parse_with(src, &opts);
    assert_eq!(off.frontmatter(), None);
    let links: Vec<_> = off
        .links()
        .map(|l| (l.target.raw.as_str(), l.context))
        .collect();
    assert_eq!(links, [("y", Context::Prose), ("z", Context::Prose)]);
    // MultiMarkdown and JSON headers stay prose too; fenced blocks don't.
    for src in [
        "Title: T\nAuthor: A\n\nbody\n",
        "{\n  \"a\": 1\n}\n\nbody\n",
    ] {
        assert!(md(src).frontmatter().is_some(), "{src:?}");
        assert_eq!(parse_with(src, &opts).frontmatter(), None, "{src:?}");
    }
    let src = "---\ntitle: T\n---\n\nbody\n";
    assert_eq!(parse_with(src, &opts), md(src));
    let mut org = ParseOptions::new(Dialect::Org);
    org.unfenced_frontmatter = false;
    let src = "#+TITLE: T\n\n* H\n";
    assert_eq!(parse_with(src, &org), parse(src, Dialect::Org));
}
