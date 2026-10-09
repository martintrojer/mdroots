//! The resolution ladder (docs/specs/index.md §2.4): one function
//! every feature uses to turn a parsed link into targets and a status.
//!
//! The code-mention rules (reject whitespace, `://`, over 512 bytes and
//! `~user/`; full text before the `:LINE[:COL]`-stripped form; only regular
//! files are hits) are donated from ramble 75b8285 `src/app/codepath.rs`.

use std::path::{Component, Path, PathBuf};

use mdroots_syntax::{Confidence, Link, LinkKind, slug};

use crate::env::ResolveEnv;
use crate::keys::{KeyKind, KeyLookup, ResolveStep};
use crate::normalize::{Normalized, normalize, normalize_str, percent_decode};

/// Code spans longer than this are never paths.
const MAX_CODE_LEN: usize = 512;

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkStatus {
    /// Found in the index (one target).
    Resolved,
    /// Several hits at the stopping step; targets are best first.
    Ambiguous,
    /// On disk but not indexed (gitignored, non-markdown, a directory).
    Unindexed,
    /// An explicit link whose target is neither indexed nor on disk.
    Broken,
    /// Another scheme, or a file outside the root.
    External,
    /// An implicit link that did not resolve: silently dropped.
    Unchecked,
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    /// Root-relative paths, best first.
    pub targets: Vec<String>,
    pub step: Option<ResolveStep>,
    pub status: LinkStatus,
    /// Found by the Partial step: not rewritten by rename, no diagnostics.
    pub hint: bool,
}

impl Resolution {
    fn none(status: LinkStatus) -> Self {
        Resolution {
            targets: Vec::new(),
            step: None,
            status,
            hint: false,
        }
    }
}

/// What the ladder may consult for one root.
#[non_exhaustive]
pub struct ResolveCtx<'a> {
    pub env: &'a dyn ResolveEnv,
    pub keys: &'a dyn KeyLookup,
    /// Run the Partial step (goto, hover, completion: true; diagnostics: false).
    pub allow_partial: bool,
    /// Extra root-relative dirs for code mentions (e.g. the VCS root); may
    /// start with `..`.
    pub code_dirs: &'a [String],
    /// The root's absolute path, to map absolute and `~/` targets into it.
    pub root_abs: Option<&'a Path>,
    /// Docs dir (`docs`, `content`, `src`) for site-rooted links, set when
    /// an mkdocs/Hugo/Docusaurus marker exists.
    pub docs_dir: Option<&'a str>,
}

impl<'a> ResolveCtx<'a> {
    pub fn new(env: &'a dyn ResolveEnv, keys: &'a dyn KeyLookup) -> Self {
        ResolveCtx {
            env,
            keys,
            allow_partial: false,
            code_dirs: &[],
            root_abs: None,
            docs_dir: None,
        }
    }

    fn fold(&self, s: &str) -> String {
        if self.env.case_sensitive() {
            s.to_owned()
        } else {
            s.to_lowercase()
        }
    }

    fn lookup(&self, kind: KeyKind, key: &str) -> Vec<String> {
        self.keys.lookup(kind, &self.fold(key))
    }
}

