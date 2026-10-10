//! Dialect detection and the corpus convention vote.

use std::collections::HashMap;
use std::path::Path;

use mdroots_resolve::ResolveEnv;
use mdroots_resolve::ResolveStep;
use mdroots_resolve::dialect::{
    DialectMarker, LinkStyle, RootConventions, Setting, Severity, Source, Vote, VoteResult, detect,
    explain, link_severity, link_style,
};
use mdroots_syntax::{Dialect, Link, parse};

/// In-memory root: paths map to file contents; dirs are listed with a
/// trailing `/` and contain no content.
#[derive(Default)]
struct FakeEnv {
    files: HashMap<String, String>,
}

impl FakeEnv {
    fn new(entries: &[(&str, &str)]) -> Self {
        Self {
            files: entries
                .iter()
                .map(|(p, c)| ((*p).to_owned(), (*c).to_owned()))
                .collect(),
        }
    }
}

impl ResolveEnv for FakeEnv {
    fn exists(&self, root_rel: &str) -> bool {
        assert!(!root_rel.ends_with("notebook.db"), "touched notebook.db");
        self.files.contains_key(root_rel)
            || self.files.keys().any(|k| {
                k.strip_prefix(root_rel)
                    .is_some_and(|rest| rest.starts_with('/'))
            })
    }
    fn is_file(&self, root_rel: &str) -> bool {
        self.files.contains_key(root_rel)
    }
    fn case_sensitive(&self) -> bool {
        true
    }
    fn home_dir(&self) -> Option<&Path> {
        None
    }
    fn read_config(&self, root_rel: &str) -> Option<String> {
        assert!(!root_rel.ends_with("notebook.db"), "touched notebook.db");
        self.files.get(root_rel).cloned()
    }
}

const ZK_DOCS: &str = r#"
[note]
filename = "{{id}}-{{slug title}}"

[format.markdown]
link-format = "wiki"
hashtags = true
colon-tags = false
multiword-tags = false

[lsp.diagnostics]
dead-link = "error"
"#;

#[test]
fn zk_root_like_docs() {
    let env = FakeEnv::new(&[(".zk/config.toml", ZK_DOCS), ("a.md", "# A")]);
    let c = detect(&env);
    assert_eq!(c.markers, vec![DialectMarker::Zk]);
    assert_eq!(c.dead_link_severity, Some(Severity::Error));
    assert!(!c.dead_link_off);
    assert_eq!(c.hashtags, Some(true));
    assert_eq!(c.colon_tags, Some(false));
    assert_eq!(c.multiword_tags, Some(false));
    assert_eq!(c.wiki_link_format.as_deref(), Some("wiki"));
}

#[test]
fn several_markers_are_merged() {
    let env = FakeEnv::new(&[
        (".zk/config.toml", ""),
        (".obsidian/workspace.json", "{}"),
        (".git/HEAD", "ref"),
    ]);
    assert_eq!(
        detect(&env).markers,
        vec![DialectMarker::Zk, DialectMarker::Obsidian]
    );
}

#[test]
fn obsidian_app_json() {
    let env = FakeEnv::new(&[(
        ".obsidian/app.json",
        r#"{"useMarkdownLinks": true, "newLinkFormat": "relative"}"#,
    )]);
    let c = detect(&env);
    assert_eq!(c.markers, vec![DialectMarker::Obsidian]);
    assert_eq!(c.use_markdown_links, Some(true));
    assert_eq!(c.new_link_format.as_deref(), Some("relative"));
}

