//! The filesystem seam (docs/specs/library.md §3.5): everything core reads goes
//! through [`FileSystem`], so embedders (and tests) can supply their own.

use std::collections::BTreeMap;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;

/// What core needs to know about a file or directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Meta {
    pub ino: u64,
    pub ctime_ns: i128,
    pub mtime_ns: i128,
    pub size: u64,
    pub is_dir: bool,
    pub is_file: bool,
    /// A cloud placeholder whose contents are not local.
    pub dataless: bool,
}

/// The kind of filesystem a directory lives on.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsKind {
    Local,
    Virtual,
    Remote,
    Cloud,
    Unknown,
}

pub trait FileSystem: Send + Sync {
    /// The file's bytes and its metadata, taken from the same open file.
    fn read(&self, p: &Path) -> io::Result<(Arc<[u8]>, Meta)>;
    /// Metadata of `p`, following symlinks.
    fn stat(&self, p: &Path) -> io::Result<Meta>;
    /// Entry names and their metadata (symlinks not followed), sorted by name.
    fn read_dir(&self, p: &Path) -> io::Result<Vec<(String, Meta)>>;
    fn canonicalize(&self, p: &Path) -> io::Result<PathBuf>;
    fn case_sensitive(&self, dir: &Path) -> bool;
    fn fs_kind(&self, dir: &Path) -> FsKind;
    /// The birth time of `p`, following symlinks; `None` when the
    /// filesystem does not record one or the stat fails. A default method
    /// (not a [`Meta`] field) so existing implementations keep compiling.
    fn created(&self, p: &Path) -> Option<SystemTime> {
        let _ = p;
        None
    }
}

/// [`FileSystem`] over `std::fs`.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, Default)]
pub struct StdFs;

/// `SF_DATALESS` from `<sys/stat.h>`: the file's contents are not local
/// (cloud placeholder). See Apple's stat(2) man page:
/// <https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/stat.2.html>
#[cfg(target_os = "macos")]
const SF_DATALESS: u32 = 0x4000_0000;

#[cfg(unix)]
fn std_meta(m: &std::fs::Metadata) -> Meta {
    use std::os::unix::fs::MetadataExt;
    #[cfg(target_os = "macos")]
    let dataless = {
        use std::os::macos::fs::MetadataExt as _;
        m.st_flags() & SF_DATALESS != 0
    };
    #[cfg(not(target_os = "macos"))]
    let dataless = false;
    let ns = |s: i64, n: i64| i128::from(s) * 1_000_000_000 + i128::from(n);
    Meta {
        ino: m.ino(),
        ctime_ns: ns(m.ctime(), m.ctime_nsec()),
        mtime_ns: ns(m.mtime(), m.mtime_nsec()),
        size: m.size(),
        is_dir: m.is_dir(),
        is_file: m.is_file(),
        dataless,
    }
}

#[cfg(unix)]
impl FileSystem for StdFs {
    fn read(&self, p: &Path) -> io::Result<(Arc<[u8]>, Meta)> {
        use std::io::Read;
        let mut f = std::fs::File::open(p)?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        let meta = std_meta(&f.metadata()?);
        Ok((buf.into(), meta))
    }

    fn stat(&self, p: &Path) -> io::Result<Meta> {
        Ok(std_meta(&std::fs::metadata(p)?))
    }

