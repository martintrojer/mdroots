use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use mdroots_core::{
    AnchorStatus, Cancel, DiagCode, DiagnosticPolicy, ErrorKind, FileSystem, FsKind, MemFs,
    MemStore, Meta, StdFs,
};
use mdroots_resolve::dialect::{LinkStyle, link_style};
use mdroots_resolve::keys::{KeyKind, KeyLookup, ResolveStep};
use mdroots_resolve::ladder::{LinkStatus, Resolution};
use mdroots_syntax::{Anchor, Context, Link};

fn corpus(name: &str) -> MemStore {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/corpus")
        .join(name);
    MemStore::open(Arc::new(StdFs), root, &Cancel::new()).unwrap()
}

fn mem(fs: MemFs) -> MemStore {
    MemStore::open(Arc::new(fs), PathBuf::from("/"), &Cancel::new()).unwrap()
}

/// The resolution of the only link in `from` whose raw target is `raw`.
fn link(s: &MemStore, from: &str, raw: &str) -> (Link, Resolution) {
    let mut hits: Vec<(Link, Resolution)> = s
        .links(from)
        .into_iter()
        .filter(|(l, _)| l.target.raw == raw)
        .collect();
    assert!(!hits.is_empty(), "no link {raw:?} in {from}");
    hits.swap_remove(0)
}

/// Raw targets of the links in `rel` that the diagnostics policy reports
/// with `code`, in source order.
fn diagnosed(s: &MemStore, rel: &str, code: DiagCode) -> Vec<String> {
    let links = s.links(rel);
    DiagnosticPolicy::for_store(s, false)
        .diagnostics(s, rel)
        .into_iter()
        .filter(|d| d.code == code)
        .map(|d| {
            let (l, _) = links.iter().find(|(l, _)| l.range == d.range).unwrap();
            l.target.raw.clone()
        })
        .collect()
}

/// Broken links in `rel`.
fn broken(s: &MemStore, rel: &str) -> Vec<String> {
    diagnosed(s, rel, DiagCode::BrokenLink)
}

/// Links in `rel` to one note in which their anchor is missing.
fn broken_anchors(s: &MemStore, rel: &str) -> Vec<String> {
    diagnosed(s, rel, DiagCode::BrokenAnchor)
}

fn froms(back: &[(String, Link)]) -> Vec<&str> {
    let mut v: Vec<&str> = back.iter().map(|(f, _)| f.as_str()).collect();
    v.dedup();
    v
}

#[test]
fn zkvault_resolutions() {
    let s = corpus("zkvault");
    assert!(s.skipped().is_empty(), "{:?}", s.skipped());
    assert!(s.files().any(|f| f == "org-archive/old.org"));
    assert!(s.files().is_sorted());

    // Root-relative wiki written in a subdirectory file.
    let (_, r) = link(&s, "journal/2026/W40.md", "reference/gardening");
    assert_eq!(r.status, LinkStatus::Resolved);
    assert_eq!(r.step, Some(ResolveStep::RootRelative));
    assert_eq!(r.targets, ["reference/gardening.md"]);

    // The duplicate W40 stem, written root-relative: step 2, one target.
    let (_, r) = link(&s, "journal/2026/W40.md", "journal/2025/W40");
    assert_eq!(r.step, Some(ResolveStep::RootRelative));
    assert_eq!(r.targets, ["journal/2025/W40.md"]);
    assert_eq!(r.status, LinkStatus::Resolved);

    // Gitignored cache/x.html exists on disk but is not markdown.
    let (_, r) = link(&s, "README.md", "cache/x.html");
    assert_eq!(r.status, LinkStatus::Unindexed);
    assert_eq!(r.targets, ["cache/x.html"]);

    // The broken link.
    let (_, r) = link(&s, "reference/composting.md", "reference/does-not-exist");
    assert_eq!(r.status, LinkStatus::Broken);
    assert!(broken(&s, "reference/composting.md").contains(&"reference/does-not-exist".to_owned()));

    // Org `[[T:1][x]]` expands through #+LINK to another scheme.
    let (_, r) = link(&s, "org-archive/old.org", "https://t/1");
    assert_eq!(r.status, LinkStatus::External);

    // Wiki candidates in inline code and fences are never broken.
    assert!(broken(&s, "reference/gardening.md").is_empty());
    assert!(broken(&s, "org-archive/old.org").is_empty());

    // Piped wiki: the left side resolves.
    let (l, r) = link(&s, "README.md", "reference/composting");
    assert_eq!(l.label.as_deref(), Some("composting notes"));
    assert_eq!(r.targets, ["reference/composting.md"]);
    assert_eq!(r.status, LinkStatus::Resolved);
}

