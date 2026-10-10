//! The filesystem seam for root discovery (docs/specs/roots.md §0, §1).
//!
//! Hard rule: mdroots never calls `read_dir` on a tree before it has
//! established that the tree is local and bounded. Every filesystem access
//! made by discovery goes through [`Probe`], so tests can supply a
//! [`FakeProbe`] and wrap it in [`Counting`] to count and forbid `read_dir`.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::io::{self, Read};
use std::ops::Bound;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use mdroots_core::fs::abs_path;

/// What discovery needs to know about one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsStat {
    pub dev: u64,
    pub ino: u64,
    pub is_dir: bool,
    pub is_file: bool,
    /// Only ever true from `lstat`: the path itself is a symlink.
    pub is_symlink: bool,
    /// A cloud placeholder whose contents are not on local disk.
    pub dataless: bool,
}

/// The class of filesystem a path lives on.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsClass {
    Local,
    /// Virtual filesystem (fetches on demand); the fs type as reported.
    Virtual(String),
    /// Network filesystem; the fs type as reported.
    Remote(String),
    /// A cloud-synced folder on a local volume.
    Cloud,
}

/// The mount a path lives on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountInfo {
    /// The fs type name as reported (macOS `f_fstypename`, Linux mountinfo).
    pub fs_type: String,
    /// Mount source (macOS `f_mntfromname`, Linux mountinfo source).
    pub from: String,
    /// macOS `MNT_LOCAL`; on Linux derived from the fs type.
    pub local: bool,
    /// `st_dev` of the queried path.
    pub dev: u64,
}

pub trait Probe: Send + Sync {
    /// Follows symlinks.
    fn stat(&self, p: &Path) -> io::Result<FsStat>;
    /// Does not follow a symlink in the last component.
    fn lstat(&self, p: &Path) -> io::Result<FsStat>;
    fn mount(&self, p: &Path) -> io::Result<MountInfo>;
    /// Entry names sorted, each with its `lstat`. Names are returned as the
    /// OS gives them (not necessarily UTF-8), so joining one onto `p` names
    /// the real entry.
    fn read_dir(&self, p: &Path) -> io::Result<Vec<(OsString, FsStat)>>;
    fn read_link(&self, p: &Path) -> io::Result<PathBuf>;
    /// The whole file; `InvalidData` if it is larger than `cap` bytes.
    fn read_small(&self, p: &Path, cap: usize) -> io::Result<Vec<u8>>;
    /// The first `n` bytes only (fewer if the file is shorter).
    fn read_prefix(&self, p: &Path, n: usize) -> io::Result<Vec<u8>>;
    /// Identifies the volume `p` lives on; see the implementations for how
    /// stable it is.
    fn volume_id(&self, p: &Path) -> io::Result<String>;
    fn home(&self) -> Option<PathBuf>;
    /// Monotonic time since an arbitrary epoch.
    fn now(&self) -> Duration;
}

/// Name prefixes of virtual filesystems: [EdenFS](https://github.com/facebook/sapling)
/// (the virtual filesystem from the [Sapling](https://sapling-scm.com/) project; macOS reports
/// `edenfs:` with a trailing colon), [macFUSE](https://macfuse.github.io)
/// and other FUSE mounts, virtiofs and 9p.
const VIRTUAL_PREFIXES: &[&str] = &["edenfs", "fuse", "macfuse", "osxfuse", "virtiofs", "9p"];
/// Kernel pseudo-filesystems (Linux; `devfs` on macOS): flagged local but
/// generated on read, never note roots, so they are treated as virtual
/// (no walk). Exact fs type names.
const PSEUDO_FS: &[&str] = &[
    "proc",
    "sysfs",
    "devtmpfs",
    "devfs",
    "devpts",
    "cgroup",
    "cgroup2",
    "tracefs",
    "debugfs",
    "securityfs",
    "pstore",
    "bpf",
    "configfs",
    "efivarfs",
    "mqueue",
    "hugetlbfs",
    "selinuxfs",
    "rpc_pipefs",
    "binfmt_misc",
    "autofs",
    "nsfs",
];
/// Name prefixes of network filesystems.
const REMOTE_PREFIXES: &[&str] = &["nfs", "smbfs", "afpfs", "webdav", "sshfs", "cifs", "smb3"];

