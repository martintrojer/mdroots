//! Tests for the editor queries (outline, search, preview, text), rename and
//! [`Workspaces`]. Trees are in memory (`MemFs` with a `FakeProbe` holding
//! the same files) or temp dirs.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mdroots::{
    Cancel, ErrorKind, FileSystem, IndexMode, NoEnumerator, Options, StdFs, StdProbe, TextEdit,
    Workspace, Workspaces,
};
use mdroots_core::MemFs;
use mdroots_roots::probe::FakeProbe;

fn p(s: &str) -> PathBuf {
    PathBuf::from(s)
}

fn both(files: &[(&str, &str)]) -> (MemFs, FakeProbe) {
    let mut fs = MemFs::new();
    let mut probe = FakeProbe::new().home("/h");
    for (path, text) in files {
        fs = fs.with_file(path, text);
        probe = probe.file(path, text);
    }
    (fs, probe)
}

fn opts(fs: Arc<dyn FileSystem>, probe: Arc<dyn mdroots::Probe>) -> Options {
    Options::default()
        .fs(fs)
        .probe(probe)
        .enumerator(Arc::new(NoEnumerator))
}

fn open(files: &[(&str, &str)], at: &str) -> Workspace {
    let (fs, probe) = both(files);
    Workspace::open_for(Path::new(at), opts(Arc::new(fs), Arc::new(probe))).unwrap()
}

#[test]
fn outline_in_source_order() {
    let ws = open(
        &[
            ("/v/.mdroots", ""),
            ("/v/a.md", "# One\n\n## Two\n\ntext\n\n# Three\n"),
        ],
        "/v/a.md",
    );
    let h = ws.outline(Path::new("/v/a.md")).unwrap();
    let got: Vec<(u8, &str)> = h.iter().map(|h| (h.level, h.text.as_str())).collect();
    assert_eq!(got, [(1, "One"), (2, "Two"), (1, "Three")]);
    assert!(ws.outline(Path::new("/v/none.md")).unwrap().is_empty());
    ws.set_overlay(Path::new("/v/a.md"), "## Only\n").unwrap();
    assert_eq!(ws.outline(Path::new("/v/a.md")).unwrap()[0].text, "Only");
}

#[test]
fn search_ranks_exact_prefix_substring_subsequence_and_aliases() {
    let ws = open(
        &[
            ("/v/.mdroots", ""),
            ("/v/plan.md", ""),
            ("/v/planning.md", ""),
            ("/v/my-plan.md", ""),
            ("/v/p-l-a-n.md", ""),
            ("/v/zeta.md", "---\naliases: [Plan B]\n---\n"),
            ("/v/other.md", "# Unrelated\n"),
        ],
        "/v/plan.md",
    );
    let names = |q: &str, n: usize| -> Vec<PathBuf> {
        ws.search_notes(q, n).into_iter().map(|s| s.path).collect()
    };
    assert_eq!(
        names("PLAN", 10),
        [
            "/v/plan.md",     // exact stem
            "/v/planning.md", // prefix
            "/v/zeta.md",     // alias prefix "Plan B"
            "/v/my-plan.md",  // substring
            "/v/p-l-a-n.md",  // subsequence
        ]
        .map(p)
    );
    assert_eq!(names("plan", 2), ["/v/plan.md", "/v/planning.md"].map(p));
    // Fewer gaps first: one gap each, then p-l-a-n with two.
    assert_eq!(
        names("pln", 10),
        [
            "/v/my-plan.md",
            "/v/plan.md",
            "/v/planning.md",
            "/v/zeta.md",
            "/v/p-l-a-n.md"
        ]
        .map(p)
    );
    assert_eq!(names("unrel", 10), [p("/v/other.md")]);
    assert!(names("qqq", 10).is_empty());
    // Empty query: every note by path, truncated.
    assert_eq!(names("", 2), ["/v/my-plan.md", "/v/other.md"].map(p));
}

#[test]
fn preview_frontmatter_and_excerpt() {
    let ws = open(
        &[
            ("/v/.mdroots", ""),
            (
                "/v/a.md",
                "---\ntitle: Alpha\ntags: [x, y]\ndraft:\n---\n\n\n# Head\nline 2\nline 3\n",
            ),
            ("/v/b.md", "plain\n"),
        ],
        "/v/a.md",
    );
    let pv = ws.preview(Path::new("/v/a.md"), 2).unwrap();
    assert_eq!(pv.title, "Alpha");
    assert_eq!(
        pv.frontmatter,
        [
            ("title".to_owned(), "Alpha".to_owned()),
            ("tags".to_owned(), "x, y".to_owned()),
            ("draft".to_owned(), String::new()),
        ]
    );
    assert_eq!(pv.excerpt, "# Head\nline 2");
    let pb = ws.preview(Path::new("/v/b.md"), 5).unwrap();
    assert_eq!((pb.title.as_str(), pb.excerpt.as_str()), ("b", "plain"));
    assert!(pb.frontmatter.is_empty());
    let err = ws.preview(Path::new("/v/none.md"), 5).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Unsupported);
}

