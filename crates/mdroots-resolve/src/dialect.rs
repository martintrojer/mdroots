//! Dialect markers and the per-root convention vote (docs/specs/index.md
//! §3). Markers are probed by fixed names through [`ResolveEnv`]; small
//! configs are read with `read_config`. `.zk/notebook.db` is never touched.

use std::collections::HashSet;

use mdroots_syntax::{Confidence, Document, Link, LinkKind, TagSyntax};

use crate::env::ResolveEnv;
use crate::keys::ResolveStep;

/// A tool or site generator whose marker was found at the root (§3.1).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DialectMarker {
    Zk,
    Obsidian,
    Marksman,
    Foam,
    Dendron,
    Logseq,
    OrgRoam,
    Gollum,
    Mkdocs,
    Docusaurus,
    Hugo,
    Jekyll,
    MdBook,
    Zettlr,
}

/// Diagnostic severity, as in LSP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Info,
    Hint,
}

/// What a root's markers and configs say. Several markers are merged, not
/// ranked; every field is `None` when no config states it.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RootConventions {
    /// Found markers, in enum declaration order.
    pub markers: Vec<DialectMarker>,
    /// zk `[format.markdown] hashtags`.
    pub hashtags: Option<bool>,
    /// zk `[format.markdown] colon-tags`.
    pub colon_tags: Option<bool>,
    /// zk `[format.markdown] multiword-tags`.
    pub multiword_tags: Option<bool>,
    /// zk `[format.markdown] link-format`.
    pub wiki_link_format: Option<String>,
    /// zk `[format.markdown] link-drop-extension`.
    pub link_drop_extension: Option<bool>,
    /// zk `[lsp.diagnostics] dead-link`.
    pub dead_link_severity: Option<Severity>,
    /// zk `dead-link = "none"`: broken-link diagnostics are switched off.
    pub dead_link_off: bool,
    /// Obsidian `app.json` `useMarkdownLinks`.
    pub use_markdown_links: Option<bool>,
    /// Obsidian `app.json` `newLinkFormat`.
    pub new_link_format: Option<String>,
    /// Obsidian `app.json` `attachmentFolderPath`.
    pub attachment_folder: Option<String>,
    /// marksman `core.title_from_heading`.
    pub title_from_heading: Option<bool>,
    /// Logseq `:file/name-format`, without the leading `:`.
    pub logseq_name_format: Option<String>,
    /// mkdocs `docs_dir` (default `docs`), Docusaurus `docs`, Hugo `content`.
    pub docs_dir: Option<String>,
}

/// Probe the root's markers and read their small configs. Never panics on
/// missing or malformed configs; the affected fields stay `None`.
pub fn detect(env: &dyn ResolveEnv) -> RootConventions {
    use DialectMarker::*;
    let e = |p: &str| env.exists(p);
    let probes: [(DialectMarker, bool); 14] = [
        (Zk, e(".zk")),
        (Obsidian, e(".obsidian")),
        (Marksman, e(".marksman.toml")),
        (Foam, e(".foam") || e(".vscode/foam.json")),
        (Dendron, e("dendron.yml")),
        (Logseq, e("logseq/config.edn")),
        (OrgRoam, e(".orgids") || e("org-roam.db")),
        (Gollum, e("Home.md") && e("_Sidebar.md")),
        (Mkdocs, e("mkdocs.yml")),
        (
            Docusaurus,
            ["js", "ts", "mjs", "cjs"]
                .iter()
                .any(|x| e(&format!("docusaurus.config.{x}"))),
        ),
        (Hugo, e("hugo.toml") || (e("config.toml") && e("content"))),
        (Jekyll, e("_config.yml")),
        (MdBook, e("book.toml")),
        (Zettlr, e(".ztr-directory")),
    ];
    let markers: Vec<DialectMarker> = probes.iter().filter(|p| p.1).map(|p| p.0).collect();
    let has = |m| markers.contains(&m);
    let mut conv = RootConventions::default();

    if has(Zk)
        && let Some(text) = env.read_config(".zk/config.toml")
    {
        read_zk(&text, &mut conv);
    }
    if has(Obsidian)
        && let Some(text) = env.read_config(".obsidian/app.json")
        && let Ok(serde_json::Value::Object(app)) = serde_json::from_str::<serde_json::Value>(&text)
    {
        let s = |k: &str| app.get(k).and_then(|v| v.as_str()).map(str::to_owned);
        conv.use_markdown_links = app.get("useMarkdownLinks").and_then(|v| v.as_bool());
        conv.new_link_format = s("newLinkFormat");
        conv.attachment_folder = s("attachmentFolderPath");
    }
    if has(Marksman)
        && let Some(text) = env.read_config(".marksman.toml")
        && let Ok(t) = text.parse::<toml::Table>()
    {
        conv.title_from_heading = t
            .get("core")
            .and_then(|c| c.get("title_from_heading"))
            .and_then(|v| v.as_bool());
    }
    if has(Logseq)
        && let Some(text) = env.read_config("logseq/config.edn")
    {
        conv.logseq_name_format = edn_keyword(&text, ":file/name-format");
    }
    conv.docs_dir = if has(Mkdocs) {
        Some(
            env.read_config("mkdocs.yml")
                .and_then(|t| yaml_top_scalar(&t, "docs_dir"))
                .unwrap_or_else(|| "docs".into()),
        )
    } else if has(Docusaurus) {
        Some("docs".into())
    } else if has(Hugo) {
        Some("content".into())
    } else {
        None
    };
    conv.markers = markers;
    conv
}

