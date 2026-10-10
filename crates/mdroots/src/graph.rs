//! Link-graph queries over a workspace: orphans, missing backlinks and
//! related notes. A link counts when it is explicit, outside code and
//! footnotes, and resolves (an ambiguous link counts for every candidate);
//! frontmatter links count. Self-links never count.

use std::path::{Path, PathBuf};

use crate::{Error, NoteSummary, Workspace};

impl Workspace {
    /// Notes no other note links to, sorted by path.
    pub fn orphans(&self) -> Result<Vec<NoteSummary>, Error> {
        let _ = self;
        todo!("U1")
    }

    /// `(from, to)` pairs where `from` links to `to` but `to` does not link
    /// back, sorted by `from` then `to`.
    pub fn missing_backlinks(&self) -> Result<Vec<(PathBuf, PathBuf)>, Error> {
        let _ = self;
        todo!("U1")
    }

    /// Notes two hops from `path` in either direction that `path` does not
    /// link to and that do not link to `path` (as `zk list --related`), with
    /// the number of shared neighbours, highest first, then by path.
    pub fn related(&self, path: &Path) -> Result<Vec<(NoteSummary, usize)>, Error> {
        let _ = (self, path);
        todo!("U1")
    }
}
