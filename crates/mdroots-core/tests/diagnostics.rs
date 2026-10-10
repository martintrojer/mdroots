use std::path::{Path, PathBuf};
use std::sync::Arc;

use mdroots_core::memstore::counts;
use mdroots_core::{
    AnchorStatus, Cancel, DiagCode, Diagnostic, DiagnosticPolicy, MemFs, MemStore, Severity, StdFs,
};
use mdroots_resolve::ladder::LinkStatus;
use mdroots_syntax::Confidence;

fn mem(fs: MemFs) -> MemStore {
    MemStore::open(Arc::new(fs), PathBuf::from("/"), &Cancel::new()).unwrap()
}

fn diags(fs: MemFs, lazy: bool, rel: &str) -> Vec<Diagnostic> {
    let s = mem(fs);
    DiagnosticPolicy::for_store(&s, lazy).diagnostics(&s, rel)
}

fn codes(d: &[Diagnostic]) -> Vec<DiagCode> {
    d.iter().map(|d| d.code).collect()
}

/// A vault whose `index.md` has `ok` resolving links and `bad` broken ones.
fn vault(ok: usize, bad: usize) -> MemFs {
    let mut text = String::new();
    for i in 0..ok {
        text.push_str(&format!("[o{i}](t.md)\n"));
    }
    for i in 0..bad {
        text.push_str(&format!("[b{i}](gone{i}.md)\n"));
    }
    MemFs::new()
        .with_file("t.md", "# T\n")
        .with_file("index.md", &text)
}

#[test]
fn broken_link() {
    let fs = vault(1, 1);
    let d = diags(fs, false, "index.md");
    assert_eq!(codes(&d), [DiagCode::BrokenLink]);
    assert_eq!(d[0].message, "broken link: gone0.md");
    let src = "[o0](t.md)\n[b0](gone0.md)\n";
    assert_eq!(&src[d[0].range.clone()], "[b0](gone0.md)");
    assert!(d[0].related.is_empty());
}

#[test]
fn severity_follows_resolved_share() {
    let s = mem(vault(99, 1));
    let p = DiagnosticPolicy::for_store(&s, false);
    assert_eq!(p.broken, Some(Severity::Error));
    assert_eq!(p.resolved_share, Some(0.99));
    assert_eq!(p.diagnostics(&s, "index.md")[0].severity, Severity::Error);

    let s = mem(vault(85, 15));
    assert_eq!(
        DiagnosticPolicy::for_store(&s, false).broken,
        Some(Severity::Warning)
    );
    let s = mem(vault(50, 50));
    assert_eq!(
        DiagnosticPolicy::for_store(&s, false).broken,
        Some(Severity::Hint)
    );

    // No explicit links: warning, no share.
    let s = mem(MemFs::new().with_file("a.md", "plain text\n"));
    let p = DiagnosticPolicy::for_store(&s, false);
    assert_eq!(p.broken, Some(Severity::Warning));
    assert_eq!(p.resolved_share, None);
}

#[test]
fn zk_dead_link_config_wins() {
    let zk = |v: &str| {
        vault(50, 50).with_file(
            ".zk/config.toml",
            &format!("[lsp.diagnostics]\ndead-link = \"{v}\"\n"),
        )
    };
    let s = mem(zk("error"));
    let p = DiagnosticPolicy::for_store(&s, false);
    assert_eq!(p.broken, Some(Severity::Error));
    assert!(
        p.diagnostics(&s, "index.md")
            .iter()
            .all(|d| d.severity == Severity::Error)
    );

    let s = mem(zk("none"));
    let p = DiagnosticPolicy::for_store(&s, false);
    assert_eq!(p.broken, None);
    assert!(p.diagnostics(&s, "index.md").is_empty());
}

#[test]
fn broken_anchor() {
    let fs = MemFs::new()
        .with_file("t.md", "# Intro\n\npara ^blk\n")
        .with_file(
            "a.md",
            "[ok](t.md#intro) [bad](t.md#nope) [[t#^blk]] [[t#^gone]]\n",
        );
    let d = diags(fs, false, "a.md");
    assert_eq!(codes(&d), [DiagCode::BrokenAnchor, DiagCode::BrokenAnchor]);
    assert_eq!(d[0].message, "missing anchor: #nope");
    assert_eq!(d[1].message, "missing anchor: #^gone");
}

