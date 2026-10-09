//! Tests for the `mdroots` facade. Trees are in memory (`MemFs` with a
//! `FakeProbe` holding the same files), temp dirs, or `tests/corpus`.
//! Virtual mounts use the `edenfs` type name of
//! [EdenFS](https://github.com/facebook/sapling) (the virtual filesystem from
//! the [Sapling](https://sapling-scm.com/) project); the git repo of
//! [git](https://git-scm.com) is a bare `.git` directory.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mdroots::index::{IndexDb, SqliteRegistry};
use mdroots::syntax::{LinkKind, PositionEncoding};
use mdroots::{
    Cancel, DiagCode, ErrorKind, FileSystem, Freshness, IndexMode, LinkStatus, NoEnumerator,
    Options, ResolveStep, Role, RootMode, StdFs, StdProbe, Workspace, Workspaces,
};
use mdroots_core::{DiagnosticPolicy, FsKind, MemFs, MemStore, Meta};
use mdroots_roots::discover::Enumerator;
use mdroots_roots::probe::{Counting, FakeProbe, MountInfo};
use mdroots_roots::registry::Registry;

fn p(s: &str) -> PathBuf {
    PathBuf::from(s)
}

/// A [`FileSystem`] recording every `read_dir` path and every `read` of a
/// note (`.md`, `.markdown`, `.org`).
struct Recording {
    inner: MemFs,
    read_dirs: Mutex<Vec<PathBuf>>,
    note_reads: Mutex<Vec<PathBuf>>,
}

impl Recording {
    fn new(inner: MemFs) -> Arc<Self> {
        Arc::new(Recording {
            inner,
            read_dirs: Mutex::default(),
            note_reads: Mutex::default(),
        })
    }

    fn read_dirs(&self) -> Vec<PathBuf> {
        self.read_dirs.lock().unwrap().clone()
    }

    /// The notes read so far, and forget them.
    fn take_note_reads(&self) -> Vec<PathBuf> {
        std::mem::take(&mut *self.note_reads.lock().unwrap())
    }
}

impl FileSystem for Recording {
    fn read(&self, p: &Path) -> io::Result<(Arc<[u8]>, Meta)> {
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or_default();
        if ["md", "markdown", "org"].contains(&ext) {
            self.note_reads.lock().unwrap().push(p.to_path_buf());
        }
        self.inner.read(p)
    }
    fn stat(&self, p: &Path) -> io::Result<Meta> {
        self.inner.stat(p)
    }
    fn read_dir(&self, p: &Path) -> io::Result<Vec<(String, Meta)>> {
        self.read_dirs.lock().unwrap().push(p.to_path_buf());
        self.inner.read_dir(p)
    }
    fn canonicalize(&self, p: &Path) -> io::Result<PathBuf> {
        self.inner.canonicalize(p)
    }
    fn case_sensitive(&self, d: &Path) -> bool {
        self.inner.case_sensitive(d)
    }
    fn fs_kind(&self, d: &Path) -> FsKind {
        self.inner.fs_kind(d)
    }
}

/// The same files in a `MemFs` and a `FakeProbe` (home `/h`).
fn both(files: &[(&str, &str)]) -> (MemFs, FakeProbe) {
    let mut fs = MemFs::new();
    let mut probe = FakeProbe::new().home("/h");
    for (path, text) in files {
        fs = fs.with_file(path, text);
        probe = probe.file(path, text);
    }
    (fs, probe)
}

/// A marker root at `/v` (`.mdroots`).
const VAULT: &[(&str, &str)] = &[
    ("/v/.mdroots", ""),
    (
        "/v/a.md",
        "---\ntitle: Alpha\ntags: [x]\n---\n# A\n\nSee [[b]], [b](b.md), [[gone]] and [[b#^blk]].\n\
         Also [[dup]], [pic](pic.png), <https://example.com> and `src/x.rs:12`. #x #y\n",
    ),
    ("/v/b.md", "# Beta\n\nBack to [[a]]. #y\n\nline ^blk\n"),
    ("/v/sub/c.md", "Up: [a](../a.md) and [[b]].\n"),
    ("/v/one/dup.md", ""),
    ("/v/two/dup.md", ""),
    ("/v/pic.png", "png"),
    ("/v/src/x.rs", "fn main() {}\n"),
];

fn opts(fs: Arc<dyn FileSystem>, probe: Arc<dyn mdroots::Probe>) -> Options {
    Options::default()
        .fs(fs)
        .probe(probe)
        .enumerator(Arc::new(NoEnumerator))
}

fn vault() -> Workspace {
    let (fs, probe) = both(VAULT);
    Workspace::open_for(Path::new("/v/a.md"), opts(Arc::new(fs), Arc::new(probe))).unwrap()
}

fn corpus(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/corpus")
        .join(name)
        .canonicalize()
        .unwrap()
}

fn std_opts() -> Options {
    Options::default()
        .fs(Arc::new(StdFs))
        .probe(Arc::new(StdProbe))
        .enumerator(Arc::new(NoEnumerator))
        .index(IndexMode::Memory)
}

// --- open_for ----------------------------------------------------------

#[test]
fn corpus_zkvault_is_a_marker_root_with_memstore_files() {
    let root = corpus("zkvault");
    let ws = Workspace::open_for(&root.join("journal/2026/W40.md"), std_opts()).unwrap();
    let info = ws.root();
    assert_eq!(info.mode, RootMode::Marker);
    assert_eq!(info.path, root);
    assert!(info.reason.starts_with("marker .zk"), "{}", info.reason);
    assert_eq!(ws.freshness(), Freshness::Fresh);
    let store = MemStore::open(Arc::new(StdFs), root.clone(), &Cancel::new()).unwrap();
    let want: Vec<PathBuf> = store.files().map(|f| root.join(f)).collect();
    assert_eq!(ws.files(), want);
}

