//! `Workspace::full_text` on in-memory trees and on a copy of
//! tests/corpus/zkvault in a temp dir, with a temp cache dir.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mdroots::{
    Cancel, ErrorKind, Hit, IndexMode, NoEnumerator, Options, Role, StdFs, StdProbe, Workspace,
};
use mdroots_core::MemFs;
use mdroots_roots::probe::FakeProbe;

fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for e in fs::read_dir(src).unwrap() {
        let e = e.unwrap();
        let to = dst.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &to);
        } else {
            fs::copy(e.path(), to).unwrap();
        }
    }
}

/// A copy of tests/corpus/zkvault: (temp dir, canonical vault dir).
fn zkvault() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = fs::canonicalize(tmp.path()).unwrap().join("vault");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus/zkvault");
    copy_dir(&src, &dir);
    (tmp, dir)
}

fn std_opts() -> Options {
    Options::default()
        .fs(Arc::new(StdFs))
        .probe(Arc::new(StdProbe))
        .enumerator(Arc::new(NoEnumerator))
}

fn paths(hits: &[Hit]) -> BTreeSet<PathBuf> {
    hits.iter().map(|h| h.path.clone()).collect()
}

fn search(ws: &Workspace, q: &str) -> Vec<Hit> {
    ws.full_text(q, 100, &Cancel::new()).unwrap()
}

#[test]
fn the_db_and_the_naive_scan_find_the_same_notes() {
    let (tmp, dir) = zkvault();
    let note = dir.join("journal/2026/W40.md");
    let cached = std_opts().cache_dir(tmp.path().join("cache"));
    let db = Workspace::open_for(&note, cached).unwrap();
    assert_eq!(db.role(), Some(Role::Reconciler));
    let mem = Workspace::open_for(&note, std_opts().index(IndexMode::Memory)).unwrap();
    assert_eq!(mem.role(), None);
    assert_eq!(db.files(), mem.files());
    let mut found = 0;
    for q in [
        "gardening",
        "garden",
        "week",
        "journal",
        "related composting",
        "the",
        "retro last",
        "planning",
        "nothingmatchesthis",
        "a",
        "t",
    ] {
        let (d, m) = (search(&db, q), search(&mem, q));
        assert_eq!(paths(&d), paths(&m), "{q}");
        found += d.len();
        for h in d.iter().chain(&m) {
            assert!(!h.snippet.contains('\n'), "{q}: {:?}", h.snippet);
            assert!(h.snippet.chars().count() <= 120, "{q}: {:?}", h.snippet);
        }
    }
    assert!(found > 10, "{found}");
    // Line numbers come from the current text in both.
    let line = |hits: &[Hit], p: &Path| hits.iter().find(|h| h.path == p).unwrap().line;
    let (d, m) = (search(&db, "retro"), search(&mem, "retro"));
    assert_eq!(line(&d, &note), 15);
    assert_eq!(line(&m, &note), 15);
    assert_eq!(
        m[0].snippet,
        "Retro; last year's same week was [[journal/2025/W40]]."
    );
}

#[test]
fn hits_are_capped_at_the_limit() {
    let (tmp, dir) = zkvault();
    let note = dir.join("journal/2026/W40.md");
    let ws = Workspace::open_for(&note, std_opts().cache_dir(tmp.path().join("cache"))).unwrap();
    assert!(search(&ws, "the").len() > 2);
    assert_eq!(ws.full_text("the", 2, &Cancel::new()).unwrap().len(), 2);
    assert!(ws.full_text("the", 0, &Cancel::new()).unwrap().is_empty());
    assert!(search(&ws, "").is_empty());
    assert!(search(&ws, "- * \"").is_empty());
}

