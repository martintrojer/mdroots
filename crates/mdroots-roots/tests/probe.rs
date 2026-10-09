//! Probe tests. Filesystems named here include [EdenFS](https://github.com/facebook/sapling)
//! (the virtual filesystem from the [Sapling](https://sapling-scm.com/) project); every
//! tree is a tempdir or a FakeProbe.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use mdroots_roots::probe::{Counting, FakeProbe, FsClass, MountInfo, Probe, StdProbe, classify};

fn mi(fs_type: &str, from: &str, local: bool, dev: u64) -> MountInfo {
    MountInfo {
        fs_type: fs_type.into(),
        from: from.into(),
        local,
        dev,
    }
}

// ---------------------------------------------------------------- classify

#[test]
fn classify_table() {
    let home = Path::new("/home/alice");
    let notes = Path::new("/home/alice/notes");
    let v = |s: &str| FsClass::Virtual(s.into());
    let r = |s: &str| FsClass::Remote(s.into());
    let cases: &[(MountInfo, &Path, FsClass)] = &[
        (mi("edenfs:", "edenfs", false, 5), notes, v("edenfs:")),
        (mi("nfs", "server:/export", false, 5), notes, r("nfs")),
        (
            mi("smbfs", "//alice@nas/share", false, 5),
            notes,
            r("smbfs"),
        ),
        (mi("apfs", "/dev/disk3s1", true, 1), notes, FsClass::Local),
        (mi("ext4", "/dev/sda1", true, 1), notes, FsClass::Local),
        // Case-insensitive name match.
        (mi("EdenFS:", "x", false, 5), notes, v("EdenFS:")),
        (mi("NFS4", "x", false, 5), notes, r("NFS4")),
        // Match on `from` when the type name says nothing.
        (mi("unknownfs", "edenfs", false, 5), notes, v("unknownfs")),
        (
            mi("unknownfs", "sshfs#host:/", true, 5),
            notes,
            r("unknownfs"),
        ),
        // A local-flagged mount with a virtual name is still virtual.
        (mi("macfuse", "x", true, 5), notes, v("macfuse")),
        (mi("fuse.sshfs", "host:/", false, 5), notes, v("fuse.sshfs")),
        // Unknown non-local type → remote.
        (mi("weirdfs", "x", false, 5), notes, r("weirdfs")),
        // Linux kernel pseudo-filesystems are never note roots.
        (mi("proc", "proc", true, 5), notes, v("proc")),
        (mi("sysfs", "sysfs", true, 5), notes, v("sysfs")),
        (mi("devtmpfs", "devtmpfs", true, 5), notes, v("devtmpfs")),
        (mi("cgroup2", "cgroup2", true, 5), notes, v("cgroup2")),
        (mi("tracefs", "tracefs", true, 5), notes, v("tracefs")),
        (mi("rpc_pipefs", "sunrpc", true, 5), notes, v("rpc_pipefs")),
        // Ordinary local filesystems stay local.
        (mi("tmpfs", "tmpfs", true, 5), notes, FsClass::Local),
        (
            mi("btrfs", "/dev/nvme0n1p3", true, 5),
            notes,
            FsClass::Local,
        ),
        (mi("overlay", "composefs", true, 5), notes, FsClass::Local),
    ];
    for (m, p, want) in cases {
        assert_eq!(&classify(m, p, Some(home)), want, "{m:?}");
    }
}