/// Resolve `link`, found in the file at `from_root_rel`.
pub fn resolve(from_root_rel: &str, link: &Link, ctx: &ResolveCtx) -> Resolution {
    if link.confidence == Confidence::External {
        return Resolution::none(LinkStatus::External);
    }
    match link.kind {
        LinkKind::Footnote => return same_file(from_root_rel),
        LinkKind::CodeMention => return code_mention(from_root_rel, link, ctx),
        _ => {}
    }
    let explicit = link.confidence == Confidence::Explicit;
    let n = normalize(link, true);
    if n.is_external() {
        return Resolution::none(LinkStatus::External);
    }
    if n.id_ref {
        let t = ctx.lookup(KeyKind::Id, &n.key);
        return match t.is_empty() {
            true => Resolution::none(no_hit_status(explicit)),
            false => finish(from_root_rel, Hit::new(t, ResolveStep::Id)),
        };
    }
    if n.key.is_empty() {
        return match n.anchor {
            Some(_) => same_file(from_root_rel),
            None => Resolution::none(no_hit_status(explicit)),
        };
    }
    let q = match Query::new(&n, &link.target.path, explicit, ctx) {
        Ok(q) => q,
        Err(r) => return r,
    };
    let alt = n
        .piped_alt
        .as_ref()
        .and(link.label.as_deref())
        .filter(|l| *l != link.target.raw)
        .map(|l| normalize_str(l, true))
        .filter(|a| !a.is_external() && !a.id_ref && !a.key.is_empty())
        .and_then(|a| {
            let src = l_path(link.label.as_deref().unwrap_or(""));
            Query::new(&a, src, explicit, ctx).ok()
        });
    let dir = parent(from_root_rel);
    let tries = [Some(&q), alt.as_ref()];
    for q in tries.iter().flatten() {
        if let Some(h) = ladder(q, dir, link.kind, ctx) {
            return finish(from_root_rel, h);
        }
    }
    if ctx.allow_partial && explicit {
        for q in tries.iter().flatten() {
            let t = ctx.keys.partial(&ctx.fold(&q.key));
            if !t.is_empty() {
                let mut h = Hit::new(t, ResolveStep::Partial);
                h.hint = true;
                return finish(from_root_rel, h);
            }
        }
    }
    // Precedence: left indexed > right indexed > (partial) > left on disk
    // > right on disk > Broken.
    // Implicit `./`, `../` (and mapped `~/`) bare paths get one stat (spec
    // §2.3); other implicit forms resolve through the index only.
    let dotted = matches!(link.kind, LinkKind::BarePath)
        && (link.target.path.starts_with("./")
            || link.target.path.starts_with("../")
            || link.target.path.starts_with("~/"));
    if (explicit || dotted)
        && let Some(p) = tries.iter().flatten().find_map(|q| on_disk(q, dir, ctx))
    {
        return Resolution {
            targets: vec![p],
            step: None,
            status: LinkStatus::Unindexed,
            hint: false,
        };
    }
    Resolution::none(no_hit_status(explicit))
}

fn no_hit_status(explicit: bool) -> LinkStatus {
    if explicit {
        LinkStatus::Broken
    } else {
        LinkStatus::Unchecked
    }
}

fn same_file(from_root_rel: &str) -> Resolution {
    Resolution {
        targets: vec![from_root_rel.to_owned()],
        step: Some(ResolveStep::FileRelative),
        status: LinkStatus::Resolved,
        hint: false,
    }
}

/// A label's path part (before any `#`), for the written extension.
fn l_path(label: &str) -> &str {
    label.split('#').next().unwrap_or(label)
}

/// A link target ready for the ladder: original case, `.md`-stripped.
struct Query {
    /// Root-relative or from-dir-relative key; may contain `..`.
    key: String,
    /// The written extension when it was md/markdown/org (stripped from key).
    md_ext: Option<String>,
    /// Lowercase extension as written, if any.
    had_ext: Option<String>,
    /// Site-rooted `/x` (no `file:`): SiteRooted, then RootRelative.
    rooted: bool,
    /// Mapped from an absolute or `~/` path: RootRelative only.
    mapped: bool,
}

impl Query {
    /// `Err` carries the final answer for absolute and `~/` targets that
    /// do not map into the root.
    fn new(
        n: &Normalized,
        src_path: &str,
        explicit: bool,
        ctx: &ResolveCtx,
    ) -> Result<Query, Resolution> {
        let md_ext = match n.had_extension.as_deref() {
            Some("md" | "markdown" | "org") => written_ext(src_path),
            _ => None,
        };
        let mut q = Query {
            key: n.key.clone(),
            md_ext,
            had_ext: n.had_extension.clone(),
            rooted: false,
            mapped: false,
        };
        let abs: Option<PathBuf> = if n.home {
            match ctx.env.home_dir() {
                Some(h) => Some(h.join(&n.key)),
                None if explicit => return Err(Resolution::none(LinkStatus::External)),
                None => return Err(Resolution::none(LinkStatus::Unchecked)),
            }
        } else if n.rooted && n.scheme.as_deref() == Some("file") {
            Some(Path::new("/").join(&n.key))
        } else {
            q.rooted = n.rooted;
            None
        };
        let Some(abs) = abs else { return Ok(q) };
        if let Some(rel) = ctx
            .root_abs
            .and_then(|r| clean(&abs).strip_prefix(clean(r)).ok().map(path_str))
        {
            q.key = rel;
            q.mapped = true;
            return Ok(q);
        }
        if explicit {
            return Err(Resolution::none(LinkStatus::External));
        }
        // Implicit (bare path) outside the root: only a file on disk counts.
        let disk = q.disk(&path_str(&abs));
        let found = outside_rel(ctx.root_abs, Path::new(&disk)).filter(|p| ctx.env.is_file(p));
        Err(match found {
            Some(p) => Resolution {
                targets: vec![p],
                step: Some(ResolveStep::RootRelative),
                status: LinkStatus::Unindexed,
                hint: false,
            },
            None => Resolution::none(LinkStatus::Unchecked),
        })
    }

