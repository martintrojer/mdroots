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
    let out = Command::new(env!("CARGO_BIN_EXE_mdroots"))
        .args(args)
        .current_dir(cwd)
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
fn lsp_is_not_implemented_yet() {
    let v = Vault::corpus("zk-min");
    let r = v.run(&["lsp"]);
    assert_eq!(r.code, 2);
    assert_eq!(r.stderr, "mdroots: lsp: not implemented yet\n");
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
    insta::assert_snapshot!("roots_zk_min", redact_ms(&r.stdout));
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

#[test]
fn redact_ms_replaces_only_timings() {
    assert_eq!(redact_ms("6 md in 12 ms, in a"), "6 md in N ms, in a");
    assert_eq!(redact_ms("in 3 dirs"), "in 3 dirs");
}