#[test]
fn notesvault_resolutions() {
    let s = corpus("notesvault");

    // Stem: the target is not in the linking file's directory.
    let (_, r) = link(&s, "README.md", "note-a");
    assert_eq!(r.step, Some(ResolveStep::Stem));
    assert_eq!(r.targets, ["notes/note-a.md"]);
    let (_, r) = link(&s, "references/paper-1.md", "note-a");
    assert_eq!(r.step, Some(ResolveStep::Stem));
    // Same directory: step 1 wins.
    let (_, r) = link(&s, "notes/note-b.md", "note-a");
    assert_eq!(r.step, Some(ResolveStep::FileRelative));

    // Piped links resolve on their target.
    let (l, r) = link(&s, "concepts/c.md", "b");
    assert_eq!(l.label.as_deref(), Some("Concept B"));
    assert_eq!(r.targets, ["concepts/b.md"]);
    assert_eq!(r.status, LinkStatus::Resolved);

    // `[[project-scope]]` inside the README fence: a link, but never broken.
    let (l, _) = link(&s, "README.md", "project-scope");
    assert_eq!(l.context, Context::CodeBlock);
    assert!(broken(&s, "README.md").is_empty());
    // Nor does it make README a referrer of project-scope.
    assert!(s.backlinks("notes/project-scope.md").is_empty());

    assert_eq!(broken(&s, "references/paper-2.md"), ["missing-paper"]);

    // Backlinks list every referrer, in referencing contexts only
    // (note-b is listed for its prose `[[note-a|A]]`, not its inline code).
    let back = s.backlinks("notes/note-a.md");
    assert_eq!(
        froms(&back),
        [
            "README.md",
            "concepts/b.md",
            "notes/note-b.md",
            "notes/project-scope.md",
            "references/paper-1.md"
        ]
    );
    assert!(back.iter().all(|(_, l)| l.context != Context::InlineCode));
}

#[test]
fn bare_duplicate_stem_is_ambiguous() {
    let s = mem(MemFs::new()
        .with_file("journal/2025/W40.md", "# 2025")
        .with_file("journal/2026/W40.md", "# 2026")
        .with_file("notes/n.md", "See [[W40]] and [[journal/2026/W40]]."));
    let (_, r) = link(&s, "notes/n.md", "W40");
    assert_eq!(r.status, LinkStatus::Ambiguous);
    assert_eq!(r.step, Some(ResolveStep::Stem));
    assert_eq!(r.targets, ["journal/2025/W40.md", "journal/2026/W40.md"]);
    let (_, r) = link(&s, "notes/n.md", "journal/2026/W40");
    assert_eq!(r.step, Some(ResolveStep::RootRelative));
    assert_eq!(r.status, LinkStatus::Resolved);
    // Ambiguous links count as backlinks of each candidate.
    assert_eq!(froms(&s.backlinks("journal/2025/W40.md")), ["notes/n.md"]);
}

#[test]
fn stem_from_another_directory_and_backlink_pair() {
    let s = mem(MemFs::new()
        .with_file("a/one.md", "To [[two]].\n\n```\n[[gone]]\n```\n`[[gone]]`")
        .with_file(
            "b/two.md",
            "Back to [one](../a/one.md). Missing [[gone]].[^1]\n\n[^1]: x",
        ));
    let (_, r) = link(&s, "a/one.md", "two");
    assert_eq!(r.step, Some(ResolveStep::Stem));
    assert_eq!(r.targets, ["b/two.md"]);
    assert_eq!(froms(&s.backlinks("b/two.md")), ["a/one.md"]);
    assert_eq!(froms(&s.backlinks("a/one.md")), ["b/two.md"]);
    // Code and inline-code candidates are not broken; footnotes never are.
    assert!(broken(&s, "a/one.md").is_empty());
    assert_eq!(broken(&s, "b/two.md"), ["gone"]);
}