#[test]
fn temp_git_repo_is_a_vcs_root() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap().join("repo");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::write(root.join("README.md"), "# Repo\n\n[guide](docs/guide.md)\n").unwrap();
    std::fs::write(root.join("docs/guide.md"), "# Guide\n").unwrap();
    // Opened through a non-canonical path: results are canonical.
    let opened = root.join("docs/../README.md");
    let ws = Workspace::open_for(&opened, std_opts()).unwrap();
    assert_eq!(ws.root().mode, RootMode::Vcs);
    assert_eq!(ws.root().path, root);
    assert_eq!(
        ws.files(),
        [root.join("README.md"), root.join("docs/guide.md")]
    );
    let back = ws.backlinks(&root.join("docs/guide.md")).unwrap();
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].from, root.join("README.md"));
    assert_eq!(back[0].from_title, "Repo");
}

#[test]
fn marker_root_in_memory() {
    let ws = vault();
    assert_eq!(ws.root().mode, RootMode::Marker);
    assert_eq!(ws.root().path, p("/v"));
    assert_eq!(ws.freshness(), Freshness::Fresh);
    assert_eq!(
        ws.files(),
        [
            "/v/a.md",
            "/v/b.md",
            "/v/one/dup.md",
            "/v/sub/c.md",
            "/v/two/dup.md"
        ]
        .map(p)
    );
}

fn eden() -> MountInfo {
    MountInfo {
        fs_type: "edenfs:".into(),
        from: "edenfs".into(),
        local: false,
        dev: 5,
    }
}

const EDEN: &[(&str, &str)] = &[
    (
        "/eden/repo/docs/a.md",
        "[[b]] and [[nowhere]] and [x](../far.md)",
    ),
    ("/eden/repo/docs/b.md", "# B"),
    ("/eden/repo/docs/notes.org", "* N"),
    ("/eden/repo/docs/.hidden.md", ""),
    ("/eden/repo/docs/b.md~", ""),
    ("/eden/repo/docs/#a.md#", ""),
    ("/eden/repo/docs/img.png", ""),
    ("/eden/repo/docs/sub/deep.md", ""),
    ("/eden/repo/far.md", ""),
];

/// An EdenFS checkout at `/eden/repo` holding [`EDEN`] (and `extra`).
fn eden_repo(extra: &[(&str, &str)]) -> (MemFs, FakeProbe) {
    let mut files = EDEN.to_vec();
    files.extend_from_slice(extra);
    let (fs, probe) = both(&files);
    let probe = probe
        .mount("/eden", eden())
        .symlink("/eden/repo/.eden/root", "/eden/repo")
        .symlink("/eden/repo/docs/.eden/root", "/eden/repo");
    (fs, probe)
}

#[test]
fn lazy_root_indexes_one_dir_listing_only() {
    let (fs, probe) = eden_repo(&[]);
    let fs = Recording::new(fs.with_dataless("/eden/repo/docs/cloud.md"));
    let probe = Arc::new(Counting::new(probe));
    let dir = p("/eden/repo/docs");
    probe.forbid_read_dir_outside(std::slice::from_ref(&dir));
    let ws = Workspace::open_for(
        Path::new("/eden/repo/docs/a.md"),
        opts(fs.clone(), probe.clone()),
    )
    .unwrap();
    assert_eq!(ws.root().mode, RootMode::Lazy);
    assert_eq!(ws.root().path, p("/eden/repo"));
    assert_eq!(ws.freshness(), Freshness::Lazy);
    assert_eq!(
        ws.files(),
        [
            "/eden/repo/docs/a.md",
            "/eden/repo/docs/b.md",
            "/eden/repo/docs/notes.org"
        ]
        .map(p)
    );
    assert!(probe.violations().is_empty(), "{:?}", probe.violations());
    assert_eq!(fs.read_dirs(), [dir]);
    // Lazy diagnostics: the bare stem outside the working set is a hint.
    let d = ws
        .diagnostics(Path::new("/eden/repo/docs/a.md"), &Cancel::new())
        .unwrap();
    assert_eq!(
        d.iter().map(|d| d.code).collect::<Vec<_>>(),
        [DiagCode::NotInWorkingSet]
    );
}

#[test]
fn lazy_working_set_is_capped_at_2000_including_the_opened_file() {
    let names: Vec<String> = (0..2_005)
        .map(|i| format!("/eden/repo/docs/n{i:04}.md"))
        .collect();
    let extra: Vec<(&str, &str)> = names.iter().map(|n| (n.as_str(), "")).collect();
    let (fs, probe) = eden_repo(&extra);
    let ws = Workspace::open_for(
        Path::new("/eden/repo/docs/n2004.md"),
        opts(Recording::new(fs), Arc::new(Counting::new(probe))),
    )
    .unwrap();
    let files = ws.files();
    assert_eq!(files.len(), 2_000);
    assert!(files.contains(&p("/eden/repo/docs/n2004.md")));
}

#[test]
fn lazy_opened_dataless_file_is_read() {
    let (fs, probe) = eden_repo(&[]);
    let fs = fs
        .with_file("/eden/repo/docs/cloud.md", "# Cloud\n")
        .with_dataless("/eden/repo/docs/cloud.md");
    let probe = probe.file("/eden/repo/docs/cloud.md", "# Cloud\n");
    let ws = Workspace::open_for(
        Path::new("/eden/repo/docs/cloud.md"),
        opts(Arc::new(fs), Arc::new(probe)),
    )
    .unwrap();
    let notes = ws.notes();
    let cloud = notes
        .iter()
        .find(|n| n.path == p("/eden/repo/docs/cloud.md"))
        .unwrap();
    assert_eq!(cloud.title, "Cloud");
}

