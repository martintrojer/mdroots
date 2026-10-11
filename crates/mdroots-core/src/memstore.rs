//! An in-memory index of one root: every note parsed, its keys indexed, and
//! link queries answered through the resolve ladder. The store `default-features
//! = false` embedders and tests use (docs/specs/library.md §2).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use mdroots_resolve::dialect::{RootConventions, Vote, VoteResult, detect};
use mdroots_resolve::env::ResolveEnv;
use mdroots_resolve::keys::{KeyKind, KeyLookup, doc_keys};
use mdroots_resolve::ladder::{LinkStatus, Resolution, ResolveCtx, resolve};
use mdroots_resolve::normalize::percent_decode;
use mdroots_syntax::{
    Anchor, Context, Dialect, Document, Link, LinkKind, ParseOptions, parse_bytes, parse_with, slug,
};

use crate::cancel::Cancel;
use crate::diagnostics::DiagnosticPolicy;
use crate::error::{Error, ErrorKind};
use crate::fs::FileSystem;

/// Config files larger than this are ignored by dialect detection.
const MAX_CONFIG: usize = 256 * 1024;

/// A note's disk parse and editor overlay; the overlay wins. Its keys are
/// not stored: [`doc_keys`] recomputes them from the current parse when the
/// note changes.
#[derive(Default)]
struct Entry {
    disk: Option<Document>,
    overlay: Option<Document>,
}

impl Entry {
    fn current(&self) -> Option<&Document> {
        self.overlay.as_ref().or(self.disk.as_ref())
    }
}

/// A note's index in [`MemStore::slots`]: stable while the note is indexed,
/// reused after it is dropped, so ids say nothing about path order.
type NoteId = u32;

/// An indexed note: its path (shared with [`MemStore::paths`]) and entry.
struct Slot {
    path: Arc<str>,
    entry: Box<Entry>,
}

