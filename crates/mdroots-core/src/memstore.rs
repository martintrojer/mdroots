//! An in-memory index of one root: every note parsed, its keys indexed, and
//! link queries answered through the resolve ladder. The store `default-features
//! = false` embedders and tests use (docs/specs/library.md §2).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mdroots_resolve::dialect::{RootConventions, detect};
use mdroots_resolve::env::ResolveEnv;
use mdroots_resolve::keys::{KeyKind, KeyLookup, doc_keys};
use mdroots_resolve::ladder::{LinkStatus, Resolution, ResolveCtx, resolve};
use mdroots_resolve::normalize::percent_decode;
use mdroots_syntax::{
    Anchor, Confidence, Context, Dialect, Document, Link, LinkKind, ParseOptions, parse_bytes,
    parse_with, slug,
};

use crate::cancel::Cancel;
use crate::error::{Error, ErrorKind};
use crate::fs::FileSystem;

/// Config files larger than this are ignored by dialect detection.
const MAX_CONFIG: usize = 256 * 1024;

/// One parse of a note and the keys it is found by.
struct Parsed {
    doc: Document,
    keys: Vec<(KeyKind, String)>,
}

/// A note's disk parse and editor overlay; the overlay wins.
struct Entry {
    disk: Option<Parsed>,
    overlay: Option<Parsed>,
}

impl Entry {
    fn current(&self) -> Option<&Parsed> {
        self.overlay.as_ref().or(self.disk.as_ref())
    }
}

/// [`ResolveEnv`] over a [`FileSystem`] and a root.
struct FsEnv {
    fs: Arc<dyn FileSystem>,
    root: PathBuf,
    case_sensitive: bool,
    home: Option<PathBuf>,
}

impl ResolveEnv for FsEnv {
    fn exists(&self, root_rel: &str) -> bool {
        self.fs.stat(&self.root.join(root_rel)).is_ok()
    }

    fn is_file(&self, root_rel: &str) -> bool {
        self.fs
            .stat(&self.root.join(root_rel))
            .is_ok_and(|m| m.is_file)
    }

    fn case_sensitive(&self) -> bool {
        self.case_sensitive
    }

    fn home_dir(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    fn read_config(&self, root_rel: &str) -> Option<String> {
        let p = self.root.join(root_rel);
        if self.fs.stat(&p).ok()?.size > MAX_CONFIG as u64 {
            return None;
        }
        let (bytes, _) = self.fs.read(&p).ok()?;
        (bytes.len() <= MAX_CONFIG)
            .then(|| String::from_utf8(bytes.to_vec()).ok())
            .flatten()
    }
}

/// Whether a link's anchor exists in its target. Reported separately from
/// the link status, which depends only on the file.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorStatus {
    /// Not checkable: org search anchors, or the target is not an indexed note.
    NotChecked,
    Found,
    Missing,
}

pub struct MemStore {
    env: FsEnv,
    conventions: RootConventions,
    entries: BTreeMap<String, Entry>,
    index: BTreeMap<(KeyKind, String), BTreeSet<String>>,
    skipped: Vec<(String, Error)>,
}

impl MemStore {
    /// Detect the root's conventions, walk it and parse every note: the
    /// root is canonicalized, walked with [`walk_md`](crate::walk::walk_md)
    /// and indexed by [`open_files`](Self::open_files). A file that cannot be
    /// read or is binary is listed in [`skipped`](Self::skipped) and never
    /// fails the open; cancellation and an unreadable root do.
    pub fn open(
        fs: Arc<dyn FileSystem>,
        root: PathBuf,
        cancel: &Cancel,
    ) -> Result<MemStore, Error> {
        cancel.check()?;
        let root = fs.canonicalize(&root).unwrap_or(root);
        let files = crate::walk::walk_md(&*fs, &root, cancel)?;
        Self::open_files(fs, root, files, &[], cancel)
    }

