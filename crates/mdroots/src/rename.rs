//! [`Workspace::rename_note`]: the text edits that keep links working when
//! a note moves. Never writes files.

use std::collections::{HashMap, HashSet};
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mdroots_core::memstore::counts;
use mdroots_core::{Cancel, Error, ErrorKind, FileSystem, FsKind, MemStore, Meta};
use mdroots_resolve::ladder::LinkStatus;
use mdroots_resolve::normalize::percent_decode;
use mdroots_resolve::{KeyKind, KeyLookup, ResolveStep, doc_keys};
use mdroots_syntax::{Dialect, Document, Link, LinkKind, parse};

use crate::links::{encode_destination, relative_path};
use crate::workspace::{Workspace, is_note};

/// File changes for an editor to apply: create, then edit, then rename.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Default)]
pub struct WorkspaceEdit {
    /// Per file (absolute, sorted), edits sorted by range, not overlapping.
    pub edits: Vec<(PathBuf, Vec<TextEdit>)>,
    /// The file rename to perform after the edits: `(old, new)`.
    pub rename: Option<(PathBuf, PathBuf)>,
    /// Files to create (absolute, not existing) with their content, before
    /// the edits.
    pub create: Vec<(PathBuf, String)>,
}

/// Replace the bytes `range` of the file's current text with `new_text`.
#[derive(Debug, Clone, PartialEq)]
pub struct TextEdit {
    pub range: Range<usize>,
    pub new_text: String,
}

impl Workspace {
    /// The edits that rename the indexed note `old` to `new` (inside the
    /// root, a note extension, not on disk and not indexed). Links whose
    /// best target is `old` are rewritten in the style they were written
    /// in (file-relative, root-relative, site-rooted or by stem); links
    /// found by id, title, alias, a dialect transform or a partial match
    /// are left alone, as are links in code. When `old` moves to another
    /// directory its own file-relative links are rewritten too. Anchors are
    /// kept. Checks `cancel` per note.
    pub fn rename_note(
        &self,
        old: &Path,
        new: &Path,
        cancel: &Cancel,
    ) -> Result<WorkspaceEdit, Error> {
        cancel.check()?;
        let old_rel = self.rel(old)?;
        let new_rel = self.rel(new)?;
        let store = self.store();
        if store.document(&old_rel).is_none() {
            return Err(refuse(format!("{old_rel}: not an indexed note")));
        }
        let new_name = new_rel.rsplit('/').next().unwrap_or_default();
        if !is_note(new_name) {
            return Err(refuse(format!("{new_rel}: not a note extension")));
        }
        let new_abs = self.abs(&new_rel);
        if store.document(&new_rel).is_some() || self.fs().stat(&new_abs).is_ok() {
            return Err(refuse(format!("{new_rel}: already exists")));
        }
        let mut out = self.plan(&store, &[(old.to_owned(), new.to_owned())], cancel)?;
        out.rename = Some((self.abs(&old_rel), new_abs));
        Ok(out)
    }

    /// The link edits for several moves planned together, so notes that
    /// move together keep correct links. Each pair is a note or a folder:
    /// a note moves to `new`; a folder maps every indexed note under it to
    /// the same sub-path under `new`. A pair whose old path is neither an
    /// indexed note nor a folder holding one, or whose new path is outside
    /// the root or not a note extension (for a note), is skipped. A note
    /// moved by two pairs is [`ErrorKind::Unsupported`]. Links are
    /// rewritten as for [`rename_note`](Self::rename_note), against the
    /// layout after all the moves. Edits only: `rename` is `None`, `create`
    /// is empty. Checks `cancel` per note.
    pub fn rename_notes(
        &self,
        moves: &[(PathBuf, PathBuf)],
        cancel: &Cancel,
    ) -> Result<WorkspaceEdit, Error> {
        cancel.check()?;
        self.plan(&self.store(), moves, cancel)
    }

