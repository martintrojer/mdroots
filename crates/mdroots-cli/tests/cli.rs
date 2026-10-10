//! Runs the built `mdroots` binary on copies of tests/corpus in temp dirs.
//! Snapshots are [insta](https://insta.rs) files with the temp dir replaced
//! by `<TMP>` (plain string replacement: insta's `filters` feature would
//! pull in regex).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

/// A temp dir holding a copy of one corpus vault at `<tmp>/vault`.
struct Vault {
    tmp: TempDir,
    /// Canonical (`/private/var/...` on macOS).
    canon: PathBuf,
}

impl Vault {
    fn corpus(name: &str) -> Vault {
        let tmp = tempfile::tempdir().unwrap();
        let canon = fs::canonicalize(tmp.path()).unwrap();
        let src = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/corpus")
            .join(name);
        copy_dir(&src, &canon.join("vault"));
        Vault { tmp, canon }
    }

    fn dir(&self) -> PathBuf {
        self.canon.join("vault")
    }

    fn write(&self, rel: &str, text: &str) {
        let p = self.dir().join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    fn remove(&self, rel: &str) {
        fs::remove_file(self.dir().join(rel)).unwrap();
    }

    /// Run `mdroots args` in the vault directory.
    fn run(&self, args: &[&str]) -> Run {
        run_in(&self.dir(), args, self)
    }

    fn redact(&self, s: &str) -> String {
        let s = s.replace(&self.canon.display().to_string(), "<TMP>");
        s.replace(&self.tmp.path().display().to_string(), "<TMP>")
    }
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run_in(cwd: &Path, args: &[&str], v: &Vault) -> Run {
    // The cache dir resolves under XDG_CACHE_HOME: never the real one. An
    // inherited MDROOTS_CACHE_DIR (CI sets one) would override it.
    let cache = v.canon.join("cache");
    let out = Command::new(env!("CARGO_BIN_EXE_mdroots"))
        .args(args)
        .current_dir(cwd)
        .env_remove("MDROOTS_CACHE_DIR")
        .env("XDG_CACHE_HOME", &cache)
        .env("HOME", &cache)
        .output()
        .unwrap();
    Run {
        code: out.status.code().unwrap(),
        stdout: v.redact(&String::from_utf8(out.stdout).unwrap()),
        stderr: v.redact(&String::from_utf8(out.stderr).unwrap()),
    }
}

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

/// Walk timings in discovery's reason vary run to run: `in 3 ms` -> `in N ms`.
fn redact_ms(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find(" in ") {
        let (head, tail) = rest.split_at(i + 4);
        out.push_str(head);
        let digits = tail
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(tail.len());
        if digits > 0 && tail[digits..].starts_with(" ms") {
            out.push('N');
            rest = &tail[digits..];
        } else {
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

#[test]
fn no_args_prints_usage_and_exits_2() {
    let v = Vault::corpus("zk-min");
    let r = v.run(&[]);
    assert_eq!(r.code, 2);
    assert!(r.stdout.is_empty());
    assert!(r.stderr.starts_with("usage: mdroots"), "{}", r.stderr);
}

#[test]
fn help_and_unknown_args_print_usage_and_exit_2() {
    let v = Vault::corpus("zk-min");
    for args in [
        &["--help"][..],
        &["-h"],
        &["check", "-h"],
        &["frobnicate"],
        &["check", "--bogus"],
        &["roots"],
        &["roots", "a.md", "b.md"],
        &["resolve", "a.md"],
        &["backlinks"],
        &["search"],
        &["search", "--"],
        &["search", "-x"],
        &["search", "q", "a.md", "b.md"],
        &["search", "q", "--bogus"],
    ] {
        let r = v.run(args);
        assert_eq!(r.code, 2, "{args:?}");
        assert!(
            r.stderr.starts_with("usage: mdroots"),
            "{args:?}: {}",
            r.stderr
        );
    }
    insta::assert_snapshot!("usage", v.run(&[]).stderr);
}

#[test]
fn version_prints_the_version_and_exits_0() {
    let v = Vault::corpus("zk-min");
    for flag in ["--version", "-V"] {
        let r = v.run(&[flag]);
        assert_eq!(r.code, 0, "{flag}");
        assert_eq!(
            r.stdout,
            format!("mdroots {}\n", env!("CARGO_PKG_VERSION")),
            "{flag}"
        );
        assert!(r.stderr.is_empty(), "{flag}: {}", r.stderr);
    }
}

/// Frames each JSON-RPC message with its Content-Length header.
fn frame(msgs: &[serde_json::Value]) -> Vec<u8> {
    let mut out = Vec::new();
    for m in msgs {
        let body = m.to_string();
        out.extend_from_slice(format!("Content-Length: {}\r\n\r\n{body}", body.len()).as_bytes());
    }
    out
}

/// Runs `mdroots lsp <extra>` with `stdin` and the cache dirs pointed into
/// the vault's temp dir; returns (exit code, stdout).
fn run_lsp(v: &Vault, extra: &[&str], stdin: &[u8]) -> (i32, String) {
    use std::io::Write;
    let cache = v.canon.join("cache");
    let mut child = Command::new(env!("CARGO_BIN_EXE_mdroots"))
        .arg("lsp")
        .args(extra)
        .current_dir(v.dir())
        .env_remove("MDROOTS_CACHE_DIR")
        .env("XDG_CACHE_HOME", &cache)
        .env("HOME", &cache)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap(),
        String::from_utf8(out.stdout).unwrap(),
    )
}

fn lsp_session() -> Vec<u8> {
    use serde_json::json;
    frame(&[
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
               "params": {"capabilities": {"general": {"positionEncodings": ["utf-8"]}}}}),
        json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "shutdown"}),
        json!({"jsonrpc": "2.0", "method": "exit"}),
    ])
}

