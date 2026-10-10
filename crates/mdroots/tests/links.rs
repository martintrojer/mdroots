//! `Workspace::link_style` and `Workspace::link_to` on in-memory roots
//! (`MemFs` with a `FakeProbe` holding the same files). Tool configs are
//! those of [zk](https://github.com/zk-org/zk) and
//! [Obsidian](https://obsidian.md).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mdroots::{ErrorKind, LinkStyle, NoEnumerator, Options, Workspace, relative_path};
use mdroots_core::MemFs;
use mdroots_roots::probe::FakeProbe;

const NOTES: &[(&str, &str)] = &[
    ("/v/a.md", "# Alpha\n"),
    ("/v/sub/deep/b.md", "---\ntitle: Bee note\n---\n"),
    ("/v/my notes/c (1).md", ""),
    ("/v/one/dup.md", ""),
    ("/v/two/dup.md", ""),
];

/// A root at `/v` holding `NOTES` plus `extra` (configs or a marker).
fn ws(extra: &[(&str, &str)]) -> Workspace {
    let mut fs = MemFs::new();
    let mut probe = FakeProbe::new().home("/h");
    for (path, text) in NOTES.iter().chain(extra) {
        fs = fs.with_file(path, text);
        probe = probe.file(path, text);
    }
    let opts = Options::default()
        .fs(Arc::new(fs))
        .probe(Arc::new(probe))
        .enumerator(Arc::new(NoEnumerator));
    Workspace::open_for(Path::new("/v/a.md"), opts).unwrap()
}

fn p(s: &str) -> PathBuf {
    PathBuf::from(s)
}

fn link(w: &Workspace, from: &str, to: &str, label: Option<&str>) -> String {
    w.link_to(&p(from), &p(to), label).unwrap()
}

const ZK_WIKI: &str = "[format.markdown]\nlink-format = \"wiki\"\n";

#[test]
fn wiki_path_from_zk_config() {
    let w = ws(&[("/v/.zk/config.toml", ZK_WIKI)]);
    assert_eq!(w.link_style(), LinkStyle::WikiPath);
    assert_eq!(
        link(&w, "/v/a.md", "/v/sub/deep/b.md", None),
        "[[sub/deep/b]]"
    );
    assert_eq!(
        link(&w, "/v/sub/deep/b.md", "/v/a.md", Some("the A")),
        "[[a|the A]]"
    );
    assert_eq!(
        link(&w, "/v/a.md", "/v/my notes/c (1).md", None),
        "[[my notes/c (1)]]"
    );
}

#[test]
fn wiki_stem_from_obsidian_falls_back_to_path_for_shared_stems() {
    let w = ws(&[("/v/.obsidian/app.json", "{}")]);
    assert_eq!(w.link_style(), LinkStyle::WikiStem);
    assert_eq!(link(&w, "/v/a.md", "/v/sub/deep/b.md", None), "[[b]]");
    assert_eq!(link(&w, "/v/a.md", "/v/one/dup.md", None), "[[one/dup]]");
    // A new note whose stem is taken.
    assert_eq!(link(&w, "/v/a.md", "/v/new/b.md", None), "[[new/b]]");
}