    fn is_md(&self) -> bool {
        self.had_ext.is_none() || self.md_ext.is_some()
    }

    /// The on-disk name of `key`: with the written md extension put back.
    fn disk(&self, key: &str) -> String {
        match &self.md_ext {
            Some(e) => format!("{key}.{e}"),
            None => key.to_owned(),
        }
    }
}

/// The last segment's extension as written (original case).
fn written_ext(path: &str) -> Option<String> {
    let dec = percent_decode(path);
    let last = dec.rsplit(['/', '\\']).next()?;
    let (stem, ext) = last.rsplit_once('.')?;
    (!stem.is_empty() && !ext.is_empty()).then(|| ext.to_owned())
}

struct Hit {
    targets: Vec<String>,
    step: ResolveStep,
    unindexed: bool,
    hint: bool,
}

impl Hit {
    fn new(targets: Vec<String>, step: ResolveStep) -> Self {
        Hit {
            targets,
            step,
            unindexed: false,
            hint: false,
        }
    }

    fn unindexed(path: String, step: ResolveStep) -> Self {
        Hit {
            unindexed: true,
            ..Hit::new(vec![path], step)
        }
    }
}

/// Steps 1-8; the first step with a hit stops. Partial runs in `resolve`.
fn ladder(q: &Query, dir: &str, kind: LinkKind, ctx: &ResolveCtx) -> Option<Hit> {
    use ResolveStep as S;
    if !q.rooted
        && !q.mapped
        && let Some(j) = join(dir, &q.key)
        && let Some(h) = path_step(q, &j, kind, ctx, S::FileRelative)
    {
        return Some(h);
    }
    if q.rooted {
        let mut t = ctx.lookup(KeyKind::SitePath, &q.key);
        if t.is_empty()
            && let Some(d) = ctx.docs_dir
            && let Some(j) = join(d, &q.key)
        {
            t = ctx.lookup(KeyKind::Path, &j);
        }
        if !t.is_empty() {
            return Some(Hit::new(t, S::SiteRooted));
        }
    }
    if let Some(j) = join("", &q.key)
        && let Some(h) = path_step(q, &j, kind, ctx, S::RootRelative)
    {
        return Some(h);
    }
    // Bare paths are path-like by construction (docs/specs/index.md §2.3 guards); key
    // steps would turn prose like `mode/fast` into title or alias hits.
    if q.mapped || !q.is_md() || kind == LinkKind::BarePath {
        return None;
    }
    let steps: [(S, Vec<(KeyKind, String)>); 5] = [
        (
            S::Stem,
            match q.key.contains('/') {
                true => vec![],
                false => vec![(KeyKind::Stem, q.key.clone())],
            },
        ),
        (S::Id, vec![(KeyKind::Id, q.key.clone())]),
        (S::Title, vec![(KeyKind::TitleSlug, slug::github(&q.key))]),
        (S::Alias, vec![(KeyKind::Alias, q.key.clone())]),
        (S::DialectTransform, logseq(&q.key)),
    ];
    for (step, lookups) in steps {
        // Collect every variant's hits so ties within a step are Ambiguous.
        // TitleSlug keys are not case-folded; slugs are lowercase anyway.
        let mut t: Vec<String> = lookups
            .iter()
            .flat_map(|(kind, key)| ctx.lookup(*kind, key))
            .collect();
        t.sort();
        t.dedup();
        if !t.is_empty() {
            return Some(Hit::new(t, step));
        }
    }
    None
}