    /// Like [`open`](Self::open), but index exactly `files` (root-relative,
    /// `/`-separated) without listing any directory. Exact duplicates are
    /// dropped; there is no extension filter. Each file is statted first.
    /// These go to [`skipped`](Self::skipped) instead of the index: invalid
    /// paths (empty, absolute, or with an empty, `.` or `..` component;
    /// `Unsupported`), failed stats and reads (`Io`), binary files
    /// (`Unsupported`), and dataless files (`Unsupported`), which are not read
    /// because reading would download them, unless listed in `force_read`.
    pub fn open_files(
        fs: Arc<dyn FileSystem>,
        root: PathBuf,
        mut files: Vec<String>,
        force_read: &[String],
        cancel: &Cancel,
    ) -> Result<MemStore, Error> {
        cancel.check()?;
        let root = fs.canonicalize(&root).unwrap_or(root);
        let env = FsEnv {
            case_sensitive: fs.case_sensitive(&root),
            home: std::env::var_os("HOME").map(PathBuf::from),
            fs,
            root,
        };
        let conventions = detect(&env);
        let mut store = MemStore {
            env,
            conventions,
            entries: BTreeMap::new(),
            index: BTreeMap::new(),
            skipped: Vec::new(),
        };
        files.sort();
        files.dedup();
        for rel in files {
            cancel.check()?;
            let force = force_read.contains(&rel);
            match store.read_listed(&rel, force) {
                Ok(p) => store.replace(&rel, |e| e.disk = Some(p)),
                Err(e) => store.skipped.push((rel, e)),
            }
        }
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.env.root
    }

    pub fn conventions(&self) -> &RootConventions {
        &self.conventions
    }

