//! The resolution ladder against an in-memory root.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use mdroots_resolve::keys::ResolveStep;
use mdroots_resolve::ladder::{LinkStatus, Resolution, ResolveCtx, resolve};
use mdroots_resolve::{KeyKind, KeyLookup, ResolveEnv};
use mdroots_syntax::{Anchor, Confidence, Context, Dialect, Link, LinkKind, parse};

/// Files on disk (root-relative; may start with `..` for outside the root).
struct FakeEnv {
    files: BTreeSet<String>,
    dirs: BTreeSet<String>,
    case_sensitive: bool,
    home: Option<PathBuf>,
}

impl FakeEnv {
    fn new(files: &[&str]) -> Self {
        let files: BTreeSet<String> = files.iter().map(|s| s.to_string()).collect();
        let mut dirs = BTreeSet::new();
        for f in &files {
            let mut p = f.as_str();
            while let Some(i) = p.rfind('/') {
                p = &p[..i];
                dirs.insert(p.to_owned());
            }
        }
        FakeEnv {
            files,
            dirs,
            case_sensitive: true,
            home: None,
        }
    }

    fn norm(&self, s: &str) -> String {
        if self.case_sensitive {
            s.to_owned()
        } else {
            s.to_lowercase()
        }
    }

    fn has(&self, set: &BTreeSet<String>, p: &str) -> bool {
        set.iter().any(|f| self.norm(f) == self.norm(p))
    }
}

impl ResolveEnv for FakeEnv {
    fn exists(&self, p: &str) -> bool {
        self.has(&self.files, p) || self.has(&self.dirs, p)
    }
    fn is_file(&self, p: &str) -> bool {
        self.has(&self.files, p)
    }
    fn case_sensitive(&self) -> bool {
        self.case_sensitive
    }
    fn home_dir(&self) -> Option<&Path> {
        self.home.as_deref()
    }
    fn read_config(&self, _: &str) -> Option<String> {
        None
    }
}

/// Indexed keys: `(kind, key, path)`; Path and Stem keys are derived.
#[derive(Default)]
struct FakeKeys {
    keys: Vec<(KeyKind, String, String)>,
}

impl FakeKeys {
    fn new(paths: &[&str], case_sensitive: bool) -> Self {
        let mut k = FakeKeys::default();
        for p in paths {
            let doc = parse("", Dialect::detect_from_path(Path::new(p)));
            for (kind, key) in mdroots_resolve::doc_keys(p, &doc, case_sensitive) {
                k.keys.push((kind, key, p.to_string()));
            }
        }
        k
    }

    /// Keys of parsed documents `(path, source)`.
    fn from_docs(docs: &[(&str, &str)], case_sensitive: bool) -> Self {
        let mut k = FakeKeys::default();
        for (p, src) in docs {
            let doc = parse(src, Dialect::detect_from_path(Path::new(p)));
            for (kind, key) in mdroots_resolve::doc_keys(p, &doc, case_sensitive) {
                k.keys.push((kind, key, p.to_string()));
            }
        }
        k
    }

    fn with(mut self, kind: KeyKind, key: &str, path: &str) -> Self {
        self.keys.push((kind, key.into(), path.into()));
        self
    }
}

impl KeyLookup for FakeKeys {
    fn lookup(&self, kind: KeyKind, key: &str) -> Vec<String> {
        let mut v: Vec<String> = self
            .keys
            .iter()
            .filter(|(k, kk, _)| *k == kind && kk == key)
            .map(|(_, _, p)| p.clone())
            .collect();
        v.sort();
        v
    }
    fn partial(&self, needle: &str) -> Vec<String> {
        let mut v: Vec<String> = self
            .keys
            .iter()
            .filter(|(k, _, p)| *k == KeyKind::Path && p.contains(needle))
            .map(|(_, _, p)| p.clone())
            .collect();
        v.sort();
        v.dedup();
        v
    }
}