/// A fake enumerator returning `paths`.
struct FakeEnum(Vec<String>);

impl Enumerator for FakeEnum {
    fn md_paths(&self, _: &Path, _: Duration, _: usize) -> Option<Vec<String>> {
        Some(self.0.clone())
    }
}

#[test]
fn vcs_enumerated_root_indexes_the_enumerated_files() {
    let (fs, probe) = eden_repo(&[]);
    let fs = Recording::new(fs);
    let probe = Arc::new(Counting::new(probe));
    probe.forbid_read_dir_outside(&[]);
    let en = FakeEnum(vec!["far.md".into(), "docs/b.md".into()]);
    let o = opts(fs.clone(), probe.clone()).enumerator(Arc::new(en));
    let ws = Workspace::open_for(Path::new("/eden/repo/docs/a.md"), o).unwrap();
    assert_eq!(ws.root().mode, RootMode::VcsEnumerated);
    assert_eq!(ws.freshness(), Freshness::Fresh);
    assert_eq!(
        ws.files(),
        [
            "/eden/repo/docs/a.md",
            "/eden/repo/docs/b.md",
            "/eden/repo/far.md"
        ]
        .map(p)
    );
    assert!(probe.violations().is_empty());
    assert!(fs.read_dirs().is_empty());
}

#[test]
fn single_file_mode_indexes_the_file_alone() {
    let (fs, probe) = both(&[("/h/a.md", "[[b]]"), ("/h/b.md", "")]);
    let fs = Recording::new(fs);
    let probe = Arc::new(Counting::new(probe));
    probe.forbid_read_dir_outside(&[]);
    let ws = Workspace::open_for(Path::new("/h/a.md"), opts(fs.clone(), probe.clone())).unwrap();
    let info = ws.root();
    assert_eq!(info.mode, RootMode::SingleFile);
    assert_eq!(info.path, p("/h"));
    assert_eq!(info.reason, "single-file: home directory");
    assert_eq!(ws.freshness(), Freshness::Lazy);
    assert_eq!(ws.files(), [p("/h/a.md")]);
    assert!(probe.violations().is_empty());
    assert!(fs.read_dirs().is_empty());
}

#[test]
fn lazy_without_a_root_is_single_file_like() {
    let nfs = MountInfo {
        fs_type: "nfs".into(),
        from: "server:/export".into(),
        local: false,
        dev: 7,
    };
    let (fs, probe) = both(&[("/net/share/a.md", ""), ("/net/share/b.md", "")]);
    let probe = probe.mount("/net", nfs);
    let ws = Workspace::open_for(
        Path::new("/net/share/a.md"),
        opts(Arc::new(fs), Arc::new(probe)),
    )
    .unwrap();
    assert_eq!(ws.root().mode, RootMode::Lazy);
    assert_eq!(ws.root().path, p("/net/share"));
    assert_eq!(ws.freshness(), Freshness::Lazy);
    assert_eq!(ws.files(), [p("/net/share/a.md")]);
}

#[test]
fn open_for_errors() {
    let (fs, probe) = both(VAULT);
    let (fs, probe): (Arc<dyn FileSystem>, Arc<dyn mdroots::Probe>) =
        (Arc::new(fs), Arc::new(probe));
    let missing = Workspace::open_for(Path::new("/v/nope.md"), opts(fs.clone(), probe.clone()));
    assert_eq!(missing.err().unwrap().kind(), ErrorKind::Io);
    let dir = Workspace::open_for(Path::new("/v/sub"), opts(fs.clone(), probe.clone()));
    assert_eq!(dir.err().unwrap().kind(), ErrorKind::Io);
    let half = Workspace::open_for(Path::new("/v/a.md"), Options::default().fs(fs.clone()));
    let e = half.err().unwrap();
    assert_eq!(
        (e.kind(), e.message()),
        (ErrorKind::Unsupported, "fs and probe must both be set")
    );
    let half = Workspace::open_for(
        Path::new("/v/a.md"),
        Options::default().probe(probe.clone()),
    );
    assert_eq!(half.err().unwrap().kind(), ErrorKind::Unsupported);
    let c = Cancel::new();
    c.cancel();
    let cancelled = Workspace::open_for(Path::new("/v/a.md"), opts(fs, probe).cancel(c));
    assert_eq!(cancelled.err().unwrap().kind(), ErrorKind::Cancelled);
}

#[test]
fn open_at_skips_discovery() {
    let (fs, _) = both(VAULT);
    let o = Options::default()
        .fs(Arc::new(fs))
        .probe(Arc::new(Counting::new(FakeProbe::new())));
    let ws = Workspace::open_at(Path::new("/v/sub"), o).unwrap();
    let info = ws.root();
    assert_eq!(info.mode, RootMode::Marker);
    assert_eq!(info.reason, "opened at /v/sub");
    assert_eq!(ws.freshness(), Freshness::Fresh);
    assert_eq!(ws.files(), [p("/v/sub/c.md")]);
}

// --- queries -----------------------------------------------------------