#[test]
fn text_is_current_and_overlay_wins() {
    let ws = open(&[("/v/.mdroots", ""), ("/v/a.md", "disk\n")], "/v/a.md");
    let a = Path::new("/v/a.md");
    assert_eq!(ws.text(a).unwrap(), "disk\n");
    ws.set_overlay(a, "edited").unwrap();
    assert_eq!(ws.text(a).unwrap(), "edited");
    ws.clear_overlay(a).unwrap();
    assert_eq!(ws.text(a).unwrap(), "disk\n");
    let err = ws.text(Path::new("/v/none.md")).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Unsupported);
}

/// Apply edits (sorted, non-overlapping) to `text`.
fn apply(text: &str, edits: &[TextEdit]) -> String {
    let mut out = text.to_owned();
    for e in edits.iter().rev() {
        out.replace_range(e.range.clone(), &e.new_text);
    }
    out
}

/// The new text of every edited file.
fn rename(ws: &Workspace, old: &str, new: &str) -> Vec<(PathBuf, String)> {
    let we = ws
        .rename_note(Path::new(old), Path::new(new), &Cancel::new())
        .unwrap();
    assert_eq!(we.rename, Some((p(old), p(new))));
    we.edits
        .iter()
        .map(|(f, e)| {
            let w: Vec<_> = e
                .windows(2)
                .map(|w| w[0].range.end <= w[1].range.start)
                .collect();
            assert!(w.iter().all(|ok| *ok), "{f:?}: unsorted or overlapping");
            (f.clone(), apply(&ws.text(f).unwrap(), e))
        })
        .collect()
}

const RENAME_VAULT: &[(&str, &str)] = &[
    ("/v/.mdroots", ""),
    (
        "/v/notes/old.md",
        "# Old\n\nSee [sib](sib.md) and [up](../top.md).\n",
    ),
    (
        "/v/notes/sib.md",
        "Next: [o](old.md#intro) and [o2](./old).\n",
    ),
    (
        "/v/top.md",
        "[o](notes/old.md) [[old]] [[old#sec|label]] [[notes/old]] [[notes/old.md]]\n\
         `[[old]]` [x][r]\n\n```\n[o](notes/old.md)\n```\n\n[r]: notes/old.md\n",
    ),
    ("/v/deep/x/y.md", "[o](../../notes/old.md) and [[ol]]\n"),
    ("/v/sib.md", ""),
];

#[test]
fn rename_rewrites_links_in_their_written_style() {
    let ws = open(RENAME_VAULT, "/v/top.md");
    let got = rename(&ws, "/v/notes/old.md", "/v/archive/new.md");
    let want = [
        (
            "/v/deep/x/y.md",
            // [[ol]] is a partial hint only: never rewritten.
            "[o](../../archive/new.md) and [[ol]]\n",
        ),
        (
            "/v/notes/old.md",
            // Its own file-relative links follow it to archive/.
            "# Old\n\nSee [sib](../notes/sib.md) and [up](../top.md).\n",
        ),
        (
            "/v/notes/sib.md",
            "Next: [o](../archive/new.md#intro) and [o2](../archive/new).\n",
        ),
        (
            "/v/top.md",
            // Code links stay; the reference usage stays, its definition moves.
            "[o](archive/new.md) [[new]] [[new#sec|label]] [[archive/new]] [[archive/new.md]]\n\
             `[[old]]` [x][r]\n\n```\n[o](notes/old.md)\n```\n\n[r]: archive/new.md\n",
        ),
    ];
    let want: Vec<(PathBuf, String)> = want.iter().map(|(f, t)| (p(f), t.to_string())).collect();
    assert_eq!(got, want);
}

#[test]
fn rename_in_place_and_percent_encoding() {
    let ws = open(
        &[
            ("/v/.mdroots", ""),
            ("/v/a.md", "[b](b.md) [s](<b.md>) [[b]] [[b|B]]\n"),
            ("/v/b.md", "[a](a.md)\n"),
        ],
        "/v/a.md",
    );
    let got = rename(&ws, "/v/b.md", "/v/new name.md");
    assert_eq!(
        got,
        [(
            p("/v/a.md"),
            "[b](new%20name.md) [s](<new name.md>) [[new name]] [[new name|B]]\n".to_owned()
        )]
    );
}

