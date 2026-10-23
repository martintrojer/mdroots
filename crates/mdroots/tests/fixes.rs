//! `Workspace::fixes` on in-memory roots (`MemFs` with a `FakeProbe`
//! holding the same files): link repair for broken links and anchors and
//! ambiguous links, link conversion and link selection.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mdroots::{
    Cancel, DiagCode, Fix, FixKind, NoEnumerator, Options, TextEdit, Workspace, WorkspaceEdit,
};
use mdroots_core::MemFs;
use mdroots_roots::probe::FakeProbe;

/// A root at `/v` (a `.obsidian` marker: wiki links by stem) holding
/// `files`, opened at the first one.
fn ws(files: &[(&str, &str)]) -> Workspace {
    ws_with(files, "{}")
}

/// [`ws`] with `app` as the Obsidian config (`.obsidian/app.json`).
fn ws_with(files: &[(&str, &str)], app: &str) -> Workspace {
    let mut fs = MemFs::new();
    let mut probe = FakeProbe::new().home("/h");
    for (path, text) in files.iter().chain(&[("/v/.obsidian/app.json", app)]) {
        fs = fs.with_file(path, text);
        probe = probe.file(path, text);
    }
    let opts = Options::default()
        .fs(Arc::new(fs))
        .probe(Arc::new(probe))
        .enumerator(Arc::new(NoEnumerator));
    Workspace::open_for(Path::new(files[0].0), opts).unwrap()
}

fn p(s: &str) -> PathBuf {
    PathBuf::from(s)
}

/// The byte range of the first `needle` in `hay`.
fn at(hay: &str, needle: &str) -> Range<usize> {
    let s = hay.find(needle).unwrap();
    s..s + needle.len()
}

fn fixes(w: &Workspace, path: &str, range: Range<usize>) -> Vec<Fix> {
    w.fixes(&p(path), range, &Cancel::new()).unwrap()
}

/// [`fixes`] without conversion and link selection: the quick fixes only.
fn quick(w: &Workspace, path: &str, range: Range<usize>) -> Vec<Fix> {
    let mut f = fixes(w, path, range);
    f.retain(|f| matches!(f.kind, FixKind::QuickFix(_)));
    f
}

fn titles(f: &[Fix]) -> Vec<&str> {
    f.iter().map(|f| f.title.as_str()).collect()
}

/// `text` with the fix's edits (all in `file`) applied.
fn apply(text: &str, file: &str, edit: &WorkspaceEdit) -> String {
    let mut out = text.to_owned();
    for (f, edits) in &edit.edits {
        assert_eq!(f, &p(file));
        for TextEdit { range, new_text } in edits.iter().rev() {
            out.replace_range(range.clone(), new_text);
        }
    }
    out
}

/// Only the fixes of `kind`.
fn only(f: Vec<Fix>, kind: &FixKind) -> Vec<Fix> {
    f.into_iter().filter(|f| &f.kind == kind).collect()
}

fn code(f: &Fix) -> DiagCode {
    match &f.kind {
        FixKind::QuickFix(d) => d.code,
        k => panic!("not a quick fix: {k:?}"),
    }
}

#[test]
fn change_to_keeps_anchor_and_label() {
    let a = "See [[project-pl#Goals|g]].\n";
    let w = ws(&[
        ("/v/a.md", a),
        ("/v/project-plan.md", "# Plan\n\n## Goals\n"),
        ("/v/other.md", "# Other\n"),
    ]);
    let f = fixes(&w, "/v/a.md", at(a, "[[project-pl#Goals|g]]"));
    assert_eq!(titles(&f), ["Change to Plan", "Create note project-pl.md"]);
    assert!(f[0].preferred, "the only candidate");
    assert_eq!(code(&f[0]), DiagCode::BrokenLink);
    let FixKind::QuickFix(d) = &f[0].kind else {
        unreachable!()
    };
    assert_eq!(d.range, at(a, "[[project-pl#Goals|g]]"));
    assert_eq!(
        apply(a, "/v/a.md", &f[0].edit),
        "See [[project-plan#Goals|g]].\n"
    );
    assert!(f[0].edit.create.is_empty() && f[0].edit.rename.is_none());
}