#[test]
fn resolve_wiki_md_and_relative() {
    let ws = vault();
    let a = p("/v/a.md");
    let r = ws.resolve(&a, "[[b]]").unwrap();
    assert_eq!(
        (r.status, r.targets),
        (LinkStatus::Resolved, vec![p("/v/b.md")])
    );
    let r = ws.resolve(&a, "[x](sub/c.md)").unwrap();
    assert_eq!(r.targets, [p("/v/sub/c.md")]);
    assert_eq!(r.step, Some(ResolveStep::FileRelative));
    let r = ws
        .resolve(Path::new("/v/sub/c.md"), "[up](../a.md)")
        .unwrap();
    assert_eq!(r.targets, std::slice::from_ref(&a));
    // goto semantics: the Partial step is allowed.
    let r = ws.resolve(&a, "[[ub/c]]").unwrap();
    assert!(r.hint, "{r:?}");
    assert_eq!(r.step, Some(ResolveStep::Partial));
    assert_eq!(r.targets, [p("/v/sub/c.md")]);
    let r = ws.resolve(&a, "[[dup]]").unwrap();
    assert_eq!(r.status, LinkStatus::Ambiguous);
    assert_eq!(r.targets.len(), 2);
    let e = ws.resolve(&a, "just words").unwrap_err();
    assert_eq!(
        (e.kind(), e.message()),
        (ErrorKind::Unsupported, "not a link")
    );
}

#[test]
fn document_links_statuses() {
    let ws = vault();
    let links = ws.document_links(Path::new("/v/a.md")).unwrap();
    let find = |kind: LinkKind, target: Option<&str>, status: LinkStatus| {
        links
            .iter()
            .find(|l| {
                l.kind == kind && l.target.as_deref() == target.map(Path::new) && l.status == status
            })
            .unwrap_or_else(|| panic!("{kind:?} {target:?} {status:?} in {links:#?}"))
    };
    find(LinkKind::Wiki, Some("/v/b.md"), LinkStatus::Resolved);
    find(LinkKind::Markdown, Some("/v/b.md"), LinkStatus::Resolved);
    find(LinkKind::Wiki, None, LinkStatus::Broken);
    find(LinkKind::Wiki, Some("/v/one/dup.md"), LinkStatus::Ambiguous);
    find(
        LinkKind::Markdown,
        Some("/v/pic.png"),
        LinkStatus::Unindexed,
    );
    find(LinkKind::Autolink, None, LinkStatus::External);
    let block = links.iter().find(|l| l.anchor.is_some()).unwrap();
    assert_eq!(block.anchor.as_deref(), Some("^blk"));
    assert_eq!(block.target.as_deref(), Some(Path::new("/v/b.md")));
    let code = find(
        LinkKind::CodeMention,
        Some("/v/src/x.rs"),
        LinkStatus::Unindexed,
    );
    assert_eq!(code.line, Some(12));
    assert!(links.is_sorted_by_key(|l| l.range.start));
}

#[test]
fn target_outside_root_is_an_absolute_path() {
    let (fs, probe) = both(&[
        ("/v/.mdroots", ""),
        ("/v/a.md", "Compare `../w/x.md` here.\n"),
        ("/w/x.md", ""),
    ]);
    let ws =
        Workspace::open_for(Path::new("/v/a.md"), opts(Arc::new(fs), Arc::new(probe))).unwrap();
    let links = ws.document_links(Path::new("/v/a.md")).unwrap();
    assert!(
        links
            .iter()
            .any(|l| l.target.as_deref() == Some(Path::new("/w/x.md"))),
        "{links:#?}"
    );
}

#[test]
fn backlinks_with_titles_and_lines() {
    let ws = vault();
    let back = ws.backlinks(Path::new("/v/b.md")).unwrap();
    let got: Vec<(&Path, &str, u32)> = back
        .iter()
        .map(|b| (b.from.as_path(), b.from_title.as_str(), b.line))
        .collect();
    assert_eq!(
        got,
        [
            (Path::new("/v/a.md"), "Alpha", 6),
            (Path::new("/v/a.md"), "Alpha", 6),
            (Path::new("/v/a.md"), "Alpha", 6),
            (Path::new("/v/sub/c.md"), "c", 0),
        ]
    );
    assert!(back.iter().all(|b| !b.in_code));
    let back = ws.backlinks(Path::new("/v/a.md")).unwrap();
    let titles: Vec<&str> = back.iter().map(|b| b.from_title.as_str()).collect();
    assert_eq!(titles, ["Beta", "c"]);
}

#[test]
fn notes_and_tags() {
    let ws = vault();
    let notes = ws.notes();
    let a = notes.iter().find(|n| n.path == p("/v/a.md")).unwrap();
    assert_eq!(a.title, "Alpha");
    assert_eq!(a.tags, ["x", "y"]);
    assert_eq!(
        notes.iter().find(|n| n.path == p("/v/b.md")).unwrap().title,
        "Beta"
    );
    assert_eq!(ws.tags(), [("x".to_owned(), 1), ("y".to_owned(), 2)]);
}

/// A temp git repo with `files`, opened in memory mode.
fn temp_repo(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf, Workspace) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap().join("repo");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    for (name, text) in files {
        std::fs::write(root.join(name), text).unwrap();
    }
    let ws = Workspace::open_for(&root.join(files[0].0), std_opts()).unwrap();
    (tmp, root, ws)
}

#[test]
fn note_summary_modified_is_the_file_mtime() {
    let (_tmp, root, ws) = temp_repo(&[("a.md", "# A\n"), ("b.md", "# B\n")]);
    let notes = ws.notes();
    assert_eq!(notes.len(), 2);
    for n in &notes {
        let mtime = std::fs::metadata(&n.path).unwrap().modified().unwrap();
        assert_eq!(n.modified, Some(mtime), "{}", n.path.display());
    }
    // An unsaved note has no time on disk.
    let fresh = root.join("fresh.md");
    ws.set_overlay(&fresh, "# Fresh\n").unwrap();
    let n = ws.notes().into_iter().find(|n| n.path == fresh).unwrap();
    assert_eq!(n.modified, None);
}