#[test]
fn lsp_answers_initialize_and_exits_0_after_shutdown() {
    let v = Vault::corpus("zk-min");
    let (code, stdout) = run_lsp(&v, &[], &lsp_session());
    assert_eq!(code, 0, "{stdout}");
    assert!(stdout.contains(r#""positionEncoding":"utf-8""#), "{stdout}");
    assert!(stdout.contains(r#""name":"mdroots""#), "{stdout}");
    assert!(stdout.contains(r#""id":2"#), "{stdout}");
}

#[test]
fn lsp_accepts_and_ignores_stdio() {
    let v = Vault::corpus("zk-min");
    let log = v.canon.join("lsp.log");
    let log = log.to_str().unwrap();
    for extra in [
        &["--stdio"][..],
        &["--stdio", "--log", log],
        &["--log", log, "--stdio"],
    ] {
        let (code, stdout) = run_lsp(&v, extra, &lsp_session());
        assert_eq!(code, 0, "{extra:?}: {stdout}");
        assert!(stdout.contains(r#""name":"mdroots""#), "{extra:?}");
    }
}

#[test]
fn lsp_exits_0_at_stdin_eof() {
    let v = Vault::corpus("zk-min");
    let (code, stdout) = run_lsp(&v, &[], b"");
    assert_eq!(code, 0, "{stdout}");
}

#[test]
fn lsp_log_appends_one_line_per_message() {
    let v = Vault::corpus("zk-min");
    let log = v.canon.join("lsp.log");
    let (code, _) = run_lsp(&v, &["--log", log.to_str().unwrap()], &lsp_session());
    assert_eq!(code, 0);
    let text = fs::read_to_string(&log).unwrap();
    // Two threads write the log, so only the order per direction is fixed.
    let lines: Vec<&str> = text.lines().map(|l| l.split_once(' ').unwrap().1).collect();
    let dir =
        |d: &str| -> Vec<&str> { lines.iter().copied().filter(|l| l.starts_with(d)).collect() };
    assert_eq!(
        dir("<-"),
        [
            "<- initialize id=1",
            "<- initialized",
            "<- shutdown id=2",
            "<- exit"
        ]
    );
    assert_eq!(dir("->"), ["-> response id=1", "-> response id=2"]);
    assert_eq!(lines.len(), 6, "{text}");
}

#[test]
fn lsp_rejects_unknown_arguments() {
    let v = Vault::corpus("zk-min");
    for args in [
        &["lsp", "--bogus"][..],
        &["lsp", "--log"],
        &["lsp", "x"],
        &["lsp", "--stdio", "--stdio"],
    ] {
        let r = v.run(args);
        assert_eq!(r.code, 2, "{args:?}");
        assert!(r.stderr.starts_with("usage: mdroots"), "{args:?}");
    }
}

#[test]
fn errors_print_the_message_and_exit_2() {
    let v = Vault::corpus("zk-min");
    let r = v.run(&["check", "missing.md"]);
    assert_eq!(r.code, 2);
    assert!(r.stderr.starts_with("mdroots: "), "{}", r.stderr);
    assert!(r.stderr.contains("missing.md"), "{}", r.stderr);
    assert!(r.stdout.is_empty());
}

#[test]
fn check_on_a_clean_vault_exits_0() {
    let v = Vault::corpus("zk-min");
    v.remove("broken.md");
    let r = v.run(&["check"]);
    assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);
    assert_eq!(r.stdout, "");
    assert_eq!(r.stderr, "5 files, 0 errors, 0 warnings, 0 info, 0 hints\n");
}

#[test]
fn check_reports_the_one_broken_link_and_exits_1() {
    // 4 of 5 explicit links resolve: 80%, so broken links are warnings.
    let v = Vault::corpus("zk-min");
    let r = v.run(&["check"]);
    assert_eq!(r.code, 1);
    insta::assert_snapshot!("check_zk_min", format!("{}---\n{}", r.stdout, r.stderr));
}

#[test]
fn check_quiet_drops_the_summary() {
    let v = Vault::corpus("zk-min");
    let r = v.run(&["check", "--quiet"]);
    assert_eq!(r.code, 1);
    assert_eq!(
        r.stdout,
        "broken.md:3:16: warning: broken link: missing-note\n"
    );
    assert_eq!(r.stderr, "");
}

#[test]
fn check_with_zk_dead_link_error_exits_1() {
    let v = Vault::corpus("zk-min");
    v.write(
        ".zk/config.toml",
        "[note]\nfilename = \"{{id}}\"\n[lsp.diagnostics]\ndead-link = \"error\"\n",
    );
    let r = v.run(&["check", "--quiet"]);
    assert_eq!(r.code, 1);
    assert_eq!(
        r.stdout,
        "broken.md:3:16: error: broken link: missing-note\n"
    );
}

#[test]
fn check_with_zk_dead_link_none_exits_0() {
    let v = Vault::corpus("zk-min");
    v.write(
        ".zk/config.toml",
        "[note]\nfilename = \"{{id}}\"\n[lsp.diagnostics]\ndead-link = \"none\"\n",
    );
    let r = v.run(&["check"]);
    assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);
    assert_eq!(r.stdout, "");
}

#[test]
fn check_where_every_link_is_broken_reports_hints_and_exits_0() {
    // 0% of links resolve: the vault does not use this link style, so a
    // broken link is only a hint.
    let v = Vault::corpus("zk-min");
    for f in ["a.md", "b.md", "emoji.md", "tagged.md"] {
        v.remove(f);
    }
    let r = v.run(&["check"]);
    assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);
    assert_eq!(
        r.stdout,
        "broken.md:3:16: hint: broken link: missing-note\n"
    );
    assert_eq!(r.stderr, "2 files, 0 errors, 0 warnings, 0 info, 1 hints\n");
}

#[test]
fn check_counts_columns_in_characters() {
    let v = Vault::corpus("zk-min");
    // After `😀 日本 `: 5 characters, 11 bytes. Still 4 of 5 links resolve.
    v.write("broken.md", "# Broken\n\n😀 日本 [nowhere](missing-note)\n");
    let r = v.run(&["check", "--quiet"]);
    assert_eq!(r.code, 1);
    assert_eq!(
        r.stdout,
        "broken.md:3:6: warning: broken link: missing-note\n"
    );
}

#[test]
fn check_dedupes_files_across_paths_and_prints_outside_paths_absolute() {
    let v = Vault::corpus("zk-min");
    let r = v.run(&["check", ".", "broken.md", "./"]);
    assert_eq!(r.code, 1);
    assert_eq!(
        r.stdout,
        "broken.md:3:16: warning: broken link: missing-note\n"
    );
    assert!(r.stderr.starts_with("6 files,"), "{}", r.stderr);

    let r = run_in(&v.canon, &["check", "--quiet", "vault/broken.md"], &v);
    assert_eq!(
        r.stdout,
        "vault/broken.md:3:16: warning: broken link: missing-note\n"
    );
    let sub = v.dir().join(".zk");
    let r = run_in(&sub, &["check", "--quiet", ".."], &v);
    assert_eq!(
        r.stdout,
        "<TMP>/vault/broken.md:3:16: warning: broken link: missing-note\n"
    );
}

#[test]
fn roots_explains_the_choice() {
    let v = Vault::corpus("zk-min");
    let r = v.run(&["roots", "a.md"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    insta::assert_snapshot!("roots_zk_min", redact_cache(&redact_ms(&r.stdout)));
}

/// The root id in the DB path is time-based:
/// `<TMP>/cache/mdroots/roots/<id>.v2.db` -> `<CACHE>/roots/<ID>.v2.db`.
fn redact_cache(s: &str) -> String {
    s.lines()
        .map(
            |l| match l.strip_prefix("cache: <TMP>/cache/mdroots/roots/") {
                Some(rest) if rest.ends_with(".v2.db") => {
                    "cache: <CACHE>/roots/<ID>.v2.db".to_owned()
                }
                _ => l.to_owned(),
            },
        )
        .map(|l| l + "\n")
        .collect()
}

#[test]
fn roots_second_run_reuses_the_registered_db() {
    let v = Vault::corpus("zk-min");
    let first = v.run(&["roots", "a.md"]);
    assert_eq!(first.code, 0, "{}", first.stderr);
    assert!(
        first.stdout.contains("\nrole: reconciler\n"),
        "{}",
        first.stdout
    );
    let second = v.run(&["roots", "a.md"]);
    let cache = |s: &str| {
        s.lines()
            .find(|l| l.starts_with("cache: "))
            .map(str::to_owned)
    };
    // The registry kept the root: the same DB file.
    assert_eq!(cache(&first.stdout), cache(&second.stdout));
    assert!(v.canon.join("cache/mdroots/roots.v1.db").exists());
    assert!(!v.dir().join("roots.v1.db").exists());
}

#[test]
fn roots_lists_nested_roots() {
    let v = Vault::corpus("zk-min");
    v.write("sub/.mdroots", "");
    v.write("sub/inner.md", "# Inner\n");
    let r = v.run(&["roots", "a.md"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(
        r.stdout.contains("\nnested: <TMP>/vault/sub\n"),
        "{}",
        r.stdout
    );
}

#[test]
fn resolve_prints_targets_step_and_status() {
    let v = Vault::corpus("zk-min");
    let r = v.run(&["resolve", "a.md", "[Note B](b)"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    insta::assert_snapshot!("resolve_found", r.stdout);
}

#[test]
fn resolve_without_a_target_exits_1() {
    let v = Vault::corpus("zk-min");
    let r = v.run(&["resolve", "broken.md", "[nowhere](missing-note)"]);
    assert_eq!(r.code, 1);
    insta::assert_snapshot!("resolve_broken", r.stdout);
}

#[test]
fn resolve_of_text_without_a_link_is_an_error() {
    let v = Vault::corpus("zk-min");
    let r = v.run(&["resolve", "a.md", "no link here"]);
    assert_eq!(r.code, 2);
    assert_eq!(r.stderr, "mdroots: not a link\n");
}

#[test]
fn backlinks_lists_linking_notes() {
    let v = Vault::corpus("zk-min");
    let r = v.run(&["backlinks", "a.md"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    insta::assert_snapshot!("backlinks_a", r.stdout);
    let r = v.run(&["backlinks", "broken.md"]);
    assert_eq!((r.code, r.stdout.as_str()), (0, ""));
}

/// Run `mdroots args` in the vault with `MDROOTS_CACHE_DIR` at
/// `<tmp>/search-cache`.
fn run_search(v: &Vault, args: &[&str]) -> Run {
    let out = Command::new(env!("CARGO_BIN_EXE_mdroots"))
        .args(args)
        .current_dir(v.dir())
        .env("MDROOTS_CACHE_DIR", v.canon.join("search-cache"))
        .env("XDG_CACHE_HOME", v.canon.join("cache"))
        .env("HOME", v.canon.join("cache"))
        .output()
        .unwrap();
    Run {
        code: out.status.code().unwrap(),
        stdout: v.redact(&String::from_utf8(out.stdout).unwrap()),
        stderr: v.redact(&String::from_utf8(out.stderr).unwrap()),
    }
}

#[test]
fn search_a_file_searches_its_root_through_the_db() {
    let v = Vault::corpus("zk-min");
    let r = run_search(&v, &["search", "note", "a.md"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    insta::assert_snapshot!("search_zk_min", r.stdout);
    let dbs: Vec<_> = fs::read_dir(v.canon.join("search-cache/roots"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".v2.db"))
        .collect();
    assert_eq!(dbs.len(), 1, "{dbs:?}");
}

#[test]
fn search_a_directory_finds_the_same_notes() {
    let v = Vault::corpus("zk-min");
    let names = |s: &str| -> Vec<String> {
        let mut v: Vec<String> = s
            .lines()
            .map(|l| l.split(':').next().unwrap().to_owned())
            .collect();
        v.sort();
        v
    };
    let file = run_search(&v, &["search", "note", "a.md"]);
    let dir = run_search(&v, &["search", "note"]);
    assert_eq!(dir.code, 0, "{}", dir.stderr);
    assert_eq!(names(&dir.stdout), names(&file.stdout));
    // After `--` a query may start with `-` (a term of no letters or
    // digits, here dropped).
    let r = run_search(&v, &["search", "--", "- emoji", "."]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(names(&r.stdout), ["README.md", "emoji.md"]);
}

#[test]
fn search_without_hits_exits_1() {
    let v = Vault::corpus("zk-min");
    for args in [
        &["search", "nothingmatchesthis", "a.md"][..],
        &["search", "nothingmatchesthis"],
        &["search", "--", "-"],
    ] {
        let r = run_search(&v, args);
        assert_eq!(
            (r.code, r.stdout.as_str()),
            (1, ""),
            "{args:?}: {}",
            r.stderr
        );
    }
}

#[test]
fn redact_ms_replaces_only_timings() {
    assert_eq!(redact_ms("6 md in 12 ms, in a"), "6 md in N ms, in a");
    assert_eq!(redact_ms("in 3 dirs"), "in 3 dirs");
}

// --- notes and tags -------------------------------------------------------

/// A small notebook (`.mdroots` marker) for the link-graph filters:
///
/// - `hub` links `a`, `b` and `sub/t`; `a` links `hub` back.
/// - `c` links `b`; `sub/s` links `a`; `lone` links only itself.
///
/// Orphans: `c`, `lone`, `sub/s`. Non-reciprocated links: hub→b, hub→sub/t,
/// c→b, sub/s→a. Related to `a` (undirected neighbours hub and sub/s):
/// `b` and `sub/t` through hub.
fn graph_vault() -> Vault {
    let tmp = tempfile::tempdir().unwrap();
    let canon = fs::canonicalize(tmp.path()).unwrap();
    let v = Vault { tmp, canon };
    v.write(".mdroots", "");
    v.write(
        "hub.md",
        "---\ntags: [index]\ndate: 2024-03-01\n---\n# Hub\n\n[A](a.md) [B](b.md) [T](sub/t.md)\n",
    );
    v.write(
        "a.md",
        "---\ndate: 2024-01-15\n---\n# Alpha\n\n#Project #draft [Hub](hub.md)\n",
    );
    v.write("b.md", "# Beta\n\n#project\n");
    v.write(
        "c.md",
        "---\ndate: 2023-12-31T23:30:00Z\n---\n# Gamma\n\n[B](b.md)\n",
    );
    v.write("lone.md", "# Lone\n\n[me](lone.md)\n");
    v.write("sub/s.md", "# Sub S\n\n#project [A](../a.md)\n");
    v.write("sub/t.md", "# Sub T\n");
    v
}

/// Run `mdroots args` in `cwd` with `MDROOTS_CACHE_DIR` in the temp dir.
fn run_cached(v: &Vault, cwd: &Path, args: &[&str]) -> Run {
    let out = Command::new(env!("CARGO_BIN_EXE_mdroots"))
        .args(args)
        .current_dir(cwd)
        .env("MDROOTS_CACHE_DIR", v.canon.join("cache-dir"))
        .env("XDG_CACHE_HOME", v.canon.join("cache"))
        .env("HOME", v.canon.join("cache"))
        .output()
        .unwrap();
    Run {
        code: out.status.code().unwrap(),
        stdout: v.redact(&String::from_utf8(out.stdout).unwrap()),
        stderr: v.redact(&String::from_utf8(out.stderr).unwrap()),
    }
}

/// `mdroots notes args` in the vault: exit 0, the printed lines.
fn notes(v: &Vault, args: &[&str]) -> Vec<String> {
    notes_in(v, &v.dir(), args)
}

fn notes_in(v: &Vault, cwd: &Path, args: &[&str]) -> Vec<String> {
    let mut all = vec!["notes"];
    all.extend_from_slice(args);
    let r = run_cached(v, cwd, &all);
    assert_eq!(r.code, 0, "{args:?}: {}", r.stderr);
    r.stdout.lines().map(str::to_owned).collect()
}

#[test]
fn notes_lists_every_note_by_title() {
    let v = graph_vault();
    assert_eq!(
        notes(&v, &[]),
        [
            "a.md", "b.md", "c.md", "hub.md", "lone.md", "sub/s.md", "sub/t.md"
        ]
    );
}

#[test]
fn notes_graph_filters() {
    let v = graph_vault();
    assert_eq!(notes(&v, &["--orphan"]), ["c.md", "lone.md", "sub/s.md"]);
    assert_eq!(
        notes(&v, &["--missing-backlink"]),
        ["a.md", "b.md", "sub/t.md"]
    );
    assert_eq!(notes(&v, &["--related", "a.md"]), ["b.md", "sub/t.md"]);
    assert_eq!(notes(&v, &["--link-to", "b.md"]), ["c.md", "hub.md"]);
    assert_eq!(notes(&v, &["-l", "a.md"]), ["hub.md", "sub/s.md"]);
    assert_eq!(
        notes(&v, &["--linked-by", "hub.md"]),
        ["a.md", "b.md", "sub/t.md"]
    );
    assert_eq!(notes(&v, &["-L", "lone.md"]), Vec::<String>::new());
    // Filters combine.
    assert_eq!(notes(&v, &["--orphan", "-L", "c.md"]), Vec::<String>::new());
    assert_eq!(notes(&v, &["-l", "b.md", "-t", "index"]), ["hub.md"]);
}

#[test]
fn notes_path_is_a_filter_in_the_whole_notebook() {
    // sub/t is linked from hub, outside sub: not an orphan though sub is
    // the PATH, from the root or from inside sub.
    let v = graph_vault();
    assert_eq!(notes(&v, &["--orphan", "sub"]), ["sub/s.md"]);
    let sub = v.dir().join("sub");
    assert_eq!(notes_in(&v, &sub, &["--orphan", "."]), ["s.md"]);
    // Without a PATH the whole notebook counts, as in zk.
    assert_eq!(
        notes_in(&v, &sub, &["--orphan"]),
        ["<TMP>/vault/c.md", "<TMP>/vault/lone.md", "s.md"]
    );
    assert_eq!(
        notes_in(&v, &sub, &["-l", "../a.md"]),
        ["<TMP>/vault/hub.md", "s.md"]
    );
    assert_eq!(notes_in(&v, &sub, &["-l", "../a.md", "."]), ["s.md"]);
    assert_eq!(notes(&v, &["sub/t.md", "lone.md"]), ["lone.md", "sub/t.md"]);
    assert_eq!(notes(&v, &["-x", "sub", "--orphan"]), ["c.md", "lone.md"]);
}

#[test]
fn notes_tag_expressions() {
    let v = graph_vault();
    assert_eq!(notes(&v, &["-t", "project"]), ["a.md", "b.md", "sub/s.md"]);
    assert_eq!(
        notes(&v, &["-t", "#PROJECT, NOT draft"]),
        ["b.md", "sub/s.md"]
    );
    assert_eq!(notes(&v, &["-t", "index OR draft"]), ["a.md", "hub.md"]);
    assert_eq!(
        notes(&v, &["-t", "pro*", "-t", "-draft"]),
        ["b.md", "sub/s.md"]
    );
    assert_eq!(notes(&v, &["--tagless"]), ["c.md", "lone.md", "sub/t.md"]);
    let r = run_cached(&v, &v.dir(), &["notes", "--tag", "a OR -b"]);
    assert_eq!(r.code, 2);
    assert_eq!(
        r.stderr,
        "mdroots: --tag: column 6: cannot negate a tag in an OR group\n"
    );
}

#[test]
fn notes_dates_sort_and_limit() {
    let v = graph_vault();
    // Notes without a frontmatter date fall back to the file's birth time
    // (now), so bound both sides.
    assert_eq!(
        notes(
            &v,
            &["--created-after", "2024-01-01", "--created-before", "2025"]
        ),
        ["a.md", "hub.md"]
    );
    assert_eq!(notes(&v, &["--created-before", "2024-01-01"]), ["c.md"]);
    assert_eq!(notes(&v, &["--created", "2024-01-15"]), ["a.md"]);
    assert_eq!(
        notes(&v, &["--modified-after", "2000-01-01", "-n", "2"]),
        ["a.md", "b.md"]
    );
    assert_eq!(
        notes(&v, &["--modified-before", "2000-01-01"]),
        Vec::<String>::new()
    );
    assert_eq!(
        notes(&v, &["--created-before", "2025", "-s", "created"]),
        ["hub.md", "a.md", "c.md"]
    );
    assert_eq!(
        notes(&v, &["--sort", "path-", "--limit", "3"]),
        ["sub/t.md", "sub/s.md", "lone.md"]
    );
    let r = run_cached(&v, &v.dir(), &["notes", "--created", "someday"]);
    assert_eq!(r.code, 2);
    assert!(r.stderr.starts_with("mdroots: --created: "), "{}", r.stderr);
}

#[test]
fn notes_match_and_search_paths_agree() {
    let v = graph_vault();
    // hub's text holds `sub/t.md`.
    assert_eq!(
        notes(&v, &["-m", "sub"]),
        ["hub.md", "sub/s.md", "sub/t.md"]
    );
    let r = run_cached(&v, &v.dir(), &["search", "--paths", "sub"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let mut got: Vec<&str> = r.stdout.lines().collect();
    got.sort();
    assert_eq!(got, ["hub.md", "sub/s.md", "sub/t.md"]);
    let r = run_cached(&v, &v.dir(), &["search", "--paths", "--", "nothingmatches"]);
    assert_eq!((r.code, r.stdout.as_str()), (1, ""));
}

#[test]
fn notes_formats() {
    let v = graph_vault();
    v.write(
        "tab.md",
        "---\ndate: 2024-02-29T08:09:10Z\n---\n# Tab\there\\\n",
    );
    let r = run_cached(&v, &v.dir(), &["notes", "-f", "tsv", "tab.md", "a.md"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let cut = |s: &str| -> String {
        // Drop the modified column (an mtime).
        s.lines()
            .map(|l| {
                let f: Vec<&str> = l.split('\t').collect();
                assert_eq!(f.len(), 5, "{l}");
                assert!(f[3].ends_with('Z'), "{l}");
                format!("{}\t{}\t{}\t{}\n", f[0], f[1], f[2], f[4])
            })
            .collect()
    };
    assert_eq!(
        cut(&r.stdout),
        "a.md\tAlpha\tProject,draft\t2024-01-15T00:00:00Z\n\
         tab.md\tTab\\there\\\\\t\t2024-02-29T08:09:10Z\n"
    );

    let r = run_cached(&v, &v.dir(), &["notes", "-f", "json", "-t", "index"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let j: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(j.as_array().unwrap().len(), 1);
    assert_eq!(j[0]["path"], "<TMP>/vault/hub.md");
    assert_eq!(j[0]["title"], "Hub");
    assert_eq!(j[0]["tags"], serde_json::json!(["index"]));
    assert_eq!(j[0]["created"], "2024-03-01T00:00:00Z");
    assert!(j[0]["modified"].as_str().unwrap().ends_with('Z'));

    let r = run_cached(&v, &v.dir(), &["notes", "-f", "jsonl", "-t", "project"]);
    let titles: Vec<String> = r
        .stdout
        .lines()
        .map(|l| {
            let j: serde_json::Value = serde_json::from_str(l).unwrap();
            j["title"].as_str().unwrap().to_owned()
        })
        .collect();
    assert_eq!(titles, ["Alpha", "Beta", "Sub S"]);

    let r = run_cached(&v, &v.dir(), &["notes", "-0", "--orphan"]);
    assert_eq!(r.stdout, "c.md\0lone.md\0sub/s.md\0tab.md\0");
    // An empty json result is still an array.
    let r = run_cached(&v, &v.dir(), &["notes", "-f", "json", "-t", "nope"]);
    assert_eq!((r.code, r.stdout.as_str()), (0, "[]\n"));
}

#[test]
fn notes_and_tags_exit_0_without_results_and_2_on_usage_errors() {
    let v = graph_vault();
    for args in [
        &["notes", "-t", "nope"][..],
        &["notes", "--orphan", "-t", "index"],
        &["tags", "sub/t.md"],
    ] {
        let r = run_cached(&v, &v.dir(), args);
        assert_eq!(
            (r.code, r.stdout.as_str()),
            (0, ""),
            "{args:?}: {}",
            r.stderr
        );
    }
    for args in [
        &["notes", "--bogus"][..],
        &["notes", "-t"],
        &["notes", "-f", "yaml"],
        &["tags", "--sort", "size"],
        &["tags", "--format"],
        &["tags", "a.md", "b.md"],
        &["check", "--fail-on", "info"],
        &["check", "--fail-on"],
        &["search", "--paths"],
    ] {
        let r = run_cached(&v, &v.dir(), args);
        assert_eq!(r.code, 2, "{args:?}");
        assert!(
            r.stderr.starts_with("usage: mdroots"),
            "{args:?}: {}",
            r.stderr
        );
    }
    for args in [&["notes", "-n", "x"][..], &["notes", "-s", "size"]] {
        let r = run_cached(&v, &v.dir(), args);
        assert_eq!(r.code, 2, "{args:?}");
        assert!(r.stderr.starts_with("mdroots: "), "{args:?}: {}", r.stderr);
    }
}

#[test]
fn notes_graph_filters_on_a_lazy_root_are_an_error() {
    // A directory without a marker or VCS: `open_dir` falls back to
    // `open_at`, which indexes it all, so graph filters still work there.
    // A lazy root only comes from discovery on a big or slow tree, which a
    // test cannot build cheaply; the library tests cover that error.
    let tmp = tempfile::tempdir().unwrap();
    let canon = fs::canonicalize(tmp.path()).unwrap();
    let v = Vault { tmp, canon };
    v.write("x.md", "# X\n");
    assert_eq!(notes(&v, &["--orphan"]), ["x.md"]);
}

#[test]
fn tags_counts_notes_per_tag() {
    let v = graph_vault();
    let r = run_cached(&v, &v.dir(), &["tags"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.stdout, "draft\t1\nindex\t1\nproject\t3\n");
    let r = run_cached(&v, &v.dir(), &["tags", "--sort", "count"]);
    assert_eq!(r.stdout, "project\t3\ndraft\t1\nindex\t1\n");
    let r = run_cached(&v, &v.dir(), &["tags", "sub"]);
    assert_eq!(r.stdout, "project\t1\n");
    let r = run_cached(
        &v,
        &v.dir(),
        &["tags", "--format", "json", "--sort", "count"],
    );
    let j: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(j[0], serde_json::json!({"name": "project", "count": 3}));
    assert_eq!(j.as_array().unwrap().len(), 3);
}

#[test]
fn check_fail_on_sets_the_exit_threshold() {
    // zk-min has one broken-link warning.
    let v = Vault::corpus("zk-min");
    for (level, code) in [("warning", 1), ("error", 0), ("never", 0)] {
        let r = v.run(&["check", "--quiet", "--fail-on", level]);
        assert_eq!(r.code, code, "{level}");
        assert_eq!(
            r.stdout,
            "broken.md:3:16: warning: broken link: missing-note\n"
        );
    }
    v.write(
        ".zk/config.toml",
        "[note]\nfilename = \"{{id}}\"\n[lsp.diagnostics]\ndead-link = \"error\"\n",
    );
    for (level, code) in [("warning", 1), ("error", 1), ("never", 0)] {
        let r = v.run(&["check", "--fail-on", level, "--quiet"]);
        assert_eq!(r.code, code, "{level}");
    }
}

#[test]
fn roots_prints_settings_and_their_sources() {
    let v = Vault::corpus("zk-min");
    v.write(
        ".zk/config.toml",
        "[note]\nfilename = \"{{id}}\"\n[format.markdown]\nhashtags = false\n[lsp.diagnostics]\ndead-link = \"error\"\n",
    );
    let r = v.run(&["roots", "a.md"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let settings: Vec<&str> = r
        .stdout
        .lines()
        .skip_while(|l| !l.starts_with("link style:"))
        .collect();
    assert_eq!(
        settings,
        [
            "link style: markdown-relative without .md (vote)",
            "hashtags: off (zk .zk/config.toml hashtags)",
            "colon tags: off (default)",
            "multiword tags: off (default)",
            "broken links: error (zk .zk/config.toml dead-link)",
        ]
    );
}