/// Classify the mount `m` holding `path` (docs/specs/roots.md §1 stage 3).
///
/// `MNT_LOCAL` alone is not trusted: a known virtual or remote name prefix in
/// `fs_type` or `from` (case-insensitive) wins even on a local-flagged mount.
/// Any other non-local mount is remote. Cloud folders are on a local volume
/// and only visible by path: strictly under `<home>/Library/CloudStorage` or
/// `<home>/Library/Mobile Documents` (exact case, as macOS creates them).
pub fn classify(m: &MountInfo, path: &Path, home: Option<&Path>) -> FsClass {
    let names = [m.fs_type.to_lowercase(), m.from.to_lowercase()];
    let matches = |prefixes: &[&str]| {
        names
            .iter()
            .any(|n| prefixes.iter().any(|p| n.starts_with(p)))
    };
    if matches(VIRTUAL_PREFIXES) || PSEUDO_FS.contains(&names[0].as_str()) {
        return FsClass::Virtual(m.fs_type.clone());
    }
    if matches(REMOTE_PREFIXES) || !m.local {
        return FsClass::Remote(m.fs_type.clone());
    }
    if let Some(home) = home {
        let lib = home.join("Library");
        for d in ["CloudStorage", "Mobile Documents"] {
            let d = lib.join(d);
            if path != d && path.starts_with(&d) {
                return FsClass::Cloud;
            }
        }
    }
    FsClass::Local
}

// ---------------------------------------------------------------------------
// Linux /proc/self/mountinfo

/// One line of `/proc/self/mountinfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MountLine {
    point: PathBuf,
    fs_type: String,
    source: String,
}

/// Parse mountinfo (proc(5)): field 5 is the mount point (octal-escaped),
/// then optional fields up to `-`, then fs type and source.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_mountinfo(text: &str) -> Vec<MountLine> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.split(' ');
            let point = f.nth(4)?;
            let mut rest = f.skip_while(|x| *x != "-");
            rest.next()?;
            let fs_type = rest.next()?.to_owned();
            let source =
                String::from_utf8_lossy(&unescape_octal(rest.next().unwrap_or(""))).into_owned();
            Some(MountLine {
                point: bytes_to_path(unescape_octal(point)),
                fs_type,
                source,
            })
        })
        .collect()
}

/// Undo the kernel's `\ooo` escaping (space, tab, newline, backslash).
fn unescape_octal(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let esc = b
            .get(i + 1..i + 4)
            .filter(|d| b[i] == b'\\' && d.iter().all(|c| (b'0'..=b'7').contains(c)));
        let val = esc.and_then(|d| {
            u8::try_from(d.iter().fold(0u32, |a, c| a * 8 + u32::from(c - b'0'))).ok()
        });
        match val {
            Some(v) => {
                out.push(v);
                i += 4;
            }
            None => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    out
}

#[cfg(unix)]
fn bytes_to_path(b: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_vec(b))
}

#[cfg(not(unix))]
fn bytes_to_path(b: Vec<u8>) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(&b).into_owned())
}

/// The mount holding `path`: longest component-wise prefix; on a tie the
/// later line (an overmount) wins.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn mount_for<'a>(lines: &'a [MountLine], path: &Path) -> Option<&'a MountLine> {
    lines
        .iter()
        .filter(|l| path.starts_with(&l.point))
        .max_by_key(|l| l.point.components().count())
}