#[test]
fn markdown_relative_with_suffix_and_encoding() {
    let w = ws(&[("/v/.obsidian/app.json", r#"{"useMarkdownLinks": true}"#)]);
    assert_eq!(
        w.link_style(),
        LinkStyle::MarkdownRelative { md_suffix: true }
    );
    // Default label: the frontmatter title, else H1, else stem.
    assert_eq!(
        link(&w, "/v/a.md", "/v/sub/deep/b.md", None),
        "[Bee note](sub/deep/b.md)"
    );
    assert_eq!(
        link(&w, "/v/sub/deep/b.md", "/v/a.md", None),
        "[Alpha](../../a.md)"
    );
    assert_eq!(
        link(&w, "/v/sub/deep/b.md", "/v/my notes/c (1).md", None),
        "[c (1)](../../my%20notes/c%20%281%29.md)"
    );
    assert_eq!(
        link(&w, "/v/one/dup.md", "/v/two/dup.md", Some("[x]")),
        "[\\[x\\]](../two/dup.md)"
    );
}

#[test]
fn markdown_root_relative_from_obsidian_absolute() {
    let w = ws(&[(
        "/v/.obsidian/app.json",
        r#"{"useMarkdownLinks": true, "newLinkFormat": "absolute"}"#,
    )]);
    assert_eq!(
        w.link_style(),
        LinkStyle::MarkdownRootRelative { md_suffix: true }
    );
    assert_eq!(
        link(&w, "/v/sub/deep/b.md", "/v/my notes/c (1).md", Some("C")),
        "[C](my%20notes/c%20%281%29.md)"
    );
}

/// The note `link` (written in `from`) resolves to.
fn target(w: &Workspace, from: &str, link: &str) -> Vec<PathBuf> {
    w.resolve(&p(from), link).unwrap().targets
}

#[test]
fn root_relative_links_are_not_shadowed_by_a_sibling() {
    let shadow = [("/v/sub/ref.md", ""), ("/v/sub/a.md", "# Sub A\n")];
    let obsidian_abs = (
        "/v/.obsidian/app.json",
        r#"{"useMarkdownLinks": true, "newLinkFormat": "absolute"}"#,
    );
    let zk_wiki = ("/v/.zk/config.toml", ZK_WIKI);
    for cfg in [obsidian_abs, zk_wiki] {
        let w = ws(&[shadow[0], shadow[1], cfg]);
        let l = link(&w, "/v/sub/ref.md", "/v/a.md", None);
        assert_eq!(target(&w, "/v/sub/ref.md", &l), [p("/v/a.md")], "{l}");
        // Unshadowed links keep the bare root-relative form.
        let l = link(&w, "/v/sub/ref.md", "/v/sub/deep/b.md", None);
        assert!(
            l.contains("(sub/deep/b.md)") || l == "[[sub/deep/b]]",
            "{l}"
        );
    }
}

#[test]
fn wiki_stem_shared_with_a_root_note_is_not_shadowed() {
    let w = ws(&[
        ("/v/.obsidian/app.json", "{}"),
        ("/v/one/x.md", ""),
        ("/v/dup.md", ""),
    ]);
    for to in ["/v/dup.md", "/v/one/dup.md", "/v/two/dup.md"] {
        let l = link(&w, "/v/one/x.md", to, None);
        assert_eq!(target(&w, "/v/one/x.md", &l), [p(to)], "{l}");
    }
}

#[test]
fn relative_paths() {
    let r = |a: &str, b: &str| relative_path(Path::new(a), Path::new(b));
    assert_eq!(r("/r/a", "/r/b/c.md"), "../b/c.md");
    assert_eq!(r("/r", "/r/c.md"), "c.md");
    assert_eq!(r("/r/a/b", "/r/a/c.md"), "../c.md");
    // `..` in either path is folded before comparing.
    assert_eq!(r("/r/a/../b", "/r/c.md"), "../c.md");
    assert_eq!(r("/r/a", "/r/a/x/../c.md"), "c.md");
    // Root-relative input, as `link_to` passes it.
    assert_eq!(r("", "a/b.md"), "a/b.md");
    assert_eq!(r("x", "a/b.md"), "../a/b.md");
    // A leading `..` of a relative target is kept, not folded away.
    assert_eq!(r("", "../b.md"), "../b.md");
    assert_eq!(r("a", "../b.md"), "../../b.md");
    assert_eq!(r("a", "../../b.md"), "../../../b.md");
    assert_eq!(r("../x", "../b.md"), "../b.md");
    // Above `/` there is nothing to climb to.
    assert_eq!(r("/", "/../b.md"), "b.md");
}

#[test]
fn markdown_destinations_encode_characters_that_end_or_alter_them() {
    let names = [
        "a#b.md", "a)b.md", "a(b.md", "a<b>.md", "t\tab.md", "100%.md",
    ];
    let extra: Vec<(String, &str)> = names.iter().map(|n| (format!("/v/x/{n}"), "")).collect();
    let mut files: Vec<(&str, &str)> = extra.iter().map(|(f, t)| (f.as_str(), *t)).collect();
    files.push(("/v/.obsidian/app.json", r#"{"useMarkdownLinks": true}"#));
    let w = ws(&files);
    for (to, _) in &extra {
        let l = link(&w, "/v/a.md", to, None);
        assert_eq!(target(&w, "/v/a.md", &l), [p(to)], "{l}");
    }
    assert_eq!(
        link(&w, "/v/a.md", "/v/x/a#b.md", Some("L")),
        "[L](x/a%23b.md)"
    );
}

#[test]
fn markdown_destinations_are_not_character_references() {
    // `&copy;` in a destination decodes to `©`: `a&copy;.md` must not
    // lead to `a©.md`.
    let names = ["/v/x/a&copy;.md", "/v/x/a©.md"];
    for cfg in [
        r#"{"useMarkdownLinks": true}"#,
        r#"{"useMarkdownLinks": true, "newLinkFormat": "absolute"}"#,
    ] {
        let w = ws(&[
            (names[0], ""),
            (names[1], ""),
            ("/v/.obsidian/app.json", cfg),
        ]);
        for to in names {
            let l = link(&w, "/v/a.md", to, Some("L"));
            assert_eq!(target(&w, "/v/a.md", &l), [p(to)], "{l}");
        }
        assert_eq!(
            link(&w, "/v/a.md", names[0], Some("L")),
            "[L](x/a%26copy;.md)"
        );
    }
}

#[test]
fn zk_default_drops_the_extension() {
    let w = ws(&[("/v/.zk/config.toml", "")]);
    assert_eq!(
        w.link_style(),
        LinkStyle::MarkdownRelative { md_suffix: false }
    );
    assert_eq!(
        link(&w, "/v/a.md", "/v/sub/deep/b.md", None),
        "[Bee note](sub/deep/b)"
    );
}

#[test]
fn non_existent_target_uses_its_stem() {
    let w = ws(&[("/v/.obsidian/app.json", r#"{"useMarkdownLinks": true}"#)]);
    assert_eq!(
        link(&w, "/v/sub/deep/b.md", "/v/sub/fresh idea.md", None),
        "[fresh idea](../fresh%20idea.md)"
    );
    let w = ws(&[("/v/.zk/config.toml", ZK_WIKI)]);
    assert_eq!(
        link(&w, "/v/a.md", "/v/sub/fresh.md", None),
        "[[sub/fresh]]"
    );
}

#[test]
fn vote_decides_without_config_and_follows_overlays() {
    let w = ws(&[("/v/.mdroots", "")]);
    // No explicit links yet.
    assert_eq!(
        w.link_style(),
        LinkStyle::MarkdownRelative { md_suffix: true }
    );
    w.set_overlay(&p("/v/a.md"), "# Alpha\n\n[[b]] and [[dup]] [[one/dup]]\n")
        .unwrap();
    assert_eq!(w.link_style(), LinkStyle::WikiStem);
    assert_eq!(link(&w, "/v/sub/deep/b.md", "/v/a.md", None), "[[a]]");
}

#[test]
fn unwritable_wiki_targets_and_outside_paths_are_unsupported() {
    let w = ws(&[("/v/.zk/config.toml", ZK_WIKI)]);
    let err =
        |to: &str, label: Option<&str>| w.link_to(&p("/v/a.md"), &p(to), label).unwrap_err().kind();
    assert_eq!(err("/v/a|b.md", None), ErrorKind::Unsupported);
    assert_eq!(err("/v/a]b.md", None), ErrorKind::Unsupported);
    assert_eq!(err("/v/ok.md", Some("x]]y")), ErrorKind::Unsupported);
    assert_eq!(err("/elsewhere/x.md", None), ErrorKind::Unsupported);
}