#[test]
fn change_to_takes_three_close_notes_not_subsequences() {
    let a = "[[plan]]\n";
    let w = ws(&[
        ("/v/a.md", a),
        ("/v/plan-a.md", ""),
        ("/v/plan-b.md", ""),
        ("/v/my-plan.md", ""),
        ("/v/plan-c.md", ""),
        ("/v/p-l-a-n.md", ""),
    ]);
    let f = fixes(&w, "/v/a.md", 0..0);
    // Prefix tier first (by path), at most three; the gapped `p-l-a-n` never.
    assert_eq!(
        titles(&f),
        [
            "Change to plan-a",
            "Change to plan-b",
            "Change to plan-c",
            "Create note plan.md"
        ]
    );
    assert!(f.iter().all(|f| !f.preferred));
}

#[test]
fn markdown_relative_path_keeps_its_form() {
    let a = "[x](sub/missing.md#top) and [y](/sub/missing.md)\n";
    let w = ws(&[("/v/a.md", a), ("/v/sub/missing-not.md", "# Not\n")]);
    let f = fixes(&w, "/v/a.md", at(a, "[x]"));
    assert_eq!(titles(&f), ["Change to Not", "Create note sub/missing.md"]);
    assert_eq!(
        apply(a, "/v/a.md", &f[0].edit),
        "[x](sub/missing-not.md#top) and [y](/sub/missing.md)\n"
    );
    let f = fixes(&w, "/v/a.md", at(a, "[y]"));
    assert_eq!(
        apply(a, "/v/a.md", &f[0].edit),
        "[x](sub/missing.md#top) and [y](/sub/missing-not.md)\n"
    );
}

#[test]
fn file_relative_from_a_subdirectory() {
    let a = "[x](../notes/missin.md)\n";
    let w = ws(&[("/v/d/a.md", a), ("/v/notes/missing.md", "")]);
    let f = fixes(&w, "/v/d/a.md", 0..0);
    assert_eq!(
        apply(a, "/v/d/a.md", &f[0].edit),
        "[x](../notes/missing.md)\n"
    );
    assert_eq!(f[1].title, "Create note notes/missin.md");
}

#[test]
fn extension_is_kept_iff_written() {
    let a = "[[proj]] [[proj.md]]\n";
    let w = ws(&[("/v/a.md", a), ("/v/project.md", "")]);
    let f = fixes(&w, "/v/a.md", 0..a.len());
    assert_eq!(code(&f[0]), DiagCode::BrokenLink);
    let out: Vec<String> = f
        .iter()
        .filter(|f| f.title.starts_with("Change"))
        .map(|f| apply(a, "/v/a.md", &f.edit))
        .collect();
    assert_eq!(
        out,
        ["[[project]] [[proj.md]]\n", "[[proj]] [[project.md]]\n"]
    );
}

#[test]
fn wiki_path_falls_back_when_the_stem_is_shared() {
    let a = "[[du]] [[du.md]]\n";
    let w = ws(&[("/v/a.md", a), ("/v/one/dup.md", ""), ("/v/two/dup.md", "")]);
    let changes = |r| -> Vec<String> {
        fixes(&w, "/v/a.md", r)
            .iter()
            .filter(|f| f.title.starts_with("Change"))
            .map(|f| apply(a, "/v/a.md", &f.edit))
            .collect()
    };
    assert_eq!(
        changes(at(a, "[[du]]")),
        ["[[one/dup]] [[du.md]]\n", "[[two/dup]] [[du.md]]\n"]
    );
    // The written extension stays through the fallback.
    assert_eq!(
        changes(at(a, "[[du.md]]")),
        ["[[du]] [[one/dup.md]]\n", "[[du]] [[two/dup.md]]\n"]
    );
}

#[test]
fn cursor_at_link_end_selects_it() {
    let a = "x [[missing]] y\n";
    let w = ws(&[("/v/a.md", a), ("/v/missing-note.md", "")]);
    let end = at(a, "[[missing]]").end;
    assert_eq!(fixes(&w, "/v/a.md", end..end).len(), 2);
    let start = at(a, "[[missing]]").start;
    assert_eq!(fixes(&w, "/v/a.md", start..start).len(), 2);
    assert!(fixes(&w, "/v/a.md", end + 1..end + 1).is_empty());
    // A selection overlapping the link selects it; touching it does not.
    assert_eq!(fixes(&w, "/v/a.md", 0..start + 1).len(), 2);
    assert!(fixes(&w, "/v/a.md", 0..start).is_empty());
}

#[test]
fn fixes_follow_link_order() {
    let a = "[[zz-miss]] then [[aa-miss]]\n";
    let w = ws(&[("/v/a.md", a)]);
    let f = fixes(&w, "/v/a.md", 0..a.len());
    assert_eq!(
        titles(&f),
        ["Create note zz-miss.md", "Create note aa-miss.md"]
    );
}