/// Backlinks of every note as (source, raw target, range).
fn all_backlinks(s: &MemStore) -> Vec<(String, String, String, std::ops::Range<usize>)> {
    s.files()
        .flat_map(|t| {
            s.backlinks(t)
                .into_iter()
                .map(move |(f, l)| (t.to_owned(), f, l.target.raw, l.range))
        })
        .collect()
}

#[test]
fn backlinks_after_an_overlay_match_a_fresh_store() {
    let fs = MemFs::new()
        .with_file("a.md", "[[b]] [[c]]\n")
        .with_file("b.md", "[[c]]\n")
        .with_file("c.md", "[[a]]\n");
    let mut s = mem(fs);
    // Built before the overlay, so the overlay must drop the cached index.
    assert_eq!(froms(&s.backlinks("b.md")), ["a.md"]);

    let text = "[[b]] and again [[b]]\n";
    s.set_overlay("c.md", text);
    let fresh = mem(MemFs::new()
        .with_file("a.md", "[[b]] [[c]]\n")
        .with_file("b.md", "[[c]]\n")
        .with_file("c.md", text));
    assert_eq!(all_backlinks(&s), all_backlinks(&fresh));
    let back = s.backlinks("b.md");
    assert_eq!(froms(&back), ["a.md", "c.md"]);
    assert_eq!(back.len(), 3, "both links from c.md, in link order");
    assert!(back[1].1.range.start < back[2].1.range.start);
    assert!(s.backlinks("a.md").is_empty());
}

#[test]
fn overlay_changes_links_and_backlinks() {
    let mut s = mem(MemFs::new()
        .with_file("a.md", "nothing")
        .with_file("sub/b.md", "# B"));
    assert!(s.links("a.md").is_empty());
    assert!(s.backlinks("sub/b.md").is_empty());

    s.set_overlay("a.md", "now [[b]]");
    let (_, r) = link(&s, "a.md", "b");
    assert_eq!(r.targets, ["sub/b.md"]);
    assert_eq!(froms(&s.backlinks("sub/b.md")), ["a.md"]);

    s.clear_overlay("a.md");
    assert!(s.links("a.md").is_empty());
    assert!(s.backlinks("sub/b.md").is_empty());

    // An overlay on a file with no disk copy adds it, and its keys.
    s.set_overlay("new/c.md", "# C\n[[a]]");
    assert!(s.files().any(|f| f == "new/c.md"));
    assert_eq!(froms(&s.backlinks("a.md")), ["new/c.md"]);
    s.set_overlay("a.md", "to [[c]]");
    assert_eq!(link(&s, "a.md", "c").1.targets, ["new/c.md"]);
    s.clear_overlay("new/c.md");
    assert!(!s.files().any(|f| f == "new/c.md"));
    assert_eq!(link(&s, "a.md", "c").1.status, LinkStatus::Broken);

    // Overlays re-key: a changed H1 is found by the new title only.
    assert_eq!(s.lookup(KeyKind::TitleSlug, "b"), ["sub/b.md"]);
    s.set_overlay("sub/b.md", "# Bee");
    assert!(s.lookup(KeyKind::TitleSlug, "b").is_empty());
    assert_eq!(s.lookup(KeyKind::TitleSlug, "bee"), ["sub/b.md"]);
    s.clear_overlay("sub/b.md");
    assert_eq!(s.lookup(KeyKind::TitleSlug, "b"), ["sub/b.md"]);
}

#[test]
fn binary_files_are_skipped() {
    let fs = MemFs::new()
        .with_file("ok.md", "fine")
        .with_file("bin.md", "a\0b")
        .with_file("org/x.org", "* H");
    let s = mem(fs);
    let skipped: Vec<(&str, ErrorKind)> = s
        .skipped()
        .iter()
        .map(|(p, e)| (p.as_str(), e.kind()))
        .collect();
    assert_eq!(skipped, [("bin.md", ErrorKind::Unsupported)]);
    assert_eq!(s.files().collect::<Vec<_>>(), ["ok.md", "org/x.org"]);
}

