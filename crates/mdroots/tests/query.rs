//! `mdroots::query`: tag expressions and dates in the syntax of
//! [zk](https://github.com/zk-org/zk), and `Workspace::query` on in-memory
//! roots (`MemFs` with a `FakeProbe` holding the same files). Link-graph
//! filters backed by the graph module (orphan, missing backlink, related)
//! are tested with the CLI.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mdroots::query::{NoteQuery, SortKey, TagExpr, day_range, parse_date, parse_sort};
use mdroots::{ErrorKind, Freshness, IndexMode, NoEnumerator, Options, Workspace};
use mdroots_core::MemFs;
use mdroots_roots::probe::{FakeProbe, MountInfo};

fn tags(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

// --- tag expressions -------------------------------------------------------

#[test]
fn tag_expressions_follow_zk() {
    // (expression, note tags, matches)
    let table: &[(&str, &[&str], bool)] = &[
        ("a", &["a"], true),
        ("a", &["b"], false),
        ("a", &[], false),
        ("#a", &["a"], true),
        ("A", &["a"], true),
        ("a", &["A"], true),
        // comma: and
        ("a, b", &["a", "b"], true),
        ("a, b", &["a"], false),
        ("a,b,c", &["c", "b", "a"], true),
        // OR and |: or
        ("a OR b", &["b"], true),
        ("a|b", &["a"], true),
        ("a | b", &["c"], false),
        // lowercase "or" is part of a name, not a keyword
        ("a or b", &["a"], false),
        ("a or b", &["a or b"], true),
        // NOT and -: not
        ("NOT a", &["b"], true),
        ("NOT a", &["a"], false),
        ("-a", &[], true),
        ("-#a", &["A"], false),
        ("not", &["not"], true),
        // OR binds tighter than the comma
        ("inbox OR todo, NOT done", &["todo"], true),
        ("inbox OR todo, NOT done", &["todo", "done"], false),
        ("inbox OR todo, NOT done", &["done"], false),
        ("a, b OR c", &["a", "c"], true),
        ("a, b OR c", &["b", "c"], false),
        // globs
        ("year/201*", &["year/2019"], true),
        ("year/201*", &["year/2020"], false),
        ("year/201?", &["year/2015"], true),
        ("year/201?", &["year/201"], false),
        ("*", &["x"], true),
        ("*", &[], false),
        ("Proj*", &["project"], true),
        ("*a*b", &["xxaxxb"], true),
        ("*a*b", &["xxaxxbx"], false),
        ("café", &["CAFÉ"], true),
    ];
    for (expr, note, want) in table {
        let e = TagExpr::parse(expr).unwrap_or_else(|e| panic!("{expr}: {e}"));
        assert_eq!(e.matches(&tags(note)), *want, "{expr:?} on {note:?}");
    }
}

#[test]
fn tag_expressions_take_and_and_parentheses() {
    let table: &[(&str, &[&str], bool)] = &[
        // AND: the same as a comma
        ("career AND projects", &["career", "projects"], true),
        ("career AND projects", &["career"], false),
        ("career AND NOT projects", &["career"], true),
        ("career AND NOT projects", &["career", "projects"], false),
        ("a AND b, c", &["a", "b", "c"], true),
        // lowercase "and" stays part of a name
        ("a and b", &["a and b"], true),
        // parentheses group
        ("(xt26)", &["xt26"], true),
        ("(xt26)", &["career"], false),
        ("(a OR b), NOT c", &["b"], true),
        ("(a OR b), NOT c", &["b", "c"], false),
        ("(a OR b) AND NOT c", &["a"], true),
        ("a OR (b, c)", &["b", "c"], true),
        ("a OR (b, c)", &["b"], false),
        ("a OR (b, c)", &["a"], true),
        ("NOT (a OR b)", &["c"], true),
        ("NOT (a OR b)", &["a"], false),
        ("-(a, b)", &["a"], true),
        ("-(a, b)", &["a", "b"], false),
        ("((a))", &["a"], true),
        // tabs separate keywords like spaces
        ("a\tAND\tb", &["a", "b"], true),
        ("a AND\tb", &["a"], false),
        ("a\tOR\tb", &["b"], true),
        ("NOT\ta", &["b"], true),
        // keyword-shaped names stay names
        ("ANDROID", &["android"], true),
        ("ORANGE, NOTES", &["orange", "notes"], true),
        // a negated tag inside an or group is fine with parentheses
        ("a OR (NOT b)", &[], true),
        ("a OR (NOT b)", &["b"], false),
    ];
    for (expr, note, want) in table {
        let e = TagExpr::parse(expr).unwrap_or_else(|e| panic!("{expr}: {e}"));
        assert_eq!(e.matches(&tags(note)), *want, "{expr:?} on {note:?}");
    }
}

#[test]
fn tag_expression_syntax_errors() {
    let table: &[(&str, &str)] = &[
        ("(a", "column 2: expected )"),
        ("a)", "column 1: unexpected )"),
        ("()", "column 1: expected a tag"),
        ("a AND", "column 5: expected a tag"),
        ("AND a", "column 0: expected a tag"),
        ("a AND OR b", "column 6: expected a tag"),
    ];
    for (expr, want) in table {
        assert_eq!(TagExpr::parse(expr).unwrap_err(), *want, "{expr:?}");
    }
}

#[test]
fn tag_expression_errors_name_a_column() {
    let table: &[(&str, &str)] = &[
        ("", "column 0: expected a tag"),
        ("a,", "column 2: expected a tag"),
        ("a, ", "column 3: expected a tag"),
        ("a OR ", "column 5: expected a tag"),
        ("a, -", "column 4: expected a tag"),
        ("a OR NOT b", "column 9: cannot negate a tag in an OR group"),
        ("x, -a|b", "column 4: cannot negate a tag in an OR group"),
    ];
    for (expr, want) in table {
        assert_eq!(TagExpr::parse(expr).unwrap_err(), *want, "{expr:?}");
    }
}

// --- dates -----------------------------------------------------------------

/// Seconds since the epoch as a time.
fn at(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

/// 2026-10-10T12:30:00Z, a Saturday.
const NOW: u64 = 1_791_635_400;
const DAY: u64 = 86_400;
/// 2026-10-10T00:00:00Z.
const TODAY: u64 = NOW - 12 * 3600 - 30 * 60;

#[test]
fn absolute_dates() {
    let now = at(NOW);
    let table: &[(&str, u64)] = &[
        ("1970-01-01", 0),
        ("2026-10-10", TODAY),
        ("2026", 1_767_225_600),
        ("2026-10", TODAY - 9 * DAY),
        ("2024-02-29", 1_709_164_800),
        ("2026-10-10T12:30", NOW),
        ("2026-10-10 12:30", NOW),
        ("2026-10-10T12:30:00", NOW),
        ("2026-10-10T12:30:00Z", NOW),
        ("2026-10-10t12:30:00z", NOW),
        ("2026-10-10T14:30:00+02:00", NOW),
        ("2026-10-10T07:30:00-05:00", NOW),
        (" 2026-10-10 ", TODAY),
    ];
    for (s, want) in table {
        assert_eq!(parse_date(s, now), Ok(at(*want)), "{s:?}");
    }
    assert_eq!(
        parse_date("2026-10-10T12:30:00.25Z", now),
        Ok(at(NOW) + Duration::from_millis(250))
    );
    assert_eq!(
        parse_date("1969-12-31", now),
        Ok(UNIX_EPOCH - Duration::from_secs(DAY))
    );
}

#[test]
fn relative_dates() {
    let now = at(NOW);
    let table: &[(&str, u64)] = &[
        ("now", NOW),
        ("today", TODAY),
        ("Today", TODAY),
        ("yesterday", TODAY - DAY),
        ("2 days ago", NOW - 2 * DAY),
        ("1 day ago", NOW - DAY),
        ("an hour ago", NOW - 3600),
        ("3 weeks ago", NOW - 21 * DAY),
        ("last week", NOW - 7 * DAY),
        ("last two weeks", NOW - 14 * DAY),
        ("last 7 days", NOW - 7 * DAY),
        ("last 30 days", NOW - 30 * DAY),
        ("7d", NOW - 7 * DAY),
        ("2w", NOW - 14 * DAY),
        // 2026-09-10T12:30Z and 2025-10-10T12:30Z
        ("last month", NOW - 30 * DAY),
        ("1 year ago", NOW - 365 * DAY),
        // Saturday today: the previous Monday, and a week back for Saturday
        ("last monday", TODAY - 5 * DAY),
        ("last friday", TODAY - DAY),
        ("last saturday", TODAY - 7 * DAY),
    ];
    for (s, want) in table {
        assert_eq!(parse_date(s, now), Ok(at(*want)), "{s:?}");
    }
}

#[test]
fn months_back_clamp_the_day() {
    // 2026-03-31T00:00Z, one month back: 2026-02-28.
    let mar31 = parse_date("2026-03-31", at(0)).unwrap();
    assert_eq!(
        parse_date("last month", mar31),
        parse_date("2026-02-28", at(0))
    );
}

#[test]
fn bad_dates_are_errors() {
    for s in [
        "",
        "soon",
        "2026-13-01",
        "2026-02-30",
        "2025-02-29",
        "2026-1-1",
        "2026-10-10T25:00",
        "2026-10-10T12:30+0200",
        "2026-10-10X12:30",
        "last fortnight",
        "x days ago",
    ] {
        assert!(parse_date(s, at(NOW)).is_err(), "{s:?}");
    }
}

#[test]
fn day_range_is_one_utc_day() {
    assert_eq!(
        day_range("2026-10-10T12:30Z", at(NOW)),
        Ok((at(TODAY), at(TODAY + DAY)))
    );
    assert_eq!(
        day_range("yesterday", at(NOW)),
        Ok((at(TODAY - DAY), at(TODAY)))
    );
}

#[test]
fn sort_keys_follow_zk() {
    let table: &[(&str, (SortKey, bool))] = &[
        ("title", (SortKey::Title, true)),
        ("t", (SortKey::Title, true)),
        ("title-", (SortKey::Title, false)),
        ("path", (SortKey::Path, true)),
        ("p-", (SortKey::Path, false)),
        ("created", (SortKey::Created, false)),
        ("c+", (SortKey::Created, true)),
        ("modified", (SortKey::Modified, false)),
        ("m", (SortKey::Modified, false)),
        ("Modified+", (SortKey::Modified, true)),
    ];
    for (s, want) in table {
        assert_eq!(parse_sort(s), Ok(*want), "{s:?}");
    }
    assert!(parse_sort("random").is_err());
}

// --- Workspace::query ------------------------------------------------------

/// Written in this order, so each note's (in-memory) mtime is later than
/// the previous one's: b, a, c, d, e, idx.
const VAULT: &[(&str, &str)] = &[
    ("/v/.mdroots", ""),
    (
        "/v/b.md",
        "---\ntitle: Beta\ndate: 2026-01-15\ntags: [work]\n---\nLinks [[a]].\n",
    ),
    (
        "/v/a.md",
        "---\ntitle: alpha\ncreated: 2026-03-01T10:00:00Z\ntags: [Work, year/2019]\n---\n\
         The quick brown fox. [[b]] [[c]]\n",
    ),
    ("/v/sub/c.md", "# Gamma\n\nquick notes #todo\n`[[d]]`\n"),
    ("/v/sub/deep/d.md", "# delta\n\n#inbox #done\n[[c]] [[d]]\n"),
    ("/v/e.md", "---\ntitle: Epsilon\ndate: soon\n---\nplain\n"),
    ("/v/idx.md", "# Index\n\n[[e]]\n"),
];

fn vault() -> Workspace {
    let mut fs = MemFs::new();
    let mut probe = FakeProbe::new().home("/h");
    for (path, text) in VAULT {
        fs = fs.with_file(path, text);
        probe = probe.file(path, text);
    }
    let opts = Options::default()
        .fs(Arc::new(fs))
        .probe(Arc::new(probe))
        .enumerator(Arc::new(NoEnumerator))
        .index(IndexMode::Memory);
    let ws = Workspace::open_for(Path::new("/v/a.md"), opts).unwrap();
    assert_eq!(ws.freshness(), Freshness::Fresh);
    ws
}

/// The file names of the notes `q` returns, in order.
fn names(ws: &Workspace, q: &NoteQuery) -> Vec<String> {
    ws.query(q)
        .unwrap()
        .into_iter()
        .map(|n| n.path.file_name().unwrap().to_string_lossy().into_owned())
        .collect()
}

fn p(s: &str) -> PathBuf {
    PathBuf::from(s)
}

fn tag(s: &str) -> TagExpr {
    TagExpr::parse(s).unwrap()
}

#[test]
fn no_filter_returns_every_note_by_title() {
    let ws = vault();
    // alpha, Beta, delta, Epsilon, Gamma, Index: case-insensitive A–Z
    assert_eq!(
        names(&ws, &NoteQuery::default()),
        ["a.md", "b.md", "d.md", "e.md", "c.md", "idx.md"]
    );
}

#[test]
fn sort_keys_and_directions() {
    let ws = vault();
    let sorted = |key, asc| {
        let mut q = NoteQuery::default();
        q.sort = Some((key, asc));
        names(&ws, &q)
    };
    assert_eq!(
        sorted(SortKey::Title, false),
        ["idx.md", "c.md", "e.md", "d.md", "b.md", "a.md"]
    );
    assert_eq!(
        sorted(SortKey::Path, true),
        ["a.md", "b.md", "e.md", "idx.md", "c.md", "d.md"]
    );
    assert_eq!(
        sorted(SortKey::Path, false),
        ["d.md", "c.md", "idx.md", "e.md", "b.md", "a.md"]
    );
    // newest first by default (written b, a, c, d, e, idx)
    let (key, asc) = parse_sort("modified").unwrap();
    assert_eq!(
        sorted(key, asc),
        ["idx.md", "e.md", "d.md", "c.md", "a.md", "b.md"]
    );
    assert_eq!(
        sorted(SortKey::Modified, true),
        ["b.md", "a.md", "c.md", "d.md", "e.md", "idx.md"]
    );
    // only a and b have a parseable created date; the rest follow by title
    let (key, asc) = parse_sort("created").unwrap();
    assert_eq!(
        sorted(key, asc),
        ["a.md", "b.md", "d.md", "e.md", "c.md", "idx.md"]
    );
    assert_eq!(
        sorted(SortKey::Created, true),
        ["b.md", "a.md", "d.md", "e.md", "c.md", "idx.md"]
    );
}

#[test]
fn limit_applies_after_the_sort() {
    let ws = vault();
    let mut q = NoteQuery::default();
    q.limit = Some(2);
    assert_eq!(names(&ws, &q), ["a.md", "b.md"]);
    q.sort = Some((SortKey::Modified, false));
    assert_eq!(names(&ws, &q), ["idx.md", "e.md"]);
    q.limit = Some(0);
    assert!(names(&ws, &q).is_empty());
    q.limit = Some(100);
    assert_eq!(names(&ws, &q).len(), 6);
}

#[test]
fn paths_and_exclude_are_prefixes() {
    let ws = vault();
    let mut q = NoteQuery::default();
    q.paths = vec![p("/v/sub")];
    assert_eq!(names(&ws, &q), ["d.md", "c.md"]);
    q.exclude = vec![p("/v/sub/deep")];
    assert_eq!(names(&ws, &q), ["c.md"]);
    // a file path; and a prefix is a whole component (not "/v/su")
    q = NoteQuery::default();
    q.paths = vec![p("/v/a.md"), p("/v/su")];
    assert_eq!(names(&ws, &q), ["a.md"]);
    q = NoteQuery::default();
    q.exclude = vec![p("/v/sub"), p("/v/idx.md")];
    assert_eq!(names(&ws, &q), ["a.md", "b.md", "e.md"]);
}

#[test]
fn tag_filters_and_tagless() {
    let ws = vault();
    let mut q = NoteQuery::default();
    q.tag = vec![tag("work")];
    assert_eq!(names(&ws, &q), ["a.md", "b.md"]);
    q.tag = vec![tag("work"), tag("year/201*")];
    assert_eq!(names(&ws, &q), ["a.md"]);
    q.tag = vec![tag("todo OR inbox, -done")];
    assert_eq!(names(&ws, &q), ["c.md"]);
    q.tag = vec![tag("NOT work")];
    assert_eq!(names(&ws, &q), ["d.md", "e.md", "c.md", "idx.md"]);
    q = NoteQuery::default();
    q.tagless = true;
    assert_eq!(names(&ws, &q), ["e.md", "idx.md"]);
}

#[test]
fn matching_uses_full_text() {
    let ws = vault();
    let mut q = NoteQuery::default();
    q.matching = Some("quick".into());
    assert_eq!(names(&ws, &q), ["a.md", "c.md"]);
    q.matching = Some("quick fox".into());
    assert_eq!(names(&ws, &q), ["a.md"]);
    q.matching = Some("nothing-here".into());
    assert!(names(&ws, &q).is_empty());
}

#[test]
fn created_bounds_use_the_frontmatter_date() {
    let ws = vault();
    let d = |s| parse_date(s, at(NOW)).unwrap();
    let mut q = NoteQuery::default();
    q.created_after = Some(d("2026-02-01"));
    assert_eq!(names(&ws, &q), ["a.md"]);
    q = NoteQuery::default();
    q.created_before = Some(d("2026-02-01"));
    assert_eq!(names(&ws, &q), ["b.md"]);
    // after is inclusive, before exclusive
    q.created_after = Some(d("2026-01-15"));
    assert_eq!(names(&ws, &q), ["b.md"]);
    q.created_before = Some(d("2026-01-15"));
    assert!(names(&ws, &q).is_empty());
    // a day range
    let (start, end) = day_range("2026-03-01", at(NOW)).unwrap();
    q.created_after = Some(start);
    q.created_before = Some(end);
    assert_eq!(names(&ws, &q), ["a.md"]);
}

#[test]
fn modified_bounds_use_the_file_time() {
    let ws = vault();
    let all = ws.query(&NoteQuery::default()).unwrap();
    let mtime = |name: &str| {
        all.iter()
            .find(|n| n.path.ends_with(name))
            .and_then(|n| n.modified)
            .unwrap()
    };
    let mut q = NoteQuery::default();
    q.modified_after = Some(mtime("d.md"));
    assert_eq!(names(&ws, &q), ["d.md", "e.md", "idx.md"]);
    q.modified_before = Some(mtime("idx.md"));
    assert_eq!(names(&ws, &q), ["d.md", "e.md"]);
}

#[test]
fn link_to_and_linked_by() {
    let ws = vault();
    let mut q = NoteQuery::default();
    // notes linking to c: a (d links to c too); d's [[d]] is a self-link
    q.link_to = vec![p("/v/sub/c.md")];
    assert_eq!(names(&ws, &q), ["a.md", "d.md"]);
    q.link_to = vec![p("/v/sub/deep/d.md")];
    // c's link to d is in code, d's own is a self-link
    assert!(names(&ws, &q).is_empty());
    q = NoteQuery::default();
    q.linked_by = vec![p("/v/a.md")];
    assert_eq!(names(&ws, &q), ["b.md", "c.md"]);
    // Repeated, every source must link: a and idx share no target, a and
    // d share c.
    q.linked_by = vec![p("/v/a.md"), p("/v/idx.md")];
    assert!(names(&ws, &q).is_empty());
    q.linked_by = vec![p("/v/a.md"), p("/v/sub/deep/d.md")];
    assert_eq!(names(&ws, &q), ["c.md"]);
    // combined with other filters
    q.linked_by = vec![p("/v/a.md")];
    q.tag = vec![tag("todo")];
    assert_eq!(names(&ws, &q), ["c.md"]);
}

#[test]
fn filters_combine() {
    let ws = vault();
    let mut q = NoteQuery::default();
    q.tag = vec![tag("work")];
    q.matching = Some("links".into());
    assert_eq!(names(&ws, &q), ["b.md"]);
    q.exclude = vec![p("/v/b.md")];
    assert!(names(&ws, &q).is_empty());
}

#[test]
fn graph_filters_on_a_lazy_root_are_unsupported() {
    let nfs = MountInfo {
        fs_type: "nfs".into(),
        from: "server:/export".into(),
        local: false,
        dev: 7,
    };
    let mut fs = MemFs::new();
    let mut probe = FakeProbe::new().home("/h");
    for (path, text) in [("/net/a.md", "[[b]] #x"), ("/net/b.md", "")] {
        fs = fs.with_file(path, text);
        probe = probe.file(path, text);
    }
    let probe = probe.mount("/net", nfs);
    let opts = Options::default()
        .fs(Arc::new(fs))
        .probe(Arc::new(probe))
        .enumerator(Arc::new(NoEnumerator))
        .index(IndexMode::Memory);
    let ws = Workspace::open_for(Path::new("/net/a.md"), opts).unwrap();
    assert_eq!(ws.freshness(), Freshness::Lazy);

    // non-graph filters answer over the working set
    let mut q = NoteQuery::default();
    q.tag = vec![tag("x")];
    assert_eq!(names(&ws, &q), ["a.md"]);

    let graph: [fn(&mut NoteQuery); 5] = [
        |q| q.orphan = true,
        |q| q.missing_backlink = true,
        |q| q.link_to = vec![p("/net/b.md")],
        |q| q.linked_by = vec![p("/net/a.md")],
        |q| q.related = vec![p("/net/a.md")],
    ];
    for set in graph {
        let mut q = NoteQuery::default();
        set(&mut q);
        let err = ws.query(&q).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Unsupported, "{q:?}");
    }
}

#[test]
fn created_falls_back_to_the_file_birth_time() {
    // No frontmatter date: the created filter uses the file's birth time.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("a.md"), "# A\n").unwrap();
    let ws = Workspace::open_at(&root, Options::default().index(IndexMode::Memory)).unwrap();
    let n = ws.notes().into_iter().next().unwrap();
    let Some(born) = n.created else {
        return; // This filesystem reports no birth time.
    };
    let mut q = NoteQuery::default();
    q.created_after = Some(born - Duration::from_secs(60));
    q.created_before = Some(born + Duration::from_secs(60));
    assert_eq!(ws.query(&q).unwrap().len(), 1);
    q.created_after = Some(born + Duration::from_secs(60));
    q.created_before = None;
    assert!(ws.query(&q).unwrap().is_empty());
}
