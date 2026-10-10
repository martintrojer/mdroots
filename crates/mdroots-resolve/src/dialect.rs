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
    /// mkdocs `docs_dir` (default `docs`), Docusaurus `docs`, Hugo `content`.
    pub docs_dir: Option<String>,
    /// `docs_dir` was stated in `mkdocs.yml` (for [`explain`]).
    docs_dir_from_config: bool,
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
        conv.use_markdown_links = app.get("useMarkdownLinks").and_then(|v| v.as_bool());
        conv.new_link_format = app
            .get("newLinkFormat")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
    }
    conv.docs_dir = if has(Mkdocs) {
        let stated = env
            .read_config("mkdocs.yml")
            .and_then(|t| yaml_top_scalar(&t, "docs_dir"));
        conv.docs_dir_from_config = stated.is_some();
        Some(stated.unwrap_or_else(|| "docs".into()))
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
    /// resolved explicit links; ties go to RootRelative. A wiki link without
    /// a `/` never counts as RootRelative (it counts as Stem).
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
        // A bare wiki stem (`[[top]]`) that only resolved by joining the
        // root is still written stem-style: root-relative needs a slash.
        let step = match step {
            Some(ResolveStep::RootRelative)
                if link.kind == LinkKind::Wiki && !link.target.path.contains('/') =>
            {
                Some(ResolveStep::Stem)
            }
            s => s,
        };
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
    severity_with_source(conv, vote).0
}

fn severity_with_source(conv: &RootConventions, vote: &VoteResult) -> (Severity, Source) {
    if let Some(s) = conv.dead_link_severity {
        return (s, ZK_DEAD_LINK);
    }
    if vote.explicit_links == 0 {
        (Severity::Warning, Source::Default)
    } else if vote.resolved_share > 0.98 {
        (Severity::Error, Source::Vote)
    } else if vote.resolved_share < 0.80 {
        (Severity::Hint, Source::Vote)
    } else {
        (Severity::Warning, Source::Vote)
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

/// The style for inserted links. A link setting in an existing tool config
/// wins over the vote; when a root has both, [zk](https://github.com/zk-org/zk)
/// wins over [Obsidian](https://obsidian.md).
///
/// - zk (a `.zk` marker) with `link-format` or `link-drop-extension` set:
///   `link-format` "wiki" gives [`LinkStyle::WikiPath`] (zk's wiki links
///   are root-relative); "markdown" or absent (zk's default) gives
///   [`LinkStyle::MarkdownRelative`]; a custom template with `[[` gives
///   `WikiStem` when it uses `{{filename}}`, else `WikiPath`, and one
///   without `[[` gives `MarkdownRelative`. The `.md` suffix is kept only
///   with `link-drop-extension = false`.
/// - Obsidian (a `.obsidian` marker) with `useMarkdownLinks` or
///   `newLinkFormat` set in `app.json`: wiki unless `useMarkdownLinks`; for
///   wiki, `newLinkFormat` "shortest" (the default) gives `WikiStem` and
///   "relative"/"absolute" give `WikiPath`; for Markdown, "absolute" gives
///   `MarkdownRootRelative`, anything else `MarkdownRelative`; the suffix
///   is kept.
/// - Otherwise the vote: wiki when at least half the explicit links are
///   wiki, root-relative when the insert style is
///   [`ResolveStep::RootRelative`], and the `.md` suffix when at least half
///   the Markdown links carry it.
/// - A root without explicit links gets its marker's tool default: zk
///   `MarkdownRelative { md_suffix: false }`, Obsidian `WikiStem`; any
///   other root `MarkdownRelative { md_suffix: true }`.
pub fn link_style(conv: &RootConventions, vote: &VoteResult) -> LinkStyle {
    link_style_with_source(conv, vote).0
}

fn link_style_with_source(conv: &RootConventions, vote: &VoteResult) -> (LinkStyle, Source) {
    let zk = conv.markers.contains(&DialectMarker::Zk);
    let obsidian = conv.markers.contains(&DialectMarker::Obsidian);
    if zk && (conv.wiki_link_format.is_some() || conv.link_drop_extension.is_some()) {
        let md_suffix = conv.link_drop_extension == Some(false);
        let style = match conv.wiki_link_format.as_deref() {
            Some("wiki") => LinkStyle::WikiPath,
            None | Some("markdown") => LinkStyle::MarkdownRelative { md_suffix },
            Some(t) if t.contains("[[") && t.contains("{{filename}}") => LinkStyle::WikiStem,
            Some(t) if t.contains("[[") => LinkStyle::WikiPath,
            Some(_) => LinkStyle::MarkdownRelative { md_suffix },
        };
        let key = match (
            conv.wiki_link_format.is_some(),
            conv.link_drop_extension.is_some(),
        ) {
            (true, true) => "link-format, link-drop-extension",
            (true, false) => "link-format",
            _ => "link-drop-extension",
        };
        return (style, zk_config(key));
    }
    if obsidian && (conv.use_markdown_links.is_some() || conv.new_link_format.is_some()) {
        let format = conv.new_link_format.as_deref();
        let style = match (conv.use_markdown_links.unwrap_or(false), format) {
            (false, Some("relative" | "absolute")) => LinkStyle::WikiPath,
            (false, _) => LinkStyle::WikiStem,
            (true, Some("absolute")) => LinkStyle::MarkdownRootRelative { md_suffix: true },
            (true, _) => LinkStyle::MarkdownRelative { md_suffix: true },
        };
        let key = match (
            conv.use_markdown_links.is_some(),
            conv.new_link_format.is_some(),
        ) {
            (true, true) => "useMarkdownLinks, newLinkFormat",
            (true, false) => "useMarkdownLinks",
            _ => "newLinkFormat",
        };
        let src = Source::Config {
            tool: "Obsidian",
            file: ".obsidian/app.json",
            key,
        };
        return (style, src);
    }
    if vote.explicit_links == 0 {
        return if zk {
            (
                LinkStyle::MarkdownRelative { md_suffix: false },
                Source::Marker(DialectMarker::Zk),
            )
        } else if obsidian {
            (LinkStyle::WikiStem, Source::Marker(DialectMarker::Obsidian))
        } else {
            (
                LinkStyle::MarkdownRelative { md_suffix: true },
                Source::Default,
            )
        };
    }
    let rooted = vote.insert_style == Some(ResolveStep::RootRelative);
    let md_suffix = vote.md_suffix_share >= 0.5;
    let style = match (vote.wiki_share >= 0.5, rooted) {
        (true, true) => LinkStyle::WikiPath,
        (true, false) => LinkStyle::WikiStem,
        (false, true) => LinkStyle::MarkdownRootRelative { md_suffix },
        (false, false) => LinkStyle::MarkdownRelative { md_suffix },
    };
    (style, Source::Vote)
}

// --- provenance ----------------------------------------------------------

/// Where a [`Setting`]'s value came from.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A key in a tool's config file, e.g. zk `.zk/config.toml` `link-format`.
    /// `key` may name several keys, comma-separated.
    Config {
        tool: &'static str,
        file: &'static str,
        key: &'static str,
    },
    /// The default of the tool whose marker was found (no config states it).
    Marker(DialectMarker),
    /// The corpus vote.
    Vote,
    /// mdroots' own default.
    Default,
}

/// One root setting with its value and where it came from, as shown by
/// `mdroots roots` and the editor info command.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting {
    /// `link style`, `hashtags`, `colon tags`, `multiword tags`,
    /// `broken links` or `docs dir`.
    pub name: &'static str,
    /// e.g. `wiki-path`, `on`, `warning`, `off`, `docs`.
    pub value: String,
    pub source: Source,
}