#[test]
fn classify_cloud_by_path() {
    let home = Some(Path::new("/home/alice"));
    let apfs = mi("apfs", "/dev/disk3s1", true, 1);
    let at = |p: &str| classify(&apfs, Path::new(p), home);
    assert_eq!(
        at("/home/alice/Library/CloudStorage/Drive-alice/notes"),
        FsClass::Cloud
    );
    assert_eq!(
        at("/home/alice/Library/Mobile Documents/iCloud~md~obsidian/Documents/v"),
        FsClass::Cloud
    );
    // Not under the cloud dirs, a sibling with a shared string prefix, other
    // case, no home.
    assert_eq!(at("/home/alice/Library/CloudStorageX/a"), FsClass::Local);
    assert_eq!(at("/home/alice/library/cloudstorage/a"), FsClass::Local);
    assert_eq!(at("/home/alice/notes"), FsClass::Local);
    assert_eq!(
        classify(&apfs, Path::new("/home/alice/Library/CloudStorage/a"), None),
        FsClass::Local
    );
    // A virtual mount under a cloud path is virtual first.
    assert_eq!(
        classify(
            &mi("edenfs:", "x", false, 5),
            Path::new("/home/alice/Library/CloudStorage/a"),
            home
        ),
        FsClass::Virtual("edenfs:".into())
    );
}

// ---------------------------------------------------------------- StdProbe

