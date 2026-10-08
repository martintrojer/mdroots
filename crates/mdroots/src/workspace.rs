//! [`Workspace`]: one root, discovered and indexed in memory.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use mdroots_core::{Cancel, Diagnostic, DiagnosticPolicy, Error, ErrorKind, FileSystem, MemStore};
use mdroots_resolve::ResolveStep;
use mdroots_resolve::ladder::LinkStatus;
use mdroots_roots::discover::{DiscoverOptions, Enumerator, discover};
use mdroots_roots::probe::Probe;
use mdroots_roots::{MemRegistry, RootMode};
use mdroots_syntax::{Context, Dialect, Document, LinkKind, PositionEncoding, parse};

/// Most entries a lazy working set takes from the opened file's directory.
const WORKING_SET_CAP: usize = 2_000;

/// How to open a [`Workspace`]. `fs` and `probe` are set together or not at
/// all (then [`StdFs`](mdroots_core::StdFs) and
/// [`StdProbe`](mdroots_roots::probe::StdProbe)).
#[derive(Clone, Default)]
pub struct Options {
    workspace_folders: Vec<PathBuf>,
    enumerator: Option<Arc<dyn Enumerator>>,
    cancel: Cancel,
    fs: Option<Arc<dyn FileSystem>>,
    probe: Option<Arc<dyn Probe>>,
}

impl Options {
    /// Editor workspace folders, absolute: they bound the marker climb.
    pub fn workspace_folders(mut self, v: Vec<PathBuf>) -> Self {
        self.workspace_folders = v;
        self
    }

    /// Lists the markdown of a virtual checkout (vcs-enumerated mode).
    /// Default [`SlFiles`](mdroots_roots::SlFiles) (runs
    /// [Sapling](https://sapling-scm.com/)); [`NoEnumerator`](crate::NoEnumerator)
    /// turns enumeration off.
    pub fn enumerator(mut self, e: Arc<dyn Enumerator>) -> Self {
        self.enumerator = Some(e);
        self
    }

    /// Checked during discovery and indexing.
    pub fn cancel(mut self, c: Cancel) -> Self {
        self.cancel = c;
        self
    }

    pub fn fs(mut self, fs: Arc<dyn FileSystem>) -> Self {
        self.fs = Some(fs);
        self
    }

    pub fn probe(mut self, p: Arc<dyn Probe>) -> Self {
        self.probe = Some(p);
        self
    }

    fn io(&self) -> Result<Io, Error> {
        match (&self.fs, &self.probe) {
            (Some(fs), Some(p)) => Ok((fs.clone(), p.clone())),
            (None, None) => default_io(),
            _ => Err(Error::new(
                ErrorKind::Unsupported,
                "fs and probe must both be set",
            )),
        }
    }

    fn enumerator_or_default(&self) -> Arc<dyn Enumerator> {
        self.enumerator
            .clone()
            .unwrap_or_else(|| Arc::new(mdroots_roots::SlFiles))
    }
}

/// The filesystem and probe a workspace reads through.
type Io = (Arc<dyn FileSystem>, Arc<dyn Probe>);

#[cfg(unix)]
fn default_io() -> Result<Io, Error> {
    Ok((
        Arc::new(mdroots_core::StdFs),
        Arc::new(mdroots_roots::probe::StdProbe),
    ))
}

#[cfg(not(unix))]
fn default_io() -> Result<Io, Error> {
    Err(Error::new(
        ErrorKind::Unsupported,
        "no default filesystem on this platform: set fs and probe",
    ))
}

/// The root a workspace serves and why it was chosen.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct RootInfo {
    /// Canonical, absolute.
    pub path: PathBuf,
    pub mode: RootMode,
    /// One line, as discovery explains it.
    pub reason: String,
    pub nested_roots: Vec<PathBuf>,
}

/// Whether the index covers the whole root.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Every note of the root is indexed.
    Fresh,
    /// A working set only (lazy and single-file roots).
    Lazy,
}

/// The outcome of [`Workspace::resolve`].
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Resolution {
    /// Absolute, best first; may lie outside the root.
    pub targets: Vec<PathBuf>,
    pub step: Option<ResolveStep>,
    pub status: LinkStatus,
    /// Found by the Partial step only.
    pub hint: bool,
}

