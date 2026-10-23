//! Tests for the editor queries (outline, search, preview, text), rename and
//! [`Workspaces`]. Trees are in memory (`MemFs` with a `FakeProbe` holding
//! the same files) or temp dirs.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

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
    let got: Vec<(u8, &str)> = h.iter().map(|h| (h.level, &*h.text)).collect();
    assert_eq!(got, [(1, "One"), (2, "Two"), (1, "Three")]);
    assert!(ws.outline(Path::new("/v/none.md")).unwrap().is_empty());
    ws.set_overlay(Path::new("/v/a.md"), "## Only\n").unwrap();
    assert_eq!(&*ws.outline(Path::new("/v/a.md")).unwrap()[0].text, "Only");
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

/// The shared path span (fixes, rename) reads an HTML link's `href` value,
/// not the first place its text appears, and an Org link in a Markdown
/// note with its `file:` prefix outside the path.
#[test]
fn rename_edits_the_href_and_an_org_link_in_markdown() {
    let a = "<a title='b.org' href='b.org'>x</a> [[file:b.org][M]]\n";
    let ws = open(
        &[("/v/.mdroots", ""), ("/v/a.md", a), ("/v/b.org", "* B\n")],
        "/v/a.md",
    );
    let got = rename(&ws, "/v/b.org", "/v/d/b.org");
    assert_eq!(
        got,
        [(
            p("/v/a.md"),
            "<a title='b.org' href='d/b.org'>x</a> [[file:d/b.org][M]]\n".to_owned()
        )]
    );
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
fn rename_encodes_characters_that_end_or_alter_a_destination() {
    let a = "[b](b.md) [s](<b.md>) [x](b.md#sec)\n";
    let names = [
        "a)b.md",
        "a#b.md",
        "a(b.md",
        "a b.md",
        "a<b>.md",
        "100%.md",
        "a&copy;.md",
    ];
    // `&copy;` is a character reference for this note's name.
    let decoy = ("/v/move/a©.md", "");
    for new in names {
        let ws = open(
            &[("/v/.mdroots", ""), ("/v/a.md", a), ("/v/b.md", ""), decoy],
            "/v/a.md",
        );
        let new_abs = format!("/v/move/{new}");
        let got = rename(&ws, "/v/b.md", &new_abs);
        assert_eq!(got.len(), 1, "{new}: {got:?}");
        let text = &got[0].1;
        // The renamed tree: every link in a.md still reaches the note.
        let after = open(
            &[
                ("/v/.mdroots", ""),
                ("/v/a.md", text),
                (&new_abs, ""),
                decoy,
            ],
            "/v/a.md",
        );
        let back = after.backlinks(Path::new(&new_abs)).unwrap();
        assert_eq!(back.len(), 3, "{new}: {text}");
    }
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

fn mv(pairs: &[(&str, &str)]) -> Vec<(PathBuf, PathBuf)> {
    pairs.iter().map(|(a, b)| (p(a), p(b))).collect()
}

/// The new text of every file `rename_notes` edits.
fn rename_many(ws: &Workspace, pairs: &[(&str, &str)]) -> Vec<(PathBuf, String)> {
    let we = ws.rename_notes(&mv(pairs), &Cancel::new()).unwrap();
    assert_eq!(we.rename, None);
    assert!(we.create.is_empty());
    we.edits
        .iter()
        .map(|(f, e)| (f.clone(), apply(&ws.text(f).unwrap(), e)))
        .collect()
}

#[test]
fn rename_notes_keeps_links_between_notes_that_move_together() {
    let files = [
        ("/v/.mdroots", ""),
        ("/v/a.md", "[b](b.md)\n"),
        ("/v/b.md", "[a](a.md)\n"),
    ];
    let ws = open(&files, "/v/a.md");
    let both = [("/v/a.md", "/v/new/a.md"), ("/v/b.md", "/v/new/b.md")];
    assert!(rename_many(&ws, &both).is_empty());
    // Separate calls plan each move against the tree as it is.
    let got = rename(&ws, "/v/a.md", "/v/new/a.md");
    assert_eq!(
        got,
        [
            (p("/v/a.md"), "[b](../b.md)\n".to_owned()),
            (p("/v/b.md"), "[a](new/a.md)\n".to_owned()),
        ]
    );
}

#[test]
fn rename_notes_expands_folders() {
    let ws = open(
        &[
            ("/v/.mdroots", ""),
            ("/v/notes/a.md", "[c](../c.md) [s](sub/s.md)\n"),
            ("/v/notes/sub/s.md", "[a](../a.md)\n"),
            ("/v/c.md", "[x](notes/a.md) [[a]]\n"),
        ],
        "/v/c.md",
    );
    // Only the outside note changes: the moved notes keep their relative
    // links to each other and to `c.md`; `[[a]]` is by stem.
    let got = rename_many(&ws, &[("/v/notes", "/v/archive")]);
    assert_eq!(
        got,
        [(p("/v/c.md"), "[x](archive/a.md) [[a]]\n".to_owned())]
    );
    // Moved one level deeper, `../c.md` follows.
    let got = rename_many(&ws, &[("/v/notes", "/v/x/archive")]);
    assert_eq!(got[1].1, "[c](../../c.md) [s](sub/s.md)\n");
}

#[test]
fn rename_notes_skips_unknown_pairs_and_refuses_double_moves() {
    let ws = open(
        &[
            ("/v/.mdroots", ""),
            ("/v/a.md", "[b](b.md)\n"),
            ("/v/b.md", ""),
        ],
        "/v/a.md",
    );
    assert!(rename_many(&ws, &[("/v/none.md", "/v/x.md")]).is_empty());
    assert!(rename_many(&ws, &[("/v/b.md", "/elsewhere/b.md")]).is_empty());
    let err = ws
        .rename_notes(
            &mv(&[("/v/b.md", "/v/x.md"), ("/v/b.md", "/v/y.md")]),
            &Cancel::new(),
        )
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    // A folder and a file inside it: the note is mapped twice.
    let ws = open(
        &[("/v/.mdroots", ""), ("/v/d/a.md", ""), ("/v/b.md", "")],
        "/v/b.md",
    );
    let err = ws
        .rename_notes(
            &mv(&[("/v/d", "/v/e"), ("/v/d/a.md", "/v/f.md")]),
            &Cancel::new(),
        )
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Unsupported);
}

#[test]
fn rename_notes_one_pair_equals_rename_note() {
    let ws = open(RENAME_VAULT, "/v/top.md");
    let one = ws
        .rename_note(
            Path::new("/v/notes/old.md"),
            Path::new("/v/archive/new.md"),
            &Cancel::new(),
        )
        .unwrap();
    let many = ws
        .rename_notes(
            &mv(&[("/v/notes/old.md", "/v/archive/new.md")]),
            &Cancel::new(),
        )
        .unwrap();
    assert_eq!(many.rename, None);
    assert_eq!(many.edits, one.edits);
}

#[test]
fn rename_does_not_write_a_stem_another_note_shares_after_the_move() {
    let top = ("/v/top.md", "[[b]]\n");
    // Single rename: a/new.md exists, so `[[new]]` would find it.
    let ws = open(
        &[
            ("/v/.mdroots", ""),
            top,
            ("/v/old/b.md", ""),
            ("/v/a/new.md", ""),
        ],
        "/v/top.md",
    );
    let got = rename(&ws, "/v/old/b.md", "/v/z/new.md");
    assert_eq!(got, [(p("/v/top.md"), "[[z/new]]\n".to_owned())]);
    // Batch: another moved note takes the same stem.
    let ws = open(
        &[
            ("/v/.mdroots", ""),
            top,
            ("/v/old/b.md", ""),
            ("/v/else/c.md", ""),
        ],
        "/v/top.md",
    );
    let got = rename_many(
        &ws,
        &[
            ("/v/old/b.md", "/v/z/new.md"),
            ("/v/else/c.md", "/v/a/new.md"),
        ],
    );
    assert_eq!(got, [(p("/v/top.md"), "[[z/new]]\n".to_owned())]);
}

#[test]
fn rename_note_takes_the_store_lock_once() {
    use std::io;
    use std::sync::mpsc;
    use std::time::Duration;

    /// Pauses `stat` of one path until told to go on.
    struct Pausing {
        inner: MemFs,
        at: PathBuf,
        paused: Mutex<mpsc::Sender<()>>,
        go: Mutex<mpsc::Receiver<()>>,
    }
    impl FileSystem for Pausing {
        fn read(&self, p: &Path) -> io::Result<(Arc<[u8]>, mdroots_core::Meta)> {
            self.inner.read(p)
        }
        fn stat(&self, p: &Path) -> io::Result<mdroots_core::Meta> {
            if p == self.at {
                self.paused.lock().unwrap().send(()).unwrap();
                self.go.lock().unwrap().recv().unwrap();
            }
            self.inner.stat(p)
        }
        fn read_dir(&self, p: &Path) -> io::Result<Vec<(String, mdroots_core::Meta)>> {
            self.inner.read_dir(p)
        }
        fn canonicalize(&self, p: &Path) -> io::Result<PathBuf> {
            self.inner.canonicalize(p)
        }
        fn case_sensitive(&self, d: &Path) -> bool {
            self.inner.case_sensitive(d)
        }
        fn fs_kind(&self, d: &Path) -> mdroots_core::FsKind {
            self.inner.fs_kind(d)
        }
    }

    let files = [
        ("/v/.mdroots", ""),
        ("/v/a.md", "[b](b.md)\n"),
        ("/v/b.md", ""),
    ];
    let (fs, probe) = both(&files);
    let (paused_tx, paused_rx) = mpsc::channel();
    let (go_tx, go_rx) = mpsc::channel();
    let fs = Arc::new(Pausing {
        inner: fs,
        at: p("/v/new.md"),
        paused: Mutex::new(paused_tx),
        go: Mutex::new(go_rx),
    });
    let ws =
        Arc::new(Workspace::open_for(Path::new("/v/a.md"), opts(fs, Arc::new(probe))).unwrap());
    let (done_tx, done_rx) = mpsc::channel();
    let w = ws.clone();
    let tx = done_tx.clone();
    std::thread::spawn(move || {
        let r = w.rename_note(Path::new("/v/b.md"), Path::new("/v/new.md"), &Cancel::new());
        tx.send(("rename", r.is_ok())).unwrap();
    });
    paused_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    // A writer queues behind the rename's read of the store.
    let w = ws.clone();
    std::thread::spawn(move || {
        let r = w.set_overlay(Path::new("/v/a.md"), "changed");
        done_tx.send(("overlay", r.is_ok())).unwrap();
    });
    std::thread::sleep(Duration::from_millis(200));
    go_tx.send(()).unwrap();
    for _ in 0..2 {
        let (what, ok) = done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("rename_note and set_overlay both return");
        assert!(ok, "{what}");
    }
}

#[test]
fn rename_writes_a_path_that_lands_on_the_moved_note() {
    // The root-relative path `z/new` would be found file-relatively at
    // near/z/new.md: use the `/`-prefixed form.
    let ws = open(
        &[
            ("/v/.mdroots", ""),
            ("/v/near/top.md", "[[b]]\n"),
            ("/v/old/b.md", ""),
            ("/v/else/c.md", ""),
            ("/v/near/z/new.md", ""),
        ],
        "/v/near/top.md",
    );
    let got = rename_many(
        &ws,
        &[
            ("/v/old/b.md", "/v/z/new.md"),
            ("/v/else/c.md", "/v/a/new.md"),
        ],
    );
    let text = &got[0].1;
    assert_eq!(text, "[[/z/new]]\n");
    // Single rename: same.
    let got = rename(&ws, "/v/old/b.md", "/v/z/new.md");
    assert_eq!(got[0].1, "[[/z/new]]\n");
    // Reopened, every link in top.md reaches the moved note.
    let after = open(
        &[
            ("/v/.mdroots", ""),
            ("/v/near/top.md", text),
            ("/v/z/new.md", ""),
            ("/v/else/c.md", ""),
            ("/v/near/z/new.md", ""),
        ],
        "/v/near/top.md",
    );
    let r = after
        .resolve(Path::new("/v/near/top.md"), "[[/z/new]]")
        .unwrap();
    assert_eq!(r.targets.first(), Some(&p("/v/z/new.md")), "{r:?}");
}

#[test]
fn rename_keeps_a_shadowed_root_relative_link_landing() {
    // `old/b.md` moves to `z/new.md`; near/z/new.md would shadow the
    // root-relative `z/new.md` seen from near/top.md.
    let ws = open(
        &[
            ("/v/.mdroots", ""),
            ("/v/near/top.md", "[y](old/b.md)\n"),
            ("/v/old/b.md", ""),
            ("/v/near/z/new.md", ""),
        ],
        "/v/near/top.md",
    );
    let got = rename(&ws, "/v/old/b.md", "/v/z/new.md");
    assert_eq!(got[0].1, "[y](/z/new.md)\n");
    let ws = open(
        &[
            ("/v/.mdroots", ""),
            ("/v/near/top.md", "[y](../old/b.md)\n"),
            ("/v/old/b.md", ""),
            ("/v/near/z/new.md", ""),
        ],
        "/v/near/top.md",
    );
    let got = rename(&ws, "/v/old/b.md", "/v/z/new.md");
    assert_eq!(got[0].1, "[y](../z/new.md)\n");
}

#[test]
fn rename_writes_a_path_for_a_stem_shared_before_the_move() {
    let ws = open(
        &[
            ("/v/.mdroots", ""),
            ("/v/a/top.md", "[[b]]\n"),
            ("/v/a/sub/b.md", ""),
            ("/v/else/b.md", ""),
        ],
        "/v/a/top.md",
    );
    for got in [
        rename(&ws, "/v/a/sub/b.md", "/v/z/deep/b.md"),
        rename_many(&ws, &[("/v/a/sub/b.md", "/v/z/deep/b.md")]),
    ] {
        assert_eq!(got, [(p("/v/a/top.md"), "[[z/deep/b]]\n".to_owned())]);
    }
}

/// `files` with each `(old, new)` moved and every edit applied: the tree
/// after the rename, opened fresh.
fn after_moves(
    files: &[(&str, &str)],
    moves: &[(&str, &str)],
    got: &[(PathBuf, String)],
    at: &str,
) -> Workspace {
    let tree: Vec<(String, String)> = files
        .iter()
        .map(|(f, text)| {
            let text = got
                .iter()
                .find(|(g, _)| g == Path::new(f))
                .map_or(text.to_string(), |(_, t)| t.clone());
            let f = moves.iter().find(|(o, _)| o == f).map_or(*f, |(_, n)| *n);
            (f.to_owned(), text)
        })
        .collect();
    let tree: Vec<(&str, &str)> = tree.iter().map(|(f, t)| (&**f, &**t)).collect();
    open(&tree, at)
}

/// The first target of `link` written in `from`, in `ws`.
fn lands_on(ws: &Workspace, from: &str, link: &str) -> Option<PathBuf> {
    let r = ws.resolve(Path::new(from), link).unwrap();
    assert_ne!(r.status, mdroots::LinkStatus::Ambiguous, "{link}: {r:?}");
    r.targets.first().cloned()
}

#[test]
fn rename_fallback_is_not_shadowed_by_a_site_path() {
    // `/z/new` is site.md's permalink: the rooted form finds site.md, and
    // `z/new` finds near/z/new.md from near/; only `../z/new` lands.
    let files = [
        ("/v/.mdroots", ""),
        ("/v/near/top.md", "[[b]]\n"),
        ("/v/old/b.md", ""),
        ("/v/else/c.md", ""),
        ("/v/near/z/new.md", ""),
        ("/v/site.md", "---\npermalink: /z/new\n---\n"),
    ];
    let moves = [
        ("/v/old/b.md", "/v/z/new.md"),
        ("/v/else/c.md", "/v/a/new.md"),
    ];
    let ws = open(&files, "/v/near/top.md");
    let got = rename_many(&ws, &moves);
    assert_eq!(got, [(p("/v/near/top.md"), "[[../z/new]]\n".to_owned())]);
    let after = after_moves(&files, &moves, &got, "/v/near/top.md");
    assert_eq!(
        lands_on(&after, "/v/near/top.md", "[[../z/new]]"),
        Some(p("/v/z/new.md"))
    );
    // Single move of a `/`-rooted link: `/z/new.md` would be the site path.
    let files = [
        ("/v/.mdroots", ""),
        ("/v/near/top.md", "[y](/old/b.md)\n"),
        ("/v/old/b.md", ""),
        ("/v/site.md", "---\npermalink: /z/new\n---\n"),
    ];
    let ws = open(&files, "/v/near/top.md");
    let got = rename(&ws, "/v/old/b.md", "/v/z/new.md");
    assert_eq!(
        got,
        [(p("/v/near/top.md"), "[y](../z/new.md)\n".to_owned())]
    );
    let moves = [("/v/old/b.md", "/v/z/new.md")];
    let after = after_moves(&files, &moves, &got, "/v/near/top.md");
    assert_eq!(
        lands_on(&after, "/v/near/top.md", "[y](../z/new.md)"),
        Some(p("/v/z/new.md"))
    );
}

#[test]
fn rename_extensionless_link_is_not_shadowed_by_a_directory() {
    // An extensionless Markdown link finds an existing file or directory
    // first: from near/, `z/new` is the directory near/z/new.
    let files = [
        ("/v/.mdroots", ""),
        ("/v/near/top.md", "[b](old/b)\n"),
        ("/v/old/b.md", ""),
        ("/v/near/z/new/other.md", ""),
    ];
    let ws = open(&files, "/v/near/top.md");
    let got = rename(&ws, "/v/old/b.md", "/v/z/new.md");
    assert_eq!(got, [(p("/v/near/top.md"), "[b](/z/new)\n".to_owned())]);
    let moves = [("/v/old/b.md", "/v/z/new.md")];
    let after = after_moves(&files, &moves, &got, "/v/near/top.md");
    assert_eq!(
        lands_on(&after, "/v/near/top.md", "[b](/z/new)"),
        Some(p("/v/z/new.md"))
    );
}

#[test]
fn rename_stem_test_uses_normalized_keys() {
    // An NFD twin of the NFC name shares its stem key.
    let files = [
        ("/v/.mdroots", ""),
        ("/v/top.md", "[[b]]\n"),
        ("/v/old/b.md", ""),
        ("/v/near/cafe\u{301}.md", ""),
    ];
    let ws = open(&files, "/v/top.md");
    let got = rename(&ws, "/v/old/b.md", "/v/z/caf\u{e9}.md");
    assert_eq!(got, [(p("/v/top.md"), "[[z/caf\u{e9}]]\n".to_owned())]);
    let moves = [("/v/old/b.md", "/v/z/caf\u{e9}.md")];
    let after = after_moves(&files, &moves, &got, "/v/top.md");
    assert_eq!(
        lands_on(&after, "/v/top.md", "[[z/caf\u{e9}]]"),
        Some(p("/v/z/caf\u{e9}.md"))
    );
}

#[test]
fn rename_percent_named_notes_use_decoded_keys() {
    // A `%`-named decoy decodes to the new name.
    let files = [
        ("/v/.mdroots", ""),
        ("/v/near/top.md", "[[b]]\n"),
        ("/v/old/b.md", ""),
        ("/v/near/n%65w.md", ""),
    ];
    let ws = open(&files, "/v/near/top.md");
    let got = rename(&ws, "/v/old/b.md", "/v/z/new.md");
    assert_eq!(got, [(p("/v/near/top.md"), "[[z/new]]\n".to_owned())]);
    let moves = [("/v/old/b.md", "/v/z/new.md")];
    let after = after_moves(&files, &moves, &got, "/v/near/top.md");
    assert_eq!(
        lands_on(&after, "/v/near/top.md", "[[z/new]]"),
        Some(p("/v/z/new.md"))
    );
    // A `%`-named destination is still found by its decoded stem.
    let files = [
        ("/v/.mdroots", ""),
        ("/v/near/top.md", "[[b]]\n"),
        ("/v/old/b.md", ""),
    ];
    let ws = open(&files, "/v/near/top.md");
    let got = rename(&ws, "/v/old/b.md", "/v/z/n%65w.md");
    assert_eq!(got, [(p("/v/near/top.md"), "[[n%65w]]\n".to_owned())]);
    let moves = [("/v/old/b.md", "/v/z/n%65w.md")];
    let after = after_moves(&files, &moves, &got, "/v/near/top.md");
    assert_eq!(
        lands_on(&after, "/v/near/top.md", "[[n%65w]]"),
        Some(p("/v/z/n%65w.md"))
    );
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

#[test]
fn open_single_indexes_the_file_alone_without_discovery_or_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("nb");
    let cache = tmp.path().join("cache");
    write(&base.join(".mdroots"), "");
    write(
        &base.join("a.md"),
        "[[nowhere]] and [gone](gone.md) and [b](b.md)\n",
    );
    write(&base.join("b.md"), "b\n");
    let opts = std_opts().index(IndexMode::Auto).cache_dir(cache.clone());
    let ws = Workspace::open_single(&base.join("a.md"), opts.clone()).unwrap();
    let canon = base.canonicalize().unwrap();
    assert_eq!(ws.root().path, canon);
    assert_eq!(ws.root().mode, mdroots::RootMode::SingleFile);
    assert_eq!(ws.freshness(), mdroots::Freshness::Lazy);
    assert_eq!(ws.files(), [canon.join("a.md")]);
    // Lazy rules: the wiki link is only "not in the working set"; the
    // markdown links are checked with a stat.
    let codes: Vec<_> = ws
        .diagnostics(&canon.join("a.md"), &Cancel::new())
        .unwrap()
        .iter()
        .map(|d| d.code)
        .collect();
    assert_eq!(
        codes,
        [
            mdroots::DiagCode::NotInWorkingSet,
            mdroots::DiagCode::BrokenLink
        ]
    );
    // No discovery, no cache dir, no DB.
    assert!(!cache.exists());
    // Nothing cached: Workspaces::get stays empty until for_path opens it.
    let wss = Workspaces::new(std_opts());
    assert!(wss.get(&base.join("a.md")).is_none());
    let real = wss.for_path(&base.join("a.md")).unwrap();
    assert_eq!(real.root().mode, mdroots::RootMode::Marker);
    let got = wss
        .get(&base.join("b.md"))
        .expect("the marker root serves b.md");
    assert_eq!(got.root().path, canon);
}

#[test]
fn rename_rejects_a_candidate_the_link_syntax_would_split() {
    // In a wiki link `#` starts an anchor, `|` a label and `]]` ends the
    // link: no form of these names can be written, so no edit (the old
    // link breaks rather than finding a.md).
    for name in ["a#b", "a|b", "a]]b"] {
        let files = [
            ("/v/.mdroots", ""),
            ("/v/top.md", "[[b]]\n"),
            ("/v/old/b.md", ""),
            ("/v/a.md", ""),
        ];
        let ws = open(&files, "/v/top.md");
        let got = rename_many(&ws, &[("/v/old/b.md", &format!("/v/z/{name}.md"))]);
        assert!(got.is_empty(), "{name}: {got:?}");
    }
    // In an Org link `::` starts a search.
    let files = [
        ("/v/.mdroots", ""),
        ("/v/near/top.org", "[[file:../old/b.md][B]]\n"),
        ("/v/old/b.md", ""),
    ];
    let ws = open(&files, "/v/near/top.org");
    let got = rename_many(&ws, &[("/v/old/b.md", "/v/z/a::b.md")]);
    assert!(got.is_empty(), "{got:?}");
    // Control: a Markdown destination encodes `#` and lands.
    let files = [
        ("/v/.mdroots", ""),
        ("/v/top.md", "[x](old/b.md#h)\n"),
        ("/v/old/b.md", ""),
        ("/v/a.md", ""),
    ];
    let ws = open(&files, "/v/top.md");
    let got = rename(&ws, "/v/old/b.md", "/v/z/a#b.md");
    assert_eq!(got, [(p("/v/top.md"), "[x](z/a%23b.md#h)\n".to_owned())]);
}

#[test]
fn rename_moved_folder_does_not_shadow_destination() {
    // z/new/ is renamed to archive/ in the same batch: the directory z/new
    // is gone, so the extensionless `z/new` reaches z/new.md.
    let ws = open(
        &[
            ("/v/.mdroots", ""),
            ("/v/top.md", "[b](old/b)\n"),
            ("/v/old/b.md", ""),
            ("/v/z/new/other.md", ""),
        ],
        "/v/top.md",
    );
    let got = rename_many(
        &ws,
        &[("/v/old/b.md", "/v/z/new.md"), ("/v/z/new", "/v/archive")],
    );
    assert_eq!(got, [(p("/v/top.md"), "[b](z/new)\n".to_owned())]);
    let after = open(
        &[
            ("/v/.mdroots", ""),
            ("/v/top.md", "[b](z/new)\n"),
            ("/v/z/new.md", ""),
            ("/v/archive/other.md", ""),
        ],
        "/v/top.md",
    );
    assert_eq!(
        lands_on(&after, "/v/top.md", "[b](z/new)"),
        Some(p("/v/z/new.md"))
    );
}

#[test]
fn rename_folder_moves_its_other_files_too() {
    // assets/ (a note and a non-note subtree) becomes near/z/: near/z/new
    // is then a directory, which an extensionless `z/new` from near/ finds.
    let files = [
        ("/v/.mdroots", ""),
        ("/v/near/top.md", "[b](old/b)\n"),
        ("/v/old/b.md", ""),
        ("/v/assets/readme.md", ""),
        ("/v/assets/new/pic.png", ""),
    ];
    let ws = open(&files, "/v/near/top.md");
    let got = rename_many(
        &ws,
        &[("/v/old/b.md", "/v/z/new.md"), ("/v/assets", "/v/near/z")],
    );
    assert_eq!(got, [(p("/v/near/top.md"), "[b](/z/new)\n".to_owned())]);
    let after = open(
        &[
            ("/v/.mdroots", ""),
            ("/v/near/top.md", "[b](/z/new)\n"),
            ("/v/z/new.md", ""),
            ("/v/near/z/readme.md", ""),
            ("/v/near/z/new/pic.png", ""),
        ],
        "/v/near/top.md",
    );
    assert_eq!(
        lands_on(&after, "/v/near/top.md", "[b](/z/new)"),
        Some(p("/v/z/new.md"))
    );
    assert_eq!(
        lands_on(&after, "/v/near/top.md", "[b](z/new)"),
        Some(p("/v/near/z/new"))
    );
}

#[test]
fn rename_folder_moves_the_files_its_notes_link_to() {
    // notes/ -> archive/: the image moves with the note that shows it.
    let files = [
        ("/v/.mdroots", ""),
        ("/v/notes/a.md", "![pic](img.png)\n"),
        ("/v/notes/img.png", "image"),
        ("/v/top.md", "![x](notes/img.png)\n"),
    ];
    let ws = open(&files, "/v/top.md");
    let got = rename_many(&ws, &[("/v/notes", "/v/archive")]);
    assert_eq!(
        got,
        [(p("/v/top.md"), "![x](archive/img.png)\n".to_owned())]
    );
    let after = open(
        &[
            ("/v/.mdroots", ""),
            ("/v/archive/a.md", "![pic](img.png)\n"),
            ("/v/archive/img.png", "image"),
            ("/v/top.md", "![x](archive/img.png)\n"),
        ],
        "/v/top.md",
    );
    assert_eq!(
        lands_on(&after, "/v/archive/a.md", "![pic](img.png)"),
        Some(p("/v/archive/img.png"))
    );
    assert_eq!(
        lands_on(&after, "/v/top.md", "![x](archive/img.png)"),
        Some(p("/v/archive/img.png"))
    );
    // Control: only the note moves, so its link follows the image's old place.
    let got = rename_many(&ws, &[("/v/notes/a.md", "/v/archive/a.md")]);
    assert_eq!(
        got,
        [(p("/v/notes/a.md"), "![pic](../notes/img.png)\n".to_owned())]
    );
}

#[test]
fn rename_nested_folder_destinations_use_the_deepest() {
    // two/ -> near/z/ lies under one/ -> near/: near/z/new is two/new, a
    // directory, which an extensionless `z/new` from near/ finds. The order
    // of the pairs does not matter.
    let files = [
        ("/v/.mdroots", ""),
        ("/v/one/top.md", "[b](old/b)\n"),
        ("/v/old/b.md", ""),
        ("/v/two/t.md", ""),
        ("/v/two/new/pic.png", "image"),
    ];
    let ws = open(&files, "/v/one/top.md");
    for pairs in [
        [
            ("/v/old/b.md", "/v/z/new.md"),
            ("/v/one", "/v/near"),
            ("/v/two", "/v/near/z"),
        ],
        [
            ("/v/old/b.md", "/v/z/new.md"),
            ("/v/two", "/v/near/z"),
            ("/v/one", "/v/near"),
        ],
    ] {
        let got = rename_many(&ws, &pairs);
        assert_eq!(got, [(p("/v/one/top.md"), "[b](/z/new)\n".to_owned())]);
    }
    let after = open(
        &[
            ("/v/.mdroots", ""),
            ("/v/near/top.md", "[b](/z/new)\n"),
            ("/v/z/new.md", ""),
            ("/v/near/z/t.md", ""),
            ("/v/near/z/new/pic.png", "image"),
        ],
        "/v/near/top.md",
    );
    assert_eq!(
        lands_on(&after, "/v/near/top.md", "[b](/z/new)"),
        Some(p("/v/z/new.md"))
    );
}