#[test]
fn resolved_and_code_links_get_nothing() {
    let a = "[[b]] `[[nope]]`\n\n```\n[[gone]]\n```\n<!-- [[lost]] -->\n";
    let w = ws(&[("/v/a.md", a), ("/v/b.md", "")]);
    assert!(quick(&w, "/v/a.md", 0..a.len()).is_empty());
}

#[test]
fn create_note_paths_and_content() {
    let a = "[[sub/new one]] [[missing.md]] [[pic.png]] [x](../../out.md)\n";
    let w = ws(&[("/v/d/a.markdown", a)]);
    let f = fixes(&w, "/v/d/a.markdown", 0..a.len());
    assert_eq!(
        titles(&f),
        [
            "Create note sub/new one.markdown",
            "Create note d/missing.md"
        ]
    );
    assert!(f.iter().all(|f| !f.preferred));
    let e = &f[0].edit;
    assert_eq!(
        e.create,
        [(p("/v/sub/new one.markdown"), "# new one\n".to_owned())]
    );
    assert!(e.edits.is_empty() && e.rename.is_none());
}

#[test]
fn create_note_decodes_markdown_paths() {
    let a = "[x](my%20note)\n";
    let w = ws(&[("/v/a.md", a)]);
    let f = fixes(&w, "/v/a.md", 0..0);
    assert_eq!(titles(&f), ["Create note my note.md"]);
    assert_eq!(f[0].edit.create[0].1, "# my note\n");
}

#[test]
fn create_note_not_offered_when_the_file_exists() {
    // `[x](gone)` from a `.markdown` note names `gone.markdown`; the
    // resolver looks for `gone` and `gone.md`, so the link stays broken
    // once `gone.markdown` appears on disk behind the index's back.
    let a = "[x](gone)\n";
    let fs = Arc::new(
        MemFs::new()
            .with_file("/v/a.markdown", a)
            .with_file("/v/.obsidian/app.json", "{}"),
    );
    let probe = FakeProbe::new()
        .home("/h")
        .file("/v/a.markdown", a)
        .file("/v/.obsidian/app.json", "{}");
    let opts = Options::default()
        .fs(fs.clone())
        .probe(Arc::new(probe))
        .enumerator(Arc::new(NoEnumerator));
    let w = Workspace::open_for(Path::new("/v/a.markdown"), opts).unwrap();
    let f = fixes(&w, "/v/a.markdown", 0..0);
    assert_eq!(titles(&f), ["Create note gone.markdown"]);
    fs.write("/v/gone.markdown", b"");
    assert!(fixes(&w, "/v/a.markdown", 0..0).is_empty());
}

#[test]
fn anchor_fix_changes_only_the_anchor() {
    let a = "[[b#intro]] and [y](b.md#intro \"t\")\n";
    let w = ws(&[("/v/a.md", a), ("/v/b.md", "# B\n\n## Introduction\n")]);
    let f = quick(&w, "/v/a.md", at(a, "[[b#intro]]"));
    assert_eq!(titles(&f), ["Change to #introduction"]);
    assert!(f[0].preferred);
    assert_eq!(code(&f[0]), DiagCode::BrokenAnchor);
    assert_eq!(
        apply(a, "/v/a.md", &f[0].edit),
        "[[b#introduction]] and [y](b.md#intro \"t\")\n"
    );
    let f = fixes(&w, "/v/a.md", at(a, "[y]"));
    assert_eq!(
        apply(a, "/v/a.md", &f[0].edit),
        "[[b#intro]] and [y](b.md#introduction \"t\")\n"
    );
}

#[test]
fn wiki_anchor_with_spaces_is_replaced_whole() {
    let a = "[[b#Getting Start|go]]\n";
    let w = ws(&[("/v/a.md", a), ("/v/b.md", "# B\n\n## Getting started\n")]);
    let f = fixes(&w, "/v/a.md", 0..0);
    assert_eq!(titles(&f), ["Change to #getting-started"]);
    assert_eq!(
        apply(a, "/v/a.md", &f[0].edit),
        "[[b#getting-started|go]]\n"
    );
}