fn read_zk(text: &str, conv: &mut RootConventions) {
    let Ok(t) = text.parse::<toml::Table>() else {
        return;
    };
    if let Some(md) = t.get("format").and_then(|f| f.get("markdown")) {
        let b = |k: &str| md.get(k).and_then(|v| v.as_bool());
        conv.hashtags = b("hashtags");
        conv.colon_tags = b("colon-tags");
        conv.multiword_tags = b("multiword-tags");
        conv.wiki_link_format = md
            .get("link-format")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        conv.link_drop_extension = b("link-drop-extension");
    }
    let dead = t
        .get("lsp")
        .and_then(|l| l.get("diagnostics"))
        .and_then(|d| d.get("dead-link"))
        .and_then(|v| v.as_str());
    match dead {
        Some("error") => conv.dead_link_severity = Some(Severity::Error),
        Some("warning") => conv.dead_link_severity = Some(Severity::Warning),
        Some("info") => conv.dead_link_severity = Some(Severity::Info),
        Some("hint") => conv.dead_link_severity = Some(Severity::Hint),
        Some("none") => conv.dead_link_off = true,
        _ => {}
    }
}

/// Value of an EDN keyword-valued key such as `:file/name-format :triple-lowbar`,
/// without the leading `:`. Line comments (`;`) are skipped. The key matches
/// only as a whole token: preceded by line start, whitespace, `{`, `(`, `[`
/// or `,`, and followed by whitespace.
fn edn_keyword(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        let code = line.split(';').next().unwrap_or("");
        for (i, _) in code.match_indices(key) {
            let before_ok = code[..i]
                .chars()
                .next_back()
                .is_none_or(|c| c.is_whitespace() || matches!(c, '{' | '(' | '[' | ','));
            let after = &code[i + key.len()..];
            if !before_ok || !after.starts_with(char::is_whitespace) {
                continue;
            }
            let rest = after.trim_start();
            let tok: String = rest
                .chars()
                .take_while(|c| !c.is_whitespace() && !matches!(c, ',' | '}' | ']' | ')'))
                .collect();
            let tok = tok.trim_matches('"').trim_start_matches(':');
            if !tok.is_empty() {
                return Some(tok.to_owned());
            }
        }
    }
    None
}

/// Value of a top-level `key: scalar` line in YAML (unquoted or quoted).
fn yaml_top_scalar(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        let Some(rest) = line.strip_prefix(key) else {
            continue;
        };
        let Some(v) = rest.trim_start().strip_prefix(':') else {
            continue;
        };
        let v = v.trim();
        let v = if let Some(q) = v.strip_prefix('"') {
            q.split('"').next().unwrap_or("")
        } else if let Some(q) = v.strip_prefix('\'') {
            q.split('\'').next().unwrap_or("")
        } else {
            v.split(" #").next().unwrap_or("").trim()
        };
        if !v.is_empty() {
            return Some(v.to_owned());
        }
    }
    None
}