#[test]
fn cancelled_open_fails() {
    let c = Cancel::new();
    c.cancel();
    let err = MemStore::open(
        Arc::new(MemFs::new().with_file("a.md", "")),
        PathBuf::from("/"),
        &c,
    )
    .err()
    .unwrap();
    assert_eq!(err.kind(), ErrorKind::Cancelled);
}

#[test]
fn anchors_are_checked_separately_from_file_status() {
    let mut s = mem(MemFs::new()
        .with_file(
            "a.md",
            "[m](b.md#missing) [i](b.md#intro) [c](b.md#custom) [v](b.md#visible)\n\
             [[#Local]] [[#nowhere]] [[b#^blk]] [[b#^nope]] [u](u.md#x) [h](cache/x.html#y)\n\
             `[[b#gone]]`\n\n# Local\n",
        )
        .with_file("b.md", "# Intro\n\n## Visible {#custom}\n\ntext ^blk\n")
        .with_file("u1/u.md", "")
        .with_file("u2/u.md", "")
        .with_file("cache/x.html", "")
        .with_file(
            "org/o.org",
            "[[file:b.org::#cid]] [[file:b.org::#nope]] [[file:b.org::*Intro]]",
        )
        .with_file(
            "org/b.org",
            "* Intro\n:PROPERTIES:\n:CUSTOM_ID: cid\n:END:\n",
        ));

    // The file resolves either way: the anchor does not change the status.
    let (l, r) = link(&s, "a.md", "b.md#missing");
    assert_eq!(r.status, LinkStatus::Resolved);
    assert_eq!(
        s.check_anchor(&r.targets[0], l.target.anchor.as_ref().unwrap()),
        AnchorStatus::Missing
    );
    assert!(broken(&s, "a.md").is_empty());

    // Missing headings and block ids, in prose only; the ambiguous u.md and
    // the unindexed html target are never listed.
    assert_eq!(
        broken_anchors(&s, "a.md"),
        ["b.md#missing", "#nowhere", "b#^nope"]
    );
    assert_eq!(
        s.check_anchor("b.md", &Anchor::Block("blk".into())),
        AnchorStatus::Found
    );
    assert_eq!(
        s.check_anchor("cache/x.html", &Anchor::Heading("y".into())),
        AnchorStatus::NotChecked
    );

    // Org custom ids; search anchors are not checked.
    assert_eq!(broken_anchors(&s, "org/o.org"), ["file:b.org::#nope"]);
    assert_eq!(
        s.check_anchor("org/b.org", &Anchor::Search("*Nope".into())),
        AnchorStatus::NotChecked
    );

    // A block id must end its line.
    assert_eq!(
        s.check_anchor("b.md", &Anchor::Block("bl".into())),
        AnchorStatus::Missing
    );
    // An overlay that adds the heading fixes the link (and drops the ones
    // it no longer has).
    s.set_overlay("b.md", "# Intro\n\n## Missing\n\ntext ^blk\n\nmore ^nope\n");
    assert_eq!(
        broken_anchors(&s, "a.md"),
        ["b.md#custom", "b.md#visible", "#nowhere"]
    );
    s.clear_overlay("b.md");
    assert_eq!(
        broken_anchors(&s, "a.md"),
        ["b.md#missing", "#nowhere", "b#^nope"]
    );
}

/// A [`FileSystem`] wrapper that counts `read` and `read_dir` calls.
struct Recording {
    inner: MemFs,
    reads: Mutex<Vec<PathBuf>>,
    read_dirs: AtomicUsize,
}

impl Recording {
    fn new(inner: MemFs) -> Arc<Self> {
        Arc::new(Recording {
            inner,
            reads: Mutex::new(Vec::new()),
            read_dirs: AtomicUsize::new(0),
        })
    }

    fn read_paths(&self) -> Vec<PathBuf> {
        self.reads.lock().unwrap().clone()
    }
}