/// Linux fs types that are not local disks (network, FUSE, virtual).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_local(fs_type: &str) -> bool {
    !(fs_type.starts_with("nfs")
        || fs_type.starts_with("fuse.")
        || matches!(
            fs_type,
            "cifs" | "smb3" | "smbfs" | "fuse" | "9p" | "virtiofs" | "ceph" | "afs"
        ))
}

// ---------------------------------------------------------------------------
// StdProbe

/// [`Probe`] over `std::fs`, plus `statfs` via [rustix](https://github.com/bytecodealliance/rustix) on macOS.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, Default)]
pub struct StdProbe;

/// `SF_DATALESS` from `<sys/stat.h>`: the file's contents are not local
/// (cloud placeholder). See Apple's stat(2) man page:
/// <https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/stat.2.html>
#[cfg(target_os = "macos")]
const SF_DATALESS: u32 = 0x4000_0000;

/// `MNT_LOCAL` from `<sys/mount.h>`.
#[cfg(target_os = "macos")]
const MNT_LOCAL: u32 = 0x1000;

#[cfg(unix)]
fn std_stat(m: &std::fs::Metadata) -> FsStat {
    use std::os::unix::fs::MetadataExt;
    #[cfg(target_os = "macos")]
    let dataless = {
        use std::os::macos::fs::MetadataExt as _;
        m.st_flags() & SF_DATALESS != 0
    };
    #[cfg(not(target_os = "macos"))]
    let dataless = false;
    let ft = m.file_type();
    FsStat {
        dev: m.dev(),
        ino: m.ino(),
        is_dir: ft.is_dir(),
        is_file: ft.is_file(),
        is_symlink: ft.is_symlink(),
        dataless,
    }
}

#[cfg(target_os = "macos")]
#[allow(clippy::unnecessary_cast)] // c_char is i8 or u8 depending on target
fn c_chars(s: &[std::ffi::c_char]) -> String {
    let b: Vec<u8> = s
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    String::from_utf8_lossy(&b).into_owned()
}

#[cfg(unix)]
impl Probe for StdProbe {
    fn stat(&self, p: &Path) -> io::Result<FsStat> {
        Ok(std_stat(&std::fs::metadata(p)?))
    }

    fn lstat(&self, p: &Path) -> io::Result<FsStat> {
        Ok(std_stat(&std::fs::symlink_metadata(p)?))
    }