fn md(raw: &str) -> Link {
    Link::new(
        LinkKind::Markdown,
        Context::Prose,
        Confidence::Explicit,
        raw,
    )
}

fn wiki(raw: &str) -> Link {
    Link::new(LinkKind::Wiki, Context::Prose, Confidence::Explicit, raw)
}

fn bare(raw: &str) -> Link {
    Link::new(
        LinkKind::BarePath,
        Context::Prose,
        Confidence::Implicit,
        raw,
    )
}

fn code(raw: &str) -> Link {
    Link::new(
        LinkKind::CodeMention,
        Context::InlineCode,
        Confidence::Implicit,
        raw,
    )
}

/// An org link as the org pass builds it: `raw` keeps `file:`/`id:`.
fn org(raw: &str, path: &str, anchor: Option<Anchor>) -> Link {
    let mut l = Link::new(LinkKind::Org, Context::Prose, Confidence::Explicit, raw);
    l.target.path = path.into();
    l.target.anchor = anchor;
    l
}

struct Root {
    env: FakeEnv,
    keys: FakeKeys,
}

impl Root {
    /// `indexed` are in the index and on disk; `disk_only` only on disk.
    fn new(indexed: &[&str], disk_only: &[&str]) -> Self {
        let all: Vec<&str> = indexed.iter().chain(disk_only).copied().collect();
        Root {
            env: FakeEnv::new(&all),
            keys: FakeKeys::new(indexed, true),
        }
    }

    fn ctx(&self) -> ResolveCtx<'_> {
        ResolveCtx::new(&self.env, &self.keys)
    }

    fn r(&self, from: &str, link: &Link) -> Resolution {
        resolve(from, link, &self.ctx())
    }
}

/// Targets as `&'static str` for terse comparisons (test-only leak).
fn hit(r: &Resolution) -> (Vec<&'static str>, Option<ResolveStep>, LinkStatus) {
    (
        r.targets
            .iter()
            .map(|s| &*Box::leak(s.clone().into_boxed_str()))
            .collect(),
        r.step,
        r.status.clone(),
    )
}

use LinkStatus::*;
use ResolveStep::*;

#[test]
fn file_relative() {
    let root = Root::new(&["a/b.md", "a/c.md", "c.md"], &[]);
    assert_eq!(
        hit(&root.r("a/b.md", &md("c.md"))),
        (vec!["a/c.md"], Some(FileRelative), Resolved)
    );
    assert_eq!(
        hit(&root.r("a/b.md", &md("../c.md"))),
        (vec!["c.md"], Some(FileRelative), Resolved)
    );
}

#[test]
fn root_relative() {
    let root = Root::new(&["a/b.md", "x/y.md"], &[]);
    assert_eq!(
        hit(&root.r("a/b.md", &md("x/y.md"))),
        (vec!["x/y.md"], Some(RootRelative), Resolved)
    );
}

#[test]
fn site_rooted() {
    let mut root = Root::new(&["a.md", "posts/p.md", "docs/guide/x.md"], &[]);
    root.keys = root.keys.with(KeyKind::SitePath, "blog/p", "posts/p.md");
    assert_eq!(
        hit(&root.r("a.md", &md("/blog/p"))),
        (vec!["posts/p.md"], Some(SiteRooted), Resolved)
    );
    // Without a docs dir, /x is root-relative.
    assert_eq!(
        hit(&root.r("a.md", &md("/posts/p.md"))),
        (vec!["posts/p.md"], Some(RootRelative), Resolved)
    );
    assert_eq!(root.r("a.md", &md("/guide/x.md")).status, Broken);
    let mut ctx = root.ctx();
    ctx.docs_dir = Some("docs");
    assert_eq!(
        hit(&resolve("a.md", &md("/guide/x.md"), &ctx)),
        (vec!["docs/guide/x.md"], Some(SiteRooted), Resolved)
    );
}

