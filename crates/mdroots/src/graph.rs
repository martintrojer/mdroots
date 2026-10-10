//! Link-graph queries over a workspace: orphans, missing backlinks, related
//! notes and the notes one link away. An edge `a → b` is a link in `a` that
//! [`Workspace::backlinks`] reports for `b`: a link in prose, a heading,
//! HTML or frontmatter (not code, not a footnote) that resolves to the
//! indexed note `b`; an ambiguous link is an edge to every candidate.
//! Broken links and links to files that are not notes are no edges.
//! Self-links never count (zk counts them: a note linking only to itself
//! is an orphan here, not in zk).
//!
//! Every query builds the graph once from the store's backlink index, so it
//! costs O(notes + links). They need every note indexed: on a
//! [`Freshness::Lazy`] root they are an `Unsupported` error rather than a
//! partial answer.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use mdroots_core::MemStore;

use crate::{Error, ErrorKind, Freshness, NoteSummary, Workspace};

/// Root-relative note → its neighbours one way, without self-links.
type Adjacency = BTreeMap<String, BTreeSet<String>>;

/// The note graph: `out[a]` holds what `a` links to, `inc[b]` what links
/// to `b`. Every indexed note has an entry in both.
struct Graph {
    out: Adjacency,
    inc: Adjacency,
}

impl Graph {
    fn build(store: &MemStore) -> Graph {
        let notes: Vec<&str> = store
            .files()
            .filter(|f| store.document(f).is_some())
            .collect();
        let mut out: Adjacency = notes
            .iter()
            .map(|n| (n.to_string(), BTreeSet::new()))
            .collect();
        let mut inc = out.clone();
        for to in &notes {
            for (from, _) in store.backlinks(to) {
                if from != *to && store.document(&from).is_some() {
                    out.entry(from.clone()).or_default().insert(to.to_string());
                    inc.entry(to.to_string()).or_default().insert(from);
                }
            }
        }
        Graph { out, inc }
    }

    /// Outgoing and incoming neighbours of `n`.
    fn neighbours(&self, n: &str) -> BTreeSet<&str> {
        [&self.out, &self.inc]
            .into_iter()
            .filter_map(|m| m.get(n))
            .flatten()
            .map(String::as_str)
            .collect()
    }
}

impl Workspace {
    /// The graph of a fully indexed root; `Unsupported` on a lazy one.
    fn graph(&self) -> Result<Graph, Error> {
        if self.freshness() == Freshness::Lazy {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "link-graph queries need a full index (this root is lazy)",
            ));
        }
        Ok(Graph::build(&self.store()))
    }

    /// The summaries of root-relative `rels`, in path order.
    fn summaries<'a>(&self, rels: impl IntoIterator<Item = &'a str>) -> Vec<NoteSummary> {
        let want: BTreeSet<PathBuf> = rels.into_iter().map(|r| self.abs(r)).collect();
        self.notes()
            .into_iter()
            .filter(|n| want.contains(&n.path))
            .collect()
    }

    /// Notes no other note links to, sorted by path.
    pub fn orphans(&self) -> Result<Vec<NoteSummary>, Error> {
        let g = self.graph()?;
        Ok(self.summaries(
            g.inc
                .iter()
                .filter(|(_, from)| from.is_empty())
                .map(|(n, _)| n.as_str()),
        ))
    }

    /// `(from, to)` pairs where `from` links to `to` but `to` does not link
    /// back, sorted by `from` then `to`.
    pub fn missing_backlinks(&self) -> Result<Vec<(PathBuf, PathBuf)>, Error> {
        let g = self.graph()?;
        let mut v = Vec::new();
        for (a, outs) in &g.out {
            for b in outs {
                if !g.out.get(b).is_some_and(|back| back.contains(a)) {
                    v.push((self.abs(a), self.abs(b)));
                }
            }
        }
        v.sort();
        Ok(v)
    }

    /// Notes two hops from `path` in either direction that `path` does not
    /// link to and that do not link to `path` (as `zk list --related`), with
    /// the number of shared neighbours, highest first, then by path. Empty
    /// for a path inside the root that is not indexed.
    pub fn related(&self, path: &Path) -> Result<Vec<(NoteSummary, usize)>, Error> {
        let rel = self.rel(path)?;
        let g = self.graph()?;
        let direct = g.neighbours(&rel);
        let mut score: BTreeMap<&str, usize> = BTreeMap::new();
        for n in &direct {
            for m in g.neighbours(n) {
                if m != rel && !direct.contains(m) {
                    *score.entry(m).or_default() += 1;
                }
            }
        }
        let by_path: BTreeMap<PathBuf, usize> =
            score.iter().map(|(r, n)| (self.abs(r), *n)).collect();
        let mut v: Vec<(NoteSummary, usize)> = self
            .summaries(score.keys().copied())
            .into_iter()
            .map(|s| {
                let n = by_path.get(&s.path).copied().unwrap_or(0);
                (s, n)
            })
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.path.cmp(&b.0.path)));
        Ok(v)
    }

    /// The notes `path` links to (deduplicated, no self-link), sorted by
    /// path: `zk list --linked-by`.
    pub fn links_from(&self, path: &Path) -> Result<Vec<NoteSummary>, Error> {
        let rel = self.rel(path)?;
        let g = self.graph()?;
        Ok(self.summaries(g.out.get(&rel).into_iter().flatten().map(String::as_str)))
    }

    /// The notes linking to `path` (deduplicated, no self-link), sorted by
    /// path: `zk list --link-to`.
    pub fn links_to(&self, path: &Path) -> Result<Vec<NoteSummary>, Error> {
        let rel = self.rel(path)?;
        let g = self.graph()?;
        Ok(self.summaries(g.inc.get(&rel).into_iter().flatten().map(String::as_str)))
    }
}