    fn read_dir(&self, p: &Path) -> io::Result<Vec<(String, Meta)>> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(p)? {
            let e = e?;
            let Ok(m) = e.metadata() else { continue };
            out.push((e.file_name().to_string_lossy().into_owned(), std_meta(&m)));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// `std::fs::canonicalize`; for a missing path, the longest existing
    /// ancestor is canonicalized and the missing tail appended (overlays of
    /// new files). Donated from ramble 75b8285 src/lsp/uri.rs `canonical_path`.
    fn canonicalize(&self, p: &Path) -> io::Result<PathBuf> {
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else {
            std::env::current_dir()?.join(p)
        };
        let err = match abs.canonicalize() {
            Ok(c) => return Ok(c),
            Err(e) => e,
        };
        let mut tail = Vec::new();
        let mut cur = abs.as_path();
        while let Some(parent) = cur.parent() {
            if let Some(name) = cur.file_name() {
                tail.push(name.to_owned());
            }
            if let Ok(mut c) = parent.canonicalize() {
                c.extend(tail.iter().rev());
                return Ok(c);
            }
            cur = parent;
        }
        Err(err)
    }

    /// Stats the nearest cased ancestor component with its case flipped:
    /// insensitive only if that finds the same inode.
    fn case_sensitive(&self, dir: &Path) -> bool {
        let mut cur = dir;
        loop {
            let Some(name) = cur.file_name().and_then(|n| n.to_str()) else {
                return true;
            };
            let flipped = flip_case(name);
            if flipped != name {
                let Ok(orig) = std::fs::metadata(cur) else {
                    return true;
                };
                return match std::fs::metadata(cur.with_file_name(flipped)) {
                    Ok(m) => std_meta(&m).ino != std_meta(&orig).ino,
                    Err(_) => true,
                };
            }
            match cur.parent() {
                Some(p) => cur = p,
                None => return true,
            }
        }
    }

    fn fs_kind(&self, _dir: &Path) -> FsKind {
        // statfs-based detection comes with mdroots-roots.
        FsKind::Unknown
    }

    /// `std::fs::Metadata::created`: the birth time on macOS, `statx` on
    /// Linux where the kernel and filesystem support it. Copies and
    /// `git clone` ([Git](https://git-scm.com)) reset it.
    fn created(&self, p: &Path) -> Option<SystemTime> {
        std::fs::metadata(p).ok()?.created().ok()
    }
}

/// Swap upper and lower case of every cased char.
fn flip_case(s: &str) -> String {
    s.chars()
        .flat_map(|c| -> Box<dyn Iterator<Item = char>> {
            if c.is_uppercase() {
                Box::new(c.to_lowercase())
            } else if c.is_lowercase() {
                Box::new(c.to_uppercase())
            } else {
                Box::new(std::iter::once(c))
            }
        })
        .collect()
}

#[derive(Debug, Clone)]
struct Node {
    /// The path with the case it was created with.
    path: PathBuf,
    data: Option<Arc<[u8]>>,
    meta: Meta,
}

#[derive(Debug, Default)]
struct MemState {
    /// Keyed by the (case-folded when insensitive) cleaned absolute path.
    nodes: BTreeMap<String, Node>,
    clock: u64,
}

/// An in-memory tree for tests. Paths are absolute under `/`;
/// `with_file("a/b.md", …)` creates `/a/b.md` and its parents.
#[derive(Debug, Default)]
pub struct MemFs {
    state: Mutex<MemState>,
    insensitive: bool,
}

impl MemFs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold path case on lookup (like APFS/HFS+ defaults).
    pub fn case_insensitive(mut self) -> Self {
        self.insensitive = true;
        self
    }

    /// Create or overwrite a file; an overwrite bumps its ino and ctime.
    pub fn with_file(self, path: &str, text: &str) -> Self {
        self.write(path, text.as_bytes());
        self
    }

    /// Mark `path` dataless (a cloud placeholder), creating an empty file
    /// there if it does not exist. Reads still return its bytes.
    pub fn with_dataless(self, path: &str) -> Self {
        let p = abs(Path::new(path));
        if self.node(&p).is_err() {
            self.write(path, b"");
        }
        let key = self.key(&p);
        if let Some(n) = self.lock().nodes.get_mut(&key) {
            n.meta.dataless = true;
        }
        self
    }

    pub fn with_dir(self, path: &str) -> Self {
        self.mkdir_p(&abs(Path::new(path)));
        self
    }