#[test]
fn anchor_fix_ranks_headings_and_skips_blocks() {
    let a = "[[b#set|s]] [[b#^blk]]\n";
    let b = "# B\n\n## Old setup\n\n## Setup\n\n## Settings\n\n## Reset\n\n## Sxexxtx\n";
    let w = ws(&[("/v/a.md", a), ("/v/b.md", b)]);
    let f = quick(&w, "/v/a.md", at(a, "[[b#set|s]]"));
    // Prefix (document order), then substring; never a subsequence.
    assert_eq!(
        titles(&f),
        [
            "Change to #setup",
            "Change to #settings",
            "Change to #old-setup"
        ]
    );
    assert!(f.iter().all(|f| !f.preferred));
    assert_eq!(
        apply(a, "/v/a.md", &f[0].edit),
        "[[b#setup|s]] [[b#^blk]]\n"
    );
    assert!(quick(&w, "/v/a.md", at(a, "[[b#^blk]]")).is_empty());
}

#[test]
fn pick_for_an_ambiguous_link() {
    let a = "[[dup]] [[dup.md]]\n";
    let w = ws(&[("/v/a.md", a), ("/v/one/dup.md", ""), ("/v/two/dup.md", "")]);
    let f = fixes(&w, "/v/a.md", at(a, "[[dup]]"));
    assert_eq!(titles(&f), ["Pick one/dup.md", "Pick two/dup.md"]);
    assert!(f.iter().all(|f| !f.preferred));
    assert_eq!(code(&f[0]), DiagCode::AmbiguousLink);
    assert_eq!(apply(a, "/v/a.md", &f[0].edit), "[[one/dup]] [[dup.md]]\n");
    let f = fixes(&w, "/v/a.md", at(a, "[[dup.md]]"));
    let out: Vec<String> = f.iter().map(|f| apply(a, "/v/a.md", &f.edit)).collect();
    assert_eq!(
        out,
        ["[[dup]] [[one/dup.md]]\n", "[[dup]] [[two/dup.md]]\n"]
    );
}

#[test]
fn org_notes_get_link_repair_but_no_anchor_fix() {
    let a = "[[file:missin.org][M]] [[file:b.org::*Intr]]\n";
    let w = ws(&[
        ("/v/a.org", a),
        ("/v/missing.org", "* Missing\n"),
        ("/v/b.org", "* Intro\n"),
    ]);
    let f = fixes(&w, "/v/a.org", at(a, "[[file:missin.org][M]]"));
    assert_eq!(titles(&f), ["Change to Missing", "Create note missin.org"]);
    assert_eq!(
        apply(a, "/v/a.org", &f[0].edit),
        "[[file:missing.org][M]] [[file:b.org::*Intr]]\n"
    );
    assert!(fixes(&w, "/v/a.org", at(a, "[[file:b.org::*Intr]]")).is_empty());
}

#[test]
fn reference_definition_gets_fixes_not_its_usage() {
    let a = "See [x][r].\n\n[r]: missin.md\n";
    let w = ws(&[("/v/a.md", a), ("/v/missing.md", "")]);
    let usage = at(a, "[x][r]");
    assert!(fixes(&w, "/v/a.md", usage).is_empty());
    let f = fixes(&w, "/v/a.md", at(a, "[r]: missin.md"));
    assert_eq!(f[0].title, "Change to missing");
    assert_eq!(
        apply(a, "/v/a.md", &f[0].edit),
        "See [x][r].\n\n[r]: missing.md\n"
    );
}

#[test]
fn overlay_text_is_what_fixes_see() {
    let w = ws(&[("/v/a.md", "[[b]]\n"), ("/v/b.md", ""), ("/v/bee.md", "")]);
    assert!(quick(&w, "/v/a.md", 0..0).is_empty());
    let text = "[[be]]\n";
    w.set_overlay(&p("/v/a.md"), text).unwrap();
    let f = fixes(&w, "/v/a.md", 0..0);
    assert_eq!(f[0].title, "Change to bee");
    assert_eq!(apply(text, "/v/a.md", &f[0].edit), "[[bee]]\n");
}

#[test]
fn excludes_the_linking_note() {
    let a = "[[a-missing]]\n";
    let w = ws(&[("/v/a.md", a)]);
    assert_eq!(
        titles(&fixes(&w, "/v/a.md", 0..0)),
        ["Create note a-missing.md"]
    );
    let w = ws(&[("/v/aa.md", "[[a]]\n")]);
    assert_eq!(titles(&fixes(&w, "/v/aa.md", 0..0)), ["Create note a.md"]);
}

/// The one anchor fix for the whole of `a` (in `/v/a.md`, `b` in
/// `/v/b.md`), applied.
fn anchor_fixed(a: &str, b: &str) -> String {
    let w = ws(&[("/v/a.md", a), ("/v/b.md", b)]);
    let f = quick(&w, "/v/a.md", 0..a.len());
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(code(&f[0]), DiagCode::BrokenAnchor);
    apply(a, "/v/a.md", &f[0].edit)
}