    /// The body of [`rename_notes`](Self::rename_notes) over one read of
    /// the store: callers hold the guard, so a public call takes the lock
    /// once (a nested read behind a queued writer would deadlock).
    fn plan(
        &self,
        store: &MemStore,
        moves: &[(PathBuf, PathBuf)],
        cancel: &Cancel,
    ) -> Result<WorkspaceEdit, Error> {
        let mut map: HashMap<String, String> = HashMap::new();
        // Folder pairs that move at least one note: the whole directory moves.
        let mut folders: Vec<(String, String)> = Vec::new();
        for (old, new) in moves {
            cancel.check()?;
            let (Ok(old_rel), Ok(new_rel)) = (self.rel(old), self.rel(new)) else {
                continue;
            };
            let mut pairs = Vec::new();
            if store.document(&old_rel).is_some() {
                if is_note(new_rel.rsplit('/').next().unwrap_or_default()) {
                    pairs.push((old_rel.clone(), new_rel));
                }
            } else {
                let prefix = format!("{old_rel}/");
                for f in store.files() {
                    if let Some(sub) = f.strip_prefix(&prefix) {
                        pairs.push((f.to_owned(), format!("{new_rel}/{sub}")));
                    }
                }
                if !pairs.is_empty() {
                    folders.push((old_rel, new_rel));
                }
            }
            for (from, to) in pairs {
                if map.insert(from.clone(), to).is_some() {
                    return Err(refuse(format!("{from}: moved twice")));
                }
            }
        }
        let root = self.root().path;
        let docs_dir = store.conventions().docs_dir.clone();
        let mut out = WorkspaceEdit::default();
        if map.is_empty() {
            return Ok(out);
        }
        let case_sensitive = self.fs().case_sensitive(&root);
        // Most specific folder first.
        folders.sort_by_key(|(o, _)| std::cmp::Reverse(o.len()));
        let (post, post_fs) =
            self.post_move_store(store, &map, &folders, case_sensitive, cancel)?;
        // Where a path target is after the moves: a moved note, or any
        // path (file, directory) under a moved folder.
        let moved = |rel: &str| -> Option<String> {
            map.get(rel)
                .cloned()
                .or_else(|| in_folder(&folders, rel, case_sensitive))
        };
        for from in store.files() {
            cancel.check()?;
            let Some(doc) = store.document(from) else {
                continue;
            };
            let from_after = map.get(from).map_or(from, String::as_str);
            let own_moved_dir = parent(from) != parent(from_after);
            let mut edits = Vec::new();
            for (link, r) in store.links(from) {
                if !counts(&link) || r.hint || link.target().path().is_empty() {
                    continue;
                }
                let Some(first) = r.targets.first() else {
                    continue;
                };
                let moved_to = moved(first);
                let to_moved = moved_to.is_some()
                    && matches!(
                        r.status,
                        LinkStatus::Resolved | LinkStatus::Ambiguous | LinkStatus::Unindexed
                    );
                let own_relative = own_moved_dir
                    && r.step == Some(ResolveStep::FileRelative)
                    && matches!(
                        r.status,
                        LinkStatus::Resolved | LinkStatus::Ambiguous | LinkStatus::Unindexed
                    );
                if !to_moved && !own_relative {
                    continue;
                }
                if is_reference_usage(doc, &link) {
                    continue;
                }
                let Some(span) = path_span(doc.source(), &link) else {
                    continue;
                };
                let target_after = moved_to.as_deref().unwrap_or(first);
                let site_key = to_moved
                    && store
                        .document(first)
                        .and_then(|d| d.frontmatter())
                        .and_then(|f| f.site_path())
                        .is_some();
                let ctx = Rewrite {
                    root: &root,
                    from_after,
                    target_after,
                    written: &doc.source()[span.clone()],
                    bracketed: doc.source()[..span.start].ends_with('<'),
                    kind: link.kind,
                    docs_dir: docs_dir.as_deref(),
                    site_key,
                };
                // A bare stem that another note carries after the moves
                // would find that note (or be ambiguous): write the path.
                let stem_taken = stem_shared(&post, target_after, case_sensitive);
                // A slash-less wiki target is written by name even when the
                // ladder found it by path (a note at the root): keep that
                // style unless the new name is taken.
                let step = match r.step {
                    Some(ResolveStep::FileRelative | ResolveStep::RootRelative)
                        if to_moved
                            && matches!(link.kind, LinkKind::Wiki | LinkKind::WikiEmbed)
                            && !ctx.written.contains('/')
                            && !stem_taken =>
                    {
                        Some(ResolveStep::Stem)
                    }
                    Some(ResolveStep::Stem) if to_moved && stem_taken => {
                        Some(ResolveStep::RootRelative)
                    }
                    s => s,
                };
                let new_text = step.and_then(|st| ctx.text(st)).and_then(|preferred| {
                    // Every rewritten link must land on its target in the
                    // post-move store, by the resolver itself: the
                    // preferred text, else the `/`-prefixed root-relative
                    // path, else the file-relative path. Every candidate
                    // must parse back, in place in the link, as a link
                    // with exactly that path. A target that is not a note
                    // must exist on disk after the moves.
                    let src = doc.source();
                    if post.document(target_after).is_none()
                        && post_fs.stat(&post.root().join(target_after)).is_err()
                    {
                        return None;
                    }
                    let lands = |t: &str| {
                        let spliced = format!(
                            "{}{t}{}",
                            &src[link.range.start..span.start],
                            &src[span.end..link.range.end]
                        );
                        lands_after(&post, from_after, &spliced, t, target_after)
                    };
                    [preferred, ctx.slash_rooted(), ctx.file_relative()]
                        .into_iter()
                        .find(|t| lands(t))
                });
                if let Some(new_text) = new_text
                    && new_text != ctx.written
                {
                    edits.push(TextEdit {
                        range: span,
                        new_text,
                    });
                }
            }
            edits.sort_by_key(|e| (e.range.start, e.range.end));
            edits.dedup_by(|a, b| a.range == b.range);
            if !edits.is_empty() {
                out.edits.push((self.abs(from), edits));
            }
        }
        out.edits.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }
}