#[test]
fn stem() {
    let root = Root::new(&["a.md", "deep/dir/note.md"], &[]);
    assert_eq!(
        hit(&root.r("a.md", &wiki("note"))),
        (vec!["deep/dir/note.md"], Some(Stem), Resolved)
    );
    // A key with '/' never resolves by stem.
    assert_eq!(root.r("a.md", &wiki("x/note")).status, Broken);
}

#[test]
fn id_title_alias() {
    let mut root = Root::new(&["a.md", "z/one.md", "z/two.md", "z/three.md"], &[]);
    root.keys = root
        .keys
        .with(KeyKind::Id, "abc-1", "z/one.md")
        .with(KeyKind::TitleSlug, "my-title", "z/two.md")
        .with(KeyKind::Alias, "other name", "z/three.md");
    assert_eq!(
        hit(&root.r("a.md", &wiki("abc-1"))),
        (vec!["z/one.md"], Some(Id), Resolved)
    );
    assert_eq!(
        hit(&root.r("a.md", &wiki("My Title"))),
        (vec!["z/two.md"], Some(Title), Resolved)
    );
    assert_eq!(
        hit(&root.r("a.md", &wiki("other name"))),
        (vec!["z/three.md"], Some(Alias), Resolved)
    );
}

#[test]
fn dialect_transform_logseq() {
    let root = Root::new(&["pages/a.md", "pages/proj___sub.md"], &[]);
    assert_eq!(
        hit(&root.r("pages/a.md", &wiki("proj/sub"))),
        (
            vec!["pages/proj___sub.md"],
            Some(DialectTransform),
            Resolved
        )
    );
}

#[test]
fn partial_only_with_allow_partial_and_sets_hint() {
    let root = Root::new(&["a.md", "z/my-long-note.md"], &[]);
    let r = root.r("a.md", &wiki("long"));
    assert_eq!(r.status, Broken);
    assert!(!r.hint);
    let mut ctx = root.ctx();
    ctx.allow_partial = true;
    let r = resolve("a.md", &wiki("long"), &ctx);
    assert_eq!(
        hit(&r),
        (vec!["z/my-long-note.md"], Some(Partial), Resolved)
    );
    assert!(r.hint);
}

#[test]
fn first_step_stops() {
    // `b` hits FileRelative (a/b.md) and Stem (a/b.md, x/b.md): FileRelative wins alone.
    let root = Root::new(&["a/x.md", "a/b.md", "x/b.md"], &[]);
    assert_eq!(
        hit(&root.r("a/x.md", &wiki("b"))),
        (vec!["a/b.md"], Some(FileRelative), Resolved)
    );
}

#[test]
fn tie_is_ambiguous_ordered_by_distance() {
    let root = Root::new(
        &["p/q/from.md", "z/n.md", "p/n.md", "p/q/r/n.md", "a/n.md"],
        &[],
    );
    let r = root.r("p/q/from.md", &wiki("n"));
    assert_eq!(
        hit(&r),
        (
            // distance 1, 1, then 3, 3 (lexicographic within a distance)
            vec!["p/n.md", "p/q/r/n.md", "a/n.md", "z/n.md"],
            Some(Stem),
            Ambiguous
        )
    );
}

#[test]
fn piped_wiki_falls_back_to_the_right_side() {
    let root = Root::new(&["a.md", "target.md"], &[]);
    let l = wiki("shown text").with_label("target");
    assert_eq!(
        hit(&root.r("a.md", &l)),
        (vec!["target.md"], Some(FileRelative), Resolved)
    );
    // Left side wins when it resolves.
    let root = Root::new(&["a.md", "target.md", "left.md"], &[]);
    let l = wiki("left").with_label("target");
    assert_eq!(hit(&root.r("a.md", &l)).0, vec!["left.md"]);
}

#[test]
fn dotdot_escaping_the_root_is_not_file_relative() {
    let root = Root::new(&["a.md", "x.md"], &[]);
    let r = root.r("a.md", &md("../x.md"));
    assert_ne!(r.step, Some(FileRelative));
    assert_eq!(r.status, Broken);
}