/// Logseq namespaces: `a/b` is stored as `pages/a___b.md` or `pages/a%2Fb.md`.
/// Stored path keys are percent-decoded (`doc_keys`), so the `%2F` file's
/// key is `pages/a/b`.
fn logseq(key: &str) -> Vec<(KeyKind, String)> {
    if !key.contains('/') || key.contains("..") {
        return vec![];
    }
    ["___", "%2F"]
        .iter()
        .map(|sep| {
            let enc = format!("pages/{}", key.replace('/', sep));
            (KeyKind::Path, percent_decode(&enc))
        })
        .collect()
}

/// FileRelative or RootRelative at the joined path `j`.
fn path_step(
    q: &Query,
    j: &str,
    kind: LinkKind,
    ctx: &ResolveCtx,
    step: ResolveStep,
) -> Option<Hit> {
    // An extensionless markdown link: a bare file or dir on disk wins over
    // the `.md` variant (ramble c2a138b7, zk default).
    let md_link = matches!(
        kind,
        LinkKind::Markdown | LinkKind::Reference | LinkKind::Image | LinkKind::Html
    );
    if q.had_ext.is_none() && md_link && ctx.env.exists(j) {
        return Some(Hit::unindexed(j.to_owned(), step));
    }
    let t = ctx.lookup(KeyKind::Path, j);
    if !t.is_empty() {
        return Some(Hit::new(t, step));
    }
    if !q.is_md() && ctx.env.is_file(j) {
        return Some(Hit::unindexed(j.to_owned(), step));
    }
    None
}

/// An explicit link that missed every step but exists on disk: the
/// FileRelative then RootRelative candidate, as written, then with `.md`.
fn on_disk(q: &Query, dir: &str, ctx: &ResolveCtx) -> Option<String> {
    let from_dir = (!q.rooted && !q.mapped)
        .then(|| join(dir, &q.key))
        .flatten();
    [from_dir, join("", &q.key)]
        .into_iter()
        .flatten()
        .flat_map(|c| [q.disk(&c), format!("{c}.md")])
        .find(|p| ctx.env.exists(p))
}

fn finish(from: &str, h: Hit) -> Resolution {
    let mut targets = h.targets;
    targets.sort_by(|a, b| {
        distance(from, a)
            .cmp(&distance(from, b))
            .then_with(|| a.cmp(b))
    });
    targets.dedup();
    let status = if h.unindexed {
        LinkStatus::Unindexed
    } else if targets.len() > 1 {
        LinkStatus::Ambiguous
    } else {
        LinkStatus::Resolved
    };
    Resolution {
        targets,
        step: Some(h.step),
        status,
        hint: h.hint,
    }
}

/// Directory components of `from` and `to` after their common prefix.
fn distance(from: &str, to: &str) -> usize {
    let a: Vec<&str> = segs(parent(from)).collect();
    let b: Vec<&str> = segs(parent(to)).collect();
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    a.len() + b.len() - 2 * common
}

fn segs(s: &str) -> impl Iterator<Item = &str> {
    s.split('/').filter(|x| !x.is_empty())
}

/// The directory part of a root-relative path (`""` at the root).
fn parent(p: &str) -> &str {
    p.rfind('/').map_or("", |i| &p[..i])
}

/// Join `key` onto `dir`, folding `..` lexically; `None` if it escapes the root.
fn join(dir: &str, key: &str) -> Option<String> {
    let out = join_loose(dir, key);
    (!out.starts_with("../") && out != "..").then_some(out)
}

