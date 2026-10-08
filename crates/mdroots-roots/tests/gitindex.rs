//! Tests for the git index reader against indexes written by a real
//! [git](https://git-scm.com) binary in temp dirs. Tests return early with
//! a note when git (or a needed git feature) is unavailable.

use std::path::{Path, PathBuf};
use std::process::Command;

use mdroots_roots::gitindex::{read_header, scan};
use mdroots_roots::probe::{FakeProbe, StdProbe};

fn git_cmd(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "-c",
            "user.name=alice",
            "-c",
            "user.email=alice@example.com",
        ]);
    c
}

/// Run git; panics on failure.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = git_cmd(dir).args(args).output().expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf-8 git output")
}

/// Run git; false if it fails (feature unsupported).
fn git_ok(dir: &Path, args: &[&str]) -> bool {
    git_cmd(dir)
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success())
}

fn have_git() -> bool {
    let ok = Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        eprintln!("note: git not on PATH; skipping");
    }
    ok
}

fn ls_files(dir: &Path, extra: &[&str]) -> Vec<String> {
    let mut args = vec!["-c", "core.quotePath=false", "ls-files", "-z"];
    args.extend_from_slice(extra);
    git(dir, &args)
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn is_md(p: &str) -> bool {
    [".md", ".markdown", ".org"].iter().any(|e| p.ends_with(e))
}

fn write(root: &Path, rel: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, format!("# {rel}\n")).unwrap();
}

/// A repo with nested dirs, unicode names and mixed extensions, all added.
fn repo() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    git(&root, &["init", "-q"]);
    for rel in [
        "README.md",
        "notes/a.md",
        "notes/deep/b.markdown",
        "notes/deep/deeper/c.md",
        "notes/é.md",
        "日本/ノート.md",
        "todo.org",
        "plain.txt",
        "src/main.rs",
    ] {
        write(&root, rel);
    }
    git(&root, &["add", "-A"]);
    (tmp, root)
}

fn index(root: &Path) -> PathBuf {
    root.join(".git/index")
}

fn check_matches_ls_files(root: &Path, version: u32) {
    let h = read_header(&StdProbe, &index(root)).unwrap();
    assert_eq!(h.version, version);
    let all = ls_files(root, &[]);
    assert_eq!(h.entries as usize, all.len());
    let s = scan(&StdProbe, &index(root), &is_md, usize::MAX).unwrap();
    assert_eq!(s.version, version);
    assert_eq!(s.entries, h.entries);
    let want: Vec<String> = all.into_iter().filter(|p| is_md(p)).collect();
    assert_eq!(s.paths, want);
    assert!(!s.sparse && !s.split && !s.truncated);
}

#[test]
fn version2_matches_ls_files() {
    if !have_git() {
        return;
    }
    let (_t, root) = repo();
    git(&root, &["update-index", "--index-version", "2"]);
    check_matches_ls_files(&root, 2);
}

#[test]
fn version3_matches_ls_files() {
    if !have_git() {
        return;
    }
    let (_t, root) = repo();
    // v3 is only written when an entry carries an extended flag.
    git(&root, &["update-index", "--skip-worktree", "notes/a.md"]);
    git(&root, &["update-index", "--index-version", "3"]);
    check_matches_ls_files(&root, 3);
}

#[test]
fn version4_matches_ls_files() {
    if !have_git() {
        return;
    }
    let (_t, root) = repo();
    git(&root, &["update-index", "--index-version", "4"]);
    check_matches_ls_files(&root, 4);
}

#[test]
fn long_name_uses_nul_scan() {
    if !have_git() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git(root, &["init", "-q"]);
    // Path length >= 0xFFF stores 0xFFF in the flags. Added with
    // --cacheinfo so the long path never touches the filesystem.
    let long: String = (0..30)
        .map(|i| format!("{i:02}{}", "x".repeat(200)))
        .collect::<Vec<_>>()
        .join("/")
        + "/n.md";
    assert!(long.len() > 0xfff);
    write(root, "z.md");
    let blob = git(root, &["hash-object", "-w", "z.md"]);
    let info = format!("100644,{},{long}", blob.trim());
    git(root, &["update-index", "--add", "--cacheinfo", &info]);
    git(root, &["add", "z.md"]);
    for v in ["2", "4"] {
        git(root, &["update-index", "--index-version", v]);
        let s = scan(&StdProbe, &index(root), &is_md, usize::MAX).unwrap();
        assert_eq!(s.paths, vec![long.clone(), "z.md".to_owned()], "v{v}");
    }
}

#[test]
fn truncated_by_cap_counts_entries() {
    if !have_git() {
        return;
    }
    let (_t, root) = repo();
    let all = ls_files(&root, &[]);
    let cap = 3;
    let s = scan(&StdProbe, &index(&root), &is_md, cap).unwrap();
    assert!(s.truncated);
    let want: Vec<String> = all.into_iter().take(cap).filter(|p| is_md(p)).collect();
    assert_eq!(s.paths, want);
    // Exactly cap entries is not truncated.
    let n = read_header(&StdProbe, &index(&root)).unwrap().entries as usize;
    assert!(!scan(&StdProbe, &index(&root), &is_md, n).unwrap().truncated);
}