    fn mount(&self, p: &Path) -> io::Result<MountInfo> {
        use std::os::unix::fs::MetadataExt;
        let dev = std::fs::metadata(p)?.dev();
        #[cfg(target_os = "macos")]
        {
            let s = rustix::fs::statfs(p)?;
            Ok(MountInfo {
                fs_type: c_chars(&s.f_fstypename),
                from: c_chars(&s.f_mntfromname),
                local: s.f_flags & MNT_LOCAL != 0,
                dev,
            })
        }
        #[cfg(target_os = "linux")]
        {
            let canon = std::fs::canonicalize(p)?;
            let text = std::fs::read_to_string("/proc/self/mountinfo")?;
            let lines = parse_mountinfo(&text);
            Ok(match mount_for(&lines, &canon) {
                Some(l) => MountInfo {
                    fs_type: l.fs_type.clone(),
                    from: l.source.clone(),
                    local: linux_local(&l.fs_type),
                    dev,
                },
                None => MountInfo {
                    fs_type: "unknown".into(),
                    from: String::new(),
                    local: true,
                    dev,
                },
            })
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        Ok(MountInfo {
            fs_type: "unknown".into(),
            from: String::new(),
            local: true,
            dev,
        })
    }

    /// Entries whose metadata vanished between listing and `lstat` are
    /// skipped.
    fn read_dir(&self, p: &Path) -> io::Result<Vec<(OsString, FsStat)>> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(p)? {
            let e = e?;
            // DirEntry::metadata does not follow symlinks.
            let Ok(m) = e.metadata() else { continue };
            out.push((e.file_name(), std_stat(&m)));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    fn read_link(&self, p: &Path) -> io::Result<PathBuf> {
        std::fs::read_link(p)
    }

    fn read_small(&self, p: &Path, cap: usize) -> io::Result<Vec<u8>> {
        let mut buf = Vec::new();
        std::fs::File::open(p)?
            .take((cap as u64).saturating_add(1))
            .read_to_end(&mut buf)?;
        if buf.len() > cap {
            return Err(too_large(p, cap));
        }
        Ok(buf)
    }

    fn read_prefix(&self, p: &Path, n: usize) -> io::Result<Vec<u8>> {
        let mut buf = Vec::with_capacity(n.min(1 << 16));
        std::fs::File::open(p)?
            .take(n as u64)
            .read_to_end(&mut buf)?;
        Ok(buf)
    }

    /// `dev:<st_dev hex>`: stable while the volume stays mounted, not across
    /// reboots or remounts. (`ATTR_VOL_UUID` and `f_fsid` are not reachable
    /// without unsafe code; deferred.) Root-move detection is therefore
    /// limited to one boot/mount.
    fn volume_id(&self, p: &Path) -> io::Result<String> {
        use std::os::unix::fs::MetadataExt;
        Ok(format!("dev:{:x}", std::fs::metadata(p)?.dev()))
    }

    fn home(&self) -> Option<PathBuf> {
        std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
    }

    fn now(&self) -> Duration {
        static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        EPOCH.get_or_init(std::time::Instant::now).elapsed()
    }
}

fn too_large(p: &Path, cap: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{} is larger than {cap} bytes", p.display()),
    )
}

// ---------------------------------------------------------------------------
// FakeProbe

#[derive(Debug, Clone)]
enum Kind {
    Dir,
    File(Vec<u8>),
    Symlink(PathBuf),
}

#[derive(Debug, Clone)]
struct Node {
    kind: Kind,
    ino: u64,
    dataless: bool,
}

#[derive(Debug)]
struct FakeState {
    nodes: BTreeMap<PathBuf, Node>,
    mounts: Vec<(PathBuf, MountInfo)>,
    costs: Vec<(PathBuf, Duration)>,
    home: Option<PathBuf>,
    now: Duration,
    next_ino: u64,
}

/// An in-memory [`Probe`] built with a builder. Paths are absolute and
/// cleaned lexically. Parent directories are created on demand.
///
/// - `stat`/`lstat` `dev` is the `dev` of the longest-prefix (component-wise)
///   [`FakeProbe::mount`]; the default mount is local `apfs`, dev 1.
/// - Inodes are auto-assigned, unique and stable per path; [`FakeProbe::ino`]
///   overrides one, [`FakeProbe::rename`] moves a subtree keeping its inodes.
/// - Each `read_dir` advances [`Probe::now`] by the longest-prefix
///   [`FakeProbe::read_dir_cost`] (default 0.2 ms).
/// - `volume_id` is `fake:<mount dev>`.
///
/// The builder methods `home` and `mount` shadow the [`Probe`] methods of the
/// same name on a `FakeProbe` value; call those as `Probe::home(&fake)` or
/// through `&dyn Probe`.
#[derive(Debug)]
pub struct FakeProbe(Mutex<FakeState>);

const DEFAULT_COST: Duration = Duration::from_micros(200);
const MAX_SYMLINKS: usize = 40;

impl Default for FakeProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeProbe {
    pub fn new() -> Self {
        let mut nodes = BTreeMap::new();
        nodes.insert(
            PathBuf::from("/"),
            Node {
                kind: Kind::Dir,
                ino: 1,
                dataless: false,
            },
        );
        FakeProbe(Mutex::new(FakeState {
            nodes,
            mounts: Vec::new(),
            costs: Vec::new(),
            home: None,
            now: Duration::ZERO,
            next_ino: 2,
        }))
    }

    fn st(&mut self) -> &mut FakeState {
        self.0.get_mut().unwrap_or_else(|e| e.into_inner())
    }