impl FileSystem for Recording {
    fn read(&self, p: &Path) -> io::Result<(Arc<[u8]>, Meta)> {
        self.reads.lock().unwrap().push(p.to_path_buf());
        self.inner.read(p)
    }
    fn stat(&self, p: &Path) -> io::Result<Meta> {
        self.inner.stat(p)
    }
    fn read_dir(&self, p: &Path) -> io::Result<Vec<(String, Meta)>> {
        self.read_dirs.fetch_add(1, Ordering::SeqCst);
        self.inner.read_dir(p)
    }
    fn canonicalize(&self, p: &Path) -> io::Result<PathBuf> {
        self.inner.canonicalize(p)
    }
    fn case_sensitive(&self, dir: &Path) -> bool {
        self.inner.case_sensitive(dir)
    }
    fn fs_kind(&self, dir: &Path) -> FsKind {
        self.inner.fs_kind(dir)
    }
}

fn open_files(fs: Arc<Recording>, files: &[&str], force: &[&str]) -> MemStore {
    let files = files.iter().map(|s| s.to_string()).collect();
    let force: Vec<String> = force.iter().map(|s| s.to_string()).collect();
    MemStore::open_files(fs, PathBuf::from("/"), files, &force, &Cancel::new()).unwrap()
}

fn skipped(s: &MemStore) -> Vec<(&str, ErrorKind, &str)> {
    s.skipped()
        .iter()
        .map(|(p, e)| (p.as_str(), e.kind(), e.message()))
        .collect()
}

#[test]
fn open_files_indexes_exactly_the_listed_files() {
    let fs = Recording::new(
        MemFs::new()
            .with_file("a.md", "[[b]]")
            .with_file("sub/b.md", "# B")
            .with_file("unlisted.md", "")
            .with_file("notes.txt", "plain"),
    );
    let s = open_files(
        fs.clone(),
        &["sub/b.md", "a.md", "gone.md", "a.md", "notes.txt"],
        &[],
    );
    assert_eq!(
        s.files().collect::<Vec<_>>(),
        ["a.md", "notes.txt", "sub/b.md"]
    );
    let sk = skipped(&s);
    assert_eq!(sk.len(), 1);
    assert_eq!((sk[0].0, sk[0].1), ("gone.md", ErrorKind::Io));
    assert!(sk[0].2.starts_with("gone.md: "), "{}", sk[0].2);
    assert_eq!(fs.read_dirs.load(Ordering::SeqCst), 0);
    assert_eq!(link(&s, "a.md", "b").1.targets, ["sub/b.md"]);
}

#[test]
fn open_files_rejects_invalid_paths() {
    let fs = Recording::new(MemFs::new().with_file("a.md", "").with_file("d/b.md", ""));
    let s = open_files(
        fs.clone(),
        &[
            "",
            "/a.md",
            "./a.md",
            "d/../a.md",
            "d//b.md",
            "d/b.md/",
            "d/b.md",
        ],
        &[],
    );
    assert_eq!(s.files().collect::<Vec<_>>(), ["d/b.md"]);
    let sk = skipped(&s);
    let want: Vec<(&str, ErrorKind, String)> =
        ["", "./a.md", "/a.md", "d/../a.md", "d//b.md", "d/b.md/"]
            .into_iter()
            .map(|p| (p, ErrorKind::Unsupported, format!("{p}: invalid path")))
            .collect();
    let got: Vec<(&str, ErrorKind, String)> = sk
        .into_iter()
        .map(|(p, k, m)| (p, k, m.to_owned()))
        .collect();
    assert_eq!(got, want);
    assert_eq!(fs.read_paths(), [PathBuf::from("/d/b.md")]);
}

#[test]
fn dataless_files_are_skipped_unread_unless_forced() {
    let fs = Recording::new(
        MemFs::new()
            .with_file("a.md", "local")
            .with_file("cloud.md", "remote")
            .with_file("opened.md", "remote too")
            .with_dataless("cloud.md")
            .with_dataless("opened.md"),
    );
    let s = open_files(
        fs.clone(),
        &["a.md", "cloud.md", "opened.md"],
        &["opened.md"],
    );
    assert_eq!(s.files().collect::<Vec<_>>(), ["a.md", "opened.md"]);
    assert_eq!(
        skipped(&s),
        [(
            "cloud.md",
            ErrorKind::Unsupported,
            "cloud.md: dataless: not downloaded"
        )]
    );
    assert!(!fs.read_paths().contains(&PathBuf::from("/cloud.md")));
    assert!(fs.read_paths().contains(&PathBuf::from("/opened.md")));
}