/// Counters for the per-root corpus vote (§3.2).
#[non_exhaustive]
#[derive(Debug, Clone, Default)]
pub struct Vote {
    explicit_links: u32,
    resolved: u32,
    wiki: u32,
    markdown: u32,
    md_suffix: u32,
    /// FileRelative, RootRelative, Stem, Title.
    steps: [u32; 4],
    docs: u32,
    docs_one_h1: u32,
    hash_tags: HashSet<String>,
    files_with_hash_tags: u32,
}

/// The outcome of a [`Vote`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Default)]
pub struct VoteResult {
    /// Most common step among FileRelative, RootRelative, Stem, Title over
    /// resolved explicit links; ties go to RootRelative.
    pub insert_style: Option<ResolveStep>,
    /// Wiki / (Wiki + Markdown) among explicit links.
    pub wiki_share: f32,
    /// Markdown links whose target path ends in `.md`, over Markdown links.
    pub md_suffix_share: f32,
    /// At least 70% of docs have exactly one H1.
    pub h1_is_title: bool,
    /// At least 3 distinct `#tags` seen in at least 2 files.
    pub hashtags_seen: bool,
    /// Resolved explicit links / explicit links (1.0 when there are none).
    pub resolved_share: f32,
    pub explicit_links: u32,
}

const STEP_ORDER: [ResolveStep; 4] = [
    ResolveStep::FileRelative,
    ResolveStep::RootRelative,
    ResolveStep::Stem,
    ResolveStep::Title,
];

fn share(n: u32, d: u32) -> f32 {
    if d == 0 { 0.0 } else { n as f32 / d as f32 }
}

impl Vote {
    pub fn new() -> Self {
        Self::default()
    }

    /// Count one link and the ladder step that resolved it (`None` = broken).
    /// Only explicit links count.
    pub fn record_link(&mut self, link: &Link, step: Option<ResolveStep>) {
        if link.confidence != Confidence::Explicit {
            return;
        }
        self.explicit_links += 1;
        match link.kind {
            LinkKind::Wiki => self.wiki += 1,
            LinkKind::Markdown => {
                self.markdown += 1;
                if link.target.path.ends_with(".md") {
                    self.md_suffix += 1;
                }
            }
            _ => {}
        }
        if let Some(step) = step {
            self.resolved += 1;
            if let Some(i) = STEP_ORDER.iter().position(|s| *s == step) {
                self.steps[i] += 1;
            }
        }
    }

    /// Count one document's headings and prose hashtags.
    pub fn record_doc(&mut self, doc: &Document) {
        self.docs += 1;
        if doc.headings().filter(|h| h.level == 1).count() == 1 {
            self.docs_one_h1 += 1;
        }
        let mut any = false;
        for t in doc.tags().filter(|t| t.syntax == TagSyntax::Hash) {
            any = true;
            self.hash_tags.insert(t.name.clone());
        }
        if any {
            self.files_with_hash_tags += 1;
        }
    }

    pub fn result(&self) -> VoteResult {
        let insert_style = {
            let max = *self.steps.iter().max().unwrap_or(&0);
            if max == 0 {
                None
            } else if self.steps[1] == max {
                Some(ResolveStep::RootRelative)
            } else {
                STEP_ORDER
                    .iter()
                    .zip(self.steps)
                    .find(|(_, n)| *n == max)
                    .map(|(s, _)| *s)
            }
        };
        VoteResult {
            insert_style,
            wiki_share: share(self.wiki, self.wiki + self.markdown),
            md_suffix_share: share(self.md_suffix, self.markdown),
            h1_is_title: self.docs > 0 && self.docs_one_h1 * 10 >= self.docs * 7,
            hashtags_seen: self.hash_tags.len() >= 3 && self.files_with_hash_tags >= 2,
            resolved_share: if self.explicit_links == 0 {
                1.0
            } else {
                share(self.resolved, self.explicit_links)
            },
            explicit_links: self.explicit_links,
        }
    }
}