/// A link to a note, seen from the note it is in.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Backlink {
    /// The note containing the link, absolute.
    pub from: PathBuf,
    /// Frontmatter title, else the first level-1 heading, else the file stem.
    pub from_title: String,
    /// Byte range of the link in `from`.
    pub range: Range<usize>,
    /// 0-based line of `range.start`.
    pub line: u32,
    pub in_code: bool,
}

#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct NoteSummary {
    pub path: PathBuf,
    pub title: String,
    /// Tag names, deduplicated, in document order.
    pub tags: Vec<String>,
}

/// One link of a document, resolved.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct DocLink {
    pub range: Range<usize>,
    pub text_range: Range<usize>,
    pub kind: LinkKind,
    pub context: Context,
    /// The best target, absolute, when Resolved, Ambiguous or Unindexed.
    pub target: Option<PathBuf>,
    /// The text after the first `#` of the target as written (`^b` for a
    /// block anchor); `None` when there is none or it is empty.
    pub anchor: Option<String>,
    /// Line of a code mention `path:LINE`.
    pub line: Option<u32>,
    pub status: LinkStatus,
}

struct Inner {
    fs: Arc<dyn FileSystem>,
    root: RootInfo,
    freshness: Freshness,
    store: RwLock<MemStore>,
}

/// One root indexed in memory. Cheap to clone; `Send + Sync`.
#[derive(Clone)]
pub struct Workspace {
    inner: Arc<Inner>,
}

const _: () = {
    const fn assert<T: Clone + Send + Sync>() {}
    assert::<Workspace>();
};

impl Workspace {
    /// Discover the root of the existing file `path` and index it: the
    /// whole root, or for lazy and single-file decisions a working set (the
    /// file's directory, one level; or the file alone). The opened file is
    /// always indexed, and read even if dataless.
    pub fn open_for(path: &Path, opts: Options) -> Result<Workspace, Error> {
        let (fs, probe) = opts.io()?;
        let cancel = &opts.cancel;
        cancel.check()?;
        let file = fs.canonicalize(path)?;
        let meta = fs.stat(&file)?;
        if !meta.is_file {
            return Err(Error::new(
                ErrorKind::Io,
                format!("not a file: {}", file.display()),
            ));
        }
        let dopts = DiscoverOptions {
            workspace_folders: opts.workspace_folders.clone(),
            now_ms: now_ms(),
            ..Default::default()
        };
        let enumerator = opts.enumerator_or_default();
        let mut registry = MemRegistry::new();
        let d = discover(&*probe, &mut registry, &*enumerator, &file, &dopts, cancel);
        // Discovery reports cancellation as a lazy decision.
        cancel.check()?;
        let parent = file.parent().map(Path::to_path_buf).unwrap_or_default();
        let name = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let in_root = d.root.as_deref().and_then(|r| {
            file.strip_prefix(r)
                .ok()
                .map(|rel| (r.to_path_buf(), slash(rel)))
        });
        let (root, files, opened, freshness) = match (d.mode, in_root) {
            (RootMode::Lazy, Some((root, rel))) => {
                let files = working_set(&*fs, &root, &parent, &name, &rel);
                (root, files, rel, Freshness::Lazy)
            }
            (RootMode::Lazy | RootMode::SingleFile, _) | (_, None) => {
                (parent, vec![name.clone()], name, Freshness::Lazy)
            }
            (_, Some((root, rel))) => {
                let mut files = d.md.clone();
                files.push(rel.clone());
                (root, files, rel, Freshness::Fresh)
            }
        };
        let store = MemStore::open_files(fs.clone(), root, files, &[opened], cancel)?;
        let info = RootInfo {
            path: store.root().to_path_buf(),
            mode: d.mode,
            reason: d.reason,
            nested_roots: d.nested_roots,
        };
        Ok(Workspace::new(fs, info, freshness, store))
    }

    /// Index the directory `root` as a root, without discovery.
    pub fn open_at(root: &Path, opts: Options) -> Result<Workspace, Error> {
        let (fs, _) = opts.io()?;
        let root = fs.canonicalize(root)?;
        let store = MemStore::open(fs.clone(), root, &opts.cancel)?;
        let path = store.root().to_path_buf();
        let info = RootInfo {
            reason: format!("opened at {}", path.display()),
            path,
            mode: RootMode::Marker,
            nested_roots: Vec::new(),
        };
        Ok(Workspace::new(fs, info, Freshness::Fresh, store))
    }