const ZK_FILE: &str = ".zk/config.toml";
const ZK_DEAD_LINK: Source = zk_config("dead-link");

const fn zk_config(key: &'static str) -> Source {
    Source::Config {
        tool: "zk",
        file: ZK_FILE,
        key,
    }
}

/// `wiki-stem`, `wiki-path`, `markdown-relative` or `markdown-root-relative`,
/// the Markdown ones followed by ` with .md` or ` without .md`.
fn style_name(style: LinkStyle) -> String {
    let md = |base: &str, md_suffix: bool| {
        format!("{base} {} .md", if md_suffix { "with" } else { "without" })
    };
    match style {
        LinkStyle::WikiStem => "wiki-stem".into(),
        LinkStyle::WikiPath => "wiki-path".into(),
        LinkStyle::MarkdownRelative { md_suffix } => md("markdown-relative", md_suffix),
        LinkStyle::MarkdownRootRelative { md_suffix } => md("markdown-root-relative", md_suffix),
    }
}

/// `error`, `warning`, `info` or `hint`.
pub fn severity_name(s: Severity) -> &'static str {
    match s {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Info => "info",
        Severity::Hint => "hint",
    }
}

/// The root's effective settings and where each came from, so a stale tool
/// config is visible: link style, the three tag syntaxes, broken-link
/// severity (`off` for zk `dead-link = "none"`), and the docs dir when a
/// site generator's marker sets one. Values match what [`link_style`],
/// [`link_severity`] and the parser use.
pub fn explain(conv: &RootConventions, vote: &VoteResult) -> Vec<Setting> {
    let row = |name, value: String, source| Setting {
        name,
        value,
        source,
    };
    let on = |b: bool| if b { "on" } else { "off" }.to_owned();
    let mut out = Vec::new();

    let (style, src) = link_style_with_source(conv, vote);
    out.push(row("link style", style_name(style), src));

    // The parser's defaults (mdroots_syntax::ParseOptions::new).
    for (name, key, value, default) in [
        ("hashtags", "hashtags", conv.hashtags, true),
        ("colon tags", "colon-tags", conv.colon_tags, false),
        (
            "multiword tags",
            "multiword-tags",
            conv.multiword_tags,
            false,
        ),
    ] {
        out.push(match value {
            Some(b) => row(name, on(b), zk_config(key)),
            None => row(name, on(default), Source::Default),
        });
    }

    out.push(if conv.dead_link_off {
        row("broken links", "off".into(), ZK_DEAD_LINK)
    } else {
        let (s, src) = severity_with_source(conv, vote);
        row("broken links", severity_name(s).into(), src)
    });

    if let Some(dir) = &conv.docs_dir {
        let src = if conv.docs_dir_from_config {
            Source::Config {
                tool: "mkdocs",
                file: "mkdocs.yml",
                key: "docs_dir",
            }
        } else {
            [
                DialectMarker::Mkdocs,
                DialectMarker::Docusaurus,
                DialectMarker::Hugo,
            ]
            .into_iter()
            .find(|m| conv.markers.contains(m))
            .map_or(Source::Default, Source::Marker)
        };
        out.push(row("docs dir", dir.clone(), src));
    }
    out
}
