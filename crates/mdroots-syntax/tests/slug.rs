//! GitHub heading slugs (cases from ramble tests/doc.rs).

use std::collections::HashSet;

use mdroots_syntax::{Dialect, parse, slug};

#[test]
fn heading_slugs() {
    let src = "# Hello World\n\n## Hello World\n\n## Hello, World!\n\n### What's new? 😀 Emoji\n\n\
               ## 日本語\n\n## snake_case and-dash\n\n# Custom *styled* {#my-id}\n\n## Hello World\n";
    let doc = parse(src, Dialect::Markdown);
    let got: Vec<(u8, &str, &str)> = doc
        .headings()
        .map(|h| (h.level, h.text.as_str(), h.slug.as_str()))
        .collect();
    assert_eq!(
        got,
        vec![
            (1, "Hello World", "hello-world"),
            (2, "Hello World", "hello-world-1"),
            (2, "Hello, World!", "hello-world-2"),
            (3, "What's new? 😀 Emoji", "whats-new--emoji"),
            (2, "日本語", "日本語"),
            (2, "snake_case and-dash", "snake_case-and-dash"),
            (1, "Custom styled", "custom-styled"),
            (2, "Hello World", "hello-world-3"),
        ]
    );
    let custom = doc.headings().nth(6).unwrap();
    assert_eq!(custom.id.as_deref(), Some("my-id"));
}

#[test]
fn heading_slugs_keep_combining_marks() {
    let doc = parse(
        "## हिन्दी\n\n## e\u{301}te!\n\n## \u{20DD}x 😀\n",
        Dialect::Markdown,
    );
    let slugs: Vec<&str> = doc.headings().map(|h| h.slug.as_str()).collect();
    // Mn (virama U+094D, acute U+0301) and Me (U+20DD) are kept like
    // github-slugger; punctuation and emoji are still dropped.
    assert_eq!(slugs, ["हिन्दी", "e\u{301}te", "\u{20DD}x-"]);
}

#[test]
fn unique_counts_up() {
    let mut used = HashSet::new();
    assert_eq!(slug::unique("a", &mut used), "a");
    assert_eq!(slug::unique("a", &mut used), "a-1");
    assert_eq!(slug::unique("a", &mut used), "a-2");
    assert_eq!(slug::unique("a-1", &mut used), "a-1-1");
    assert_eq!(slug::github("  Hello, World!  "), "hello-world");
}