    fn new(fs: Arc<dyn FileSystem>, root: RootInfo, freshness: Freshness, s: MemStore) -> Self {
        Workspace {
            inner: Arc::new(Inner {
                fs,
                root,
                freshness,
                store: RwLock::new(s),
            }),
        }
    }

    pub fn root(&self) -> RootInfo {
        self.inner.root.clone()
    }

    pub fn freshness(&self) -> Freshness {
        self.inner.freshness
    }

    /// Indexed notes, absolute, sorted.
    pub fn files(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = self.store().files().map(|f| self.abs(f)).collect();
        v.sort();
        v
    }

    pub fn notes(&self) -> Vec<NoteSummary> {
        let store = self.store();
        let mut v: Vec<NoteSummary> = store
            .files()
            .filter_map(|rel| {
                let doc = store.document(rel)?;
                Some(NoteSummary {
                    path: self.abs(rel),
                    title: title(rel, doc),
                    tags: tag_names(doc),
                })
            })
            .collect();
        v.sort_by(|a, b| a.path.cmp(&b.path));
        v
    }

    /// Each tag with the number of notes carrying it, sorted by name.
    pub fn tags(&self) -> Vec<(String, usize)> {
        let store = self.store();
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for rel in store.files() {
            if let Some(doc) = store.document(rel) {
                for t in tag_names(doc) {
                    *counts.entry(t).or_default() += 1;
                }
            }
        }
        counts.into_iter().collect()
    }

    /// Resolve the first link in `link_text` (e.g. `[[note]]`), written in
    /// `from`, with goto semantics (the Partial step is allowed).
    pub fn resolve(&self, from: &Path, link_text: &str) -> Result<Resolution, Error> {
        let rel = self.rel(from)?;
        let doc = parse(link_text, Dialect::detect_from_path(Path::new(&rel)));
        let link = doc
            .links()
            .next()
            .ok_or_else(|| Error::new(ErrorKind::Unsupported, "not a link"))?;
        let r = self.store().resolve_link(&rel, link, true);
        Ok(Resolution {
            targets: r.targets.iter().map(|t| self.abs(t)).collect(),
            step: r.step,
            status: r.status,
            hint: r.hint,
        })
    }

    /// Every link of the note, in source order. Empty for a path inside
    /// the root that is not indexed.
    pub fn document_links(&self, path: &Path) -> Result<Vec<DocLink>, Error> {
        let rel = self.rel(path)?;
        let store = self.store();
        Ok(store
            .links(&rel)
            .into_iter()
            .map(|(l, r)| {
                let target = match r.status {
                    LinkStatus::Resolved | LinkStatus::Ambiguous | LinkStatus::Unindexed => {
                        r.targets.first().map(|t| self.abs(t))
                    }
                    _ => None,
                };
                let anchor = l
                    .target
                    .raw
                    .split_once('#')
                    .map(|(_, a)| a.to_owned())
                    .filter(|a| !a.is_empty());
                DocLink {
                    range: l.range,
                    text_range: l.text_range,
                    kind: l.kind,
                    context: l.context,
                    target,
                    anchor,
                    line: l.target.line,
                    status: r.status,
                }
            })
            .collect())
    }

    /// Links to the note from indexed notes, sorted by source path. Empty
    /// for a path inside the root that is not indexed.
    pub fn backlinks(&self, path: &Path) -> Result<Vec<Backlink>, Error> {
        let rel = self.rel(path)?;
        let store = self.store();
        if store.document(&rel).is_none() {
            return Ok(Vec::new());
        }
        Ok(store
            .backlinks(&rel)
            .into_iter()
            .filter_map(|(from, l)| {
                let doc = store.document(&from)?;
                let (line, _) = doc
                    .line_index()
                    .line_col(l.range.start, PositionEncoding::Utf8);
                Some(Backlink {
                    from: self.abs(&from),
                    from_title: title(&from, doc),
                    range: l.range,
                    line,
                    in_code: matches!(l.context, Context::CodeBlock | Context::InlineCode),
                })
            })
            .collect())
    }

    /// The note's diagnostics under the root's [`DiagnosticPolicy`]
    /// (computed per call). Empty for a path inside the root that is not
    /// indexed.
    pub fn diagnostics(&self, path: &Path, cancel: &Cancel) -> Result<Vec<Diagnostic>, Error> {
        cancel.check()?;
        let rel = self.rel(path)?;
        let store = self.store();
        if store.document(&rel).is_none() {
            return Ok(Vec::new());
        }
        let policy = DiagnosticPolicy::for_store(&store, self.inner.freshness == Freshness::Lazy);
        Ok(policy.diagnostics(&store, &rel))
    }