/// Severity for broken explicit links (§3.3): the root's config wins; else
/// more than 98% resolved gives Error, under 80% gives Hint, and otherwise
/// (or with no explicit links seen) Warning.
///
/// Callers must suppress broken-link diagnostics entirely when
/// `conv.dead_link_off` is true; the vote never re-enables them.
pub fn link_severity(conv: &RootConventions, vote: &VoteResult) -> Severity {
    if let Some(s) = conv.dead_link_severity {
        return s;
    }
    if vote.explicit_links == 0 {
        Severity::Warning
    } else if vote.resolved_share > 0.98 {
        Severity::Error
    } else if vote.resolved_share < 0.80 {
        Severity::Hint
    } else {
        Severity::Warning
    }
}

/// How a new link to a note is written in a root (§3.2).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkStyle {
    /// `[[stem]]`: the target's file name without extension.
    WikiStem,
    /// `[[dir/stem]]`: root-relative, without extension.
    WikiPath,
    /// `[label](../dir/note.md)`: relative to the linking file.
    MarkdownRelative { md_suffix: bool },
    /// `[label](dir/note.md)`: relative to the root.
    MarkdownRootRelative { md_suffix: bool },
}

/// The style for inserted links. An existing tool config wins over the
/// vote; when a root has both, [zk](https://github.com/zk-org/zk) wins over
/// [Obsidian](https://obsidian.md).
///
/// - zk (a `.zk` marker): `link-format` "wiki" gives [`LinkStyle::WikiPath`]
///   (zk's wiki links are root-relative); "markdown" or absent (zk's
///   default) gives [`LinkStyle::MarkdownRelative`]; a custom template with
///   `[[` gives `WikiStem` when it uses `{{filename}}`, else `WikiPath`,
///   and one without `[[` gives `MarkdownRelative`. The `.md` suffix is
///   kept only with `link-drop-extension = false`.
/// - Obsidian (a `.obsidian` marker): wiki unless `useMarkdownLinks`; for
///   wiki, `newLinkFormat` "shortest" (the default) gives `WikiStem` and
///   "relative"/"absolute" give `WikiPath`; for Markdown, "absolute" gives
///   `MarkdownRootRelative`, anything else `MarkdownRelative`; the suffix
///   is kept.
/// - Otherwise the vote: wiki when at least half the explicit links are
///   wiki, root-relative when the insert style is
///   [`ResolveStep::RootRelative`], and the `.md` suffix when at least half
///   the Markdown links carry it. A root without explicit links gets
///   `MarkdownRelative { md_suffix: true }`.
pub fn link_style(conv: &RootConventions, vote: &VoteResult) -> LinkStyle {
    if conv.markers.contains(&DialectMarker::Zk) {
        let md_suffix = conv.link_drop_extension == Some(false);
        return match conv.wiki_link_format.as_deref() {
            Some("wiki") => LinkStyle::WikiPath,
            None | Some("markdown") => LinkStyle::MarkdownRelative { md_suffix },
            Some(t) if t.contains("[[") && t.contains("{{filename}}") => LinkStyle::WikiStem,
            Some(t) if t.contains("[[") => LinkStyle::WikiPath,
            Some(_) => LinkStyle::MarkdownRelative { md_suffix },
        };
    }
    if conv.markers.contains(&DialectMarker::Obsidian) {
        let format = conv.new_link_format.as_deref();
        return match (conv.use_markdown_links.unwrap_or(false), format) {
            (false, Some("relative" | "absolute")) => LinkStyle::WikiPath,
            (false, _) => LinkStyle::WikiStem,
            (true, Some("absolute")) => LinkStyle::MarkdownRootRelative { md_suffix: true },
            (true, _) => LinkStyle::MarkdownRelative { md_suffix: true },
        };
    }
    if vote.explicit_links == 0 {
        return LinkStyle::MarkdownRelative { md_suffix: true };
    }
    let rooted = vote.insert_style == Some(ResolveStep::RootRelative);
    let md_suffix = vote.md_suffix_share >= 0.5;
    match (vote.wiki_share >= 0.5, rooted) {
        (true, true) => LinkStyle::WikiPath,
        (true, false) => LinkStyle::WikiStem,
        (false, true) => LinkStyle::MarkdownRootRelative { md_suffix },
        (false, false) => LinkStyle::MarkdownRelative { md_suffix },
    }
}