#[test]
fn rename_refusals() {
    let ws = open(RENAME_VAULT, "/v/top.md");
    let c = Cancel::new();
    let refuse = |old: &str, new: &str| {
        ws.rename_note(Path::new(old), Path::new(new), &c)
            .unwrap_err()
            .kind()
    };
    // Exists on disk / indexed.
    assert_eq!(
        refuse("/v/notes/old.md", "/v/top.md"),
        ErrorKind::Unsupported
    );
    // Overlay-only note.
    ws.set_overlay(Path::new("/v/fresh.md"), "x").unwrap();
    assert_eq!(
        refuse("/v/notes/old.md", "/v/fresh.md"),
        ErrorKind::Unsupported
    );
    // Not a note extension, not indexed, outside the root.
    assert_eq!(
        refuse("/v/notes/old.md", "/v/new.txt"),
        ErrorKind::Unsupported
    );
    assert_eq!(refuse("/v/none.md", "/v/new.md"), ErrorKind::Unsupported);
    assert_eq!(
        refuse("/v/notes/old.md", "/elsewhere/new.md"),
        ErrorKind::Unsupported
    );
    let cancelled = Cancel::new();
    cancelled.cancel();
    let err = ws
        .rename_note(
            Path::new("/v/notes/old.md"),
            Path::new("/v/n.md"),
            &cancelled,
        )
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Cancelled);
}

/// Temp-dir trees through the non-canonical path the OS hands out (on
/// macOS `/var/…`, a symlink to `/private/var/…`).
fn std_opts() -> Options {
    Options::default()
        .fs(Arc::new(StdFs))
        .probe(Arc::new(StdProbe))
        .enumerator(Arc::new(NoEnumerator))
        .index(IndexMode::Memory)
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[test]
fn workspaces_share_one_root_and_split_nested_roots() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("nb");
    write(&base.join(".mdroots"), "");
    write(&base.join("a.md"), "a\n");
    write(&base.join("sub/b.md"), "b\n");
    write(&base.join("proj/.mdroots"), "");
    write(&base.join("proj/c.md"), "c\n");
    let wss = Workspaces::new(std_opts());
    let w1 = wss.for_path(&base.join("a.md")).unwrap();
    let w2 = wss.for_path(&base.join("sub/../sub/b.md")).unwrap();
    let canon = base.canonicalize().unwrap();
    assert_eq!(w1.root().path, canon);
    assert_eq!(w1.root().nested_roots, [canon.join("proj")]);
    // One workspace: an overlay set through one handle shows in the other.
    w1.set_overlay(&canon.join("sub/b.md"), "edited").unwrap();
    assert_eq!(w2.text(&base.join("sub/b.md")).unwrap(), "edited");
    // The nested root is its own workspace.
    let w3 = wss.for_path(&base.join("proj/c.md")).unwrap();
    assert_eq!(w3.root().path, canon.join("proj"));
    w3.set_overlay(&canon.join("proj/c.md"), "nested").unwrap();
    let w4 = wss.for_path(&base.join("proj/c.md")).unwrap();
    assert_eq!(w4.text(&canon.join("proj/c.md")).unwrap(), "nested");
    let roots: Vec<PathBuf> = wss.all().iter().map(|w| w.root().path).collect();
    assert_eq!(roots, [canon.clone(), canon.join("proj")]);
}

#[test]
fn workspaces_single_file_serves_only_its_file() {
    // Files directly in the home directory: single-file mode.
    let (fs, probe) = both(&[("/h/a.md", "a"), ("/h/b.md", "b")]);
    let wss = Workspaces::new(opts(Arc::new(fs), Arc::new(probe)));
    let wa = wss.for_path(Path::new("/h/a.md")).unwrap();
    assert_eq!(wa.root().mode, mdroots::RootMode::SingleFile);
    let wb = wss.for_path(Path::new("/h/b.md")).unwrap();
    assert_eq!(wb.files(), [p("/h/b.md")]);
    wa.set_overlay(Path::new("/h/a.md"), "edited").unwrap();
    let wa2 = wss.for_path(Path::new("/h/a.md")).unwrap();
    assert_eq!(wa2.text(Path::new("/h/a.md")).unwrap(), "edited");
    assert!(wb.text(Path::new("/h/a.md")).is_err());
    assert_eq!(wss.all().len(), 2);
}

#[test]
fn frontmatter_range_and_anchor_backlinks() {
    let ws = open(
        &[
            ("/v/.mdroots", ""),
            (
                "/v/t.md",
                "---\ntitle: T\n---\n# T\n\n## Set Up\n\n## Other {#oth}\n\n## Lonely\n\nSee [[#Lonely]].\n",
            ),
            (
                "/v/a.md",
                "[[t#Set Up]] [x](t.md#set-up) [[t#oth]] [[t#Set%20Up]] [[t]] [[t#nope]]\n",
            ),
        ],
        "/v/t.md",
    );
    let t = Path::new("/v/t.md");
    assert_eq!(ws.frontmatter_range(t).unwrap(), Some(0..16));
    assert_eq!(ws.frontmatter_range(Path::new("/v/a.md")).unwrap(), None);
    // Headings: 0 "T", 1 "Set Up", 2 "Other", 3 "Lonely" (self-links only).
    assert_eq!(ws.anchor_backlinks(t).unwrap(), [(1, 3), (2, 1)]);
    let b = ws.heading_backlinks(t, 2).unwrap();
    assert_eq!(b.len(), 1);
    assert_eq!(b[0].from, p("/v/a.md"));
    assert!(ws.heading_backlinks(t, 3).unwrap().is_empty());
}