    fn lock(&self) -> MutexGuard<'_, FakeState> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn dir(mut self, p: impl AsRef<Path>) -> Self {
        self.st().put(&abs_path(p.as_ref()), Kind::Dir);
        self
    }

    pub fn file(mut self, p: impl AsRef<Path>, bytes: impl AsRef<[u8]>) -> Self {
        self.st()
            .put(&abs_path(p.as_ref()), Kind::File(bytes.as_ref().to_vec()));
        self
    }

    pub fn symlink(mut self, p: impl AsRef<Path>, target: impl AsRef<Path>) -> Self {
        self.st().put(
            &abs_path(p.as_ref()),
            Kind::Symlink(target.as_ref().to_path_buf()),
        );
        self
    }

    /// Everything under `prefix` (component-wise, longest prefix wins) is on
    /// mount `m`.
    pub fn mount(mut self, prefix: impl AsRef<Path>, m: MountInfo) -> Self {
        let prefix = abs_path(prefix.as_ref());
        let st = self.st();
        st.mounts.retain(|(p, _)| *p != prefix);
        st.mounts.push((prefix, m));
        self
    }

    /// Mark `p` dataless, creating it as an empty file if missing.
    pub fn dataless(mut self, p: impl AsRef<Path>) -> Self {
        let p = abs_path(p.as_ref());
        let st = self.st();
        if !st.nodes.contains_key(&p) {
            st.put(&p, Kind::File(Vec::new()));
        }
        if let Some(n) = st.nodes.get_mut(&p) {
            n.dataless = true;
        }
        self
    }

    pub fn home(mut self, p: impl AsRef<Path>) -> Self {
        self.st().home = Some(abs_path(p.as_ref()));
        self
    }

    /// Each `read_dir` under `prefix` advances `now()` by `cost`.
    pub fn read_dir_cost(mut self, prefix: impl AsRef<Path>, cost: Duration) -> Self {
        let prefix = abs_path(prefix.as_ref());
        let st = self.st();
        st.costs.retain(|(p, _)| *p != prefix);
        st.costs.push((prefix, cost));
        self
    }

    /// Set the inode of an existing path.
    ///
    /// # Panics
    /// If `p` has not been added.
    pub fn ino(mut self, p: impl AsRef<Path>, ino: u64) -> Self {
        let p = abs_path(p.as_ref());
        match self.st().nodes.get_mut(&p) {
            Some(n) => n.ino = ino,
            None => panic!("FakeProbe::ino: {} not added", p.display()),
        }
        self
    }

    /// Move `from` and its subtree to `to`, keeping inodes.
    ///
    /// # Panics
    /// If `from` has not been added.
    pub fn rename(mut self, from: impl AsRef<Path>, to: impl AsRef<Path>) -> Self {
        let (from, to) = (abs_path(from.as_ref()), abs_path(to.as_ref()));
        let st = self.st();
        let moved: Vec<PathBuf> = st
            .nodes
            .keys()
            .filter(|k| k.starts_with(&from))
            .cloned()
            .collect();
        assert!(
            !moved.is_empty(),
            "FakeProbe::rename: {} not added",
            from.display()
        );
        if let Some(parent) = to.parent() {
            st.put(parent, Kind::Dir);
        }
        let nodes: Vec<(PathBuf, Node)> = moved
            .into_iter()
            .filter_map(|k| st.nodes.remove(&k).map(|n| (k, n)))
            .collect();
        st.nodes.retain(|k, _| !k.starts_with(&to));
        for (k, n) in nodes {
            let rel = k.strip_prefix(&from).unwrap_or(Path::new(""));
            let dest = if rel.as_os_str().is_empty() {
                to.clone()
            } else {
                to.join(rel)
            };
            st.nodes.insert(dest, n);
        }
        self
    }
}