#[test]
fn anchor_fix_replaces_the_whole_destination_anchor() {
    let started = "# B\n\n## Getting started\n";
    assert_eq!(
        anchor_fixed("[x](<b.md#Getting Start>)\n", started),
        "[x](<b.md#getting-started>)\n"
    );
    assert_eq!(
        // The whole `getting(start)` is read: a prefix of the heading text.
        anchor_fixed(
            "[x](b.md#getting(start))\n",
            "# B\n\n## Getting(start) guide\n"
        ),
        "[x](b.md#gettingstart-guide)\n"
    );
    assert_eq!(
        anchor_fixed("[x](b.md#Getting%20Start)\n", started),
        "[x](b.md#getting-started)\n"
    );
}

#[test]
fn anchor_fix_in_html_hrefs_keeps_the_quotes() {
    let intro = "# B\n\n## Introduction\n";
    assert_eq!(
        anchor_fixed("<a href=\"b.md#intro\">x</a>\n", intro),
        "<a href=\"b.md#introduction\">x</a>\n"
    );
    assert_eq!(
        anchor_fixed("<a href=b.md#intro>x</a>\n", intro),
        "<a href=b.md#introduction>x</a>\n"
    );
    assert_eq!(
        anchor_fixed("<a href='b.md#don'>x</a>\n", "# B\n\n## Don't panic\n"),
        "<a href='b.md#dont-panic'>x</a>\n"
    );
}

#[cfg(unix)]
#[test]
fn create_note_not_offered_through_a_symlink_out_of_the_root() {
    use mdroots::{IndexMode, StdFs, StdProbe};
    let tmp = tempfile::tempdir().unwrap();
    let v = tmp.path().join("v");
    std::fs::create_dir_all(v.join("inside")).unwrap();
    std::fs::create_dir_all(tmp.path().join("outside")).unwrap();
    std::os::unix::fs::symlink(tmp.path().join("outside"), v.join("escape")).unwrap();
    std::fs::write(v.join(".mdroots"), "").unwrap();
    let a = "[x](escape/new.md) [x](inside/new.md) [[escape/other]]\n";
    std::fs::write(v.join("a.md"), a).unwrap();
    let opts = Options::default()
        .fs(Arc::new(StdFs))
        .probe(Arc::new(StdProbe))
        .enumerator(Arc::new(NoEnumerator))
        .index(IndexMode::Memory);
    let w = Workspace::open_for(&v.join("a.md"), opts).unwrap();
    let root = w.root().path;
    let f = w
        .fixes(&root.join("a.md"), 0..a.len(), &Cancel::new())
        .unwrap();
    assert_eq!(titles(&f), ["Create note inside/new.md"]);
    // Canonical, like every public path (temp dirs may sit behind a symlink).
    assert_eq!(f[0].edit.create[0].0, root.join("inside/new.md"));
}

/// The preferred fix for the whole of `a` (in `/v/a.md`), applied; then
/// the note has no diagnostics left.
fn preferred_clears(w: &Workspace, a: &str) -> String {
    let f = fixes(w, "/v/a.md", 0..a.len());
    let pick: Vec<&Fix> = f.iter().filter(|f| f.preferred).collect();
    assert_eq!(pick.len(), 1, "{f:?}");
    let out = apply(a, "/v/a.md", &pick[0].edit);
    w.set_overlay(&p("/v/a.md"), &out).unwrap();
    assert_eq!(w.diagnostics(&p("/v/a.md"), &Cancel::new()).unwrap(), []);
    out
}

// Reviewer repros (medium_html_anchor_fixes_locate_the): the destination
// is the `href` value, not the first place its text appears in the tag.
#[test]
fn html_anchor_fix_edits_href_not_an_earlier_attribute() {
    let b = "## Introduction\n";
    let a = "<a title='b.md#intro' href='b.md#intro'>x</a>";
    let w = ws(&[("/v/a.md", a), ("/v/b.md", b)]);
    assert_eq!(
        preferred_clears(&w, a),
        "<a title='b.md#intro' href='b.md#introduction'>x</a>"
    );
    let a = "<a title='b.md' href='b.md#intro'>x</a>";
    let w = ws(&[("/v/a.md", a), ("/v/b.md", b)]);
    assert_eq!(
        preferred_clears(&w, a),
        "<a title='b.md' href='b.md#introduction'>x</a>"
    );
    // Control.
    let a = "<a href='b.md#intro'>x</a>";
    let w = ws(&[("/v/a.md", a), ("/v/b.md", b)]);
    assert_eq!(preferred_clears(&w, a), "<a href='b.md#introduction'>x</a>");
}