    /// Create or overwrite a file through a shared handle.
    pub fn write(&self, path: &str, bytes: &[u8]) {
        let p = abs(Path::new(path));
        if let Some(parent) = p.parent() {
            self.mkdir_p(parent);
        }
        let mut st = self.lock();
        let meta = st.next_meta(bytes.len() as u64, false);
        let key = self.key(&p);
        st.nodes.insert(
            key,
            Node {
                path: p,
                data: Some(bytes.into()),
                meta,
            },
        );
    }

    fn mkdir_p(&self, p: &Path) {
        let mut st = self.lock();
        let mut cur = PathBuf::from("/");
        for c in p.components().skip(1) {
            cur.push(c);
            let key = self.key(&cur);
            if !st.nodes.contains_key(&key) {
                let meta = st.next_meta(0, true);
                st.nodes.insert(
                    key,
                    Node {
                        path: cur.clone(),
                        data: None,
                        meta,
                    },
                );
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, MemState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn key(&self, p: &Path) -> String {
        let s = p.to_string_lossy();
        match self.insensitive {
            true => s.to_lowercase(),
            false => s.into_owned(),
        }
    }

    fn node(&self, p: &Path) -> io::Result<Node> {
        let p = abs(p);
        if p == Path::new("/") {
            return Ok(Node {
                path: p,
                data: None,
                meta: dir_meta(0),
            });
        }
        self.lock()
            .nodes
            .get(&self.key(&p))
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, p.display().to_string()))
    }
}

impl MemState {
    fn next_meta(&mut self, size: u64, is_dir: bool) -> Meta {
        self.clock += 1;
        let t = i128::from(self.clock);
        Meta {
            ino: self.clock,
            ctime_ns: t,
            mtime_ns: t,
            size,
            is_dir,
            is_file: !is_dir,
            dataless: false,
        }
    }
}

fn dir_meta(ino: u64) -> Meta {
    Meta {
        ino,
        ctime_ns: 0,
        mtime_ns: 0,
        size: 0,
        is_dir: true,
        is_file: false,
        dataless: false,
    }
}

/// `p` as a lexically cleaned absolute path under `/`.
fn abs(p: &Path) -> PathBuf {
    let mut out = PathBuf::from("/");
    for c in p.components() {
        match c {
            Component::Normal(n) => out.push(n),
            Component::ParentDir => {
                out.pop();
            }
            _ => {}
        }
    }
    out
}

impl FileSystem for MemFs {
    fn read(&self, p: &Path) -> io::Result<(Arc<[u8]>, Meta)> {
        let n = self.node(p)?;
        match n.data {
            Some(d) => Ok((d, n.meta)),
            None => Err(io::Error::other(format!("is a directory: {}", p.display()))),
        }
    }

    fn stat(&self, p: &Path) -> io::Result<Meta> {
        Ok(self.node(p)?.meta)
    }

