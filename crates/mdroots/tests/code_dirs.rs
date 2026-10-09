//! `Options::code_dirs`: code mentions resolve against extra absolute dirs
//! (after the linking note's dir, before the root). Real temp dirs, memory
//! index or a temp cache.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mdroots::syntax::LinkKind;
use mdroots::{Cancel, DocLink, LinkStatus, NoEnumerator, Options, Workspace, Workspaces};

/// `<tmp>/proj/src/main.rs` and `<tmp>/proj/notes/docs/a.md` mentioning
/// `` `src/main.rs:12` ``; returns the canonical project dir.
fn project(tmp: &Path) -> PathBuf {
    let proj = tmp.canonicalize().unwrap().join("proj");
    fs::create_dir_all(proj.join("src")).unwrap();
    fs::create_dir_all(proj.join("notes/docs")).unwrap();
    fs::write(proj.join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(proj.join("notes/docs/a.md"), "See `src/main.rs:12`.\n").unwrap();
    proj
}

fn opts() -> Options {
    Options::default().enumerator(Arc::new(NoEnumerator))
}

fn mention(ws: &Workspace, note: &Path, raw_end: &str) -> DocLink {
    let text = fs::read_to_string(note).unwrap();
    ws.document_links(note)
        .unwrap()
        .into_iter()
        .find(|l| l.kind == LinkKind::CodeMention && text[l.range.clone()].contains(raw_end))
        .unwrap_or_else(|| panic!("no code mention {raw_end:?}"))
}

#[test]
fn unresolved_without_code_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    let proj = project(tmp.path());
    let note = proj.join("notes/docs/a.md");
    let ws = Workspace::open_at(&proj.join("notes"), opts()).unwrap();
    let l = mention(&ws, &note, "main.rs");
    assert_eq!(l.status, LinkStatus::Unchecked);
    assert_eq!(l.target, None);
}

#[test]
fn code_dir_resolves_outside_the_root_and_survives_refresh() {
    let tmp = tempfile::tempdir().unwrap();
    let proj = project(tmp.path());
    let note = proj.join("notes/docs/a.md");
    let ws = Workspace::open_at(&proj.join("notes"), opts().code_dirs(vec![proj.clone()])).unwrap();
    let check = |ws: &Workspace| {
        let l = mention(ws, &note, "main.rs");
        assert_eq!(l.kind, LinkKind::CodeMention);
        assert_eq!(l.status, LinkStatus::Unindexed);
        assert_eq!(l.target, Some(proj.join("src/main.rs")));
        assert_eq!(l.line, Some(12));
    };
    check(&ws);
    ws.refresh(&Cancel::new()).unwrap();
    check(&ws);
}

#[test]
fn note_dir_wins_over_a_code_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let proj = project(tmp.path());
    fs::create_dir_all(proj.join("notes/docs/src")).unwrap();
    fs::write(proj.join("notes/docs/src/main.rs"), "").unwrap();
    let note = proj.join("notes/docs/a.md");
    let ws = Workspace::open_at(&proj.join("notes"), opts().code_dirs(vec![proj.clone()])).unwrap();
    let l = mention(&ws, &note, "main.rs");
    assert_eq!(l.status, LinkStatus::Unindexed);
    assert_eq!(l.target, Some(proj.join("notes/docs/src/main.rs")));
}

#[test]
fn relative_code_dirs_are_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    let proj = project(tmp.path());
    let note = proj.join("notes/docs/a.md");
    let o = opts().code_dirs(vec![PathBuf::from(".."), PathBuf::from("proj")]);
    let ws = Workspace::open_at(&proj.join("notes"), o).unwrap();
    let l = mention(&ws, &note, "main.rs");
    assert_eq!(l.status, LinkStatus::Unchecked);
    assert_eq!(l.target, None);
}

#[test]
fn workspaces_pass_code_dirs_on() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let proj = project(tmp.path());
    let note = proj.join("notes/docs/a.md");
    let o = opts()
        .cache_dir(cache.path().to_path_buf())
        .code_dirs(vec![proj.clone(), proj.clone()]);
    let ws = Workspaces::new(o).for_path(&note).unwrap();
    let l = mention(&ws, &note, "main.rs");
    assert_eq!(l.status, LinkStatus::Unindexed);
    assert_eq!(l.target, Some(proj.join("src/main.rs")));
    assert_eq!(l.line, Some(12));
}