impl FakeState {
    /// Insert or replace `p`, creating missing parents as directories.
    fn put(&mut self, p: &Path, kind: Kind) {
        if let Some(parent) = p.parent()
            && !matches!(
                self.nodes.get(parent),
                Some(Node {
                    kind: Kind::Dir,
                    ..
                })
            )
        {
            self.put(parent, Kind::Dir);
        }
        let ino = match self.nodes.get(p) {
            Some(n) => n.ino,
            None => {
                self.next_ino += 1;
                self.next_ino - 1
            }
        };
        let dataless = self.nodes.get(p).is_some_and(|n| n.dataless);
        self.nodes.insert(
            p.to_path_buf(),
            Node {
                kind,
                ino,
                dataless,
            },
        );
    }

    /// `p` with symlinks resolved (the last one only if `follow_last`).
    fn resolve(&self, p: &Path, follow_last: bool) -> io::Result<PathBuf> {
        let mut pending = abs_path(p);
        'restart: for _ in 0..=MAX_SYMLINKS {
            let comps: Vec<_> = pending
                .components()
                .filter_map(|c| match c {
                    Component::Normal(n) => Some(n.to_owned()),
                    _ => None,
                })
                .collect();
            let mut cur = PathBuf::from("/");
            for (i, c) in comps.iter().enumerate() {
                let cand = cur.join(c);
                let last = i + 1 == comps.len();
                match self.nodes.get(&cand) {
                    None => return Err(not_found(p)),
                    Some(Node {
                        kind: Kind::Symlink(t),
                        ..
                    }) if !last || follow_last => {
                        let mut next = cur.join(t);
                        next.extend(&comps[i + 1..]);
                        pending = abs_path(&next);
                        continue 'restart;
                    }
                    Some(Node {
                        kind: Kind::File(_) | Kind::Symlink(_),
                        ..
                    }) if !last => {
                        return Err(io::Error::new(
                            io::ErrorKind::NotADirectory,
                            p.display().to_string(),
                        ));
                    }
                    Some(_) => cur = cand,
                }
            }
            return Ok(cur);
        }
        Err(io::Error::other(format!(
            "too many levels of symbolic links: {}",
            p.display()
        )))
    }

    fn mount_of(&self, p: &Path) -> MountInfo {
        longest(&self.mounts, p)
            .cloned()
            .unwrap_or_else(|| MountInfo {
                fs_type: "apfs".into(),
                from: "/dev/fake".into(),
                local: true,
                dev: 1,
            })
    }

    fn stat_at(&self, p: &Path) -> io::Result<FsStat> {
        let n = self.nodes.get(p).ok_or_else(|| not_found(p))?;
        Ok(FsStat {
            dev: self.mount_of(p).dev,
            ino: n.ino,
            is_dir: matches!(n.kind, Kind::Dir),
            is_file: matches!(n.kind, Kind::File(_)),
            is_symlink: matches!(n.kind, Kind::Symlink(_)),
            dataless: n.dataless,
        })
    }

    fn file_bytes(&self, p: &Path) -> io::Result<&[u8]> {
        let r = self.resolve(p, true)?;
        match &self.nodes.get(&r).map(|n| &n.kind) {
            Some(Kind::File(b)) => Ok(b),
            _ => Err(io::Error::new(
                io::ErrorKind::IsADirectory,
                p.display().to_string(),
            )),
        }
    }
}

/// The value of the longest component-wise prefix of `p` in `v`.
fn longest<'a, T>(v: &'a [(PathBuf, T)], p: &Path) -> Option<&'a T> {
    v.iter()
        .filter(|(pre, _)| p.starts_with(pre))
        .max_by_key(|(pre, _)| pre.components().count())
        .map(|(_, t)| t)
}

fn not_found(p: &Path) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, p.display().to_string())
}

impl Probe for FakeProbe {
    fn stat(&self, p: &Path) -> io::Result<FsStat> {
        let st = self.lock();
        st.stat_at(&st.resolve(p, true)?)
    }

