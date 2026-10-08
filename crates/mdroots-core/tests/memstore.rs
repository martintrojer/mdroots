use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use mdroots_core::{
    AnchorStatus, Cancel, ErrorKind, FileSystem, FsKind, MemFs, MemStore, Meta, StdFs,
};
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

fn raws(links: &[Link]) -> Vec<&str> {
    links.iter().map(|l| l.target.raw.as_str()).collect()
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
    assert!(raws(&s.broken("reference/composting.md")).contains(&"reference/does-not-exist"));

    // Org `[[T:1][x]]` expands through #+LINK to another scheme.
    let (_, r) = link(&s, "org-archive/old.org", "https://t/1");
    assert_eq!(r.status, LinkStatus::External);

    // Wiki candidates in inline code and fences are never broken.
    assert!(s.broken("reference/gardening.md").is_empty());
    assert!(s.broken("org-archive/old.org").is_empty());

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
    assert!(s.broken("README.md").is_empty());
    // Nor does it make README a referrer of project-scope.
    assert!(s.backlinks("notes/project-scope.md").is_empty());

    assert_eq!(raws(&s.broken("references/paper-2.md")), ["missing-paper"]);

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
    assert!(s.broken("a/one.md").is_empty());
    assert_eq!(raws(&s.broken("b/two.md")), ["gone"]);
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
    assert!(s.broken("a.md").is_empty());

    // Missing headings and block ids, in prose only; the ambiguous u.md and
    // the unindexed html target are never listed.
    assert_eq!(
        raws(&s.broken_anchors("a.md")),
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
    assert_eq!(raws(&s.broken_anchors("org/o.org")), ["file:b.org::#nope"]);
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
        raws(&s.broken_anchors("a.md")),
        ["b.md#custom", "b.md#visible", "#nowhere"]
    );
    s.clear_overlay("b.md");
    assert_eq!(
        raws(&s.broken_anchors("a.md")),
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
