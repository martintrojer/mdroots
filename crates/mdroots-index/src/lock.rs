//! Per-root flock roles (docs/DECISIONS.md D3).
//!
//! Every process with a root's DB open holds `LOCK_SH` on `<stem>.open`, so
//! GC (which needs it exclusively) never unlinks a DB in use. A rebuild
//! does not take it exclusively: it writes a new generation file and leaves
//! the old one for GC.
//! The process that wins a non-blocking `LOCK_EX` on `<stem>.lock` is the
//! reconciler, the only writer of the DB; everyone else is a read-only peer.
//! The kernel releases both locks when the process exits or crashes, so there
//! is no stale-lock cleanup. Lock files are never unlinked, because a process
//! waiting on an unlinked lock file would elect a second reconciler.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::Path;

/// This process's role for one root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Holds `<stem>.lock` exclusively; the only DB writer.
    Reconciler,
    /// Read-only; may become the reconciler via [`RootLocks::try_promote`].
    Peer,
}

/// The locks one process holds on one root; released on drop.
#[derive(Debug)]
pub struct RootLocks {
    /// Held shared for as long as this value lives.
    _open: File,
    lock: File,
    role: Role,
}

fn open_lock_file(p: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(p)
}

impl RootLocks {
    /// Create `dir` (0700) if missing, take `LOCK_SH` on `<dir>/<stem>.open`
    /// and try `LOCK_EX|LOCK_NB` on `<dir>/<stem>.lock`.
    ///
    /// The shared lock blocks only while an exclusive holder (GC) runs.
    /// A second acquire in the same process uses a new file handle, which
    /// flock treats as a separate holder, so it becomes a peer.
    pub fn acquire(dir: &Path, stem: &str) -> io::Result<RootLocks> {
        crate::create_private_dir(dir)?;
        let open = open_lock_file(&dir.join(format!("{stem}.open")))?;
        open.lock_shared()?;
        let lock = open_lock_file(&dir.join(format!("{stem}.lock")))?;
        let mut locks = RootLocks {
            _open: open,
            lock,
            role: Role::Peer,
        };
        locks.try_promote()?;
        Ok(locks)
    }

    pub fn role(&self) -> Role {
        self.role
    }

    /// Retry the exclusive lock if this process is a peer; returns the
    /// resulting role. A reconciler stays one.
    pub fn try_promote(&mut self) -> io::Result<Role> {
        if self.role == Role::Peer {
            match self.lock.try_lock() {
                Ok(()) => self.role = Role::Reconciler,
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Error(e)) => return Err(e),
            }
        }
        Ok(self.role)
    }
}