#[test]
fn split_index_sets_split() {
    if !have_git() {
        return;
    }
    let (_t, root) = repo();
    git(&root, &["update-index", "--split-index"]);
    write(&root, "new.md");
    git(&root, &["add", "new.md"]);
    let s = scan(&StdProbe, &index(&root), &is_md, usize::MAX).unwrap();
    assert!(s.split);
}

#[test]
fn sparse_index_sets_sparse_and_hides_dir_entries() {
    if !have_git() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git(root, &["init", "-q"]);
    for rel in ["a/b/n.md", "c/m.md", "c/d/x.md", "r.md", "r.txt"] {
        write(root, rel);
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "init"]);
    if !git_ok(
        root,
        &["sparse-checkout", "init", "--cone", "--sparse-index"],
    ) || !git_ok(root, &["sparse-checkout", "set", "a"])
    {
        eprintln!("note: git lacks sparse-index support; skipping");
        return;
    }
    let s = scan(&StdProbe, &index(root), &is_md, usize::MAX).unwrap();
    if !s.sparse {
        eprintln!("note: git did not write a sparse index; skipping");
        return;
    }
    let sparse_dirs: Vec<String> = ls_files(root, &["--sparse"])
        .into_iter()
        .filter(|p| p.ends_with('/'))
        .collect();
    assert!(!sparse_dirs.is_empty());
    let want: Vec<String> = ls_files(root, &[])
        .into_iter()
        .filter(|p| is_md(p) && !sparse_dirs.iter().any(|d| p.starts_with(d.as_str())))
        .collect();
    assert_eq!(s.paths, want);
    assert!(s.paths.iter().all(|p| !p.ends_with('/')));
}

#[test]
fn sha256_repo_rejected() {
    if !have_git() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    if !git_ok(root, &["init", "-q", "--object-format=sha256"]) {
        eprintln!("note: git lacks sha256 support; skipping");
        return;
    }
    write(root, "x.md");
    git(root, &["add", "-A"]);
    let e = scan(&StdProbe, &index(root), &is_md, usize::MAX).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(e.to_string(), "git index: sha256 index not supported");
}

#[test]
fn sha256_linked_worktree_rejected() {
    if !have_git() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("main");
    std::fs::create_dir(&root).unwrap();
    if !git_ok(&root, &["init", "-q", "--object-format=sha256"]) {
        eprintln!("note: git lacks sha256 support; skipping");
        return;
    }
    write(&root, "x.md");
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "init"]);
    let wt = tmp.path().join("wt");
    git(&root, &["worktree", "add", "-q", wt.to_str().unwrap()]);
    let git_dir = git(&wt, &["rev-parse", "--absolute-git-dir"]);
    let wt_index = Path::new(git_dir.trim()).join("index");
    assert!(wt_index.parent().unwrap().join("commondir").exists());
    let e = scan(&StdProbe, &wt_index, &is_md, usize::MAX).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(e.to_string(), "git index: sha256 index not supported");
}

#[test]
fn bad_header_is_invalid_data() {
    let p = FakeProbe::new()
        .file("/r/.git/index", b"DIRX\0\0\0\x02\0\0\0\0")
        .file("/r/.git/v9", b"DIRC\0\0\0\x09\0\0\0\0")
        .file("/r/.git/short", b"DIRC");
    for f in ["index", "v9", "short"] {
        let path = Path::new("/r/.git").join(f);
        let e = read_header(&p, &path).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidData, "{f}");
        let e = scan(&p, &path, &is_md, usize::MAX).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidData, "{f}");
    }
}

#[test]
fn oversized_index_is_invalid_data() {
    let mut big = b"DIRC\0\0\0\x02\0\0\0\0".to_vec();
    big.resize((32 << 20) + 1, 0);
    let p = FakeProbe::new().file("/r/.git/index", big);
    let e = scan(&p, Path::new("/r/.git/index"), &is_md, usize::MAX).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn random_mutations_never_panic() {
    if !have_git() {
        return;
    }
    let (_t, root) = repo();
    let mut originals = Vec::new();
    for v in ["2", "4"] {
        git(&root, &["update-index", "--index-version", v]);
        originals.push(std::fs::read(index(&root)).unwrap());
    }
    git(&root, &["update-index", "--skip-worktree", "notes/a.md"]);
    git(&root, &["update-index", "--index-version", "3"]);
    originals.push(std::fs::read(index(&root)).unwrap());

    // xorshift64, fixed seed: deterministic.
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for case in 0..256 {
        let mut bytes = originals[case % originals.len()].clone();
        let n = 1 + (next() % 8) as usize;
        for _ in 0..n {
            let i = (next() % bytes.len() as u64) as usize;
            bytes[i] = next() as u8;
        }
        if next() % 4 == 0 {
            let len = (next() % bytes.len() as u64) as usize;
            bytes.truncate(len);
        }
        let p = FakeProbe::new().file("/r/.git/index", bytes);
        let path = Path::new("/r/.git/index");
        let _ = read_header(&p, path);
        let _ = scan(&p, path, &is_md, usize::MAX);
        let _ = scan(&p, path, &is_md, 2);
    }
}