    fn lstat(&self, p: &Path) -> io::Result<FsStat> {
        let st = self.lock();
        st.stat_at(&st.resolve(p, false)?)
    }

    fn mount(&self, p: &Path) -> io::Result<MountInfo> {
        let st = self.lock();
        Ok(st.mount_of(&st.resolve(p, true)?))
    }

    fn read_dir(&self, p: &Path) -> io::Result<Vec<(OsString, FsStat)>> {
        let mut st = self.lock();
        let dir = st.resolve(p, true)?;
        if !matches!(st.nodes.get(&dir).map(|n| &n.kind), Some(Kind::Dir)) {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                p.display().to_string(),
            ));
        }
        let cost = longest(&st.costs, &dir).copied().unwrap_or(DEFAULT_COST);
        st.now += cost;
        let mut out = Vec::new();
        for k in st
            .nodes
            .range::<PathBuf, _>((Bound::Excluded(&dir), Bound::Unbounded))
            .map(|(k, _)| k)
            .take_while(|k| k.starts_with(&dir))
            .filter(|k| k.parent() == Some(dir.as_path()))
        {
            let name = k.file_name().unwrap_or_default().to_owned();
            out.push((name, st.stat_at(k)?));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    fn read_link(&self, p: &Path) -> io::Result<PathBuf> {
        let st = self.lock();
        let r = st.resolve(p, false)?;
        match st.nodes.get(&r).map(|n| &n.kind) {
            Some(Kind::Symlink(t)) => Ok(t.clone()),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("not a symlink: {}", p.display()),
            )),
        }
    }

    fn read_small(&self, p: &Path, cap: usize) -> io::Result<Vec<u8>> {
        let st = self.lock();
        let b = st.file_bytes(p)?;
        if b.len() > cap {
            return Err(too_large(p, cap));
        }
        Ok(b.to_vec())
    }

    fn read_prefix(&self, p: &Path, n: usize) -> io::Result<Vec<u8>> {
        let st = self.lock();
        let b = st.file_bytes(p)?;
        Ok(b[..n.min(b.len())].to_vec())
    }

    fn volume_id(&self, p: &Path) -> io::Result<String> {
        Ok(format!("fake:{}", self.stat(p)?.dev))
    }

    fn home(&self) -> Option<PathBuf> {
        self.lock().home.clone()
    }

    fn now(&self) -> Duration {
        self.lock().now
    }
}

// ---------------------------------------------------------------------------
// Counting

#[derive(Debug, Default)]
struct CountState {
    counts: HashMap<PathBuf, usize>,
    total: usize,
    allowed: Option<Vec<PathBuf>>,
    violations: Vec<PathBuf>,
}

/// Wraps a [`Probe`], counting `read_dir` calls per path (as passed) and
/// recording calls outside an allowed set. Forbidden calls are recorded,
/// then still delegated.
#[derive(Debug)]
pub struct Counting<P: Probe> {
    inner: P,
    state: Mutex<CountState>,
}

