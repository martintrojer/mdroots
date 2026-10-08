use std::path::Path;

use mdroots_core::{Cancel, ErrorKind, MemFs, walk_md};

#[test]
fn walk_skips_hidden_temp_and_pruned() {
    let fs = MemFs::new()
        .with_file("z.md", "")
        .with_file("a.markdown", "")
        .with_file("notes/b.md", "")
        .with_file("notes/deep/c.org", "")
        .with_file("notes/readme.txt", "")
        .with_file("notes/.hidden.md", "")
        .with_file("notes/b.md~", "")
        .with_file("notes/.#b.md", "")
        .with_file("notes/#b.md#", "")
        .with_file("notes/.b.md.swp", "")
        .with_file(".m-reflow-x.md", "")
        .with_file(".obsidian/x.md", "")
        .with_file(".git/x.md", "")
        .with_file(".venv/x.md", "")
        .with_file("node_modules/pkg/x.md", "")
        .with_file("target/x.md", "")
        .with_file("buck-out/x.md", "")
        .with_file("__pycache__/x.md", "")
        .with_file("dist/x.md", "")
        .with_file("build/x.md", "")
        .with_file("venv/x.md", "")
        .with_file("Pods/x.md", "")
        .with_file("DerivedData/x.md", "")
        .with_file("vendor/x.md", "")
        .with_file("sub/target-notes/x.md", "")
        .with_dir("empty");
    let got = walk_md(&fs, Path::new("/"), &Cancel::new()).unwrap();
    assert_eq!(
        got,
        [
            "a.markdown",
            "notes/b.md",
            "notes/deep/c.org",
            "sub/target-notes/x.md",
            "z.md"
        ]
    );
}

#[test]
fn walk_below_a_subdir_root_and_cancel() {
    let fs = MemFs::new()
        .with_file("v/n.md", "")
        .with_file("other.md", "");
    assert_eq!(
        walk_md(&fs, Path::new("/v"), &Cancel::new()).unwrap(),
        ["n.md"]
    );
    let c = Cancel::new();
    c.cancel();
    let err = walk_md(&fs, Path::new("/"), &c).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Cancelled);
    let err = walk_md(&fs, Path::new("/missing"), &Cancel::new()).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Io);
}

#[test]
fn walk_skips_dataless_entries() {
    let fs = MemFs::new()
        .with_file("a.md", "")
        .with_file("cloud/b.md", "")
        .with_file("local/c.md", "")
        .with_dataless("cloud")
        .with_dataless("local/d.md");
    let got = walk_md(&fs, Path::new("/"), &Cancel::new()).unwrap();
    assert_eq!(got, ["a.md", "local/c.md"]);
}