#[test]
fn note_summary_modified_keeps_pre_epoch_times() {
    let (_tmp, root, _ws) = temp_repo(&[("old.md", "# Old\n")]);
    let old = std::time::UNIX_EPOCH - std::time::Duration::from_secs(315_619_200);
    let file = std::fs::File::options()
        .write(true)
        .open(root.join("old.md"))
        .unwrap();
    file.set_modified(old).unwrap();
    drop(file);
    let ws = Workspace::open_at(&root, Options::default().index(IndexMode::Memory)).unwrap();
    let n = ws.notes().into_iter().next().unwrap();
    assert_eq!(n.modified, Some(old));
}

#[test]
fn notes_with_tag_matches_frontmatter_and_hash_tags_ignoring_case() {
    let (_tmp, root, ws) = temp_repo(&[
        ("a.md", "---\ntags: [Rust, cli]\n---\n# A\n"),
        ("b.md", "# B\n\nabout #rust here\n"),
        ("c.md", "# C\n\n#RUST and #other\n"),
        ("d.md", "# D\n\n#cli only\n"),
    ]);
    let paths = |tag: &str| -> Vec<PathBuf> {
        ws.notes_with_tag(tag).into_iter().map(|n| n.path).collect()
    };
    let rust = [root.join("a.md"), root.join("b.md"), root.join("c.md")];
    assert_eq!(paths("rust"), rust);
    assert_eq!(paths("RUST"), rust);
    assert_eq!(paths("Rust"), rust);
    assert_eq!(paths("cli"), [root.join("a.md"), root.join("d.md")]);
    assert!(paths("none").is_empty());
    assert!(paths("rus").is_empty());
    // The summaries are those of notes().
    let all = ws.notes();
    for n in ws.notes_with_tag("rust") {
        assert!(all.contains(&n));
    }
}

#[test]
fn diagnostics_equal_the_policy() {
    let (fs, _) = both(VAULT);
    let ws = vault();
    let files: Vec<String> = ws
        .files()
        .iter()
        .map(|f| f.strip_prefix("/v").unwrap().to_string_lossy().into_owned())
        .collect();
    let store = MemStore::open_files(Arc::new(fs), p("/v"), files, &[], &Cancel::new()).unwrap();
    let want = DiagnosticPolicy::for_store(&store, false).diagnostics(&store, "a.md");
    let got = ws
        .diagnostics(Path::new("/v/a.md"), &Cancel::new())
        .unwrap();
    assert_eq!(got, want);
    assert!(got.iter().any(|d| d.code == DiagCode::BrokenLink));
    let c = Cancel::new();
    c.cancel();
    let e = ws.diagnostics(Path::new("/v/a.md"), &c).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Cancelled);
}

#[test]
fn overlay_changes_resolve_and_diagnostics() {
    let ws = vault();
    let a = p("/v/a.md");
    let new = p("/v/new.md");
    assert_eq!(
        ws.resolve(&a, "[[new]]").unwrap().status,
        LinkStatus::Broken
    );
    ws.set_overlay(&new, "# New\n").unwrap();
    assert_eq!(
        ws.resolve(&a, "[[new]]").unwrap().targets,
        std::slice::from_ref(&new)
    );
    assert!(ws.files().contains(&new));
    ws.set_overlay(&a, "fine [[b]]\n").unwrap();
    assert!(ws.diagnostics(&a, &Cancel::new()).unwrap().is_empty());
    ws.clear_overlay(&a).unwrap();
    assert!(!ws.diagnostics(&a, &Cancel::new()).unwrap().is_empty());
    ws.clear_overlay(&new).unwrap();
    assert!(!ws.files().contains(&new));
}

#[test]
fn line_col_counts_the_current_text() {
    let ws = vault();
    let b = p("/v/b.md");
    ws.set_overlay(&b, "é\nxé😀y\n").unwrap();
    let y = "é\nxé😀".len();
    assert_eq!(ws.line_col(&b, y, PositionEncoding::Utf8).unwrap(), (1, 7));
    assert_eq!(ws.line_col(&b, y, PositionEncoding::Utf16).unwrap(), (1, 4));
    assert_eq!(ws.line_col(&b, y, PositionEncoding::Utf32).unwrap(), (1, 3));
    let e = ws
        .line_col(Path::new("/v/pic.png"), 0, PositionEncoding::Utf8)
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Unsupported);
}

#[test]
fn unindexed_inside_root_is_empty_and_outside_is_an_error() {
    let ws = vault();
    let png = Path::new("/v/pic.png");
    assert!(ws.document_links(png).unwrap().is_empty());
    assert!(ws.backlinks(png).unwrap().is_empty());
    assert!(ws.diagnostics(png, &Cancel::new()).unwrap().is_empty());
    let out = Path::new("/elsewhere/x.md");
    for e in [
        ws.document_links(out).err(),
        ws.backlinks(out).err(),
        ws.diagnostics(out, &Cancel::new()).err(),
        ws.resolve(out, "[[b]]").err(),
        ws.set_overlay(out, "x").err(),
    ] {
        let e = e.unwrap();
        assert_eq!(
            (e.kind(), e.message()),
            (ErrorKind::Unsupported, "outside root")
        );
    }
}

#[test]
fn workspace_is_shared_across_threads() {
    let ws = vault();
    let w2 = ws.clone();
    std::thread::spawn(move || w2.set_overlay(Path::new("/v/t.md"), "# T").unwrap())
        .join()
        .unwrap();
    assert!(ws.files().contains(&p("/v/t.md")));
}

