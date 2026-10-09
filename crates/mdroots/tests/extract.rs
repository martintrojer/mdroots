//! `Workspace::extract_note` on in-memory roots (`MemFs` with a `FakeProbe`
//! holding the same files). Link styles come from
//! [zk](https://github.com/zk-org/zk) and [Obsidian](https://obsidian.md)
//! configs.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mdroots::{Cancel, ErrorKind, NoEnumerator, Options, TextEdit, Workspace, WorkspaceEdit};
use mdroots_core::MemFs;
use mdroots_roots::probe::FakeProbe;

const SRC: &str = "# Main\n\nIntro line.\n\n## Ideas\n\nA thought.\n\nTail.\n";

fn ws(files: &[(&str, &str)], at: &str) -> Workspace {
    ws_fs(files, at).0
}

/// The workspace and its file system (to change files behind its back).
fn ws_fs(files: &[(&str, &str)], at: &str) -> (Workspace, Arc<MemFs>) {
    let mut fs = MemFs::new();
    let mut probe = FakeProbe::new().home("/h");
    for (path, text) in files {
        fs = fs.with_file(path, text);
        probe = probe.file(path, text);
    }
    let fs = Arc::new(fs);
    let opts = Options::default()
        .fs(fs.clone())
        .probe(Arc::new(probe))
        .enumerator(Arc::new(NoEnumerator));
    (Workspace::open_for(Path::new(at), opts).unwrap(), fs)
}

fn p(s: &str) -> PathBuf {
    PathBuf::from(s)
}

/// The byte range of `needle` in `hay` through the end of `until`.
fn span(hay: &str, needle: &str, until: &str) -> std::ops::Range<usize> {
    let start = hay.find(needle).unwrap();
    let end = hay[start..].find(until).unwrap() + start + until.len();
    start..end
}

fn extract(w: &Workspace, from: &str, r: std::ops::Range<usize>) -> WorkspaceEdit {
    w.extract_note(&p(from), r, &Cancel::new()).unwrap()
}

fn one_edit(e: &WorkspaceEdit) -> (&Path, &TextEdit) {
    assert_eq!(e.edits.len(), 1, "{e:?}");
    assert_eq!(e.edits[0].1.len(), 1, "{e:?}");
    (&e.edits[0].0, &e.edits[0].1[0])
}

const ZK_WIKI: &str = "[format.markdown]\nlink-format = \"wiki\"\n";

#[test]
fn wiki_style_titles_from_the_heading() {
    let w = ws(
        &[("/v/.zk/config.toml", ZK_WIKI), ("/v/notes/a.md", SRC)],
        "/v/notes/a.md",
    );
    let r = span(SRC, "## Ideas", "A thought.\n");
    let e = extract(&w, "/v/notes/a.md", r.clone());
    assert_eq!(
        e.create,
        [(
            p("/v/notes/ideas.md"),
            "## Ideas\n\nA thought.\n".to_owned()
        )]
    );
    let (file, edit) = one_edit(&e);
    assert_eq!(file, Path::new("/v/notes/a.md"));
    assert_eq!(edit.range, r);
    assert_eq!(edit.new_text, "[[notes/ideas]]");
    assert_eq!(e.rename, None);
}

