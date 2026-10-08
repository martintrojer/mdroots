//! Dialect detection and the corpus convention vote.

use std::collections::HashMap;
use std::path::Path;

use mdroots_resolve::ResolveEnv;
use mdroots_resolve::ResolveStep;
use mdroots_resolve::dialect::{
    DialectMarker, RootConventions, Severity, Vote, VoteResult, detect, link_severity,
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
    assert_eq!(c.attachment_folder, None);
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
        "attachmentFolderPath": "assets/img"} "#;
    let c = detect(&FakeEnv::new(&[(".obsidian/app.json", good)]));
    assert_eq!(c.use_markdown_links, Some(false));
    assert_eq!(c.attachment_folder.as_deref(), Some("assets/img"));
}

#[test]
fn app_json_top_level_keys_only() {
    let app = |text: &str| detect(&FakeEnv::new(&[(".obsidian/app.json", text)]));
    assert_eq!(
        app(r#"{"nested":{"useMarkdownLinks":true}}"#).use_markdown_links,
        None
    );
    assert_eq!(
        app(r#"{"attachmentFolderPath":"\uD83D\uDE00"}"#)
            .attachment_folder
            .as_deref(),
        Some("😀")
    );
    assert_eq!(
        app(r#"{"attachmentFolderPath":"a\u0062"}"#)
            .attachment_folder
            .as_deref(),
        Some("ab")
    );
    assert_eq!(
        app(r#"{"attachmentFolderPath":"ab"}"#)
            .attachment_folder
            .as_deref(),
        Some("ab")
    );
    assert_eq!(
        app(r#"{"useMarkdownLinks":"yes"}"#).use_markdown_links,
        None
    );
}

#[test]
fn edn_key_matches_whole_token_only() {
    let fmt = |text: &str| detect(&FakeEnv::new(&[("logseq/config.edn", text)])).logseq_name_format;
    assert_eq!(fmt("{:x:file/name-format :wrong}"), None);
    assert_eq!(fmt("{:file/name-formatx :wrong}"), None);
    assert_eq!(
        fmt("{:file/name-format :triple-lowbar}").as_deref(),
        Some("triple-lowbar")
    );
    assert_eq!(
        fmt("{:a 1,:file/name-format :legacy}").as_deref(),
        Some("legacy")
    );
}

#[test]
fn logseq_name_format() {
    let env = FakeEnv::new(&[(
        "logseq/config.edn",
        "{:meta/version 1\n ;; :file/name-format :legacy\n :file/name-format :triple-lowbar}\n",
    )]);
    let c = detect(&env);
    assert_eq!(c.markers, vec![DialectMarker::Logseq]);
    assert_eq!(c.logseq_name_format.as_deref(), Some("triple-lowbar"));
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