#[test]
fn malformed_app_json_leaves_obsidian_fields_none() {
    let bad = [
        r#"{"useMarkdownLinks": true garbage}"#,
        r#"{"useMarkdownLinks": true,}"#,
        r#"{"useMarkdownLinks": true} trailing"#,
        r#"{"useMarkdownLinks": true, "newLinkFormat": "rel\qx"}"#,
        r#"{"useMarkdownLinks": true, "x": [1, 2}"#,
        r#"{"useMarkdownLinks": true "newLinkFormat": "relative"}"#,
        r#"[{"useMarkdownLinks": true}]"#,
        r#"{"useMarkdownLinks": tru}"#,
    ];
    for text in bad {
        let c = detect(&FakeEnv::new(&[(".obsidian/app.json", text)]));
        assert_eq!(c.markers, vec![DialectMarker::Obsidian]);
        assert_eq!(c.use_markdown_links, None, "{text}");
        assert_eq!(c.new_link_format, None, "{text}");
    }
    let good = r#" {"a": {"b": [1, -2.5e3, null, "\u00e9"]}, "useMarkdownLinks": false,
        "newLinkFormat": "\u0072elative"} "#;
    let c = detect(&FakeEnv::new(&[(".obsidian/app.json", good)]));
    assert_eq!(c.use_markdown_links, Some(false));
    assert_eq!(c.new_link_format.as_deref(), Some("relative"));
}