impl Workspace {
    /// The store as it will be after the moves in `map`: every indexed
    /// note's current text (overlay wins) under its new path, over a
    /// filesystem that shows the moves, with the same code dirs. Re-parses
    /// every note.
    fn post_move_store(
        &self,
        store: &MemStore,
        map: &HashMap<String, String>,
        folders: &[(String, String)],
        case_sensitive: bool,
        cancel: &Cancel,
    ) -> Result<(MemStore, Arc<MovedFs>), Error> {
        let root = store.root().to_path_buf();
        let contents = store
            .files()
            .filter_map(|f| {
                let text = store.document(f)?.source();
                let at = map.get(f).map_or(f, String::as_str);
                Some((at.to_owned(), Arc::from(text.as_bytes())))
            })
            .collect();
        let fs = Arc::new(MovedFs::new(
            self.fs_arc(),
            &root,
            map,
            folders,
            case_sensitive,
        ));
        let mut post = MemStore::from_contents(fs.clone(), root, contents, cancel)?;
        post.set_code_dirs(store.code_dirs().to_vec());
        Ok((post, fs))
    }
}

/// Another note than `rel` has its stem key in `post` (normalised and
/// case-folded as the resolver keys it).
fn stem_shared(post: &MemStore, rel: &str, case_sensitive: bool) -> bool {
    let Some(doc) = post.document(rel) else {
        return false;
    };
    doc_keys(rel, doc, case_sensitive)
        .into_iter()
        .filter(|(k, _)| *k == KeyKind::Stem)
        .any(|(_, key)| post.lookup(KeyKind::Stem, &key).iter().any(|p| p != rel))
}

/// `rel` under the most specific of `folders` (old, new; longest old
/// first), as the same path under its new folder; `None` outside them.
fn in_folder(folders: &[(String, String)], rel: &str, case_sensitive: bool) -> Option<String> {
    let same = |a: &str, b: &str| match case_sensitive {
        true => a == b,
        false => a.to_lowercase() == b.to_lowercase(),
    };
    folders.iter().find_map(|(old, new)| {
        let mut rest = rel.split('/');
        for o in old.split('/') {
            if !same(o, rest.next()?) {
                return None;
            }
        }
        Some(
            std::iter::once(new.as_str())
                .chain(rest)
                .collect::<Vec<_>>()
                .join("/"),
        )
    })
}

/// Whether `spliced` (the link's source text with `text` in place of its
/// target path, written in the note at `from_after`) parses, in that
/// note's dialect, to a link whose path is exactly `text` that resolves in
/// `post` to `target` first and not ambiguously. A `#`, `|`, `]]` or `::`
/// in `text` that the parser would read as syntax fails.
fn lands_after(post: &MemStore, from_after: &str, spliced: &str, text: &str, target: &str) -> bool {
    let doc = parse(spliced, Dialect::detect_from_path(Path::new(from_after)));
    let Some(l) = doc.links().next() else {
        return false;
    };
    if l.target().path() != text {
        return false;
    }
    let r = post.resolve_link(from_after, l, false);
    r.status != LinkStatus::Ambiguous && r.targets.first().map(String::as_str) == Some(target)
}

