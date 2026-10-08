//! The org structure pass, through the public `parse`.

use std::path::Path;

use mdroots_syntax::{
    Anchor, Confidence, Context, Dialect, Document, FrontmatterFormat, Link, LinkKind, TagSyntax,
    Value, parse,
};

fn org(src: &str) -> Document {
    parse(src, Dialect::Org)
}

fn org_links(doc: &Document) -> Vec<&Link> {
    doc.links().filter(|l| l.kind == LinkKind::Org).collect()
}

fn one_link(src: &str) -> Link {
    let doc = org(src);
    let ls = org_links(&doc);
    assert_eq!(ls.len(), 1, "{src:?}: {ls:#?}");
    ls[0].clone()
}

#[test]
fn headings_with_todo_priority_tags() {
    let src = "* TODO [#A] Fix the thing :work:urgent:\n** Notes\n* DONE Notes\n";
    let doc = org(src);
    let got: Vec<(u8, &str, &str)> = doc
        .headings()
        .map(|h| (h.level, h.text.as_str(), h.slug.as_str()))
        .collect();
    assert_eq!(
        got,
        vec![
            (1, "Fix the thing", "fix-the-thing"),
            (2, "Notes", "notes"),
            (1, "Notes", "notes-1"),
        ]
    );
    let h = doc.headings().next().unwrap();
    assert_eq!(
        &src[h.range.clone()],
        "* TODO [#A] Fix the thing :work:urgent:\n"
    );
    let tags: Vec<(&str, TagSyntax, &str)> = doc
        .tags()
        .map(|t| (t.name.as_str(), t.syntax, &src[t.range.clone()]))
        .collect();
    assert_eq!(
        tags,
        vec![
            ("work", TagSyntax::OrgHeading, "work"),
            ("urgent", TagSyntax::OrgHeading, "urgent"),
        ]
    );
}

#[test]
fn heading_property_drawer() {
    let src = "* Intro\n:PROPERTIES:\n:ID: 1234-abcd\n:CUSTOM_ID: intro\n:END:\ntext\n";
    let doc = org(src);
    let h = doc.headings().next().unwrap();
    assert_eq!(h.text, "Intro");
    assert_eq!(h.id.as_deref(), Some("1234-abcd"));
    assert_eq!(h.custom_id.as_deref(), Some("intro"));
}

#[test]
fn keywords_into_frontmatter() {
    let src = "#+TITLE: My Notes\n#+FILETAGS: :a:b:\n#+DATE: 2024-01-01\n\n* H\n";
    let doc = org(src);
    let fm = doc.frontmatter().expect("frontmatter");
    assert_eq!(fm.format, FrontmatterFormat::OrgKeywords);
    assert_eq!(fm.get("title"), Some(&Value::Str("My Notes".into())));
    assert_eq!(
        fm.get("filetags"),
        Some(&Value::List(vec!["a".into(), "b".into()]))
    );
    assert_eq!(fm.get("date"), Some(&Value::Str("2024-01-01".into())));
    assert_eq!(&src[fm.range.clone()], &src[..src.find("* H").unwrap()]);
    let tags: Vec<(&str, TagSyntax)> = doc.tags().map(|t| (t.name.as_str(), t.syntax)).collect();
    assert_eq!(
        tags,
        vec![("a", TagSyntax::Frontmatter), ("b", TagSyntax::Frontmatter)]
    );
}

#[test]
fn file_level_property_drawer() {
    let src = ":PROPERTIES:\n:ID: abc\n:END:\n#+title: T\n\nbody\n";
    let fm = org(src).frontmatter().cloned().expect("frontmatter");
    assert_eq!(fm.get("id"), Some(&Value::Str("abc".into())));
    assert_eq!(fm.get("title"), Some(&Value::Str("T".into())));
}

#[test]
fn file_link_with_heading_search() {
    let src = "see [[file:notes/a.org::*Intro][see]] now";
    let l = one_link(src);
    assert_eq!(l.target.raw, "file:notes/a.org::*Intro");
    assert_eq!(l.target.path, "notes/a.org");
    assert_eq!(l.target.anchor, Some(Anchor::Heading("Intro".into())));
    assert_eq!(l.label.as_deref(), Some("see"));
    assert_eq!(l.confidence, Confidence::Explicit);
    assert_eq!(l.context, Context::Prose);
    assert_eq!(&src[l.range.clone()], "[[file:notes/a.org::*Intro][see]]");
    assert_eq!(&src[l.text_range.clone()], "see");
}

#[test]
fn file_link_custom_id_and_search() {
    let l = one_link("[[file:a.org::#cid]]");
    assert_eq!(l.target.path, "a.org");
    assert_eq!(l.target.anchor, Some(Anchor::CustomId("cid".into())));
    let l = one_link("[[file:a.org::some text]]");
    assert_eq!(l.target.anchor, Some(Anchor::Search("some text".into())));
}

#[test]
fn id_link() {
    let src = "x [[id:1234]]";
    let l = one_link(src);
    assert_eq!(l.target.raw, "id:1234");
    assert_eq!(l.target.path, "1234");
    assert_eq!(l.confidence, Confidence::Explicit);
    assert_eq!(&src[l.text_range.clone()], "id:1234");
}

#[test]
fn in_file_heading_and_custom_id_links() {
    let l = one_link("[[*Heading]]");
    assert_eq!(l.target.path, "");
    assert_eq!(l.target.anchor, Some(Anchor::Heading("Heading".into())));
    assert_eq!(l.confidence, Confidence::Explicit);
    let l = one_link("[[#cid]]");
    assert_eq!(l.target.path, "");
    assert_eq!(l.target.anchor, Some(Anchor::CustomId("cid".into())));
    assert_eq!(l.confidence, Confidence::Explicit);
}