#[test]
fn ambiguous_link_lists_candidates() {
    let fs = MemFs::new()
        .with_file("x/n.md", "# N\n")
        .with_file("y/n.md", "# N\n")
        .with_file("a.md", "see [[n]]\n");
    let d = diags(fs, false, "a.md");
    assert_eq!(codes(&d), [DiagCode::AmbiguousLink]);
    assert_eq!(d[0].severity, Severity::Info);
    let mut related = d[0].related.clone();
    related.sort();
    assert_eq!(related, ["x/n.md", "y/n.md"]);
}

#[test]
fn invalid_frontmatter_is_info() {
    let src = "---\ntitle: [unclosed\n---\nbody\n";
    let d = diags(MemFs::new().with_file("a.md", src), false, "a.md");
    assert_eq!(codes(&d), [DiagCode::InvalidFrontmatter]);
    assert_eq!(d[0].severity, Severity::Info);
    assert_eq!(d[0].range.start, 0);
    assert!(
        d[0].message.starts_with("frontmatter: "),
        "{}",
        d[0].message
    );

    let d = diags(MemFs::new().with_file("a.md", "---\n---\n"), false, "a.md");
    assert!(d.is_empty());
}

#[test]
fn links_in_code_are_never_diagnosed() {
    let src = "`[[gone]]` and `[x](gone.md)`\n\n```\n[[gone]] [x](gone.md)\n```\n";
    let d = diags(MemFs::new().with_file("a.md", src), false, "a.md");
    assert!(d.is_empty(), "{d:?}");
}

#[test]
fn unindexed_target_is_not_diagnosed() {
    let fs = MemFs::new()
        .with_file("img.png", "not markdown")
        .with_file("a.md", "![i](img.png) [x](img.png)\n");
    assert!(diags(fs, false, "a.md").is_empty());
}

#[test]
fn lazy_mode_checks_only_stat_checkable_links() {
    let fs = MemFs::new().with_file(
        "a.md",
        "[x](missing.md) [y](missing) [[missing]] [[v1.2 notes]] [[sub/gone]]\n",
    );
    let d = diags(fs, true, "a.md");
    let got: Vec<(DiagCode, Severity, &str)> = d
        .iter()
        .map(|d| (d.code, d.severity, d.message.as_str()))
        .collect();
    let broken = DiagnosticPolicy::for_store(
        &mem(MemFs::new().with_file("a.md", "[x](missing.md)\n")),
        true,
    )
    .broken
    .unwrap();
    assert_eq!(
        got,
        [
            (DiagCode::BrokenLink, broken, "broken link: missing.md"),
            (DiagCode::BrokenLink, broken, "broken link: missing"),
            (
                DiagCode::NotInWorkingSet,
                Severity::Hint,
                "not in indexed set: missing"
            ),
            (
                DiagCode::NotInWorkingSet,
                Severity::Hint,
                "not in indexed set: v1.2 notes"
            ),
            (DiagCode::BrokenLink, broken, "broken link: sub/gone"),
        ]
    );
}

#[test]
fn lazy_share_ignores_wiki_stems() {
    // Five resolving path links and many unresolved stems: the stems do
    // not drag the severity down in lazy mode.
    let mut text = String::from("[a](t.md) [b](t.md) [c](t.md) [d](t.md) [e](t.md)\n");
    for i in 0..20 {
        text.push_str(&format!("[[stem{i}]]\n"));
    }
    let s = mem(MemFs::new()
        .with_file("t.md", "# T\n")
        .with_file("a.md", &text));
    let lazy = DiagnosticPolicy::for_store(&s, true);
    assert_eq!(lazy.resolved_share, Some(1.0));
    assert_eq!(lazy.broken, Some(Severity::Error));
    assert_eq!(
        DiagnosticPolicy::for_store(&s, false).broken,
        Some(Severity::Hint)
    );
}

#[test]
fn sorted_by_start() {
    let src = "---\ntitle: [unclosed\n---\n[[gone]] [x](t.md#nope)\n";
    let fs = MemFs::new()
        .with_file("t.md", "# T\n")
        .with_file("a.md", src);
    let d = diags(fs, false, "a.md");
    assert_eq!(
        codes(&d),
        [
            DiagCode::InvalidFrontmatter,
            DiagCode::BrokenLink,
            DiagCode::BrokenAnchor
        ]
    );
    assert!(d.is_sorted_by_key(|d| d.range.start));
}