    /// Indexed notes (disk and overlay-only), sorted.
    pub fn files(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    /// Files that were not indexed, sorted by path: read errors (`Io`) and
    /// binary files (`Unsupported`).
    pub fn skipped(&self) -> &[(String, Error)] {
        &self.skipped
    }

    /// The current parse of a note (overlay wins).
    pub fn document(&self, root_rel: &str) -> Option<&Document> {
        self.entries
            .get(root_rel)
            .and_then(Entry::current)
            .map(|p| &p.doc)
    }

    /// Replace a note's content with unsaved editor text; adds the note if
    /// it is not indexed.
    pub fn set_overlay(&mut self, root_rel: &str, text: &str) {
        let doc = parse_with(text, &self.options(root_rel));
        let parsed = self.parsed(root_rel, doc);
        self.replace(root_rel, |e| e.overlay = Some(parsed));
    }

    /// Drop the overlay: back to the disk parse, or gone if there is none.
    pub fn clear_overlay(&mut self, root_rel: &str) {
        self.replace(root_rel, |e| e.overlay = None);
    }

    pub fn resolve_link(
        &self,
        from_root_rel: &str,
        link: &Link,
        allow_partial: bool,
    ) -> Resolution {
        let mut ctx = ResolveCtx::new(&self.env, self);
        ctx.allow_partial = allow_partial;
        ctx.root_abs = Some(&self.env.root);
        ctx.docs_dir = self.conventions.docs_dir.as_deref();
        resolve(from_root_rel, link, &ctx)
    }

    /// Every link in the note, resolved without the Partial step.
    pub fn links(&self, root_rel: &str) -> Vec<(Link, Resolution)> {
        let Some(doc) = self.document(root_rel) else {
            return Vec::new();
        };
        doc.links()
            .map(|l| (l.clone(), self.resolve_link(root_rel, l, false)))
            .collect()
    }

    /// Links from any note whose targets include `root_rel` (Resolved or
    /// Ambiguous), in referencing contexts only; sorted by source path.
    pub fn backlinks(&self, root_rel: &str) -> Vec<(String, Link)> {
        let mut out = Vec::new();
        for (from, e) in &self.entries {
            let Some(p) = e.current() else { continue };
            for l in p.doc.links().filter(|l| counts(l)) {
                let r = self.resolve_link(from, l, false);
                if matches!(r.status, LinkStatus::Resolved | LinkStatus::Ambiguous)
                    && r.targets.iter().any(|t| t == root_rel)
                {
                    out.push((from.clone(), l.clone()));
                }
            }
        }
        out
    }

    /// Explicit links in referencing contexts whose status is Broken.
    pub fn broken(&self, root_rel: &str) -> Vec<Link> {
        let Some(doc) = self.document(root_rel) else {
            return Vec::new();
        };
        doc.links()
            .filter(|l| l.confidence == Confidence::Explicit && counts(l))
            .filter(|l| self.resolve_link(root_rel, l, false).status == LinkStatus::Broken)
            .cloned()
            .collect()
    }

    /// Whether `anchor` exists in the indexed note `target_root_rel`.
    /// Heading: a heading slug, org `:ID:` or `:CUSTOM_ID:` equal to the
    /// anchor (as written or percent-decoded), or a slug equal to the
    /// anchor's GitHub slug (`#Intro` finds "# Intro"). CustomId: `:CUSTOM_ID:`
    /// or `:ID:`. Block: an Obsidian `^id` token ending a line.
    pub fn check_anchor(&self, target_root_rel: &str, anchor: &Anchor) -> AnchorStatus {
        let Some(doc) = self.document(target_root_rel) else {
            return AnchorStatus::NotChecked;
        };
        let found = match anchor {
            Anchor::Heading(a) => {
                let forms = anchor_forms(a);
                doc.headings().any(|h| {
                    forms.iter().any(|s| {
                        h.slug == *s
                            || slug::github(s) == h.slug
                            || h.id.as_deref() == Some(s)
                            || h.custom_id.as_deref() == Some(s)
                    })
                })
            }
            Anchor::CustomId(a) => {
                let forms = anchor_forms(a);
                doc.headings().any(|h| {
                    forms
                        .iter()
                        .any(|s| h.custom_id.as_deref() == Some(s) || h.id.as_deref() == Some(s))
                })
            }
            Anchor::Block(b) => has_block_id(doc.source(), b),
            _ => return AnchorStatus::NotChecked,
        };
        match found {
            true => AnchorStatus::Found,
            false => AnchorStatus::Missing,
        }
    }

    /// Links (same context filter as [`broken`](Self::broken)) that resolve
    /// to one note in which their anchor is missing.
    pub fn broken_anchors(&self, root_rel: &str) -> Vec<Link> {
        let Some(doc) = self.document(root_rel) else {
            return Vec::new();
        };
        doc.links()
            .filter(|l| counts(l))
            .filter(|l| {
                let Some(anchor) = &l.target.anchor else {
                    return false;
                };
                let r = self.resolve_link(root_rel, l, false);
                r.status == LinkStatus::Resolved
                    && r.targets.len() == 1
                    && self.check_anchor(&r.targets[0], anchor) == AnchorStatus::Missing
            })
            .cloned()
            .collect()
    }

    fn options(&self, root_rel: &str) -> ParseOptions {
        let c = &self.conventions;
        let mut o = ParseOptions::new(Dialect::detect_from_path(Path::new(root_rel)));
        o.hashtags = c.hashtags.unwrap_or(o.hashtags);
        o.colon_tags = c.colon_tags.unwrap_or(o.colon_tags);
        o.multiword_tags = c.multiword_tags.unwrap_or(o.multiword_tags);
        o
    }

    fn parsed(&self, root_rel: &str, doc: Document) -> Parsed {
        let keys = doc_keys(root_rel, &doc, self.env.case_sensitive);
        Parsed { doc, keys }
    }

    /// Validate, stat and (unless dataless and not forced) read a listed file.
    fn read_listed(&self, rel: &str, force: bool) -> Result<Parsed, Error> {
        if !valid_rel(rel) {
            return Err(Error::new(
                ErrorKind::Unsupported,
                format!("{rel}: invalid path"),
            ));
        }
        let meta = self
            .env
            .fs
            .stat(&self.env.root.join(rel))
            .map_err(|e| Error::new(ErrorKind::Io, format!("{rel}: {e}")))?;
        if meta.dataless && !force {
            return Err(Error::new(
                ErrorKind::Unsupported,
                format!("{rel}: dataless: not downloaded"),
            ));
        }
        self.read_disk(rel)
    }

    fn read_disk(&self, rel: &str) -> Result<Parsed, Error> {
        let (bytes, _) = self
            .env
            .fs
            .read(&self.env.root.join(rel))
            .map_err(|e| Error::new(ErrorKind::Io, format!("{rel}: {e}")))?;
        let doc = parse_bytes(&bytes, &self.options(rel))
            .ok_or_else(|| Error::new(ErrorKind::Unsupported, format!("{rel}: binary file")))?;
        Ok(self.parsed(rel, doc))
    }

    /// Apply `f` to the note's entry and re-index its keys.
    fn replace(&mut self, rel: &str, f: impl FnOnce(&mut Entry)) {
        let entry = self.entries.entry(rel.to_owned()).or_insert(Entry {
            disk: None,
            overlay: None,
        });
        let old: Vec<(KeyKind, String)> =
            entry.current().map(|p| p.keys.clone()).unwrap_or_default();
        f(entry);
        let new: Vec<(KeyKind, String)> =
            entry.current().map(|p| p.keys.clone()).unwrap_or_default();
        if entry.current().is_none() {
            self.entries.remove(rel);
        }
        for k in old {
            if let Some(set) = self.index.get_mut(&k) {
                set.remove(rel);
                if set.is_empty() {
                    self.index.remove(&k);
                }
            }
        }
        for k in new {
            self.index.entry(k).or_default().insert(rel.to_owned());
        }
    }
}

/// The contexts that reference a note (backlinks, broken links); footnotes
/// never do.
fn counts(l: &Link) -> bool {
    l.kind != LinkKind::Footnote
        && matches!(
            l.context,
            Context::Prose | Context::Heading | Context::Html | Context::Frontmatter
        )
}

/// The anchor as written, and percent-decoded when that differs.
fn anchor_forms(a: &str) -> Vec<String> {
    let dec = percent_decode(a);
    match dec == a {
        true => vec![a.to_owned()],
        false => vec![a.to_owned(), dec],
    }
}

/// `^id` as the last whitespace-separated token of some line.
fn has_block_id(src: &str, id: &str) -> bool {
    let token = format!("^{id}");
    src.lines().any(|line| {
        line.trim_end()
            .rsplit(char::is_whitespace)
            .next()
            .is_some_and(|t| t == token)
    })
}

impl KeyLookup for MemStore {
    fn lookup(&self, kind: KeyKind, key: &str) -> Vec<String> {
        self.index
            .get(&(kind, key.to_owned()))
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// zk-style: notes whose stem contains `needle`; failing that, notes
    /// whose path contains it.
    fn partial(&self, needle: &str) -> Vec<String> {
        let having = |kind: KeyKind| -> Vec<String> {
            self.entries
                .iter()
                .filter(|(_, e)| {
                    e.current().is_some_and(|p| {
                        p.keys.iter().any(|(k, v)| *k == kind && v.contains(needle))
                    })
                })
                .map(|(rel, _)| rel.clone())
                .collect()
        };
        let by_name = having(KeyKind::Stem);
        match by_name.is_empty() {
            true => having(KeyKind::Path),
            false => by_name,
        }
    }
}

/// Root-relative, `/`-separated, with no empty, `.` or `..` component.
fn valid_rel(rel: &str) -> bool {
    rel.split('/')
        .all(|c| !c.is_empty() && c != "." && c != "..")
}