#[test]
fn unindexed_gitignored_html() {
    let root = Root::new(&["notes/a.md"], &["cache/x.html"]);
    let r = root.r("notes/a.md", &md("../cache/x.html"));
    assert_eq!(
        hit(&r),
        (vec!["cache/x.html"], Some(FileRelative), Unindexed)
    );
    let r = root.r("notes/a.md", &md("cache/x.html"));
    assert_eq!(
        hit(&r),
        (vec!["cache/x.html"], Some(RootRelative), Unindexed)
    );
}

#[test]
fn unindexed_markdown_on_disk() {
    // A gitignored .md is on disk but not indexed: no step hits, status Unindexed.
    let root = Root::new(&["a.md"], &["ignored/n.md"]);
    let r = root.r("a.md", &md("ignored/n.md"));
    assert_eq!(hit(&r), (vec!["ignored/n.md"], None, Unindexed));
    let r = root.r("a.md", &wiki("ignored/n"));
    assert_eq!(hit(&r), (vec!["ignored/n.md"], None, Unindexed));
}

#[test]
fn broken_explicit() {
    let root = Root::new(&["a.md"], &[]);
    assert_eq!(hit(&root.r("a.md", &md("gone.md"))), (vec![], None, Broken));
    assert_eq!(hit(&root.r("a.md", &wiki("gone"))), (vec![], None, Broken));
}

#[test]
fn unchecked_bare_path() {
    let root = Root::new(&["d/a.md", "x/y.md"], &[]);
    assert_eq!(
        hit(&root.r("d/a.md", &bare("mode/fast/thing"))),
        (vec![], None, Unchecked)
    );
    assert_eq!(
        hit(&root.r("d/a.md", &bare("x/y.md"))),
        (vec!["x/y.md"], Some(RootRelative), Resolved)
    );
    // Bare paths never resolve by stem or other keys.
    assert_eq!(root.r("d/a.md", &bare("y")).status, Unchecked);
}

#[test]
fn dotted_bare_path_gets_one_stat() {
    // `./tool` exists on disk but is not indexed (no extension): docs/specs/index.md §2.3
    // gives `./`, `../` and `~/` bare paths one stat.
    let root = Root::new(&["d/a.md"], &["d/tool", "up.txt"]);
    assert_eq!(
        hit(&root.r("d/a.md", &bare("./tool"))),
        (vec!["d/tool"], None, Unindexed)
    );
    assert_eq!(
        hit(&root.r("d/a.md", &bare("../up.txt"))),
        (vec!["up.txt"], Some(FileRelative), Unindexed)
    );
    // Undotted bare paths still never stat.
    assert_eq!(root.r("d/a.md", &bare("d/tool")).status, Unchecked);
}

#[test]
fn external_url() {
    let root = Root::new(&["a.md"], &[]);
    assert_eq!(
        hit(&root.r("a.md", &md("https://example.com/a.md"))),
        (vec![], None, External)
    );
    let l = Link::new(LinkKind::Url, Context::Prose, Confidence::External, "x.org");
    assert_eq!(root.r("a.md", &l).status, External);
}

#[test]
fn org_file_with_anchor_resolves_the_file() {
    let root = Root::new(&["a.org", "sub/b.org"], &[]);
    let l = org(
        "file:sub/b.org::*Heading",
        "sub/b.org",
        Some(Anchor::Heading("Heading".into())),
    );
    assert_eq!(
        hit(&root.r("a.org", &l)),
        (vec!["sub/b.org"], Some(FileRelative), Resolved)
    );
}

#[test]
fn org_id_link_only_uses_id() {
    let mut root = Root::new(&["a.org", "b.org", "u-1.org"], &[]);
    root.keys = root.keys.with(KeyKind::Id, "u-2", "b.org");
    assert_eq!(
        hit(&root.r("a.org", &org("id:u-2", "u-2", None))),
        (vec!["b.org"], Some(Id), Resolved)
    );
    // `u-1` would hit by path/stem, but id links consult only Id.
    assert_eq!(
        hit(&root.r("a.org", &org("id:u-1", "u-1", None))),
        (vec![], None, Broken)
    );
}