// --- persistent index ----------------------------------------------------

/// [`VAULT`] behind a [`Recording`] fs and a [`Counting`] probe.
fn recorded_vault() -> (Arc<Recording>, Arc<Counting<FakeProbe>>) {
    let (fs, probe) = both(VAULT);
    (Recording::new(fs), Arc::new(Counting::new(probe)))
}

/// Options on a temp cache dir (never the user's).
fn cached(fs: &Arc<Recording>, probe: &Arc<Counting<FakeProbe>>, cache: &Path) -> Options {
    opts(fs.clone(), probe.clone()).cache_dir(cache.to_path_buf())
}

fn open_cached(fs: &Arc<Recording>, probe: &Arc<Counting<FakeProbe>>, cache: &Path) -> Workspace {
    Workspace::open_for(Path::new("/v/a.md"), cached(fs, probe, cache)).unwrap()
}

/// The DB rows of `ws`'s root: (path, content).
fn db_rows(ws: &Workspace) -> Vec<(String, String)> {
    let db = IndexDb::open(&ws.cache().unwrap()).unwrap();
    db.rows()
        .unwrap()
        .into_iter()
        .map(|r| (r.path, String::from_utf8_lossy(&r.content).into_owned()))
        .collect()
}

fn db_paths(ws: &Workspace) -> Vec<String> {
    db_rows(ws).into_iter().map(|(p, _)| p).collect()
}

fn text_of(ws: &Workspace, path: &str) -> String {
    ws.text(Path::new(path)).unwrap()
}

fn row(path: &str, text: &str) -> (String, String) {
    (path.to_owned(), text.to_owned())
}

#[test]
fn first_open_indexes_and_creates_the_db() {
    let cache = tempfile::tempdir().unwrap();
    let (fs, probe) = recorded_vault();
    let ws = open_cached(&fs, &probe, cache.path());
    assert_eq!(ws.role(), Some(Role::Reconciler));
    let db = ws.cache().unwrap();
    assert!(db.starts_with(cache.path().join("roots")), "{db:?}");
    assert!(db.to_string_lossy().ends_with(".v2.db"), "{db:?}");
    assert_eq!(
        db_paths(&ws),
        ["a.md", "b.md", "one/dup.md", "sub/c.md", "two/dup.md"]
    );
    assert_eq!(ws.files().len(), 5);
    for f in ["discover.lock", "roots.v1.db"] {
        assert!(cache.path().join(f).exists(), "{f}");
    }
}

#[test]
fn a_second_workspace_is_a_peer_that_hydrates_from_the_db() {
    let cache = tempfile::tempdir().unwrap();
    let (fs, probe) = recorded_vault();
    let first = open_cached(&fs, &probe, cache.path());
    fs.take_note_reads();
    let dirs_before = probe.read_dir_total();
    let peer = open_cached(&fs, &probe, cache.path());
    assert_eq!(peer.role(), Some(Role::Peer));
    assert_eq!(peer.cache(), first.cache());
    assert_eq!(peer.files(), first.files());
    assert_eq!(peer.notes(), first.notes());
    // Hydrated from the DB: no note opened, and discovery hit the registry
    // in stage 1 without listing a directory.
    assert_eq!(fs.take_note_reads(), Vec::<PathBuf>::new());
    assert_eq!(probe.read_dir_total(), dirs_before);
    let reg = SqliteRegistry::open(&cache.path().join("roots.v1.db")).unwrap();
    let rows = reg.all();
    assert_eq!(rows.len(), 1);
    let name = format!("{}.v2.db", rows[0].root_id);
    assert_eq!(peer.cache().unwrap().file_name().unwrap(), name.as_str());
}

#[test]
fn a_peer_on_an_empty_db_relists_the_root_on_a_registry_hit() {
    let cache = tempfile::tempdir().unwrap();
    let (fs, probe) = recorded_vault();
    let first = open_cached(&fs, &probe, cache.path());
    // The reconciler is alive (so the next open is a peer) but its DB is
    // empty, as while it is still indexing.
    let mut db = IndexDb::open(&first.cache().unwrap()).unwrap();
    let gone: Vec<mdroots::index::Change> = db
        .rows()
        .unwrap()
        .into_iter()
        .map(|r| mdroots::index::Change::Remove(r.path))
        .collect();
    db.apply(&gone).unwrap();
    let peer = open_cached(&fs, &probe, cache.path());
    assert_eq!(peer.role(), Some(Role::Peer));
    assert_eq!(peer.files(), first.files());
    assert!(db.rows().unwrap().is_empty(), "a peer never writes");
}

#[test]
fn the_registry_keeps_the_decision_across_workspaces() {
    let cache = tempfile::tempdir().unwrap();
    let (fs, probe) = recorded_vault();
    let first = open_cached(&fs, &probe, cache.path());
    let db = first.cache();
    drop(first);
    fs.take_note_reads();
    let second = open_cached(&fs, &probe, cache.path());
    // A stage-1 hit names the same root and DB; the new reconciler re-lists
    // the root but reads no unchanged note.
    assert_eq!(second.role(), Some(Role::Reconciler));
    assert_eq!(second.cache(), db);
    assert_eq!(second.root().mode, RootMode::Marker);
    assert_eq!(fs.take_note_reads(), Vec::<PathBuf>::new());
    let reg = SqliteRegistry::open(&cache.path().join("roots.v1.db")).unwrap();
    assert_eq!(reg.all().len(), 1);
}