/// Key -> the notes found by it, unsorted and without duplicates.
type Postings = HashMap<Box<str>, Vec<NoteId>>;

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
        let meta = self.fs.stat(&p).ok()?;
        // A cloud placeholder: reading it would start a download.
        if meta.dataless || meta.size > MAX_CONFIG as u64 {
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

/// A link that reaches a note: (source note, index of the link in the
/// source's [`Document::links`]).
type Backlink = (NoteId, usize);

/// Target note -> the links that reach it, sorted by source path, then
/// link order. Ids are those of the store the index was built from; every
/// content change drops it.
struct Backlinks {
    /// Indexed by the target's [`NoteId`]. Every target of a counted
    /// Resolved or Ambiguous link is an indexed path: code mentions, the one
    /// resolution that keeps a written spelling, are inline code and never
    /// count (`counts`).
    by_target: Vec<Vec<Backlink>>,
}

/// Whole-root results computed on first use and dropped on every content
/// change (`replace`, which overlays and disk entries go through), so they
/// always match the current notes. Link targets that are files on disk but
/// not notes (`Unindexed`) are not tracked: such a file appearing or
/// disappearing shows only after the next content change, or in a new store
/// (a refresh builds one).
#[derive(Default)]
struct Caches {
    /// [`DiagnosticPolicy::for_store`], indexed by the `lazy` flag.
    policy: [OnceLock<DiagnosticPolicy>; 2],
    backlinks: OnceLock<Backlinks>,
    vote: OnceLock<VoteResult>,
}

pub struct MemStore {
    env: FsEnv,
    conventions: RootConventions,
    /// Every indexed note's path -> its slot, sorted by path: the order of
    /// every path-sorted answer (files, lookups, backlinks, the vote).
    paths: BTreeMap<Arc<str>, NoteId>,
    /// Indexed by [`NoteId`]; `None` slots are listed in `free_ids`.
    slots: Vec<Option<Slot>>,
    free_ids: Vec<NoteId>,
    /// Each kind's keys and the notes they find.
    index: HashMap<KeyKind, Postings>,
    skipped: Vec<(String, Error)>,
    caches: Caches,
    /// Extra code-mention dirs, root-relative (see
    /// [`set_code_dirs`](Self::set_code_dirs)).
    code_dirs: Vec<String>,
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
        let mut store = Self::empty(fs, root);
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

    /// Like [`open_files`](Self::open_files), but the files' bytes are given
    /// (for example from a cache), so no note is read; the filesystem is
    /// still used for convention detection (tool configs) and link targets.
    /// Exact duplicate paths keep the first. Invalid paths and binary files
    /// go to [`skipped`](Self::skipped) as in `open_files`.
    pub fn from_contents(
        fs: Arc<dyn FileSystem>,
        root: PathBuf,
        mut contents: Vec<(String, Arc<[u8]>)>,
        cancel: &Cancel,
    ) -> Result<MemStore, Error> {
        cancel.check()?;
        let mut store = Self::empty(fs, root);
        contents.sort_by(|a, b| a.0.cmp(&b.0));
        contents.dedup_by(|a, b| a.0 == b.0);
        for (rel, bytes) in contents {
            cancel.check()?;
            let parsed = match valid_rel(&rel) {
                true => store.parse_disk(&rel, &bytes),
                false => Err(invalid_path(&rel)),
            };
            match parsed {
                Ok(p) => store.replace(&rel, |e| e.disk = Some(p)),
                Err(e) => store.skipped.push((rel, e)),
            }
        }
        Ok(store)
    }

    /// Update notes in place from new disk bytes: `(path, Some(bytes))`
    /// replaces (or adds) a note's disk content, `(path, None)` drops it.
    /// Overlays are kept and still win; other notes are untouched, and the
    /// whole-root caches are dropped as on every content change. Invalid
    /// paths and binary files go to [`skipped`](Self::skipped) (dropping
    /// any disk content they had); a path's earlier `skipped` entry is
    /// cleared. Returns the paths whose disk content changed, sorted (bytes
    /// equal to the current disk text are not a change).
    pub fn apply_contents(&mut self, mut changes: Vec<(String, Option<Arc<[u8]>>)>) -> Vec<String> {
        changes.sort_by(|a, b| a.0.cmp(&b.0));
        changes.dedup_by(|a, b| a.0 == b.0);
        let mut changed = Vec::new();
        for (rel, bytes) in changes {
            self.skipped.retain(|(p, _)| *p != rel);
            let disk = self.entry(&rel).and_then(|e| e.disk.as_ref());
            let had_disk = disk.is_some();
            let parsed = match bytes {
                None if !had_disk => continue,
                None => None,
                Some(b) => {
                    let same = disk.is_some_and(|d| {
                        !d.is_lossy() && std::str::from_utf8(&b) == Ok(d.source())
                    });
                    if same {
                        continue;
                    }
                    let p = match valid_rel(&rel) {
                        true => self.parse_disk(&rel, &b),
                        false => Err(invalid_path(&rel)),
                    };
                    match p {
                        Ok(p) => Some(p),
                        Err(e) => {
                            self.skipped.push((rel.clone(), e));
                            self.skipped.sort_by(|a, b| a.0.cmp(&b.0));
                            if !had_disk {
                                continue;
                            }
                            None
                        }
                    }
                }
            };
            self.replace(&rel, |e| e.disk = parsed);
            changed.push(rel);
        }
        changed
    }

    /// A store with no notes: the root canonicalized and its conventions
    /// detected.
    fn empty(fs: Arc<dyn FileSystem>, root: PathBuf) -> MemStore {
        let root = fs.canonicalize(&root).unwrap_or(root);
        let env = FsEnv {
            case_sensitive: fs.case_sensitive(&root),
            home: std::env::var_os("HOME").map(PathBuf::from),
            fs,
            root,
        };
        let conventions = detect(&env);
        MemStore {
            env,
            conventions,
            paths: BTreeMap::new(),
            slots: Vec::new(),
            free_ids: Vec::new(),
            index: HashMap::new(),
            skipped: Vec::new(),
            caches: Caches::default(),
            code_dirs: Vec::new(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.env.root
    }

    pub fn conventions(&self) -> &RootConventions {
        &self.conventions
    }

    /// Indexed notes (disk and overlay-only), sorted.
    pub fn files(&self) -> impl Iterator<Item = &str> {
        self.paths.keys().map(|p| &**p)
    }

    fn entry(&self, root_rel: &str) -> Option<&Entry> {
        self.paths.get(root_rel).map(|&id| &*self.slot(id).entry)
    }

    fn slot(&self, id: NoteId) -> &Slot {
        self.slots[id as usize]
            .as_ref()
            .expect("an indexed note has a slot")
    }

    /// Indexed notes with their entries, sorted by path.
    fn entries(&self) -> impl Iterator<Item = (&str, &Entry)> {
        self.paths
            .iter()
            .map(|(p, &id)| (&**p, &*self.slot(id).entry))
    }

    /// Files that were not indexed, sorted by path: read errors (`Io`) and
    /// binary files (`Unsupported`).
    pub fn skipped(&self) -> &[(String, Error)] {
        &self.skipped
    }

    /// The current parse of a note (overlay wins).
    pub fn document(&self, root_rel: &str) -> Option<&Document> {
        self.entry(root_rel).and_then(Entry::current)
    }

    /// Replace a note's content with unsaved editor text; adds the note if
    /// it is not indexed. Like every content change, drops the cached
    /// [`policy`](Self::policy) and backlink index.
    pub fn set_overlay(&mut self, root_rel: &str, text: &str) {
        let doc = parse_with(text, &self.options(root_rel));
        self.replace(root_rel, |e| e.overlay = Some(doc));
    }

    /// Drop the overlay: back to the disk parse, or gone if there is none.
    pub fn clear_overlay(&mut self, root_rel: &str) {
        self.replace(root_rel, |e| e.overlay = None);
    }

    /// Extra dirs code mentions ([`LinkKind::CodeMention`])
    /// resolve against: root-relative, `/`-separated, and may start with `..`
    /// (see [`outside_rel`](mdroots_resolve::ladder::outside_rel) to compute
    /// one from an absolute dir). A relative mention tries the linking
    /// note's dir, then these in order, then the root. Drops the cached
    /// [`policy`](Self::policy) and backlink index, like a content change.
    pub fn set_code_dirs(&mut self, dirs: Vec<String>) {
        self.caches = Caches::default();
        self.code_dirs = dirs;
    }

    /// The dirs set by [`set_code_dirs`](Self::set_code_dirs).
    pub fn code_dirs(&self) -> &[String] {
        &self.code_dirs
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
        ctx.code_dirs = &self.code_dirs;
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

    /// The root's [`DiagnosticPolicy`]: [`DiagnosticPolicy::for_store`],
    /// computed once per `lazy` flag and kept until the next content change
    /// (see [`set_overlay`](Self::set_overlay)).
    pub fn policy(&self, lazy: bool) -> DiagnosticPolicy {
        self.caches.policy[usize::from(lazy)]
            .get_or_init(|| DiagnosticPolicy::for_store(self, lazy))
            .clone()
    }

    /// Links from any note whose targets include `root_rel` (Resolved or
    /// Ambiguous), in referencing contexts only; sorted by source path,
    /// then link order. Answered from a reverse index of every note's
    /// links, built on first use and kept until the next content change.
    pub fn backlinks(&self, root_rel: &str) -> Vec<(String, Link)> {
        let index = self.caches.backlinks.get_or_init(|| self.build_backlinks());
        let refs = self
            .paths
            .get(root_rel)
            .and_then(|&id| index.by_target.get(id as usize));
        refs.into_iter()
            .flatten()
            .filter_map(|&(from, i)| {
                let slot = self.slot(from);
                let l = slot.entry.current()?.links().nth(i)?;
                Some((slot.path.to_string(), l.clone()))
            })
            .collect()
    }

    /// The root's convention [`Vote`] over the current notes (overlays
    /// win; a lazy working set votes over what it holds): every note's
    /// headings and tags, and its links in referencing contexts that are not
    /// External (the [`DiagnosticPolicy::for_store`] filter), with the
    /// ladder step that resolved them. Computed on first use and kept until
    /// the next content change.
    pub fn vote(&self) -> VoteResult {
        self.caches
            .vote
            .get_or_init(|| {
                let mut vote = Vote::new();
                for (rel, e) in self.entries() {
                    let Some(doc) = e.current() else { continue };
                    vote.record_doc(doc);
                    for l in doc.links().filter(|l| counts(l)) {
                        let r = self.resolve_link(rel, l, false);
                        if r.status != LinkStatus::External {
                            vote.record_link(l, r.step);
                        }
                    }
                }
                vote.result()
            })
            .clone()
    }

    /// Iterates sources in path order, so every bucket is sorted by source
    /// path, then link order.
    fn build_backlinks(&self) -> Backlinks {
        let mut out = Backlinks {
            by_target: vec![Vec::new(); self.slots.len()],
        };
        for (from, &id) in &self.paths {
            let Some(doc) = self.slot(id).entry.current() else {
                continue;
            };
            for (i, l) in doc.links().enumerate().filter(|(_, l)| counts(l)) {
                let r = self.resolve_link(from, l, false);
                if !matches!(r.status, LinkStatus::Resolved | LinkStatus::Ambiguous) {
                    continue;
                }
                let mut targets = r.targets;
                targets.sort();
                targets.dedup();
                for t in targets {
                    let to = self.paths.get(t.as_str());
                    debug_assert!(to.is_some(), "backlink target {t} is not indexed");
                    if let Some(&to) = to {
                        out.by_target[to as usize].push((id, i));
                    }
                }
            }
        }
        out.by_target.iter_mut().for_each(Vec::shrink_to_fit);
        out
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
            Anchor::Heading(_) | Anchor::CustomId(_) => heading_index(doc, anchor).is_some(),
            Anchor::Block(b) => has_block_id(doc.source(), b),
            _ => return AnchorStatus::NotChecked,
        };
        match found {
            true => AnchorStatus::Found,
            false => AnchorStatus::Missing,
        }
    }

    fn options(&self, root_rel: &str) -> ParseOptions {
        let c = &self.conventions;
        let mut o = ParseOptions::new(Dialect::detect_from_path(Path::new(root_rel)));
        o.hashtags = c.hashtags.unwrap_or(o.hashtags);
        o.colon_tags = c.colon_tags.unwrap_or(o.colon_tags);
        o.multiword_tags = c.multiword_tags.unwrap_or(o.multiword_tags);
        o
    }

    /// Validate, stat and (unless dataless and not forced) read a listed file.
    fn read_listed(&self, rel: &str, force: bool) -> Result<Document, Error> {
        if !valid_rel(rel) {
            return Err(invalid_path(rel));
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

    fn read_disk(&self, rel: &str) -> Result<Document, Error> {
        let (bytes, _) = self
            .env
            .fs
            .read(&self.env.root.join(rel))
            .map_err(|e| Error::new(ErrorKind::Io, format!("{rel}: {e}")))?;
        self.parse_disk(rel, &bytes)
    }

    /// Parse a note's disk bytes; binary files are `Unsupported`.
    fn parse_disk(&self, rel: &str, bytes: &[u8]) -> Result<Document, Error> {
        parse_bytes(bytes, &self.options(rel))
            .ok_or_else(|| Error::new(ErrorKind::Unsupported, format!("{rel}: binary file")))
    }

    /// Apply `f` to the note's entry, re-index its keys and drop the
    /// whole-root caches. A note that ends with neither a disk parse nor
    /// an overlay is dropped and its slot freed for reuse.
    fn replace(&mut self, rel: &str, f: impl FnOnce(&mut Entry)) {
        self.caches = Caches::default();
        let cs = self.env.case_sensitive;
        let found = self.paths.get(rel).copied();
        let mut fresh = Entry::default();
        let entry = match found {
            Some(id) => {
                let slot = self.slots[id as usize].as_mut();
                &mut *slot.expect("an indexed note has a slot").entry
            }
            None => &mut fresh,
        };
        let keys = |e: &Entry| e.current().map(|d| doc_keys(rel, d, cs));
        let old = keys(entry).unwrap_or_default();
        f(entry);
        let new = keys(entry).unwrap_or_default();
        let indexed = entry.current().is_some();
        let id = match (found, indexed) {
            (Some(id), true) => id,
            (Some(id), false) => {
                self.paths.remove(rel);
                self.slots[id as usize] = None;
                self.free_ids.push(id);
                id
            }
            (None, true) => self.add_slot(rel, fresh),
            (None, false) => return,
        };
        for (kind, key) in old {
            let Some(postings) = self.index.get_mut(&kind) else {
                continue;
            };
            if let Some(ids) = postings.get_mut(key.as_str()) {
                ids.retain(|&n| n != id);
                if ids.is_empty() {
                    postings.remove(key.as_str());
                }
            }
        }
        // `doc_keys` lists each key once and the old ones are gone, so the
        // id is new to every bucket it joins.
        for (kind, key) in new {
            let postings = self.index.entry(kind).or_default();
            match postings.get_mut(key.as_str()) {
                Some(ids) => ids.push(id),
                None => {
                    postings.insert(key.into_boxed_str(), vec![id]);
                }
            }
        }
    }

    /// Index a new note in a free slot, or a new one.
    fn add_slot(&mut self, rel: &str, entry: Entry) -> NoteId {
        let path: Arc<str> = Arc::from(rel);
        let slot = Some(Slot {
            path: path.clone(),
            entry: Box::new(entry),
        });
        let id = match self.free_ids.pop() {
            Some(id) => {
                self.slots[id as usize] = slot;
                id
            }
            None => {
                let id = NoteId::try_from(self.slots.len()).expect("at most u32::MAX notes");
                self.slots.push(slot);
                id
            }
        };
        self.paths.insert(path, id);
        id
    }

    /// The paths of `ids`, sorted.
    fn sorted_paths(&self, ids: impl IntoIterator<Item = NoteId>) -> Vec<String> {
        let mut out: Vec<String> = ids
            .into_iter()
            .map(|id| self.slot(id).path.to_string())
            .collect();
        out.sort();
        out.dedup();
        out
    }
}

/// The contexts that reference a note (backlinks, broken links, rename);
/// footnotes never do.
pub fn counts(l: &Link) -> bool {
    l.kind != LinkKind::Footnote
        && matches!(
            l.context,
            Context::Prose | Context::Heading | Context::Html | Context::Frontmatter
        )
}

/// The index (in [`Document::headings`] order) of the first heading
/// `anchor` names in `doc`: a heading anchor by slug, the slug of the
/// anchor, `{#id}`/`:ID:` or `:CUSTOM_ID:`; a custom-id anchor by the last
/// two. The anchor is tried as written and percent-decoded.
pub fn heading_index(doc: &Document, anchor: &Anchor) -> Option<usize> {
    let (a, by_slug) = match anchor {
        Anchor::Heading(a) => (a, true),
        Anchor::CustomId(a) => (a, false),
        _ => return None,
    };
    let dec = percent_decode(a);
    let forms: Vec<&str> = match dec == *a {
        true => vec![a],
        false => vec![a, &dec],
    };
    doc.headings().position(|h| {
        forms.iter().any(|s| {
            (by_slug && (h.slug == *s || slug::github(s) == h.slug))
                || h.id.as_deref() == Some(s)
                || h.custom_id.as_deref() == Some(s)
        })
    })
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
        let ids = self.index.get(&kind).and_then(|p| p.get(key));
        self.sorted_paths(ids.into_iter().flatten().copied())
    }

    /// zk-style: notes whose stem contains `needle`; failing that, notes
    /// whose path contains it.
    fn partial(&self, needle: &str) -> Vec<String> {
        let having = |kind: KeyKind| -> Vec<String> {
            let ids = self.index.get(&kind).into_iter().flat_map(|p| {
                p.iter()
                    .filter(|(k, _)| k.contains(needle))
                    .flat_map(|(_, ids)| ids.iter().copied())
            });
            self.sorted_paths(ids)
        };
        let by_name = having(KeyKind::Stem);
        match by_name.is_empty() {
            true => having(KeyKind::Path),
            false => by_name,
        }
    }
}

fn invalid_path(rel: &str) -> Error {
    Error::new(ErrorKind::Unsupported, format!("{rel}: invalid path"))
}

/// Whether `rel` is a usable root-relative path: `/`-separated, with no
/// empty, `.` or `..` component (so not empty and not absolute).
pub fn valid_rel(rel: &str) -> bool {
    rel.split('/')
        .all(|c| !c.is_empty() && c != "." && c != "..")
}