/// The workspace filesystem as it will be after the moves. A note pair
/// moves one file: its destination answers with the source's file and the
/// source is gone. A folder pair moves the whole directory: a path under
/// the new folder answers from the same path under the old one, and a path
/// under the old folder is gone. A directory holding a destination exists.
/// Paths are matched case-folded on a case-insensitive root, as the
/// resolver may ask for a folded key.
struct MovedFs {
    inner: Arc<dyn FileSystem>,
    case_sensitive: bool,
    /// Note destination (absolute, folded) -> source (absolute).
    dest: HashMap<PathBuf, PathBuf>,
    /// Moved note sources (absolute, folded) that are not destinations.
    gone: HashSet<PathBuf>,
    /// Folder pairs: (new, folded; old, absolute), deepest new first.
    folders: Vec<(PathBuf, PathBuf)>,
    /// Old folders (absolute, folded), deepest first.
    old_folders: Vec<PathBuf>,
    /// Directories (absolute, folded) holding a destination, up to the
    /// root.
    dirs: HashSet<PathBuf>,
    /// A directory's metadata, for the `dirs` that do not exist yet.
    dir_meta: Meta,
}

/// Where a path of the post-move tree lives before the moves.
enum Before {
    At(PathBuf),
    Gone,
}

impl MovedFs {
    fn new(
        inner: Arc<dyn FileSystem>,
        root: &Path,
        map: &HashMap<String, String>,
        folders: &[(String, String)],
        case_sensitive: bool,
    ) -> Self {
        let fold = |p: PathBuf| fold_path(p, case_sensitive);
        let dest: HashMap<PathBuf, PathBuf> = map
            .iter()
            .map(|(f, t)| (fold(root.join(t)), root.join(f)))
            .collect();
        let gone = map
            .keys()
            .map(|f| fold(root.join(f)))
            .filter(|f| !dest.contains_key(f))
            .collect();
        let mut old_folders: Vec<PathBuf> =
            folders.iter().map(|(o, _)| fold(root.join(o))).collect();
        let mut folders: Vec<(PathBuf, PathBuf)> = folders
            .iter()
            .map(|(o, n)| (fold(root.join(n)), root.join(o)))
            .collect();
        // Most specific (deepest) folder first.
        old_folders.sort_by_key(|o| std::cmp::Reverse(o.components().count()));
        folders.sort_by_key(|(n, _)| std::cmp::Reverse(n.components().count()));
        let root = fold(root.to_path_buf());
        let mut dirs = HashSet::new();
        for d in dest.keys() {
            for a in d.ancestors().skip(1) {
                if !a.starts_with(&root) || !dirs.insert(a.to_path_buf()) {
                    break;
                }
            }
        }
        let dir_meta = inner.stat(&root).unwrap_or(Meta {
            ino: 0,
            ctime_ns: 0,
            mtime_ns: 0,
            size: 0,
            is_dir: true,
            is_file: false,
            dataless: false,
        });
        MovedFs {
            inner,
            case_sensitive,
            dest,
            gone,
            folders,
            old_folders,
            dirs,
            dir_meta,
        }
    }

    fn before(&self, p: &Path) -> Before {
        let key = fold_path(p.to_path_buf(), self.case_sensitive);
        if let Some(s) = self.dest.get(&key) {
            return Before::At(s.clone());
        }
        for (new, old) in &self.folders {
            if key.starts_with(new) {
                // `key` and `p` have the same components, folded or not.
                let n = new.components().count();
                return Before::At(old.join(p.components().skip(n).collect::<PathBuf>()));
            }
        }
        if self.gone.contains(&key) || self.old_folders.iter().any(|o| key.starts_with(o)) {
            return Before::Gone;
        }
        Before::At(p.to_path_buf())
    }

    /// A directory that holds a destination.
    fn is_new_dir(&self, p: &Path) -> bool {
        self.dirs
            .contains(&fold_path(p.to_path_buf(), self.case_sensitive))
    }
}