#[test]
fn zkvault_matches_memstore() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus/zkvault");
    let s = MemStore::open(Arc::new(StdFs), root, &Cancel::new()).unwrap();
    let p = DiagnosticPolicy::for_store(&s, false);
    assert_eq!(p.broken, Some(Severity::Hint));

    let files: Vec<String> = s.files().map(str::to_owned).collect();
    let (mut links, mut anchors, mut ambiguous) = (0, 0, 0);
    for f in &files {
        let d = p.diagnostics(&s, f);
        let ranges = |c: DiagCode| -> Vec<_> {
            d.iter()
                .filter(|d| d.code == c)
                .map(|d| d.range.clone())
                .collect()
        };
        // Explicit links in referencing contexts, as the store resolves them.
        let explicit: Vec<_> = s
            .links(f)
            .into_iter()
            .filter(|(l, _)| l.confidence == Confidence::Explicit && counts(l))
            .collect();
        let want: Vec<_> = explicit
            .iter()
            .filter(|(_, r)| r.status == LinkStatus::Broken)
            .map(|(l, _)| l.range.clone())
            .collect();
        assert_eq!(ranges(DiagCode::BrokenLink), want, "{f}");
        let want: Vec<_> = explicit
            .iter()
            .filter(|(l, r)| {
                let (Some(anchor), [target]) = (&l.target.anchor, r.targets.as_slice()) else {
                    return false;
                };
                r.status == LinkStatus::Resolved
                    && s.check_anchor(target, anchor) == AnchorStatus::Missing
            })
            .map(|(l, _)| l.range.clone())
            .collect();
        assert_eq!(ranges(DiagCode::BrokenAnchor), want, "{f}");
        links += ranges(DiagCode::BrokenLink).len();
        anchors += ranges(DiagCode::BrokenAnchor).len();
        ambiguous += ranges(DiagCode::AmbiguousLink).len();
    }
    assert_eq!((links, anchors, ambiguous), (5, 0, 0));
}

/// 500 notes linking to each other: some links broken, some ambiguous
/// (the `dup` stem exists twice), some with missing anchors.
fn big_root() -> MemFs {
    let mut fs = MemFs::new()
        .with_file("x/dup.md", "# Dup\n")
        .with_file("y/dup.md", "# Dup\n");
    for i in 0..500 {
        let mut text = format!(
            "# Note {i}\n\n[[n{:03}]] [[n{:03}#Note]]\n",
            (i * 7) % 500,
            (i + 1) % 500
        );
        if i % 9 == 0 {
            text.push_str(&format!("[[gone{i}]]\n"));
        }
        if i % 13 == 0 {
            text.push_str("[[dup]]\n");
        }
        if i % 17 == 0 {
            text.push_str(&format!("[[n{:03}#nowhere]]\n", (i + 3) % 500));
        }
        fs = fs.with_file(&format!("n{i:03}.md"), &text);
    }
    fs
}

#[test]
fn cached_policy_gives_the_fresh_result_for_every_file() {
    let s = mem(big_root());
    for lazy in [false, true] {
        let fresh = DiagnosticPolicy::for_store(&s, lazy);
        assert_eq!(s.policy(lazy), fresh);
        let mut total = 0;
        for rel in s.files() {
            let want = fresh.diagnostics(&s, rel);
            total += want.len();
            assert_eq!(
                s.policy(lazy).diagnostics(&s, rel),
                want,
                "{rel} lazy={lazy}"
            );
        }
        assert!(total > 0, "the root has diagnostics to compare");
    }
}

#[test]
fn policy_cache_is_dropped_by_set_overlay() {
    let mut s = mem(vault(3, 1));
    let before = s.policy(false).resolved_share;
    assert_eq!(
        before,
        DiagnosticPolicy::for_store(&s, false).resolved_share
    );
    assert_eq!(before, Some(0.75));

    // Fix the broken link: every link resolves now.
    s.set_overlay(
        "index.md",
        "[o0](t.md)\n[o1](t.md)\n[o2](t.md)\n[b0](t.md)\n",
    );
    let after = s.policy(false).resolved_share;
    assert_eq!(after, DiagnosticPolicy::for_store(&s, false).resolved_share);
    assert_eq!(after, Some(1.0));

    s.clear_overlay("index.md");
    assert_eq!(s.policy(false).resolved_share, Some(0.75));
}