#[test]
fn same_note_html_anchor_gets_the_fix() {
    let a = "## Introduction\n\n<a href='#intro'>x</a>";
    let w = ws(&[("/v/a.md", a)]);
    assert_eq!(
        preferred_clears(&w, a),
        "## Introduction\n\n<a href='#introduction'>x</a>"
    );
    // Control: the Markdown form.
    let a = "## Introduction\n\n[x](#intro)";
    let w = ws(&[("/v/a.md", a)]);
    assert_eq!(
        preferred_clears(&w, a),
        "## Introduction\n\n[x](#introduction)"
    );
}

// Reviewer repro (medium_org_links_in_markdown_create_the): an Org link in
// a Markdown note keeps `file:` outside the path, as in an Org note.
#[test]
fn org_link_in_a_markdown_note_drops_the_file_scheme() {
    let a = "[[file:missin.org][M]]";
    let w = ws(&[("/v/a.md", a), ("/v/missing.org", "* Missing\n")]);
    let f = fixes(&w, "/v/a.md", 0..a.len());
    assert_eq!(titles(&f), ["Change to Missing", "Create note missin.org"]);
    assert_eq!(apply(a, "/v/a.md", &f[0].edit), "[[file:missing.org][M]]");
    assert_eq!(
        f[1].edit.create,
        [(p("/v/missin.org"), "# missin\n".to_owned())]
    );
    // Control: the same link in an Org note.
    let w = ws(&[("/v/a.org", a), ("/v/missing.org", "* Missing\n")]);
    let f = fixes(&w, "/v/a.org", 0..a.len());
    assert_eq!(titles(&f), ["Change to Missing", "Create note missin.org"]);
    assert_eq!(apply(a, "/v/a.org", &f[0].edit), "[[file:missing.org][M]]");
    assert_eq!(f[1].edit.create[0].0, p("/v/missin.org"));
}

/// The conversion fixes for the link `needle` in `a` (`/v/a.md`), each
/// applied.
fn converted(w: &Workspace, a: &str, needle: &str) -> Vec<(String, String)> {
    let f = only(fixes(w, "/v/a.md", at(a, needle)), &FixKind::ConvertLink);
    assert!(
        f.iter().all(|f| !f.preferred),
        "conversion is never preferred"
    );
    f.iter()
        .map(|f| (f.title.clone(), apply(a, "/v/a.md", &f.edit)))
        .collect()
}

const BEE: (&str, &str) = ("/v/b.md", "---\ntitle: Bee note\n---\n\n## Part two\n");

#[test]
fn wiki_to_markdown_keeps_label_and_anchor() {
    let a = "[[b]] [[b|the bee]] [[b#part-two]]\n";
    let w = ws(&[("/v/a.md", a), BEE]);
    let md = "Convert to Markdown link".to_owned();
    // No label: the target's title.
    assert_eq!(
        converted(&w, a, "[[b]]"),
        [(md.clone(), a.replacen("[[b]]", "[Bee note](b.md)", 1))]
    );
    assert_eq!(
        converted(&w, a, "[[b|the bee]]"),
        [(md.clone(), a.replace("[[b|the bee]]", "[the bee](b.md)"))]
    );
    assert_eq!(
        converted(&w, a, "[[b#part-two]]"),
        [(md, a.replace("[[b#part-two]]", "[Bee note](b.md#part-two)"))]
    );
}

#[test]
fn markdown_to_wiki_keeps_the_label_unless_it_is_the_target() {
    let a = "[Bee note](b.md) [b](b.md) [x](b.md#part-two)\n";
    let w = ws(&[("/v/a.md", a), BEE]);
    let wiki = "Convert to wiki link".to_owned();
    assert_eq!(
        converted(&w, a, "[Bee note](b.md)"),
        [(
            wiki.clone(),
            a.replace("[Bee note](b.md)", "[[b|Bee note]]")
        )]
    );
    assert_eq!(
        converted(&w, a, "[b](b.md)"),
        [(wiki.clone(), a.replace("[b](b.md)", "[[b]]"))]
    );
    assert_eq!(
        converted(&w, a, "[x](b.md#part-two)"),
        [(wiki, a.replace("[x](b.md#part-two)", "[[b#part-two|x]]"))]
    );
}