fn fold_path(p: PathBuf, case_sensitive: bool) -> PathBuf {
    match case_sensitive {
        true => p,
        false => PathBuf::from(p.to_string_lossy().to_lowercase()),
    }
}

fn not_found(p: &Path) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, format!("{}: moved", p.display()))
}

impl FileSystem for MovedFs {
    fn read(&self, p: &Path) -> io::Result<(Arc<[u8]>, Meta)> {
        match self.before(p) {
            Before::At(src) => self.inner.read(&src),
            Before::Gone => Err(not_found(p)),
        }
    }

    fn stat(&self, p: &Path) -> io::Result<Meta> {
        let r = match self.before(p) {
            Before::At(src) => self.inner.stat(&src),
            Before::Gone => Err(not_found(p)),
        };
        match r {
            Err(_) if self.is_new_dir(p) => Ok(self.dir_meta),
            r => r,
        }
    }

    fn read_dir(&self, p: &Path) -> io::Result<Vec<(String, Meta)>> {
        self.inner.read_dir(p)
    }

    fn canonicalize(&self, p: &Path) -> io::Result<PathBuf> {
        self.inner.canonicalize(p)
    }

    fn case_sensitive(&self, dir: &Path) -> bool {
        self.inner.case_sensitive(dir)
    }

    fn fs_kind(&self, dir: &Path) -> FsKind {
        self.inner.fs_kind(dir)
    }
}

fn refuse(msg: String) -> Error {
    Error::new(ErrorKind::Unsupported, format!("cannot rename: {msg}"))
}

/// A `[text][ref]` usage: its definition carries the destination.
pub(crate) fn is_reference_usage(doc: &Document, l: &Link) -> bool {
    l.kind == LinkKind::Reference && !doc.link_defs().any(|d| d.range == l.range)
}

/// The byte range of the link's target path (no anchor, no brackets) in
/// `src`, checked against `target.path`.
pub(crate) fn path_span(src: &str, l: &Link) -> Option<Range<usize>> {
    let slice = src.get(l.range.clone())?;
    let base = l.range.start;
    let (start, end) = match l.kind {
        LinkKind::Markdown | LinkKind::Image => {
            let from = l.text_range.end.saturating_sub(base).min(slice.len());
            let open = from + slice[from..].find("](")? + 2;
            dest(slice, open)
        }
        LinkKind::Reference => {
            let open = slice.find("]:")? + 2;
            dest(slice, open)
        }
        LinkKind::Wiki | LinkKind::WikiEmbed => {
            let open = slice.find("[[")? + 2;
            let len = slice[open..].find(['|', '#', ']'])?;
            (open, open + len)
        }
        LinkKind::Org => {
            let mut open = slice.find("[[")? + 2;
            if slice[open..].starts_with("file:") {
                open += "file:".len();
            }
            let rest = &slice[open..];
            let len = [rest.find(']'), rest.find("::"), rest.find('#')]
                .into_iter()
                .flatten()
                .min()?;
            (open, open + len)
        }
        _ => {
            let at = slice.find(l.target().path())?;
            (at, at + l.target().path().len())
        }
    };
    let text = slice.get(start..end)?;
    // Unexpected syntax (escapes, a target on the right of a pipe): no edit.
    (text == l.target().path() || text.split('#').next() == Some(l.target().path())).then(|| {
        let end = start + text.find('#').unwrap_or(text.len());
        base + start..base + end
    })
}

/// A link destination starting at or after `open` (whitespace skipped):
/// inside `<…>`, or up to whitespace or the unbalanced `)`.
fn dest(slice: &str, open: usize) -> (usize, usize) {
    let rest = &slice[open..];
    let start = open + (rest.len() - rest.trim_start().len());
    let rest = &slice[start..];
    if let Some(inner) = rest.strip_prefix('<') {
        let len = inner.find('>').unwrap_or(inner.len());
        return (start + 1, start + 1 + len);
    }
    let mut depth = 0usize;
    for (i, c) in rest.char_indices() {
        match c {
            '(' => depth += 1,
            ')' if depth == 0 => return (start, start + i),
            ')' => depth -= 1,
            c if c.is_whitespace() => return (start, start + i),
            _ => {}
        }
    }
    (start, slice.len())
}