#[test]
fn open_skips_dataless_without_reading() {
    let fs = Recording::new(
        MemFs::new()
            .with_file("a.md", "")
            .with_file("cloud.md", "")
            .with_file("cdir/x.md", "")
            .with_dataless("cloud.md")
            .with_dataless("cdir"),
    );
    let s = MemStore::open(fs.clone(), PathBuf::from("/"), &Cancel::new()).unwrap();
    assert_eq!(s.files().collect::<Vec<_>>(), ["a.md"]);
    assert!(s.skipped().is_empty());
    assert_eq!(fs.read_paths(), [PathBuf::from("/a.md")]);
}

#[test]
fn dataless_tool_config_is_not_read() {
    let fs = Recording::new(
        MemFs::new()
            .with_file(
                ".zk/config.toml",
                "[lsp.diagnostics]\ndead-link = \"error\"\n",
            )
            .with_dataless(".zk/config.toml")
            .with_file("a.md", ""),
    );
    let s = open_files(fs.clone(), &["a.md"], &[]);
    assert_eq!(fs.read_paths(), [PathBuf::from("/a.md")]);
    assert_eq!(s.conventions().dead_link_severity, None);
}

#[test]
fn cancelled_open_files_fails() {
    let c = Cancel::new();
    c.cancel();
    let fs = Recording::new(MemFs::new().with_file("a.md", ""));
    let err = MemStore::open_files(fs.clone(), PathBuf::from("/"), vec!["a.md".into()], &[], &c)
        .err()
        .unwrap();
    assert_eq!(err.kind(), ErrorKind::Cancelled);
    assert!(fs.read_paths().is_empty());
}

fn contents(v: &[(&str, &[u8])]) -> Vec<(String, Arc<[u8]>)> {
    v.iter()
        .map(|(p, b)| (p.to_string(), Arc::from(*b)))
        .collect()
}

#[test]
fn from_contents_parses_given_bytes_without_reading_notes() {
    let fs = Recording::new(
        MemFs::new()
            .with_file(
                ".zk/config.toml",
                "[lsp.diagnostics]\ndead-link = \"error\"\n",
            )
            .with_file("a.md", "stale disk text")
            .with_file("sub/b.md", "# Disk B"),
    );
    let s = MemStore::from_contents(
        fs.clone(),
        PathBuf::from("/"),
        contents(&[
            ("sub/b.md", b"# Cached B"),
            ("a.md", b"[[b]]"),
            ("a.md", b"duplicate"),
            ("bin.md", b"x\0y"),
            ("../out.md", b""),
        ]),
        &Cancel::new(),
    )
    .unwrap();
    assert_eq!(s.files().collect::<Vec<_>>(), ["a.md", "sub/b.md"]);
    assert_eq!(s.document("a.md").unwrap().source(), "[[b]]");
    assert_eq!(s.document("sub/b.md").unwrap().source(), "# Cached B");
    assert_eq!(link(&s, "a.md", "b").1.targets, ["sub/b.md"]);
    assert_eq!(
        skipped(&s),
        [
            (
                "../out.md",
                ErrorKind::Unsupported,
                "../out.md: invalid path"
            ),
            ("bin.md", ErrorKind::Unsupported, "bin.md: binary file"),
        ]
    );
    // Only the tool config was read, for convention detection.
    assert_eq!(fs.read_paths(), [PathBuf::from("/.zk/config.toml")]);
    assert!(s.conventions().dead_link_severity.is_some());
    assert_eq!(fs.read_dirs.load(Ordering::SeqCst), 0);
}

#[test]
fn cancelled_from_contents_fails() {
    let c = Cancel::new();
    c.cancel();
    let fs = Recording::new(MemFs::new());
    let err = MemStore::from_contents(fs, PathBuf::from("/"), contents(&[("a.md", b"")]), &c)
        .err()
        .unwrap();
    assert_eq!(err.kind(), ErrorKind::Cancelled);
}