#[test]
fn refresh_shows_disk_changes_peer_without_writing_reconciler_writing() {
    let cache = tempfile::tempdir().unwrap();
    let (fs, probe) = recorded_vault();
    let rec = open_cached(&fs, &probe, cache.path());
    let peer = open_cached(&fs, &probe, cache.path());
    let watch = IndexDb::open(&rec.cache().unwrap()).unwrap();
    let v0 = watch.data_version().unwrap();
    let beta = VAULT[2].1;
    fs.inner.write("/v/b.md", b"# Beta two\n");
    fs.take_note_reads();

    peer.refresh(&Cancel::new()).unwrap();
    assert_eq!(text_of(&peer, "/v/b.md"), "# Beta two\n");
    assert_eq!(fs.take_note_reads(), [p("/v/b.md")]);
    assert_eq!(watch.data_version().unwrap(), v0, "a peer never writes");
    assert!(db_rows(&rec).contains(&row("b.md", beta)));
    // The reconciler has not refreshed yet.
    assert_eq!(text_of(&rec, "/v/b.md"), beta);

    rec.refresh(&Cancel::new()).unwrap();
    assert_eq!(text_of(&rec, "/v/b.md"), "# Beta two\n");
    assert_ne!(watch.data_version().unwrap(), v0);
    assert!(db_rows(&rec).contains(&row("b.md", "# Beta two\n")));
}

#[test]
fn refresh_adds_files_that_appear() {
    let cache = tempfile::tempdir().unwrap();
    let (fs, probe) = both(VAULT);
    // Discovery's probe sees new.md, the fs has it only later.
    let probe = probe.file("/v/new.md", "# New\n");
    let fs = Recording::new(fs);
    let o = opts(fs.clone(), Arc::new(probe)).cache_dir(cache.path().to_path_buf());
    let ws = Workspace::open_for(Path::new("/v/a.md"), o).unwrap();
    assert!(!ws.files().contains(&p("/v/new.md")));
    fs.inner.write("/v/new.md", b"# New\n");
    ws.refresh(&Cancel::new()).unwrap();
    assert!(ws.files().contains(&p("/v/new.md")));
    assert!(db_paths(&ws).contains(&"new.md".to_owned()));
}

#[test]
fn dropping_the_reconciler_lets_a_peer_promote_on_refresh() {
    let cache = tempfile::tempdir().unwrap();
    let (fs, probe) = recorded_vault();
    let rec = open_cached(&fs, &probe, cache.path());
    let peer = open_cached(&fs, &probe, cache.path());
    peer.refresh(&Cancel::new()).unwrap();
    assert_eq!(peer.role(), Some(Role::Peer), "the reconciler is alive");
    drop(rec);
    fs.inner.write("/v/b.md", b"# Promoted\n");
    peer.refresh(&Cancel::new()).unwrap();
    assert_eq!(peer.role(), Some(Role::Reconciler));
    assert!(db_rows(&peer).contains(&row("b.md", "# Promoted\n")));
}

#[test]
fn overlays_survive_refresh() {
    let cache = tempfile::tempdir().unwrap();
    let (fs, probe) = recorded_vault();
    let ws = open_cached(&fs, &probe, cache.path());
    ws.set_overlay(Path::new("/v/b.md"), "# Unsaved\n").unwrap();
    ws.set_overlay(Path::new("/v/draft.md"), "# Draft\n")
        .unwrap();
    fs.inner.write("/v/b.md", b"# On disk\n");
    ws.refresh(&Cancel::new()).unwrap();
    assert_eq!(text_of(&ws, "/v/b.md"), "# Unsaved\n");
    assert!(ws.files().contains(&p("/v/draft.md")));
    // Overlays are never written to the DB.
    assert!(db_rows(&ws).contains(&row("b.md", "# On disk\n")));
    assert!(!db_paths(&ws).contains(&"draft.md".to_owned()));
    ws.clear_overlay(Path::new("/v/b.md")).unwrap();
    assert_eq!(text_of(&ws, "/v/b.md"), "# On disk\n");
    ws.refresh(&Cancel::new()).unwrap();
    assert_eq!(text_of(&ws, "/v/b.md"), "# On disk\n");
}

#[test]
fn memory_refresh_rereads_from_disk() {
    let (fs, probe) = both(VAULT);
    let fs = Arc::new(fs);
    let ws = Workspace::open_for(Path::new("/v/a.md"), opts(fs.clone(), Arc::new(probe))).unwrap();
    // Auto with an explicit fs and no cache dir: memory.
    assert_eq!((ws.role(), ws.cache()), (None, None));
    fs.write("/v/b.md", b"# Fresh\n");
    ws.refresh(&Cancel::new()).unwrap();
    assert_eq!(text_of(&ws, "/v/b.md"), "# Fresh\n");
    assert_eq!(ws.files().len(), 5);
}

#[test]
fn memory_mode_creates_nothing_in_the_cache_dir() {
    let cache = tempfile::tempdir().unwrap();
    let (fs, probe) = recorded_vault();
    let o = cached(&fs, &probe, cache.path()).index(IndexMode::Memory);
    let ws = Workspace::open_for(Path::new("/v/a.md"), o).unwrap();
    ws.refresh(&Cancel::new()).unwrap();
    assert_eq!((ws.role(), ws.cache()), (None, None));
    assert_eq!(std::fs::read_dir(cache.path()).unwrap().count(), 0);
}

