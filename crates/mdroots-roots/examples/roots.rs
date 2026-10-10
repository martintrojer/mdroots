//! Print the root discovery decision for a file:
//!
//! ```text
//! cargo run -p mdroots-roots --example roots -- [--no-read-dir] <file>
//! ```
//!
//! Runs [`discover`] over the real filesystem ([`StdProbe`]) with an
//! in-memory registry and no VCS enumerator, then prints [`explain`] and the
//! number of `read_dir` calls made. With `--no-read-dir` every `read_dir` is
//! refused (and still counted), which shows what discovery decides from
//! `stat`, `statfs` and small reads alone.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use mdroots_core::Cancel;
use mdroots_roots::{
    DiscoverOptions, FsStat, MemRegistry, MountInfo, NoEnumerator, Probe, StdProbe, discover,
    explain,
};

/// [`StdProbe`] that counts `read_dir` calls and refuses them if `refuse`.
struct Guard {
    inner: StdProbe,
    refuse: bool,
    read_dirs: AtomicUsize,
}

impl Probe for Guard {
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
        self.read_dirs.fetch_add(1, Ordering::Relaxed);
        if self.refuse {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "read_dir refused (--no-read-dir)",
            ));
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

fn main() -> ExitCode {
    let mut refuse = false;
    let mut file = None;
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "--no-read-dir" => refuse = true,
            _ if file.is_none() && !a.starts_with("--") => file = Some(PathBuf::from(a)),
            _ => file = None,
        }
    }
    let Some(file) = file else {
        eprintln!("usage: roots [--no-read-dir] <file>");
        return ExitCode::from(2);
    };
    let file = match file.canonicalize() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("roots: {}: {e}", file.display());
            return ExitCode::FAILURE;
        }
    };
    let probe = Guard {
        inner: StdProbe,
        refuse,
        read_dirs: AtomicUsize::new(0),
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let opts = DiscoverOptions {
        now_ms,
        ..Default::default()
    };
    let d = discover(
        &probe,
        &mut MemRegistry::new(),
        &NoEnumerator,
        &file,
        &opts,
        &Cancel::new(),
    );
    println!("{}", explain(&d));
    let n = probe.read_dirs.load(Ordering::Relaxed);
    let refused = if refuse { " (all refused)" } else { "" };
    println!("read_dir: {n}{refused}");
    ExitCode::SUCCESS
}