#[test]
fn valid_rel_rejects_empty_absolute_and_dot_components() {
    use mdroots_core::valid_rel;
    assert!(valid_rel("a.md") && valid_rel("d/b.md"));
    for bad in ["", "/a.md", "./a.md", "d/../a.md", "d//b.md", "d/b.md/"] {
        assert!(!valid_rel(bad), "{bad}");
    }
}

fn change(p: &str, b: Option<&[u8]>) -> (String, Option<Arc<[u8]>>) {
    (p.to_owned(), b.map(Arc::from))
}

#[test]
fn apply_contents_replaces_adds_and_removes_in_place() {
    let fs = MemFs::new()
        .with_file("/a.md", "[[b]]\n")
        .with_file("/c.md", "# C\n");
    let mut s = mem(fs);
    assert_eq!(broken(&s, "a.md"), ["b"]);
    let changed = s.apply_contents(vec![
        change("b.md", Some(b"# B\n")),
        change("c.md", None),
        change("gone.md", None),
        change("a.md", Some(b"[[b]]\n")),
    ]);
    // a.md had these bytes already; gone.md was never indexed.
    assert_eq!(changed, ["b.md", "c.md"]);
    assert_eq!(s.files().collect::<Vec<_>>(), ["a.md", "b.md"]);
    // The caches were dropped: the link now resolves and has a backlink.
    assert!(broken(&s, "a.md").is_empty());
    assert_eq!(froms(&s.backlinks("b.md")), ["a.md"]);
}

#[test]
fn apply_contents_keeps_overlays_and_records_binary_files() {
    let mut s = mem(MemFs::new().with_file("/a.md", "disk\n"));
    s.set_overlay("a.md", "overlay\n");
    assert_eq!(
        s.apply_contents(vec![change("a.md", Some(b"new disk\n"))]),
        ["a.md"]
    );
    assert_eq!(s.document("a.md").unwrap().source(), "overlay\n");
    s.clear_overlay("a.md");
    assert_eq!(s.document("a.md").unwrap().source(), "new disk\n");
    // A binary file drops the note and is listed as skipped ...
    assert_eq!(
        s.apply_contents(vec![change("a.md", Some(b"\0bin"))]),
        ["a.md"]
    );
    assert!(s.document("a.md").is_none());
    assert_eq!(s.skipped().len(), 1);
    // ... until it is text again.
    s.apply_contents(vec![change("a.md", Some(b"text\n"))]);
    assert!(s.skipped().is_empty());
    assert_eq!(s.document("a.md").unwrap().source(), "text\n");
}

// --- vote --------------------------------------------------------------

/// The notes of a corpus root in a `MemFs` at `/`, without its tool config
/// (`.zk/`, dotfiles): a root the vote alone decides.
fn corpus_without_config(name: &str) -> MemStore {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/corpus")
        .join(name);
    let mut fs = MemFs::new();
    let mut dirs = vec![PathBuf::new()];
    while let Some(dir) = dirs.pop() {
        for e in std::fs::read_dir(root.join(&dir)).unwrap() {
            let e = e.unwrap();
            let name = e.file_name().into_string().unwrap();
            if name.starts_with('.') {
                continue;
            }
            let rel = dir.join(&name);
            if e.file_type().unwrap().is_dir() {
                dirs.push(rel);
            } else if name.ends_with(".md") {
                let text = std::fs::read_to_string(e.path()).unwrap();
                fs = fs.with_file(&format!("/{}", rel.display()), &text);
            }
        }
    }
    mem(fs)
}

#[test]
fn zkvault_vote_is_root_relative_wiki() {
    let s = corpus("zkvault");
    let v = s.vote();
    assert!(v.explicit_links > 0);
    assert!(v.wiki_share >= 0.5, "{v:?}");
    assert_eq!(v.insert_style, Some(ResolveStep::RootRelative));
    // Its config says the same: `link-format = "wiki"`.
    assert_eq!(link_style(s.conventions(), &v), LinkStyle::WikiPath);
}