#[test]
fn lazy_root_uses_the_db_for_its_working_set() {
    let cache = tempfile::tempdir().unwrap();
    let (fs, probe) = eden_repo(&[]);
    let fs = Recording::new(fs);
    let probe = Arc::new(Counting::new(probe));
    let o = || opts(fs.clone(), probe.clone()).cache_dir(cache.path().to_path_buf());
    let ws = Workspace::open_for(Path::new("/eden/repo/docs/a.md"), o()).unwrap();
    assert_eq!(ws.root().mode, RootMode::Lazy);
    assert_eq!(ws.role(), Some(Role::Reconciler));
    let want = [
        "/eden/repo/docs/a.md",
        "/eden/repo/docs/b.md",
        "/eden/repo/docs/notes.org",
    ]
    .map(p);
    assert_eq!(ws.files(), want);
    let peer = Workspace::open_for(Path::new("/eden/repo/docs/b.md"), o()).unwrap();
    assert_eq!(peer.role(), Some(Role::Peer));
    assert_eq!(peer.files(), want);
    // A new file in the directory joins the working set on refresh.
    fs.inner.write("/eden/repo/docs/c.md", b"# C\n");
    ws.refresh(&Cancel::new()).unwrap();
    assert!(ws.files().contains(&p("/eden/repo/docs/c.md")));
    assert_eq!(
        db_paths(&ws),
        ["docs/a.md", "docs/b.md", "docs/c.md", "docs/notes.org"]
    );
}

#[test]
fn lazy_reconciler_keeps_rows_outside_its_working_set() {
    let cache = tempfile::tempdir().unwrap();
    let (fs, probe) = eden_repo(&[("/eden/repo/docs/sub/e.md", "# E\n")]);
    let fs = Recording::new(fs);
    let probe = probe.symlink("/eden/repo/docs/sub/.eden/root", "/eden/repo");
    let probe = Arc::new(Counting::new(probe));
    let o = || opts(fs.clone(), probe.clone()).cache_dir(cache.path().to_path_buf());
    let sub = Workspace::open_for(Path::new("/eden/repo/docs/sub/e.md"), o()).unwrap();
    assert_eq!(sub.root().path, p("/eden/repo"), "{}", sub.root().reason);
    assert_eq!(sub.role(), Some(Role::Reconciler));
    drop(sub);
    let ws = Workspace::open_for(Path::new("/eden/repo/docs/a.md"), o()).unwrap();
    assert_eq!(ws.role(), Some(Role::Reconciler));
    let rows = db_paths(&ws);
    assert!(rows.contains(&"docs/sub/e.md".to_owned()), "{rows:?}");
    assert!(rows.contains(&"docs/a.md".to_owned()), "{rows:?}");
}

#[test]
fn single_file_mode_with_a_cache_dir_has_no_db() {
    let cache = tempfile::tempdir().unwrap();
    let (fs, probe) = both(&[("/h/a.md", "[[b]]"), ("/h/b.md", "")]);
    let o = opts(Arc::new(fs), Arc::new(probe)).cache_dir(cache.path().to_path_buf());
    let ws = Workspace::open_for(Path::new("/h/a.md"), o).unwrap();
    assert_eq!(ws.root().mode, RootMode::SingleFile);
    assert_eq!((ws.role(), ws.cache()), (None, None));
    assert_eq!(ws.files(), [p("/h/a.md")]);
    assert!(!cache.path().join("roots").exists());
    ws.refresh(&Cancel::new()).unwrap();
    assert_eq!(ws.files(), [p("/h/a.md")]);
}

#[test]
fn workspaces_share_one_registry() {
    let cache = tempfile::tempdir().unwrap();
    let (fs, probe) = both(&[
        ("/v/.mdroots", ""),
        ("/v/a.md", ""),
        ("/w/.mdroots", ""),
        ("/w/b.md", ""),
    ]);
    let o = opts(Arc::new(fs), Arc::new(probe)).cache_dir(cache.path().to_path_buf());
    let wss = Workspaces::new(o);
    let a = wss.for_path(Path::new("/v/a.md")).unwrap();
    let b = wss.for_path(Path::new("/w/b.md")).unwrap();
    assert_eq!(a.role(), Some(Role::Reconciler));
    assert_eq!(b.role(), Some(Role::Reconciler));
    assert_ne!(a.cache(), b.cache());
    let reg = SqliteRegistry::open(&cache.path().join("roots.v1.db")).unwrap();
    let paths: Vec<PathBuf> = reg.all().into_iter().map(|r| r.path).collect();
    assert_eq!(paths, [p("/v"), p("/w")]);
}

#[test]
fn a_temp_dir_tree_is_cached_through_std_io() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap().join("vault");
    std::fs::create_dir_all(root.join(".zk")).unwrap();
    std::fs::write(root.join("a.md"), "[[b]]\n").unwrap();
    std::fs::write(root.join("b.md"), "# B\n").unwrap();
    std::fs::write(root.join("c.md"), "# C\n").unwrap();
    let cache = tmp.path().join("cache");
    let o = || std_opts().index(IndexMode::Auto).cache_dir(cache.clone());
    let ws = Workspace::open_for(&root.join("a.md"), o()).unwrap();
    assert_eq!(ws.role(), Some(Role::Reconciler));
    let peer = Workspace::open_for(&root.join("b.md"), o()).unwrap();
    assert_eq!(peer.role(), Some(Role::Peer));
    assert_eq!(peer.files(), ws.files());
    // A removed file leaves the reconciler's index and DB on refresh.
    std::fs::remove_file(root.join("c.md")).unwrap();
    ws.refresh(&Cancel::new()).unwrap();
    assert_eq!(ws.files(), [root.join("a.md"), root.join("b.md")]);
    assert_eq!(db_paths(&ws), ["a.md", "b.md"]);
    peer.refresh(&Cancel::new()).unwrap();
    assert_eq!(peer.files(), ws.files());
    // Lock files and DBs live in the cache dir only.
    let mut in_root: Vec<String> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    in_root.sort();
    assert_eq!(in_root, [".zk", "a.md", "b.md"]);
}