/// Join and fold `..`, keeping leading `..` that climb above the root.
fn join_loose(dir: &str, key: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for s in segs(dir).chain(segs(key)) {
        match s {
            "." => {}
            ".." if out.last().is_some_and(|l| *l != "..") => {
                out.pop();
            }
            _ => out.push(s),
        }
    }
    out.join("/")
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// Lexically clean an absolute path (drop `.`, fold `..`).
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

/// `abs` relative to the root, climbing with `..` when outside it (both
/// cleaned lexically first); `None` without a root.
pub fn outside_rel(root: Option<&Path>, abs: &Path) -> Option<String> {
    let (root, abs) = (clean(root?), clean(abs));
    let r: Vec<_> = root.components().collect();
    let a: Vec<_> = abs.components().collect();
    let common = r.iter().zip(&a).take_while(|(x, y)| x == y).count();
    let mut parts: Vec<String> = vec!["..".to_owned(); r.len() - common];
    parts.extend(
        a[common..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
    );
    Some(parts.join("/"))
}

/// Code mentions: only an existing regular file counts. Tries the full
/// text, then the form with `:LINE[:COL]` stripped; relative paths against
/// the linking file's dir, each `code_dirs` entry, then the root.
fn code_mention(from: &str, link: &Link, ctx: &ResolveCtx) -> Resolution {
    let text = link.target.raw.trim();
    if text.is_empty()
        || text.len() > MAX_CODE_LEN
        || text.contains(char::is_whitespace)
        || text.contains("://")
    {
        return Resolution::none(LinkStatus::Unchecked);
    }
    let path = link.target.path.trim();
    let stripped = if path != text && !path.is_empty() {
        Some(path)
    } else {
        strip_position(text)
    };
    let dir = parent(from);
    for cand in std::iter::once(text).chain(stripped) {
        if let Some((p, step)) = locate(cand, dir, ctx) {
            let key = normalize_str(&p, true).key;
            let indexed = ctx
                .lookup(KeyKind::Path, &key)
                .iter()
                .any(|t| ctx.fold(t) == ctx.fold(&p));
            return Resolution {
                targets: vec![p],
                step: Some(step),
                status: match indexed {
                    true => LinkStatus::Resolved,
                    false => LinkStatus::Unindexed,
                },
                hint: false,
            };
        }
    }
    Resolution::none(LinkStatus::Unchecked)
}

/// A root-relative regular file for code text `t` (may start with `..`).
fn locate(t: &str, dir: &str, ctx: &ResolveCtx) -> Option<(String, ResolveStep)> {
    use ResolveStep as S;
    let abs: Option<PathBuf> = if let Some(rest) = t.strip_prefix("~/") {
        Some(ctx.env.home_dir()?.join(rest))
    } else if t.starts_with('~') {
        return None;
    } else if Path::new(t).is_absolute() || t.starts_with('/') {
        Some(PathBuf::from(t))
    } else {
        None
    };
    if let Some(abs) = abs {
        let p = outside_rel(ctx.root_abs, &abs)?;
        return ctx.env.is_file(&p).then_some((p, S::RootRelative));
    }
    std::iter::once((dir, S::FileRelative))
        .chain(ctx.code_dirs.iter().map(|d| (d.as_str(), S::RootRelative)))
        .chain(std::iter::once(("", S::RootRelative)))
        .map(|(d, s)| (join_loose(d, t), s))
        .find(|(p, _)| !p.is_empty() && ctx.env.is_file(p))
}

/// `path:LINE:COL` or `path:LINE` (positive integers) → `path`.
fn strip_position(text: &str) -> Option<&str> {
    let num = |s: &str| {
        !s.is_empty()
            && s.bytes().all(|b| b.is_ascii_digit())
            && s.parse::<u64>().is_ok_and(|n| n > 0)
    };
    let (head, last) = text.rsplit_once(':')?;
    if !num(last) {
        return None;
    }
    if let Some((path, line)) = head.rsplit_once(':')
        && num(line)
        && !path.is_empty()
    {
        return Some(path);
    }
    (!head.is_empty()).then_some(head)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_suffixes() {
        // Donated from ramble 75b8285 src/app/codepath.rs `position_suffixes`.
        assert_eq!(strip_position("a.rs:12"), Some("a.rs"));
        assert_eq!(strip_position("a.rs:12:3"), Some("a.rs"));
        assert_eq!(strip_position("a.rs:0"), None);
        assert_eq!(strip_position("a.rs:x"), None);
        assert_eq!(strip_position("a.rs:"), None);
        assert_eq!(strip_position(":3"), None);
        assert_eq!(strip_position("a.rs"), None);
    }

    #[test]
    fn joins_and_distance() {
        assert_eq!(join("a/b", "../c").as_deref(), Some("a/c"));
        assert_eq!(join("", "../c"), None);
        assert_eq!(join_loose("", "../c"), "../c");
        assert_eq!(distance("a/x.md", "a/y.md"), 0);
        assert_eq!(distance("a/x.md", "b/y.md"), 2);
        assert_eq!(distance("a/x.md", "a/b/y.md"), 1);
        assert_eq!(
            outside_rel(Some(Path::new("/r/root")), Path::new("/r/home/x")).as_deref(),
            Some("../home/x")
        );
    }
}