#[test]
fn external_link() {
    let l = one_link("[[https://x][y]]");
    assert_eq!(l.confidence, Confidence::External);
    assert_eq!(l.target.raw, "https://x");
    assert_eq!(l.label.as_deref(), Some("y"));
}

#[test]
fn link_abbreviation_expands_and_is_external() {
    let l = one_link("#+LINK: T https://tasks/?t=%s\n\n[[T:123][task]]\n");
    assert_eq!(l.confidence, Confidence::External);
    assert_eq!(l.target.raw, "https://tasks/?t=123");
    assert_eq!(l.target.path, "https://tasks/?t=123");
    // Defined after the body text, and without %s: appended.
    let l = one_link("text\n[[D:9]]\n#+LINK: D https://d/\n");
    assert_eq!(l.target.raw, "https://d/9");
}

#[test]
fn unknown_prefix_is_external() {
    let l = one_link("[[T:9]]");
    assert_eq!(l.confidence, Confidence::External);
    assert_eq!(l.target.raw, "T:9");
}

#[test]
fn plain_target_is_internal() {
    let l = one_link("[[notes]]");
    assert_eq!(l.confidence, Confidence::Explicit);
    assert_eq!(l.target.raw, "notes");
    assert_eq!(l.target.path, "notes");
}

#[test]
fn heading_link() {
    let src = "* See [[a][b]] :t:\n";
    let doc = org(src);
    let l = org_links(&doc)[0];
    assert_eq!(l.context, Context::Heading);
    assert_eq!(doc.headings().next().unwrap().text, "See b");
}

#[test]
fn no_prose_links_in_code() {
    // Links in code are the scanner's candidates, tagged CodeBlock.
    let doc = org("#+begin_src sh\necho [[x]]\n#+end_src\n: fixed width [[y]]\n");
    assert!(prose_links(&doc).is_empty(), "{:#?}", prose_links(&doc));
    assert!(
        org_links(&doc)
            .iter()
            .all(|l| l.context == Context::CodeBlock)
    );
}

fn prose_links(doc: &Document) -> Vec<&Link> {
    doc.links()
        .filter(|l| l.context != Context::CodeBlock && l.context != Context::Comment)
        .collect()
}

#[test]
fn code_links_use_abbreviations_and_org_kind() {
    let src = "#+LINK: T https://t/%s\n\nprose [[T:1][a]] and [[T:2]]\n\n#+begin_src sh\n[[T:99][x]] [[T:98]]\n#+end_src\n: fixed [[T:100]] [[T:101][y]] [[notes]]\n";
    let doc = org(src);
    let got: Vec<(LinkKind, Context, Confidence, &str)> = doc
        .links()
        .map(|l| (l.kind, l.context, l.confidence, l.target.raw.as_str()))
        .collect();
    use Confidence::*;
    use Context::*;
    let o = LinkKind::Org;
    assert_eq!(
        got,
        [
            (o, Prose, External, "https://t/1"),
            (o, Prose, External, "https://t/2"),
            (o, CodeBlock, External, "https://t/99"),
            (o, CodeBlock, External, "https://t/98"),
            (o, CodeBlock, External, "https://t/100"),
            (o, CodeBlock, External, "https://t/101"),
            (o, CodeBlock, Explicit, "notes"),
        ]
    );
    let l = doc
        .links()
        .find(|l| l.target.raw == "https://t/101")
        .unwrap();
    assert_eq!(l.label.as_deref(), Some("y"));
    assert_eq!(&src[l.range.clone()], "[[T:101][y]]");
    assert_eq!(&src[l.text_range.clone()], "y");
    // Org search syntax applies in code too, and `!` is not an embed.
    let l = doc_link("#+begin_example\n![[file:a.org::*Sec]]\n#+end_example\n");
    assert_eq!(l.kind, LinkKind::Org);
    assert_eq!(l.target.path, "a.org");
    assert_eq!(l.target.anchor, Some(Anchor::Heading("Sec".into())));
}

fn doc_link(src: &str) -> Link {
    let doc = org(src);
    let ls: Vec<_> = doc.links().collect();
    assert_eq!(ls.len(), 1, "{src:?}: {ls:#?}");
    let l = ls[0].clone();
    assert_eq!(
        &src[l.range.clone()],
        src[l.range.clone()].trim_start_matches('!')
    );
    l
}

#[test]
fn blocks_close_only_on_exact_end_keyword() {
    let doc = org("#+begin_src\n#+end_src_extra\n[[still_inside]]\n#+end_src\n");
    assert!(prose_links(&doc).is_empty(), "{:#?}", prose_links(&doc));
    let doc = org("#+begin_comment\n#+end_commentary\n[[still_inside]]\n#+end_comment\n");
    assert!(prose_links(&doc).is_empty(), "{:#?}", prose_links(&doc));
    let l = one_link("#+begin_src\nx\n#+END_SRC  \n[[after]]\n");
    assert_eq!(l.target.raw, "after");
    assert_eq!(l.context, Context::Prose);
}

#[test]
fn tildes_in_links_are_not_code() {
    let doc = org("[[file:~/a]] and [[file:~/b]]\n");
    let ls = org_links(&doc);
    assert_eq!(ls.len(), 2);
    assert_eq!(ls[0].target.path, "~/a");
    assert_eq!(ls[1].target.path, "~/b");
}

#[test]
fn detect_dialect() {
    assert_eq!(Dialect::detect_from_path(Path::new("a.org")), Dialect::Org);
    assert_eq!(
        Dialect::detect_from_path(Path::new("a.md")),
        Dialect::Markdown
    );
}