#[test]
fn markdown_to_wiki_keeps_formatting_at_the_label_edges() {
    let a = "[pre **Bee**](b.md) [*x*](b.md) [](b.md)\n";
    let w = ws(&[("/v/a.md", a), BEE]);
    let wiki = "Convert to wiki link".to_owned();
    assert_eq!(
        converted(&w, a, "[pre **Bee**](b.md)"),
        [(
            wiki.clone(),
            a.replace("[pre **Bee**](b.md)", "[[b|pre **Bee**]]")
        )]
    );
    assert_eq!(
        converted(&w, a, "[*x*](b.md)"),
        [(wiki.clone(), a.replace("[*x*](b.md)", "[[b|*x*]]"))]
    );
    // An empty label is dropped.
    assert_eq!(
        converted(&w, a, "[](b.md)"),
        [(wiki, a.replace("[](b.md)", "[[b]]"))]
    );
}

#[test]
fn markdown_to_wiki_keeps_a_nested_image_label_whole() {
    let a = "[![Bee](img.png)](b.md) [see ![x](i.png)](b.md) [![](i.png)](b.md#part-two)\n";
    let w = ws(&[
        ("/v/a.md", a),
        BEE,
        ("/v/img.png", "png"),
        ("/v/i.png", "png"),
    ]);
    let wiki = "Convert to wiki link".to_owned();
    for (link, new) in [
        ("[![Bee](img.png)](b.md)", "[[b|![Bee](img.png)]]"),
        ("[see ![x](i.png)](b.md)", "[[b|see ![x](i.png)]]"),
        ("[![](i.png)](b.md#part-two)", "[[b#part-two|![](i.png)]]"),
    ] {
        assert_eq!(
            converted(&w, a, link),
            [(wiki.clone(), a.replace(link, new))],
            "{link}"
        );
    }
}

#[test]
fn change_to_finds_the_path_after_a_nested_image() {
    let a = "[![x](i.png)](missin.md)\n";
    let w = ws(&[("/v/a.md", a), ("/v/missing.md", ""), ("/v/i.png", "png")]);
    let f = quick(&w, "/v/a.md", at(a, "(missin.md)"));
    assert_eq!(f[0].title, "Change to missing");
    assert_eq!(
        apply(a, "/v/a.md", &f[0].edit),
        "[![x](i.png)](missing.md)\n"
    );
}

#[test]
fn markdown_conversion_uses_the_root_relative_style_the_root_votes() {
    let a = "[[deep]]\n";
    let files = [("/v/sub/a.md", a), ("/v/sub/deep/deep.md", "# Deep\n")];
    let abs = r#"{"useMarkdownLinks": true, "newLinkFormat": "absolute"}"#;
    let w = ws_with(&files, abs);
    let f = only(fixes(&w, "/v/sub/a.md", 0..0), &FixKind::ConvertLink);
    assert_eq!(titles(&f), ["Convert to Markdown link"]);
    assert_eq!(
        apply(a, "/v/sub/a.md", &f[0].edit),
        "[Deep](sub/deep/deep.md)\n"
    );
    // A wiki root writes file-relative with the extension.
    let w = ws(&files);
    let f = only(fixes(&w, "/v/sub/a.md", 0..0), &FixKind::ConvertLink);
    assert_eq!(
        apply(a, "/v/sub/a.md", &f[0].edit),
        "[Deep](deep/deep.md)\n"
    );
}

#[test]
fn conversion_follows_the_diagnostic_fixes_of_its_link() {
    let a = "[[b#part-tw]]\n";
    let w = ws(&[("/v/a.md", a), BEE]);
    assert_eq!(
        titles(&fixes(&w, "/v/a.md", 0..0)),
        ["Change to #part-two", "Convert to Markdown link"]
    );
}