/// How to write one rewritten target.
pub(crate) struct Rewrite<'a> {
    pub(crate) root: &'a Path,
    /// Root-relative path of the linking note after the rename.
    pub(crate) from_after: &'a str,
    /// Root-relative path of the target after the rename.
    pub(crate) target_after: &'a str,
    /// The path as written (no anchor).
    pub(crate) written: &'a str,
    pub(crate) bracketed: bool,
    pub(crate) kind: LinkKind,
    pub(crate) docs_dir: Option<&'a str>,
    /// The target is found by its frontmatter site path: renaming the file
    /// does not change it.
    pub(crate) site_key: bool,
}

impl Rewrite<'_> {
    pub(crate) fn text(&self, step: ResolveStep) -> Option<String> {
        let decoded = percent_decode(self.written);
        let target = self.with_ext(self.target_after, &decoded);
        let out = match step {
            ResolveStep::FileRelative => {
                let rel = self.relative(&target);
                match decoded.starts_with("./") && !rel.starts_with("../") {
                    true => format!("./{rel}"),
                    false => rel,
                }
            }
            ResolveStep::RootRelative => {
                if decoded.starts_with('~') {
                    return None;
                }
                let root = self.root.to_string_lossy();
                if Path::new(&decoded).starts_with(self.root) {
                    format!("{}/{target}", root.trim_end_matches('/'))
                } else if decoded.starts_with('/') {
                    format!("/{target}")
                } else {
                    target
                }
            }
            ResolveStep::SiteRooted => {
                if self.site_key {
                    return None;
                }
                let under_docs = self
                    .docs_dir
                    .and_then(|d| target.strip_prefix(d)?.strip_prefix('/'));
                format!("/{}", under_docs.unwrap_or(&target))
            }
            ResolveStep::Stem => {
                let name = target.rsplit('/').next().unwrap_or(&target);
                name.to_owned()
            }
            _ => return None,
        };
        Some(self.encode(out))
    }

    /// The target path as written by [`ResolveStep::FileRelative`], in
    /// the link's encoding, without a leading `./`.
    fn file_relative(&self) -> String {
        let decoded = percent_decode(self.written);
        self.encode(self.relative(&self.with_ext(self.target_after, &decoded)))
    }

    /// The target as a `/`-prefixed root-relative path, in the link's
    /// encoding.
    fn slash_rooted(&self) -> String {
        let decoded = percent_decode(self.written);
        self.encode(format!("/{}", self.with_ext(self.target_after, &decoded)))
    }

    fn relative(&self, target: &str) -> String {
        let from_dir = self.root.join(parent(self.from_after));
        relative_path(&from_dir, &self.root.join(target))
    }

    /// `path` with its note extension kept iff `written` had one.
    fn with_ext(&self, path: &str, written: &str) -> String {
        let last = written.rsplit('/').next().unwrap_or(written);
        let name_at = path.rfind('/').map_or(0, |i| i + 1);
        if is_note(last) || !is_note(&path[name_at..]) {
            return path.to_owned();
        }
        match path[name_at..].rfind('.') {
            Some(dot) => path[..name_at + dot].to_owned(),
            None => path.to_owned(),
        }
    }

    /// Markdown destinations are encoded by
    /// [`encode_destination`](crate::links::encode_destination), in `<…>`
    /// when written so.
    fn encode(&self, s: String) -> String {
        let md = matches!(
            self.kind,
            LinkKind::Markdown | LinkKind::Image | LinkKind::Reference
        );
        match md {
            true => encode_destination(&s, self.bracketed),
            false => s,
        }
    }
}

/// The file name without its extension.
#[allow(dead_code, reason = "kept for fixes.rs (unit fixes_repair)")]
pub(crate) fn stem(rel: &str) -> &str {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    name.rsplit_once('.').map_or(name, |(s, _)| s)
}

/// The directory part of a root-relative path (`""` at the root).
pub(crate) fn parent(rel: &str) -> &str {
    rel.rsplit_once('/').map_or("", |(d, _)| d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dest_forms() {
        let s = "[t](ab.md \"x\")";
        assert_eq!(dest(s, 4), (4, 9));
        let s = "[t](<a b.md>)";
        assert_eq!(&s[dest(s, 4).0..dest(s, 4).1], "a b.md");
        let s = "[t](a(1).md)";
        assert_eq!(&s[dest(s, 4).0..dest(s, 4).1], "a(1).md");
    }
}