    /// Unsaved editor text for a note inside the root; adds it to the index
    /// if it is not indexed.
    pub fn set_overlay(&self, path: &Path, text: &str) -> Result<(), Error> {
        let rel = self.rel(path)?;
        self.store_mut().set_overlay(&rel, text);
        Ok(())
    }

    pub fn clear_overlay(&self, path: &Path) -> Result<(), Error> {
        let rel = self.rel(path)?;
        self.store_mut().clear_overlay(&rel);
        Ok(())
    }

    /// 0-based (line, column) of byte `offset` in the note's current text
    /// (the overlay wins). `Unsupported` for a note that is not indexed.
    pub fn line_col(
        &self,
        path: &Path,
        offset: usize,
        enc: PositionEncoding,
    ) -> Result<(u32, u32), Error> {
        let rel = self.rel(path)?;
        let store = self.store();
        let doc = store
            .document(&rel)
            .ok_or_else(|| Error::new(ErrorKind::Unsupported, format!("{rel}: not indexed")))?;
        Ok(doc.line_index().line_col(offset, enc))
    }

    fn store(&self) -> RwLockReadGuard<'_, MemStore> {
        self.inner.store.read().unwrap_or_else(|e| e.into_inner())
    }

    fn store_mut(&self) -> RwLockWriteGuard<'_, MemStore> {
        self.inner.store.write().unwrap_or_else(|e| e.into_inner())
    }

    /// `p` canonicalized and made root-relative (`/`-separated).
    fn rel(&self, p: &Path) -> Result<String, Error> {
        let canon = self.inner.fs.canonicalize(p)?;
        let rel = canon
            .strip_prefix(&self.inner.root.path)
            .map_err(|_| Error::new(ErrorKind::Unsupported, "outside root"))?;
        if rel.as_os_str().is_empty() {
            return Err(Error::new(ErrorKind::Unsupported, "the root is not a note"));
        }
        Ok(slash(rel))
    }

    /// A root-relative path (possibly `../`-relative) as a clean absolute
    /// path.
    fn abs(&self, rel: &str) -> PathBuf {
        clean(&self.inner.root.path.join(rel))
    }
}

/// The lazy working set: the opened file plus the notes of its directory
/// (one level, no hidden, editor-temp or dataless entries, the first
/// [`WORKING_SET_CAP`] by name, opened file included), root-relative. An unreadable directory
/// leaves the opened file alone.
fn working_set(
    fs: &dyn FileSystem,
    root: &Path,
    dir: &Path,
    name: &str,
    opened: &str,
) -> Vec<String> {
    let dir_rel = dir.strip_prefix(root).map(slash).unwrap_or_default();
    let mut out: Vec<String> = fs
        .read_dir(dir)
        .unwrap_or_default()
        .into_iter()
        .filter(|(n, m)| n != name && m.is_file && !m.dataless && is_note(n) && !is_temp(n))
        .take(WORKING_SET_CAP - 1) // room for the opened file
        .map(|(n, _)| match dir_rel.is_empty() {
            true => n,
            false => format!("{dir_rel}/{n}"),
        })
        .collect();
    out.push(opened.to_owned());
    out
}

fn is_temp(name: &str) -> bool {
    name.starts_with('.')
        || name.ends_with('~')
        || (name.len() > 1 && name.starts_with('#') && name.ends_with('#'))
}

fn is_note(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(stem, ext)| {
        !stem.is_empty()
            && ["md", "markdown", "org"]
                .iter()
                .any(|e| ext.eq_ignore_ascii_case(e))
    })
}

fn title(rel: &str, doc: &Document) -> String {
    doc.frontmatter()
        .and_then(|f| f.title())
        .map(str::to_owned)
        .or_else(|| {
            doc.headings()
                .find(|h| h.level == 1)
                .map(|h| h.text.clone())
        })
        .unwrap_or_else(|| {
            Path::new(rel)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
}

fn tag_names(doc: &Document) -> Vec<String> {
    let mut seen = BTreeSet::new();
    doc.tags()
        .filter(|t| seen.insert(t.name.clone()))
        .map(|t| t.name.clone())
        .collect()
}

/// `p`'s components joined with `/`.
fn slash(p: &Path) -> String {
    p.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// `p` with `.` and `..` removed lexically.
fn clean(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}