#[test]
fn no_conversion_for_broken_unindexed_code_or_org_links() {
    let a = "[[gone]] [N](ignored/n.md) `[[b]]` <!-- [[b]] --> [a|b](b.md)\n";
    let mut w = ws(&[
        ("/v/a.md", a),
        BEE,
        ("/v/.gitignore", "ignored/\n"),
        ("/v/ignored/n.md", "# N\n"),
    ]);
    let conv =
        |w: &Workspace, path: &str, r: Range<usize>| only(fixes(w, path, r), &FixKind::ConvertLink);
    let needles = [
        "[[gone]]",
        "[N](ignored/n.md)",
        "`[[b]]`",
        "<!-- [[b]] -->",
        "[a|b](b.md)",
    ];
    for needle in needles {
        assert!(conv(&w, "/v/a.md", at(a, needle)).is_empty(), "{needle}");
    }
    // The unindexed target exists on disk: the link is fine, just not converted.
    let unindexed = at(a, "[N](ignored/n.md)");
    assert!(fixes(&w, "/v/a.md", unindexed).is_empty());
    // In-note anchor links stay.
    let h = "# H\n\n[x](#h) [[#h]]\n";
    w = ws(&[("/v/a.md", h)]);
    assert!(conv(&w, "/v/a.md", 0..h.len()).is_empty());
    // Frontmatter wiki links stay: `[` would start a YAML list.
    let f = "---\nup: \"[[b]]\"\n---\n";
    w = ws(&[("/v/a.md", f), BEE]);
    assert!(conv(&w, "/v/a.md", at(f, "[[b]]")).is_empty());
    let o = "[[file:b.org][B]] [[b]]\n";
    w = ws(&[("/v/a.org", o), ("/v/b.org", "* B\n")]);
    assert!(conv(&w, "/v/a.org", 0..o.len()).is_empty());
}

#[test]
fn selection_links_to_a_note_by_title_prefix() {
    let a = "I saw a Bee today.\n";
    let w = ws(&[("/v/a.md", a), BEE]);
    let f = fixes(&w, "/v/a.md", at(a, "Bee"));
    assert_eq!(titles(&f), ["Link to Bee note"]);
    assert_eq!(f[0].kind, FixKind::LinkSelection);
    assert!(f[0].preferred, "the only candidate");
    assert_eq!(
        apply(a, "/v/a.md", &f[0].edit),
        "I saw a [[b|Bee]] today.\n"
    );
    // Surrounding blanks stay outside the link; the stem itself needs no label.
    let s = at(a, " a Bee ");
    let f = fixes(&w, "/v/a.md", s.start + 2..s.end);
    assert_eq!(
        apply(a, "/v/a.md", &f[0].edit),
        "I saw a [[b|Bee]] today.\n"
    );
    let b = "see b here\n";
    w.set_overlay(&p("/v/a.md"), b).unwrap();
    let f = fixes(&w, "/v/a.md", at(b, " b "));
    assert_eq!(apply(b, "/v/a.md", &f[0].edit), "see [[b]] here\n");
}

#[test]
fn selection_offers_up_to_three_exact_or_prefix_matches() {
    let a = "plan\n";
    let w = ws_with(
        &[
            ("/v/a.md", a),
            ("/v/plan.md", "# Plan\n"),
            ("/v/plan-b.md", ""),
            ("/v/plan-c.md", ""),
            ("/v/plan-d.md", ""),
            ("/v/my-plan.md", ""),
        ],
        r#"{"useMarkdownLinks": true}"#,
    );
    let f = fixes(&w, "/v/a.md", at(a, "plan"));
    assert_eq!(
        titles(&f),
        ["Link to Plan", "Link to plan-b", "Link to plan-c"]
    );
    assert!(f.iter().all(|f| !f.preferred));
    assert_eq!(apply(a, "/v/a.md", &f[0].edit), "[plan](plan.md)\n");
}

#[test]
fn no_link_selection_over_a_link_two_lines_or_no_match() {
    let a = "Bee [[b]] and\nBee here. Zzz.\n";
    let w = ws(&[("/v/a.md", a), BEE]);
    let sel = |r: Range<usize>| only(fixes(&w, "/v/a.md", r), &FixKind::LinkSelection);
    let spanning = at(a, "Bee [[b]]");
    assert!(sel(spanning).is_empty());
    let two_lines = at(a, "and\nBee");
    assert!(sel(two_lines).is_empty());
    assert!(sel(at(a, "Zzz")).is_empty());
    // A cursor selects nothing.
    let s = at(a, "here").start;
    assert!(sel(s..s).is_empty());
    assert!(sel(at(a, "Bee here")).is_empty(), "no title starts with it");
    // Code, comments and frontmatter get none; Org notes neither.
    let c = "---\nk: Bee\n---\n`Bee` <!-- Bee -->\n\n```\nBee\n```\n";
    let w = ws(&[("/v/a.md", c), BEE]);
    for (i, _) in c.match_indices("Bee") {
        assert!(only(fixes(&w, "/v/a.md", i..i + 3), &FixKind::LinkSelection).is_empty());
    }
    let o = "* Bee\n";
    let w = ws(&[("/v/a.org", o), ("/v/b.org", "#+title: Bee note\n")]);
    assert!(fixes(&w, "/v/a.org", at(o, "Bee")).is_empty());
}