#[test]
fn overlay_text_is_searched_instead_of_the_db() {
    let (tmp, dir) = zkvault();
    let note = dir.join("journal/2026/W40.md");
    let ws = Workspace::open_for(&note, std_opts().cache_dir(tmp.path().join("cache"))).unwrap();
    assert_eq!(paths(&search(&ws, "retro")), BTreeSet::from([note.clone()]));
    ws.set_overlay(&note, "# Draft\n\nunsaved zebra\n").unwrap();
    assert!(search(&ws, "retro").is_empty());
    let hits = search(&ws, "zebra");
    assert_eq!(paths(&hits), BTreeSet::from([note.clone()]));
    assert_eq!(
        (hits[0].line, hits[0].snippet.as_str()),
        (2, "unsaved zebra")
    );
    // A new note that exists only as an overlay.
    let draft = dir.join("draft.md");
    ws.set_overlay(&draft, "zebra   crossing\there").unwrap();
    let hits = search(&ws, "zebra");
    assert_eq!(paths(&hits), BTreeSet::from([note.clone(), draft.clone()]));
    let d = hits.iter().find(|h| h.path == draft).unwrap();
    assert_eq!(d.snippet, "zebra crossing here");
    ws.clear_overlay(&note).unwrap();
    assert_eq!(paths(&search(&ws, "retro")), BTreeSet::from([note]));
}

#[test]
fn a_peer_scans_the_text_it_serves() {
    let (tmp, dir) = zkvault();
    let note = dir.join("journal/2026/W40.md");
    let o = std_opts().cache_dir(tmp.path().join("cache"));
    let _rec = Workspace::open_for(&note, o.clone()).unwrap();
    let peer = Workspace::open_for(&note, o).unwrap();
    assert_eq!(peer.role(), Some(Role::Peer));
    // The file changes on disk; the peer re-reads it without writing the DB.
    fs::write(&note, "# W40\n\nkangaroo\n").unwrap();
    peer.refresh(&Cancel::new()).unwrap();
    assert_eq!(paths(&search(&peer, "kangaroo")), BTreeSet::from([note]));
    assert!(search(&peer, "retro").is_empty());
}

fn mem_ws(files: &[(&str, &str)]) -> Workspace {
    let mut fs = MemFs::new().with_file("/v/.mdroots", "");
    let mut probe = FakeProbe::new().home("/h").file("/v/.mdroots", "");
    for (path, text) in files {
        fs = fs.with_file(path, text);
        probe = probe.file(path, text);
    }
    let o = Options::default()
        .fs(Arc::new(fs))
        .probe(Arc::new(probe))
        .enumerator(Arc::new(NoEnumerator));
    Workspace::open_for(Path::new("/v/a.md"), o).unwrap()
}

#[test]
fn memory_mode_matches_tokens_case_insensitively() {
    let ws = mem_ws(&[
        ("/v/a.md", "# Alpha\n\nThe Linking of NOTES.\n"),
        ("/v/b.md", "linked notes\nand more\n"),
        ("/v/c.md", "unlinked\n"),
        ("/v/d.md", "Café crème\n"),
    ]);
    assert_eq!(ws.role(), None);
    let names = |q: &str| -> Vec<String> {
        search(&ws, q)
            .into_iter()
            .map(|h| h.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    };
    assert_eq!(names("link"), ["a.md", "b.md"]);
    assert_eq!(names("notes LINK"), ["a.md", "b.md"]);
    assert!(
        names("link notes").is_empty(),
        "only the last term is a prefix"
    );
    assert_eq!(names("café"), ["d.md"]);
    assert_eq!(names("\"notes\""), ["a.md", "b.md"]);
    assert!(names("ink").is_empty());
    let hits = search(&ws, "notes linking");
    assert_eq!(
        (hits[0].line, hits[0].snippet.as_str()),
        (2, "The Linking of NOTES.")
    );
}

#[test]
fn cancel_stops_the_search() {
    let ws = mem_ws(&[("/v/a.md", "x\n")]);
    let c = Cancel::new();
    c.cancel();
    let e = ws.full_text("x", 10, &c).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Cancelled);
}

#[test]
fn accents_fold_in_both_paths_and_the_line_is_the_first_match() {
    let (tmp, dir) = zkvault();
    let note = dir.join("accent.md");
    fs::write(
        &note,
        "# Heading only\n\ntext\n\nA visit to the Café today.\n",
    )
    .unwrap();
    for opts in [
        std_opts().cache_dir(tmp.path().join("cache")),
        std_opts().index(IndexMode::Memory),
    ] {
        let ws = Workspace::open_for(&note, opts).unwrap();
        let hits: Vec<Hit> = search(&ws, "cafe")
            .into_iter()
            .filter(|h| h.path == note)
            .collect();
        assert_eq!(hits.len(), 1, "role {:?}", ws.role());
        assert_eq!(hits[0].line, 4, "role {:?}", ws.role());
    }
}