#[test]
fn same_file_links_and_footnotes() {
    let root = Root::new(&["d/a.org"], &[]);
    let l = org("*Top", "", Some(Anchor::Heading("Top".into())));
    assert_eq!(
        hit(&root.r("d/a.org", &l)),
        (vec!["d/a.org"], Some(FileRelative), Resolved)
    );
    assert_eq!(
        hit(&root.r("d/a.org", &md("#h"))),
        (vec!["d/a.org"], Some(FileRelative), Resolved)
    );
    let f = Link::new(
        LinkKind::Footnote,
        Context::Prose,
        Confidence::Explicit,
        "^x",
    );
    assert_eq!(
        hit(&root.r("d/a.org", &f)),
        (vec!["d/a.org"], Some(FileRelative), Resolved)
    );
}

#[test]
fn absolute_and_home_targets_map_into_the_root_or_are_external() {
    let mut root = Root::new(&["a.org", "sub/b.org"], &[]);
    root.env.home = Some(PathBuf::from("/home/u"));
    let abs = Path::new("/home/u/notes");
    let mut ctx = root.ctx();
    ctx.root_abs = Some(abs);
    let r = |l: &Link| hit(&resolve("a.org", l, &ctx));
    assert_eq!(
        r(&org(
            "file:/home/u/notes/sub/b.org",
            "/home/u/notes/sub/b.org",
            None
        )),
        (vec!["sub/b.org"], Some(RootRelative), Resolved)
    );
    assert_eq!(
        r(&org("file:~/notes/sub/b.org", "~/notes/sub/b.org", None)),
        (vec!["sub/b.org"], Some(RootRelative), Resolved)
    );
    assert_eq!(
        r(&org("file:~/infer/x.org", "~/infer/x.org", None)),
        (vec![], None, External)
    );
    assert_eq!(r(&md("file:///etc/hosts")), (vec![], None, External));
}

#[test]
fn code_mention_via_code_dirs() {
    let root = Root::new(&["docs/a.md"], &["../repo/src/m.rs", "tools/t.py"]);
    let dirs = vec!["../repo".to_string()];
    let mut ctx = root.ctx();
    ctx.code_dirs = &dirs;
    let r = |t: &str| hit(&resolve("docs/a.md", &code(t), &ctx));
    assert_eq!(
        r("src/m.rs"),
        (vec!["../repo/src/m.rs"], Some(RootRelative), Unindexed)
    );
    // Root-relative fallback after the code dirs.
    assert_eq!(
        r("tools/t.py"),
        (vec!["tools/t.py"], Some(RootRelative), Unindexed)
    );
    // An indexed markdown file is Resolved; the line suffix is stripped.
    assert_eq!(r("a.md:3").0, vec!["docs/a.md"]);
    assert_eq!(r("a.md:3").2, Resolved);
}