#[cfg(unix)]
#[test]
fn std_probe_stat_lstat_symlink() -> io::Result<()> {
    let t = tempfile::tempdir()?;
    let d = t.path();
    std::fs::write(d.join("a.md"), "x")?;
    std::fs::create_dir(d.join("sub"))?;
    std::os::unix::fs::symlink("a.md", d.join("link"))?;
    let p = StdProbe;

    let f = p.stat(&d.join("a.md"))?;
    assert!(f.is_file && !f.is_dir && !f.is_symlink && !f.dataless);
    let s = p.stat(&d.join("sub"))?;
    assert!(s.is_dir && !s.is_file);
    assert_eq!(s.dev, f.dev);
    assert_ne!(s.ino, f.ino);

    let followed = p.stat(&d.join("link"))?;
    assert_eq!(followed, f);
    let l = p.lstat(&d.join("link"))?;
    assert!(l.is_symlink && !l.is_file && !l.is_dir);
    assert_eq!(p.read_link(&d.join("link"))?, PathBuf::from("a.md"));
    assert!(p.read_link(&d.join("a.md")).is_err());
    assert_eq!(
        p.stat(&d.join("missing")).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn std_probe_read_dir_sorted_lstat() -> io::Result<()> {
    let t = tempfile::tempdir()?;
    let d = t.path();
    for n in ["c.md", "a.md", "B.md"] {
        std::fs::write(d.join(n), "")?;
    }
    std::fs::create_dir(d.join("dir"))?;
    std::os::unix::fs::symlink("dir", d.join("ln"))?;
    let got = StdProbe.read_dir(d)?;
    let names: Vec<&str> = got.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["B.md", "a.md", "c.md", "dir", "ln"]);
    assert!(got[3].1.is_dir);
    assert!(got[4].1.is_symlink && !got[4].1.is_dir);
    Ok(())
}

#[cfg(unix)]
#[test]
fn std_probe_read_small_and_prefix() -> io::Result<()> {
    let t = tempfile::tempdir()?;
    let f = t.path().join("index");
    std::fs::write(&f, b"DIRC\0\0\0\x02\0\0\0\x05rest-of-file")?;
    let p = StdProbe;
    assert_eq!(p.read_small(&f, 24)?.len(), 24);
    assert_eq!(p.read_small(&f, 100)?.len(), 24);
    assert_eq!(
        p.read_small(&f, 23).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(p.read_prefix(&f, 12)?, b"DIRC\0\0\0\x02\0\0\0\x05");
    assert_eq!(p.read_prefix(&f, 1000)?.len(), 24);
    assert_eq!(p.read_prefix(&f, 0)?, b"");
    Ok(())
}

#[cfg(unix)]
#[test]
fn std_probe_mount_and_volume_id() -> io::Result<()> {
    let t = tempfile::tempdir()?;
    let p = StdProbe;
    let m = p.mount(t.path())?;
    assert!(m.local, "{m:?}");
    assert!(!m.fs_type.is_empty());
    assert_eq!(m.dev, p.stat(t.path())?.dev);
    let a = p.volume_id(t.path())?;
    assert_eq!(a, p.volume_id(t.path())?);
    assert!(a.starts_with("dev:"), "{a}");
    let t0 = p.now();
    assert!(p.now() >= t0);
    Ok(())
}

// ---------------------------------------------------------------- FakeProbe

#[test]
fn fake_probe_tree_and_symlinks() -> io::Result<()> {
    let p = FakeProbe::new()
        .home("/home/alice")
        .file("/home/alice/notes/b.md", "bb")
        .file("/home/alice/notes/a.md", "a")
        .dir("/home/alice/notes/sub")
        .symlink("/home/alice/notes/ln", "sub")
        .symlink("/home/alice/notes/abs", "/home/alice/notes/a.md")
        .dataless("/home/alice/notes/cloud.md");
    assert_eq!(Probe::home(&p), Some(PathBuf::from("/home/alice")));
    let names: Vec<String> = p
        .read_dir(Path::new("/home/alice/notes"))?
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    assert_eq!(names, ["a.md", "abs", "b.md", "cloud.md", "ln", "sub"]);

    let ln = Path::new("/home/alice/notes/ln");
    assert!(p.stat(ln)?.is_dir);
    assert!(p.lstat(ln)?.is_symlink);
    assert_eq!(p.read_link(ln)?, PathBuf::from("sub"));
    assert!(p.read_link(Path::new("/home/alice/notes/a.md")).is_err());
    assert_eq!(
        p.stat(Path::new("/home/alice/notes/abs"))?,
        p.stat(Path::new("/home/alice/notes/a.md"))?
    );
    assert!(p.stat(Path::new("/home/alice/notes/cloud.md"))?.dataless);
    assert_eq!(
        p.stat(Path::new("/nope")).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );

    let b = Path::new("/home/alice/notes/b.md");
    assert_eq!(p.read_small(b, 2)?, b"bb");
    assert_eq!(
        p.read_small(b, 1).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(p.read_prefix(b, 1)?, b"b");
    Ok(())
}

#[test]
fn fake_probe_mounts_drive_stat_dev() -> io::Result<()> {
    let p = FakeProbe::new()
        .file("/repo/a.md", "")
        .file("/repo/buck-out/x.md", "")
        .file("/repo/buck-out/deep/y.md", "")
        .file("/repo2/z.md", "")
        .mount("/repo", mi("edenfs:", "edenfs", false, 7))
        .mount("/repo/buck-out", mi("apfs", "/dev/disk3s5", true, 2));
    assert_eq!(p.stat(Path::new("/"))?.dev, 1);
    assert_eq!(Probe::mount(&p, Path::new("/"))?.fs_type, "apfs");
    assert_eq!(p.stat(Path::new("/repo/a.md"))?.dev, 7);
    assert_eq!(p.lstat(Path::new("/repo/buck-out"))?.dev, 2);
    assert_eq!(p.stat(Path::new("/repo/buck-out/deep/y.md"))?.dev, 2);
    // Component-wise: /repo2 is not under /repo.
    assert_eq!(p.stat(Path::new("/repo2/z.md"))?.dev, 1);
    assert_eq!(
        Probe::mount(&p, Path::new("/repo/a.md"))?.fs_type,
        "edenfs:"
    );
    assert!(!Probe::mount(&p, Path::new("/repo"))?.local);
    assert_eq!(p.volume_id(Path::new("/repo/a.md"))?, "fake:7");
    assert_eq!(p.volume_id(Path::new("/repo/buck-out/x.md"))?, "fake:2");
    // read_dir entries carry their own mount's dev.
    let e = p.read_dir(Path::new("/repo"))?;
    let dev = |n: &str| e.iter().find(|(x, _)| x == n).map(|(_, s)| s.dev);
    assert_eq!(dev("a.md"), Some(7));
    assert_eq!(dev("buck-out"), Some(2));
    Ok(())
}

#[test]
fn fake_probe_read_dir_costs_advance_now() -> io::Result<()> {
    let p = FakeProbe::new()
        .dir("/fast/a")
        .dir("/slow/b/c")
        .read_dir_cost("/slow", Duration::from_millis(20))
        .read_dir_cost("/slow/b/c", Duration::from_millis(1));
    assert_eq!(p.now(), Duration::ZERO);
    p.read_dir(Path::new("/fast"))?;
    assert_eq!(p.now(), Duration::from_micros(200));
    p.read_dir(Path::new("/slow"))?;
    p.read_dir(Path::new("/slow/b"))?;
    assert_eq!(p.now(), Duration::from_micros(40_200));
    p.read_dir(Path::new("/slow/b/c"))?;
    assert_eq!(p.now(), Duration::from_micros(41_200));
    // stat does not advance the clock.
    p.stat(Path::new("/slow/b"))?;
    assert_eq!(p.now(), Duration::from_micros(41_200));
    Ok(())
}

#[test]
fn fake_probe_inodes_and_rename() -> io::Result<()> {
    let p = FakeProbe::new()
        .file("/a/notes/.zk/config.toml", "")
        .file("/a/notes/x.md", "")
        .file("/a/other.md", "")
        .ino("/a/notes/.zk", 4242);
    let zk = p.stat(Path::new("/a/notes/.zk"))?;
    assert_eq!(zk.ino, 4242);
    let x = p.stat(Path::new("/a/notes/x.md"))?.ino;
    let other = p.stat(Path::new("/a/other.md"))?.ino;
    assert_ne!(x, other);
    assert_eq!(p.stat(Path::new("/a/notes/x.md"))?.ino, x, "stable");

    let p = p.rename("/a/notes", "/b/moved");
    assert!(p.stat(Path::new("/a/notes")).is_err());
    assert_eq!(p.stat(Path::new("/b/moved/.zk"))?.ino, 4242);
    assert_eq!(p.stat(Path::new("/b/moved/x.md"))?.ino, x);
    assert!(p.stat(Path::new("/b/moved/.zk/config.toml"))?.is_file);
    assert_eq!(p.stat(Path::new("/a/other.md"))?.ino, other);
    Ok(())
}

// ---------------------------------------------------------------- Counting

#[test]
fn counting_counts_and_records_violations() -> io::Result<()> {
    let c = Counting::new(
        FakeProbe::new()
            .dir("/home/alice/notes/sub")
            .dir("/home/alice/notesx")
            .dir("/home/alice/big"),
    );
    c.read_dir(Path::new("/home/alice/big"))?;
    assert!(c.violations().is_empty(), "nothing forbidden yet");

    c.forbid_read_dir_outside(&[PathBuf::from("/home/alice/notes")]);
    c.read_dir(Path::new("/home/alice/notes"))?;
    c.read_dir(Path::new("/home/alice/notes/sub"))?;
    c.read_dir(Path::new("/home/alice/notes/sub"))?;
    assert!(c.violations().is_empty(), "{:?}", c.violations());

    c.read_dir(Path::new("/home/alice/notesx"))?;
    c.read_dir(Path::new("/home/alice"))?;
    assert_eq!(
        c.violations(),
        [
            PathBuf::from("/home/alice/notesx"),
            PathBuf::from("/home/alice")
        ]
    );
    assert_eq!(c.read_dir_count(Path::new("/home/alice/notes/sub")), 2);
    assert_eq!(c.read_dir_count(Path::new("/home/alice/notes")), 1);
    assert_eq!(c.read_dir_count(Path::new("/nowhere")), 0);
    assert_eq!(c.read_dir_total(), 6);
    // Other methods delegate; inner is reachable.
    assert!(c.stat(Path::new("/home/alice/big"))?.is_dir);
    assert_eq!(c.inner().now(), c.now());
    Ok(())
}

#[test]
fn counting_empty_allowed_forbids_everything() -> io::Result<()> {
    let c = Counting::new(FakeProbe::new().dir("/x"));
    c.forbid_read_dir_outside(&[]);
    c.read_dir(Path::new("/x"))?;
    assert_eq!(c.violations(), [PathBuf::from("/x")]);
    Ok(())
}

#[test]
fn probe_is_object_safe() {
    let p: Box<dyn Probe> = Box::new(Counting::new(FakeProbe::new()));
    assert!(p.stat(Path::new("/")).is_ok());
}