#[test]
fn markdown_style_titles_from_the_first_line_and_keeps_the_extension() {
    let src = "# Main\n\n   A rather long first line that keeps going on and on past sixty chars\nsecond\n";
    let w = ws(
        &[
            ("/v/.obsidian/app.json", r#"{"useMarkdownLinks": true}"#),
            ("/v/a.markdown", src),
        ],
        "/v/a.markdown",
    );
    // Not at a heading: the first line, trimmed and cut to 60 chars.
    let r = span(src, "   A rather", "second");
    let e = extract(&w, "/v/a.markdown", r.clone());
    let title = "A rather long first line that keeps going on and on past six";
    assert_eq!(title.chars().count(), 60);
    let new = "/v/a-rather-long-first-line-that-keeps-going-on-and-on-past-six.markdown";
    assert_eq!(
        e.create,
        [(p(new), format!("# {title}\n\n{}\n", &src[r.clone()]))]
    );
    let (_, edit) = one_edit(&e);
    // Not indexed yet: the label is the file stem.
    assert_eq!(
        edit.new_text,
        "[a-rather-long-first-line-that-keeps-going-on-and-on-past-six](a-rather-long-first-line-that-keeps-going-on-and-on-past-six.markdown)"
    );
}

#[test]
fn a_selection_inside_a_heading_line_is_not_the_heading() {
    let w = ws(&[("/v/.mdroots", ""), ("/v/a.md", SRC)], "/v/a.md");
    let r = span(SRC, "Ideas", "thought.");
    let e = extract(&w, "/v/a.md", r);
    assert_eq!(e.create[0].0, p("/v/ideas.md"));
    assert_eq!(e.create[0].1, "# Ideas\n\nIdeas\n\nA thought.\n");
}

#[test]
fn names_that_exist_get_a_suffix() {
    let (w, fs) = ws_fs(
        &[
            ("/v/.zk/config.toml", ZK_WIKI),
            ("/v/a.md", SRC),
            ("/v/ideas.md", ""),
            ("/v/ideas-2.md", ""),
        ],
        "/v/a.md",
    );
    let r = span(SRC, "## Ideas", "thought.");
    let e = extract(&w, "/v/a.md", r.clone());
    assert_eq!(e.create[0].0, p("/v/ideas-3.md"));
    assert_eq!(one_edit(&e).1.new_text, "[[ideas-3]]");
    // On disk but not indexed yet (an editor created it): skipped too.
    fs.write("/v/ideas-3.md", b"");
    let e = extract(&w, "/v/a.md", r);
    assert_eq!(e.create[0].0, p("/v/ideas-4.md"));
}

#[test]
fn blank_or_symbol_only_selections_are_untitled() {
    let src = "# T\n\n\n\n!!!\n";
    let w = ws(
        &[("/v/.zk/config.toml", ZK_WIKI), ("/v/a.md", src)],
        "/v/a.md",
    );
    let e = extract(&w, "/v/a.md", 4..6);
    assert_eq!(
        e.create,
        [(p("/v/untitled.md"), "# Untitled\n\n\n\n".to_owned())]
    );
    let r = span(src, "!!!", "!!!");
    let e = extract(&w, "/v/a.md", r);
    assert_eq!(
        e.create,
        [(p("/v/untitled.md"), "# !!!\n\n!!!\n".to_owned())]
    );
}

#[test]
fn the_overlay_is_the_text() {
    let w = ws(
        &[("/v/.zk/config.toml", ZK_WIKI), ("/v/a.md", SRC)],
        "/v/a.md",
    );
    w.set_overlay(&p("/v/a.md"), "Fresh words\n").unwrap();
    let e = extract(&w, "/v/a.md", 0..5);
    assert_eq!(
        e.create,
        [(p("/v/fresh.md"), "# Fresh\n\nFresh\n".to_owned())]
    );
}

#[test]
fn refuses_empty_ranges_org_and_unindexed_notes() {
    let w = ws(
        &[
            ("/v/.zk/config.toml", ZK_WIKI),
            ("/v/a.md", SRC),
            ("/v/o.org", "* Head\nText\n"),
        ],
        "/v/a.md",
    );
    let err = |f: &str, r: std::ops::Range<usize>| {
        w.extract_note(&p(f), r, &Cancel::new()).unwrap_err().kind()
    };
    assert_eq!(err("/v/a.md", 3..3), ErrorKind::Unsupported);
    assert_eq!(err("/v/a.md", 3..999), ErrorKind::Unsupported);
    assert_eq!(err("/v/o.org", 0..6), ErrorKind::Unsupported);
    assert_eq!(err("/v/none.md", 0..1), ErrorKind::Unsupported);
    let c = Cancel::new();
    c.cancel();
    let e = w.extract_note(&p("/v/a.md"), 0..3, &c).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Cancelled);
}