#[test]
fn obsidian_shaped_vault_votes_wiki_stem() {
    // Bare stems to root-level notes resolve by joining the root, but they
    // are stem links, not root-relative ones. No `.obsidian` marker: the
    // vote alone decides.
    let s = mem(MemFs::new()
        .with_file("/top.md", "# Top\n")
        .with_file("/other.md", "# Other\n")
        .with_file("/x/one.md", "# One\n\n[[top]] [[other]]\n")
        .with_file("/y/two.md", "# Two\n\n[[top]] [[one]]\n"));
    let v = s.vote();
    assert_eq!(v.explicit_links, 4);
    assert_eq!(v.insert_style, Some(ResolveStep::Stem), "{v:?}");
    assert_eq!(link_style(s.conventions(), &v), LinkStyle::WikiStem);
}

#[test]
fn notesvault_config_wins_and_its_majority_is_wiki_stem() {
    let s = corpus("notesvault");
    // `link-format = "wiki"`: zk's root-relative wiki links.
    assert_eq!(link_style(s.conventions(), &s.vote()), LinkStyle::WikiPath);

    let bare = corpus_without_config("notesvault");
    assert!(bare.conventions().markers.is_empty());
    let v = bare.vote();
    // Bare stems: same-directory ones stop at step 1, the rest at Stem;
    // either way not root-relative.
    assert_eq!(v.insert_style, Some(ResolveStep::FileRelative), "{v:?}");
    assert!(v.wiki_share >= 0.5, "{v:?}");
    assert_eq!(link_style(bare.conventions(), &v), LinkStyle::WikiStem);
}

#[test]
fn vote_counts_referencing_non_external_links_only() {
    let s = mem(MemFs::new()
        .with_file(
            "/a.md",
            "# A\n\n[b](b.md) <https://example.com> `[[b]]`\n\n```\n[[b]]\n```\n",
        )
        .with_file("/b.md", "# B\n"));
    let v = s.vote();
    assert_eq!(v.explicit_links, 1);
    assert_eq!(v.wiki_share, 0.0);
    assert_eq!(v.md_suffix_share, 1.0);
    assert_eq!(v.insert_style, Some(ResolveStep::FileRelative));
    assert!(v.h1_is_title);
}

#[test]
fn vote_cache_is_dropped_by_set_overlay() {
    let mut s = mem(MemFs::new()
        .with_file("/a.md", "[b](b.md)\n")
        .with_file("/b.md", ""));
    assert_eq!(s.vote().wiki_share, 0.0);
    s.set_overlay("a.md", "[[b]] [[b]]\n");
    let v = s.vote();
    assert_eq!(v.wiki_share, 1.0);
    assert_eq!(v.explicit_links, 2);
    s.clear_overlay("a.md");
    assert_eq!(s.vote().wiki_share, 0.0);
}

#[test]
fn code_dirs_extend_code_mention_lookup() {
    let fs = MemFs::new()
        .with_file("/p/notes/docs/a.md", "`src/main.rs:12` `lib.rs`\n")
        .with_file("/p/notes/docs/lib.rs", "")
        .with_file("/p/src/main.rs", "")
        .with_file("/p/lib.rs", "");
    let mut s = MemStore::open(Arc::new(fs), PathBuf::from("/p/notes"), &Cancel::new()).unwrap();

    let (_, r) = link(&s, "docs/a.md", "src/main.rs:12");
    assert_eq!(r.status, LinkStatus::Unchecked);
    assert!(r.targets.is_empty());

    s.set_code_dirs(vec!["..".to_owned()]);
    assert_eq!(s.code_dirs(), [".."]);
    let (_, r) = link(&s, "docs/a.md", "src/main.rs:12");
    assert_eq!(r.status, LinkStatus::Unindexed);
    assert_eq!(r.targets, ["../src/main.rs"]);
    // The linking note's dir wins over a code dir.
    let (_, r) = link(&s, "docs/a.md", "lib.rs");
    assert_eq!(r.targets, ["docs/lib.rs"]);

    s.set_code_dirs(Vec::new());
    let (_, r) = link(&s, "docs/a.md", "src/main.rs:12");
    assert_eq!(r.status, LinkStatus::Unchecked);
}