#[test]
fn code_mention_rules() {
    // Donated from ramble 75b8285 tests/codepath.rs (pure cases).
    let mut root = Root::new(
        &["a.md"],
        &[
            "a.rs",
            "odd:7",
            "has space.txt",
            "sub/inner.txt",
            "../home/notes/x.rs",
        ],
    );
    root.env.home = Some(PathBuf::from("/r/home"));
    let mut ctx = root.ctx();
    ctx.root_abs = Some(Path::new("/r/root"));
    let r = |t: &str| resolve("a.md", &code(t), &ctx).targets;
    let one = |s: &str| vec![s.to_string()];
    assert_eq!(r("a.rs"), one("a.rs"));
    assert_eq!(r("a.rs:12"), one("a.rs"));
    assert_eq!(r("a.rs:12:4"), one("a.rs"));
    assert_eq!(r("  a.rs:3  "), one("a.rs"));
    // The full text wins when it names a file.
    assert_eq!(r("odd:7"), one("odd:7"));
    assert!(r("a.rs:0").is_empty());
    assert!(r("a.rs:x").is_empty());
    assert!(r("a.rs:1:2:3").is_empty());
    // Rejections: whitespace, URLs, too long, ~user/, directories, missing.
    assert!(r("has space.txt").is_empty());
    assert!(r("https://x.io/a.rs").is_empty());
    assert!(r(&"a".repeat(600)).is_empty());
    assert!(r("~bob/x.rs").is_empty());
    assert!(r("sub").is_empty());
    assert!(r("gone.txt").is_empty());
    assert_eq!(resolve("a.md", &code("gone.txt"), &ctx).status, Unchecked);
    // Home expansion through the env.
    assert_eq!(r("~/notes/x.rs:2"), one("../home/notes/x.rs"));
    root.env.home = None;
    let ctx = root.ctx();
    assert!(
        resolve("a.md", &code("~/notes/x.rs"), &ctx)
            .targets
            .is_empty()
    );
}

#[test]
fn extensionless_markdown_link() {
    // Ported from ramble d394783 tests/nav.rs
    // `extensionless_markdown_link_gets_md_only_when_that_file_exists`.
    let root = Root::new(
        &["from.md", "a.md", "both.md", "sub.md"],
        &["both", "sub/x.txt"],
    );
    let r = |raw: &str| hit(&root.r("from.md", &md(raw)));
    assert_eq!(r("a"), (vec!["a.md"], Some(FileRelative), Resolved));
    assert_eq!(r("a#h"), (vec!["a.md"], Some(FileRelative), Resolved));
    // A bare file wins over the .md variant.
    assert_eq!(r("both"), (vec!["both"], Some(FileRelative), Unindexed));
    // A bare dir wins too.
    assert_eq!(r("sub"), (vec!["sub"], Some(FileRelative), Unindexed));
    assert_eq!(r("none"), (vec![], None, Broken));
}

#[test]
fn case_insensitive_env() {
    let mut root = Root::new(&[], &[]);
    root.env = FakeEnv::new(&["notes/a.md", "b.md"]);
    root.env.case_sensitive = false;
    root.keys = FakeKeys::new(&["notes/a.md", "b.md"], false);
    assert_eq!(
        hit(&root.r("b.md", &wiki("Notes/A"))),
        (vec!["notes/a.md"], Some(FileRelative), Resolved)
    );
    assert_eq!(
        hit(&root.r("b.md", &md("Notes/A.md"))),
        (vec!["notes/a.md"], Some(FileRelative), Resolved)
    );
}

#[test]
fn dialect_transform_collects_ties_within_step() {
    let root = Root::new(
        &["from.md", "pages/proj___sub.md", "pages/proj%2Fsub.md"],
        &[],
    );
    let r = root.r("from.md", &wiki("proj/sub"));
    assert_eq!(
        hit(&r),
        (
            vec!["pages/proj%2Fsub.md", "pages/proj___sub.md"],
            Some(DialectTransform),
            Ambiguous,
        )
    );
}

#[test]
fn piped_wiki_falls_back_to_unindexed_right_side() {
    let root = Root::new(&["from.md"], &["target.md"]);
    let l = wiki("missing").with_label("target");
    assert_eq!(
        hit(&root.r("from.md", &l)),
        (vec!["target.md"], None, Unindexed)
    );
}

#[test]
fn piped_wiki_precedence() {
    // Right indexed beats left unindexed.
    let root = Root::new(&["from.md", "right.md"], &["left.md"]);
    let l = wiki("left").with_label("right");
    assert_eq!(
        hit(&root.r("from.md", &l)),
        (vec!["right.md"], Some(FileRelative), Resolved)
    );
    // Left unindexed beats right unindexed.
    let root = Root::new(&["from.md"], &["left.md", "right.md"]);
    let l = wiki("left").with_label("right");
    assert_eq!(
        hit(&root.r("from.md", &l)),
        (vec!["left.md"], None, Unindexed)
    );
    // Neither side anywhere: Broken.
    let root = Root::new(&["from.md"], &[]);
    let l = wiki("left").with_label("right");
    assert_eq!(hit(&root.r("from.md", &l)), (vec![], None, Broken));
}