    fn read_dir(&self, p: &Path) -> io::Result<Vec<(String, Meta)>> {
        let dir = self.node(p)?;
        if !dir.meta.is_dir {
            return Err(io::Error::other(format!(
                "not a directory: {}",
                p.display()
            )));
        }
        let mut out: Vec<(String, Meta)> = self
            .lock()
            .nodes
            .values()
            .filter(|n| self.key(n.path.parent().unwrap_or(Path::new(""))) == self.key(&dir.path))
            .filter_map(|n| {
                let name = n.path.file_name()?.to_string_lossy().into_owned();
                Some((name, n.meta))
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// The cleaned absolute path, with stored case when it exists.
    fn canonicalize(&self, p: &Path) -> io::Result<PathBuf> {
        Ok(self.node(p).map(|n| n.path).unwrap_or_else(|_| abs(p)))
    }

    fn case_sensitive(&self, _dir: &Path) -> bool {
        !self.insensitive
    }

    fn fs_kind(&self, _dir: &Path) -> FsKind {
        FsKind::Virtual
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memfs_tree() {
        let fs = MemFs::new().with_file("a/b.md", "x").with_dir("c");
        let names: Vec<String> = fs
            .read_dir(Path::new("/"))
            .unwrap()
            .into_iter()
            .map(|e| e.0)
            .collect();
        assert_eq!(names, ["a", "c"]);
        let before = fs.stat(Path::new("/a/b.md")).unwrap();
        fs.write("a/b.md", b"yy");
        let (data, after) = fs.read(Path::new("a/b.md")).unwrap();
        assert_eq!(&*data, b"yy");
        assert!(after.ino > before.ino && after.ctime_ns > before.ctime_ns);
        assert!(fs.stat(Path::new("/A/B.md")).is_err());
        let ci = MemFs::new().case_insensitive().with_file("A/b.md", "");
        assert!(ci.stat(Path::new("/a/B.MD")).is_ok());
        assert!(!ci.case_sensitive(Path::new("/")));
    }

    #[cfg(unix)]
    #[test]
    fn canonicalize_missing_file_below_symlink() {
        // Ported from ramble 75b8285 tests (canonical_uri_resolves_symlinks).
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("n é.md"), "").unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let fs = StdFs;
        let a = fs.canonicalize(&real.join("n é.md")).unwrap();
        assert_eq!(a, fs.canonicalize(&link.join("n é.md")).unwrap());
        // a not-yet-existing file below a symlinked dir canonicalizes too
        assert_eq!(
            fs.canonicalize(&link.join("new.md")).unwrap(),
            fs.canonicalize(&real.join("new.md")).unwrap()
        );
        assert_eq!(
            fs.canonicalize(&link.join("new.md")).unwrap(),
            real.canonicalize().unwrap().join("new.md")
        );
    }

    #[test]
    fn memfs_dataless() {
        let fs = MemFs::new()
            .with_file("a.md", "x")
            .with_dataless("a.md")
            .with_dataless("d/new.md")
            .with_dir("dir")
            .with_dataless("dir");
        let a = fs.stat(Path::new("/a.md")).unwrap();
        assert!(a.dataless && a.is_file);
        assert_eq!(&*fs.read(Path::new("/a.md")).unwrap().0, b"x");
        let n = fs.stat(Path::new("/d/new.md")).unwrap();
        assert!(n.dataless && n.is_file && n.size == 0);
        let d = fs.stat(Path::new("/dir")).unwrap();
        assert!(d.dataless && d.is_dir);
        assert!(!fs.stat(Path::new("/d")).unwrap().dataless);
    }

    #[cfg(unix)]
    #[test]
    fn stdfs_regular_file_is_not_dataless() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("n.md");
        std::fs::write(&f, "x").unwrap();
        assert!(!StdFs.stat(&f).unwrap().dataless);
        assert!(!StdFs.read(&f).unwrap().1.dataless);
        let entries = StdFs.read_dir(tmp.path()).unwrap();
        assert!(entries.iter().all(|(_, m)| !m.dataless));
    }

    #[cfg(unix)]
    #[test]
    fn created_time() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("n.md");
        std::fs::write(&f, "x").unwrap();
        // Linux filesystems may not record a birth time.
        if cfg!(target_os = "macos") {
            assert!(StdFs.created(&f).is_some());
        }
        assert_eq!(StdFs.created(&tmp.path().join("missing.md")), None);
        let mem = MemFs::new().with_file("/a.md", "x");
        assert_eq!(mem.created(Path::new("/a.md")), None);
    }

    #[cfg(unix)]
    #[test]
    fn stdfs_case_probe() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("Probe");
        std::fs::create_dir(&d).unwrap();
        let other = std::fs::metadata(tmp.path().join("probe")).is_ok();
        assert_eq!(StdFs.case_sensitive(&d), !other);
        assert_eq!(flip_case("aB1é"), "Ab1É");
    }
}