#[test]
fn app_json_top_level_keys_only() {
    let app = |text: &str| detect(&FakeEnv::new(&[(".obsidian/app.json", text)]));
    assert_eq!(
        app(r#"{"nested":{"useMarkdownLinks":true}}"#).use_markdown_links,
        None
    );
    assert_eq!(
        app(r#"{"newLinkFormat":"\uD83D\uDE00"}"#)
            .new_link_format
            .as_deref(),
        Some("😀")
    );
    assert_eq!(
        app(r#"{"useMarkdownLinks":"yes"}"#).use_markdown_links,
        None
    );
}

#[test]
fn logseq_config_is_a_marker_only() {
    let env = FakeEnv::new(&[("logseq/config.edn", "{:file/name-format :triple-lowbar}\n")]);
    let c = detect(&env);
    assert_eq!(c.markers, vec![DialectMarker::Logseq]);
    let mut want = RootConventions::default();
    want.markers = vec![DialectMarker::Logseq];
    assert_eq!(c, want);
}

#[test]
fn mkdocs_docs_dir() {
    let env = FakeEnv::new(&[("mkdocs.yml", "site_name: X\ndocs_dir: site-docs\n")]);
    let c = detect(&env);
    assert_eq!(c.markers, vec![DialectMarker::Mkdocs]);
    assert_eq!(c.docs_dir.as_deref(), Some("site-docs"));

    let env = FakeEnv::new(&[("mkdocs.yml", "site_name: X\n")]);
    assert_eq!(detect(&env).docs_dir.as_deref(), Some("docs"));
}

#[test]
fn fixed_name_markers() {
    let cases: &[(&[&str], DialectMarker)] = &[
        (&["org-roam.db"], DialectMarker::OrgRoam),
        (&[".orgids"], DialectMarker::OrgRoam),
        (&["docusaurus.config.ts"], DialectMarker::Docusaurus),
        (&["hugo.toml"], DialectMarker::Hugo),
        (&["config.toml", "content/a.md"], DialectMarker::Hugo),
        (&["Home.md", "_Sidebar.md"], DialectMarker::Gollum),
        (&[".vscode/foam.json"], DialectMarker::Foam),
        (&["book.toml"], DialectMarker::MdBook),
        (&[".ztr-directory"], DialectMarker::Zettlr),
    ];
    for (paths, want) in cases {
        let entries: Vec<(&str, &str)> = paths.iter().map(|p| (*p, "")).collect();
        assert_eq!(
            detect(&FakeEnv::new(&entries)).markers,
            vec![*want],
            "{paths:?}"
        );
    }
    assert!(
        detect(&FakeEnv::new(&[("config.toml", "")]))
            .markers
            .is_empty()
    );
    assert!(detect(&FakeEnv::new(&[("Home.md", "")])).markers.is_empty());
    let hugo = detect(&FakeEnv::new(&[("hugo.toml", "")]));
    assert_eq!(hugo.docs_dir.as_deref(), Some("content"));
}

#[test]
fn garbage_config_leaves_fields_none() {
    let env = FakeEnv::new(&[
        (".zk/config.toml", "[format.markdown\nhashtags = = ]]\u{0}"),
        (".marksman.toml", "core = [ title_from_heading"),
        (".obsidian/app.json", "{\"useMarkdownLinks\": \"tru"),
        ("logseq/config.edn", "{:file/name-format"),
    ]);
    let c = detect(&env);
    assert_eq!(c.markers.len(), 4);
    let mut empty = RootConventions::default();
    empty.markers = c.markers.clone();
    assert_eq!(c, empty);
}

#[test]
fn zk_dead_link_none_turns_diagnostics_off() {
    let env = FakeEnv::new(&[(
        ".zk/config.toml",
        "[lsp.diagnostics]\ndead-link = \"none\"\n",
    )]);
    let c = detect(&env);
    assert!(c.dead_link_off);
    assert_eq!(c.dead_link_severity, None);
    // The vote never re-enables them: link_severity still answers, but the
    // caller must drop diagnostics because dead_link_off is set.
    let mut v = VoteResult::default();
    v.resolved_share = 1.0;
    v.explicit_links = 100;
    assert_eq!(link_severity(&c, &v), Severity::Error);
}

fn docs(n_one_h1: usize, n: usize) -> Vote {
    let mut vote = Vote::new();
    for i in 0..n {
        let src = if i < n_one_h1 {
            "# T\n\nx\n"
        } else {
            "# A\n\n# B\n"
        };
        vote.record_doc(&parse(src, Dialect::Markdown));
    }
    vote
}

#[test]
fn h1_rule_boundary() {
    assert!(docs(7, 10).result().h1_is_title);
    assert!(!docs(6, 10).result().h1_is_title);
    assert!(!Vote::new().result().h1_is_title);
}

#[test]
fn hashtags_need_three_tags_in_two_files() {
    let mut v = Vote::new();
    v.record_doc(&parse("#one #two #three\n", Dialect::Markdown));
    assert!(!v.result().hashtags_seen);
    v.record_doc(&parse("#one\n", Dialect::Markdown));
    assert!(v.result().hashtags_seen);
}

fn severity_for(share: f32) -> Severity {
    let mut v = VoteResult::default();
    v.resolved_share = share;
    v.explicit_links = 100;
    link_severity(&RootConventions::default(), &v)
}

#[test]
fn resolved_share_thresholds_when_config_is_silent() {
    assert_eq!(severity_for(0.981), Severity::Error);
    assert_eq!(severity_for(0.79), Severity::Hint);
    assert_eq!(severity_for(0.9), Severity::Warning);
    // No explicit links seen: stay at the default.
    let v = Vote::new().result();
    assert_eq!(v.resolved_share, 1.0);
    assert_eq!(
        link_severity(&RootConventions::default(), &v),
        Severity::Warning
    );
}

#[test]
fn config_wins_over_vote() {
    let mut conv = RootConventions::default();
    conv.dead_link_severity = Some(Severity::Error);
    let mut v = VoteResult::default();
    v.resolved_share = 0.5;
    v.explicit_links = 100;
    assert_eq!(link_severity(&conv, &v), Severity::Error);
    conv.dead_link_severity = Some(Severity::Info);
    v.resolved_share = 1.0;
    assert_eq!(link_severity(&conv, &v), Severity::Info);
}

fn links(src: &str) -> Vec<Link> {
    parse(src, Dialect::Markdown).links().cloned().collect()
}

#[test]
fn vote_counts_explicit_links() {
    let ls = links("[a](a.md) [b](b) [[c]] [[d]] <https://x.org>\n");
    let mut vote = Vote::new();
    let steps = [
        Some(ResolveStep::Stem),
        Some(ResolveStep::Stem),
        Some(ResolveStep::RootRelative),
        None,
    ];
    let mut explicit = ls
        .iter()
        .filter(|l| l.confidence == mdroots_syntax::Confidence::Explicit);
    for s in steps {
        vote.record_link(explicit.next().expect("explicit link"), s);
    }
    for l in explicit {
        vote.record_link(l, None);
    }
    for l in ls
        .iter()
        .filter(|l| l.confidence != mdroots_syntax::Confidence::Explicit)
    {
        vote.record_link(l, None);
    }
    let r = vote.result();
    let n_explicit = r.explicit_links as f32;
    assert!(n_explicit >= 4.0, "{r:?}");
    assert_eq!(r.insert_style, Some(ResolveStep::Stem));
    assert_eq!(r.resolved_share, 3.0 / n_explicit);
    assert_eq!(r.wiki_share, 0.5);
    assert_eq!(r.md_suffix_share, 0.5);
}

#[test]
fn insert_style_tie_goes_to_root_relative() {
    let ls = links("[a](a.md) [b](b.md)\n");
    let mut vote = Vote::new();
    vote.record_link(&ls[0], Some(ResolveStep::Stem));
    vote.record_link(&ls[1], Some(ResolveStep::RootRelative));
    assert_eq!(vote.result().insert_style, Some(ResolveStep::RootRelative));
    assert_eq!(Vote::new().result().insert_style, None);
}

// --- link_style --------------------------------------------------------

/// A vote that says Markdown, file-relative, with the suffix: what a config
/// must override.
fn md_vote() -> VoteResult {
    let mut v = VoteResult::default();
    v.explicit_links = 10;
    v.wiki_share = 0.0;
    v.md_suffix_share = 1.0;
    v.insert_style = Some(ResolveStep::FileRelative);
    v
}

fn zk_style(config: &str) -> LinkStyle {
    let env = FakeEnv::new(&[(".zk/config.toml", config)]);
    link_style(&detect(&env), &md_vote())
}

#[test]
fn zk_link_format_wins_over_the_vote() {
    let md = |s| format!("[format.markdown]\n{s}\n");
    let no_suffix = LinkStyle::MarkdownRelative { md_suffix: false };
    assert_eq!(zk_style(&md("link-format = \"wiki\"")), LinkStyle::WikiPath);
    assert_eq!(zk_style(&md("link-format = \"markdown\"")), no_suffix);
    // A config without link keys does not override the vote ...
    assert_eq!(
        zk_style("[note]\nfilename = \"{{id}}\"\n"),
        LinkStyle::MarkdownRelative { md_suffix: true }
    );
    // ... but either key alone does, with zk's defaults for the other.
    assert_eq!(zk_style(&md("link-drop-extension = true")), no_suffix);
    assert_eq!(
        zk_style(&md(
            "link-format = \"markdown\"\nlink-drop-extension = false"
        )),
        LinkStyle::MarkdownRelative { md_suffix: true }
    );
    assert_eq!(
        zk_style(&md("link-format = \"[[{{filename}}]]\"")),
        LinkStyle::WikiStem
    );
    assert_eq!(
        zk_style(&md("link-format = \"[[{{path}}]]\"")),
        LinkStyle::WikiPath
    );
    assert_eq!(
        zk_style(&md("link-format = \"[{{title}}]({{path}})\"")),
        no_suffix
    );
}

fn obsidian_style(app: &str) -> LinkStyle {
    let env = FakeEnv::new(&[(".obsidian/app.json", app)]);
    link_style(&detect(&env), &md_vote())
}

#[test]
fn obsidian_app_json_wins_over_the_vote() {
    let md = LinkStyle::MarkdownRelative { md_suffix: true };
    // No link keys: the vote (Markdown here) decides.
    assert_eq!(obsidian_style("{}"), md);
    assert_eq!(
        obsidian_style(r#"{"useMarkdownLinks": false}"#),
        LinkStyle::WikiStem
    );
    assert_eq!(
        obsidian_style(r#"{"newLinkFormat": "shortest"}"#),
        LinkStyle::WikiStem
    );
    assert_eq!(
        obsidian_style(r#"{"newLinkFormat": "relative"}"#),
        LinkStyle::WikiPath
    );
    assert_eq!(
        obsidian_style(r#"{"useMarkdownLinks": false, "newLinkFormat": "absolute"}"#),
        LinkStyle::WikiPath
    );
    assert_eq!(obsidian_style(r#"{"useMarkdownLinks": true}"#), md);
    assert_eq!(
        obsidian_style(r#"{"useMarkdownLinks": true, "newLinkFormat": "relative"}"#),
        md
    );
    assert_eq!(
        obsidian_style(r#"{"useMarkdownLinks": true, "newLinkFormat": "absolute"}"#),
        LinkStyle::MarkdownRootRelative { md_suffix: true }
    );
}

#[test]
fn zk_wins_over_obsidian() {
    let env = FakeEnv::new(&[
        (
            ".zk/config.toml",
            "[format.markdown]\nlink-format = \"wiki\"\n",
        ),
        (".obsidian/app.json", r#"{"useMarkdownLinks": true}"#),
    ]);
    assert_eq!(link_style(&detect(&env), &md_vote()), LinkStyle::WikiPath);
}

fn voted(wiki: f32, step: Option<ResolveStep>, suffix: f32) -> LinkStyle {
    let mut v = VoteResult::default();
    v.explicit_links = 10;
    v.wiki_share = wiki;
    v.insert_style = step;
    v.md_suffix_share = suffix;
    link_style(&RootConventions::default(), &v)
}

#[test]
fn vote_decides_without_tool_config() {
    use ResolveStep::*;
    assert_eq!(voted(0.5, Some(RootRelative), 0.0), LinkStyle::WikiPath);
    assert_eq!(voted(0.9, Some(Stem), 0.0), LinkStyle::WikiStem);
    assert_eq!(voted(0.9, Some(Title), 0.0), LinkStyle::WikiStem);
    assert_eq!(voted(0.9, Some(FileRelative), 0.0), LinkStyle::WikiStem);
    assert_eq!(voted(0.9, None, 0.0), LinkStyle::WikiStem);
    assert_eq!(
        voted(0.49, Some(RootRelative), 0.5),
        LinkStyle::MarkdownRootRelative { md_suffix: true }
    );
    assert_eq!(
        voted(0.0, Some(FileRelative), 0.49),
        LinkStyle::MarkdownRelative { md_suffix: false }
    );
    assert_eq!(
        voted(0.0, Some(Stem), 1.0),
        LinkStyle::MarkdownRelative { md_suffix: true }
    );
}

#[test]
fn bare_wiki_stem_never_votes_root_relative() {
    let ls = links("[[top]] [[dir/note]] [[other]]\n");
    let mut vote = Vote::new();
    vote.record_link(&ls[0], Some(ResolveStep::RootRelative));
    vote.record_link(&ls[1], Some(ResolveStep::RootRelative));
    vote.record_link(&ls[2], Some(ResolveStep::RootRelative));
    let r = vote.result();
    assert_eq!(r.insert_style, Some(ResolveStep::Stem));
    assert_eq!(r.resolved_share, 1.0);
    // A Markdown link without a slash is still root-relative.
    let ls = links("[a](a.md)\n");
    let mut vote = Vote::new();
    vote.record_link(&ls[0], Some(ResolveStep::RootRelative));
    assert_eq!(vote.result().insert_style, Some(ResolveStep::RootRelative));
}

#[test]
fn markers_give_the_style_of_a_root_without_links() {
    let empty = Vote::new().result();
    let style = |entries: &[(&str, &str)]| link_style(&detect(&FakeEnv::new(entries)), &empty);
    assert_eq!(
        style(&[(".obsidian/workspace.json", "{}")]),
        LinkStyle::WikiStem
    );
    assert_eq!(
        style(&[(".zk/config.toml", "")]),
        LinkStyle::MarkdownRelative { md_suffix: false }
    );
    assert_eq!(
        style(&[(".zk/config.toml", ""), (".obsidian/app.json", "{}")]),
        LinkStyle::MarkdownRelative { md_suffix: false }
    );
    // A config with link keys still wins over the marker default.
    assert_eq!(
        style(&[(
            ".zk/config.toml",
            "[format.markdown]\nlink-format = \"wiki\"\n"
        )]),
        LinkStyle::WikiPath
    );
    assert_eq!(
        style(&[(".obsidian/app.json", r#"{"useMarkdownLinks": true}"#)]),
        LinkStyle::MarkdownRelative { md_suffix: true }
    );
    // Once the notes have links, the vote beats the marker.
    assert_eq!(
        link_style(
            &detect(&FakeEnv::new(&[(".zk/config.toml", "")])),
            &md_vote()
        ),
        LinkStyle::MarkdownRelative { md_suffix: true }
    );
}

// --- explain -----------------------------------------------------------

fn rows(s: &[Setting]) -> Vec<(&str, &str, &Source)> {
    s.iter()
        .map(|r| (r.name, r.value.as_str(), &r.source))
        .collect()
}

const fn zk(key: &'static str) -> Source {
    Source::Config {
        tool: "zk",
        file: ".zk/config.toml",
        key,
    }
}

#[test]
fn explain_zk_config() {
    let c = detect(&FakeEnv::new(&[(".zk/config.toml", ZK_DOCS)]));
    let s = explain(&c, &md_vote());
    assert_eq!(
        rows(&s),
        vec![
            ("link style", "wiki-path", &zk("link-format")),
            ("hashtags", "on", &zk("hashtags")),
            ("colon tags", "off", &zk("colon-tags")),
            ("multiword tags", "off", &zk("multiword-tags")),
            ("broken links", "error", &zk("dead-link")),
        ]
    );
    let off = detect(&FakeEnv::new(&[(
        ".zk/config.toml",
        "[lsp.diagnostics]\ndead-link = \"none\"\n",
    )]));
    let s = explain(&off, &md_vote());
    assert_eq!(rows(&s)[4], ("broken links", "off", &zk("dead-link")));
}

#[test]
fn explain_obsidian_config() {
    let c = detect(&FakeEnv::new(&[(
        ".obsidian/app.json",
        r#"{"useMarkdownLinks": true, "newLinkFormat": "absolute"}"#,
    )]));
    let mut v = md_vote();
    v.resolved_share = 0.9;
    let s = explain(&c, &v);
    assert_eq!(
        rows(&s),
        vec![
            (
                "link style",
                "markdown-root-relative with .md",
                &Source::Config {
                    tool: "Obsidian",
                    file: ".obsidian/app.json",
                    key: "useMarkdownLinks, newLinkFormat",
                }
            ),
            ("hashtags", "on", &Source::Default),
            ("colon tags", "off", &Source::Default),
            ("multiword tags", "off", &Source::Default),
            ("broken links", "warning", &Source::Vote),
        ]
    );
}

#[test]
fn explain_marker_only_empty_root() {
    let c = detect(&FakeEnv::new(&[
        (".zk/config.toml", ""),
        ("mkdocs.yml", ""),
    ]));
    let s = explain(&c, &Vote::new().result());
    assert_eq!(
        rows(&s),
        vec![
            (
                "link style",
                "markdown-relative without .md",
                &Source::Marker(DialectMarker::Zk)
            ),
            ("hashtags", "on", &Source::Default),
            ("colon tags", "off", &Source::Default),
            ("multiword tags", "off", &Source::Default),
            ("broken links", "warning", &Source::Default),
            ("docs dir", "docs", &Source::Marker(DialectMarker::Mkdocs)),
        ]
    );
    let c = detect(&FakeEnv::new(&[("mkdocs.yml", "docs_dir: site\n")]));
    assert_eq!(
        rows(&explain(&c, &Vote::new().result()))[5],
        (
            "docs dir",
            "site",
            &Source::Config {
                tool: "mkdocs",
                file: "mkdocs.yml",
                key: "docs_dir",
            }
        )
    );
}

#[test]
fn explain_no_marker() {
    let c = detect(&FakeEnv::new(&[("a.md", "# A")]));
    let s = explain(&c, &Vote::new().result());
    assert_eq!(
        rows(&s),
        vec![
            ("link style", "markdown-relative with .md", &Source::Default),
            ("hashtags", "on", &Source::Default),
            ("colon tags", "off", &Source::Default),
            ("multiword tags", "off", &Source::Default),
            ("broken links", "warning", &Source::Default),
        ]
    );
    let mut v = md_vote();
    v.wiki_share = 1.0;
    v.insert_style = Some(ResolveStep::RootRelative);
    v.resolved_share = 1.0;
    let s = explain(&c, &v);
    assert_eq!(rows(&s)[0], ("link style", "wiki-path", &Source::Vote));
    assert_eq!(rows(&s)[4], ("broken links", "error", &Source::Vote));
}

#[test]
fn empty_root_defaults_to_markdown_with_suffix() {
    // Markers without a link config do not count as config.
    let env = FakeEnv::new(&[(".marksman.toml", "")]);
    assert_eq!(
        link_style(&detect(&env), &Vote::new().result()),
        LinkStyle::MarkdownRelative { md_suffix: true }
    );
}