#[test]
fn org_heading_id_resolves_to_the_file() {
    let mut root = Root::new(&[], &[]);
    root.env = FakeEnv::new(&["a.org", "b.org"]);
    root.keys = FakeKeys::from_docs(
        &[
            ("a.org", ""),
            (
                "b.org",
                "* Section\n:PROPERTIES:\n:ID: section-uuid\n:END:\nbody\n",
            ),
        ],
        true,
    );
    assert_eq!(
        hit(&root.r("a.org", &org("id:section-uuid", "section-uuid", None))),
        (vec!["b.org"], Some(Id), Resolved)
    );
    // A markdown `{#id}` heading attribute is an anchor, not a document id.
    let keys = mdroots_resolve::doc_keys(
        "m.md",
        &parse("# Title {#anchor-id}\n", Dialect::Markdown),
        true,
    );
    assert!(!keys.contains(&(KeyKind::Id, "anchor-id".into())));
}

#[test]
fn site_rooted_unindexed_under_docs_dir() {
    // An existing but unindexed docs/guide/x.md behind a site-rooted link.
    let root = Root::new(&["a.md"], &["docs/guide/x.md"]);
    assert_eq!(root.r("a.md", &md("/guide/x.md")).status, Broken);
    let mut ctx = root.ctx();
    ctx.docs_dir = Some("docs");
    assert_eq!(
        hit(&resolve("a.md", &md("/guide/x.md"), &ctx)),
        (vec!["docs/guide/x.md"], None, Unindexed)
    );
    assert_eq!(
        hit(&resolve("a.md", &md("/guide/x"), &ctx)),
        (vec!["docs/guide/x.md"], None, Unindexed)
    );
}

#[test]
fn title_keys_are_nfc() {
    let mut root = Root::new(&[], &[]);
    root.env = FakeEnv::new(&["a.md", "d.md", "c.md"]);
    root.keys = FakeKeys::from_docs(
        &[
            ("a.md", ""),
            ("d.md", "# Cafe\u{301}\n"),
            ("c.md", "# Tea \u{e9}t\u{e9}\n"),
        ],
        true,
    );
    for q in ["Cafe\u{301}", "Caf\u{e9}"] {
        assert_eq!(
            hit(&root.r("a.md", &wiki(q))),
            (vec!["d.md"], Some(Title), Resolved),
            "{q:?}"
        );
    }
    assert_eq!(
        hit(&root.r("a.md", &wiki("Tea e\u{301}te\u{301}"))),
        (vec!["c.md"], Some(Title), Resolved)
    );
}

#[test]
fn root_containment_follows_fs_case() {
    let mut root = Root::new(&[], &[]);
    root.env = FakeEnv::new(&["a.md", "B.md"]);
    root.env.case_sensitive = false;
    root.keys = FakeKeys::new(&["a.md", "B.md"], false);
    let abs = Path::new("/Vault");
    let mut ctx = root.ctx();
    ctx.root_abs = Some(abs);
    assert_eq!(
        hit(&resolve("a.md", &md("file:///vault/B.md"), &ctx)),
        (vec!["B.md"], Some(RootRelative), Resolved)
    );
    // A case-sensitive fs keeps /vault outside /Vault.
    let mut root = Root::new(&["a.md", "B.md"], &[]);
    root.env.case_sensitive = true;
    let mut ctx = root.ctx();
    ctx.root_abs = Some(abs);
    assert_eq!(
        hit(&resolve("a.md", &md("file:///vault/B.md"), &ctx)),
        (vec![], None, External)
    );
}