impl<P: Probe> Counting<P> {
    pub fn new(inner: P) -> Self {
        Counting {
            inner,
            state: Mutex::default(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, CountState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// From now on a `read_dir` on a path not equal to or under one of
    /// `allowed` is a violation. An empty list forbids every `read_dir`.
    pub fn forbid_read_dir_outside(&self, allowed: &[PathBuf]) {
        self.lock().allowed = Some(allowed.to_vec());
    }

    pub fn read_dir_count(&self, p: &Path) -> usize {
        self.lock().counts.get(p).copied().unwrap_or(0)
    }

    pub fn read_dir_total(&self) -> usize {
        self.lock().total
    }

    pub fn violations(&self) -> Vec<PathBuf> {
        self.lock().violations.clone()
    }

    pub fn inner(&self) -> &P {
        &self.inner
    }
}

impl<P: Probe> Probe for Counting<P> {
    fn stat(&self, p: &Path) -> io::Result<FsStat> {
        self.inner.stat(p)
    }

    fn lstat(&self, p: &Path) -> io::Result<FsStat> {
        self.inner.lstat(p)
    }

    fn mount(&self, p: &Path) -> io::Result<MountInfo> {
        self.inner.mount(p)
    }

    fn read_dir(&self, p: &Path) -> io::Result<Vec<(OsString, FsStat)>> {
        {
            let mut st = self.lock();
            *st.counts.entry(p.to_path_buf()).or_default() += 1;
            st.total += 1;
            let forbidden = st
                .allowed
                .as_ref()
                .is_some_and(|a| !a.iter().any(|d| p.starts_with(d)));
            if forbidden {
                st.violations.push(p.to_path_buf());
            }
        }
        self.inner.read_dir(p)
    }

    fn read_link(&self, p: &Path) -> io::Result<PathBuf> {
        self.inner.read_link(p)
    }

    fn read_small(&self, p: &Path, cap: usize) -> io::Result<Vec<u8>> {
        self.inner.read_small(p, cap)
    }

    fn read_prefix(&self, p: &Path, n: usize) -> io::Result<Vec<u8>> {
        self.inner.read_prefix(p, n)
    }

    fn volume_id(&self, p: &Path) -> io::Result<String> {
        self.inner.volume_id(p)
    }

    fn home(&self) -> Option<PathBuf> {
        self.inner.home()
    }

    fn now(&self) -> Duration {
        self.inner.now()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTINFO: &str = "\
22 1 0:21 / / rw,relatime shared:1 - ext4 /dev/sda1 rw
23 22 0:22 / /proc rw shared:2 - proc proc rw
30 22 0:30 / /mnt/my\\040notes rw - nfs4 server:/export rw,vers=4.2
31 30 0:31 / /mnt/my\\040notes/local rw master:3 - ext4 /dev/sdb1 rw
32 22 0:32 / /mnt/tab\\011nl\\012bs\\134 rw - fuse.sshfs host:/ rw
33 22 0:33 / /mnt/my rw - virtiofs share rw
";

    #[test]
    fn mountinfo_parses_and_unescapes() {
        let l = parse_mountinfo(MOUNTINFO);
        assert_eq!(l.len(), 6);
        assert_eq!(l[2].point, PathBuf::from("/mnt/my notes"));
        assert_eq!(l[2].fs_type, "nfs4");
        assert_eq!(l[2].source, "server:/export");
        assert_eq!(l[3].fs_type, "ext4");
        assert_eq!(l[4].point, PathBuf::from("/mnt/tab\tnl\nbs\\"));
        assert_eq!(l[4].fs_type, "fuse.sshfs");
    }

    #[test]
    fn mountinfo_longest_component_prefix() {
        let l = parse_mountinfo(MOUNTINFO);
        let at = |p: &str| mount_for(&l, Path::new(p)).map(|m| m.fs_type.as_str());
        assert_eq!(at("/home/alice/notes"), Some("ext4"));
        assert_eq!(at("/mnt/my notes/a.md"), Some("nfs4"));
        assert_eq!(at("/mnt/my notes/local/a.md"), Some("ext4"));
        // "/mnt/my" is a string prefix of "/mnt/my notes" but not a component one.
        assert_eq!(at("/mnt/my/x"), Some("virtiofs"));
        assert_eq!(at("/mnt/myx"), Some("ext4"));
    }

    #[test]
    fn linux_local_set() {
        for t in ["ext4", "xfs", "btrfs", "tmpfs", "apfs", "fuseblk"] {
            assert!(linux_local(t), "{t}");
        }
        for t in [
            "nfs",
            "nfs4",
            "cifs",
            "smb3",
            "smbfs",
            "fuse",
            "fuse.sshfs",
            "9p",
            "virtiofs",
            "ceph",
            "afs",
        ] {
            assert!(!linux_local(t), "{t}");
        }
    }
}
