//! zkdiff: compare mdroots link resolution (`MemStore`) with the links zk
//! stored in a notebook's `.zk/notebook.db` (docs/specs/index.md §5.1).
//!
//! zk's DB is opened read-only and immutable; nothing in the vault is
//! written. Links are matched per source file by a normalised href, in
//! occurrence order, and each matched pair is classified. zk stores no link
//! byte positions (its LSP references line is the first substring match of
//! the note stem, not the link), so positions are never compared; only
//! `snippet_start` (the paragraph offset) breaks ties between repeated hrefs.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::Path;

use mdroots_core::MemStore;
use mdroots_resolve::keys::ResolveStep;
use mdroots_resolve::ladder::LinkStatus;
use mdroots_resolve::normalize::{percent_decode, scheme};
use mdroots_syntax::{Confidence, Context, Link, LinkKind};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use unicode_normalization::UnicodeNormalization;

/// One row of zk's `links` table, joined to its source and target paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZkRow {
    pub id: i64,
    /// Root-relative source path (with extension).
    pub source: String,
    /// As zk stored it (markdown hrefs already joined onto the source dir).
    pub href: String,
    pub external: bool,
    /// Root-relative target path; `None` when zk did not resolve it.
    pub target: Option<String>,
    /// Byte offset of the snippet (the paragraph holding the link).
    pub snippet_start: usize,
    pub snippet_end: usize,
}

/// What zk indexed: its notes (source files) and links, by `links.id`.
#[derive(Debug, Clone, Default)]
pub struct ZkData {
    pub notes: BTreeSet<String>,
    pub rows: Vec<ZkRow>,
    /// Link rows whose source note no longer exists (zk does not enable
    /// SQLite foreign keys, so `ON DELETE CASCADE` never ran); not compared.
    pub orphans: usize,
    /// Those rows: (links.id, source_id, href, target path if any).
    pub orphan_rows: Vec<OrphanRow>,
}

/// A zk link row whose source note is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanRow {
    pub id: i64,
    pub source_id: i64,
    pub href: String,
    pub target: Option<String>,
}

/// One mdroots link, reduced to what is compared with zk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdRecord {
    pub source: String,
    pub kind: LinkKind,
    /// [`comparable_href`] of the link (not yet normalised).
    pub href: String,
    pub range: Range<usize>,
    pub status: LinkStatus,
    pub targets: Vec<String>,
    pub step: Option<ResolveStep>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Category {
    /// Same target, both unresolved, or both external.
    Agree,
    /// zk unresolved, mdroots Resolved / Ambiguous / Unindexed.
    MdrootsOnly,
    /// zk resolved, mdroots Broken / Unchecked.
    ZkOnly,
    /// Both resolved to different targets, or zk resolved and mdroots
    /// found the target only on disk (Unindexed).
    Disagree,
    /// External on one side only.
    ExternalMismatch,
    /// A zk row with no mdroots link at the same source and href.
    UnmatchedZk,
    /// An mdroots link with no zk row at the same source and href.
    UnmatchedMdroots,
    /// An mdroots link in a file zk does not index (all `.org` files).
    NotInZk,
    /// An unmatched mdroots markdown image: goldmark parses images as image
    /// nodes and zk stores no link row for them.
    ImageNotInZk,
}

impl Category {
    pub const ALL: [Category; 9] = [
        Category::Agree,
        Category::MdrootsOnly,
        Category::ZkOnly,
        Category::Disagree,
        Category::ExternalMismatch,
        Category::UnmatchedZk,
        Category::UnmatchedMdroots,
        Category::NotInZk,
        Category::ImageNotInZk,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Category::Agree => "agree",
            Category::MdrootsOnly => "mdroots-only",
            Category::ZkOnly => "zk-only",
            Category::Disagree => "disagree",
            Category::ExternalMismatch => "external-mismatch",
            Category::UnmatchedZk => "unmatched-zk",
            Category::UnmatchedMdroots => "unmatched-mdroots",
            Category::NotInZk => "not-in-zk",
            Category::ImageNotInZk => "image-not-in-zk",
        }
    }
}

/// A classified pair; one side is missing for the unmatched categories.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub category: Category,
    pub zk: Option<ZkRow>,
    pub md: Option<MdRecord>,
    /// mdroots answered Ambiguous.
    pub ambiguous: bool,
    /// Disagree because mdroots found the target only on disk.
    pub unindexed: bool,
}

impl Outcome {
    pub fn source(&self) -> &str {
        match (&self.zk, &self.md) {
            (Some(z), _) => &z.source,
            (_, Some(m)) => &m.source,
            _ => "",
        }
    }

    /// `source:href → zk target / mdroots target (step)`.
    pub fn describe(&self) -> String {
        let href = match (&self.zk, &self.md) {
            (Some(z), _) => z.href.as_str(),
            (_, Some(m)) => m.href.as_str(),
            _ => "",
        };
        let zk = match &self.zk {
            None => "-".to_owned(),
            Some(z) if z.external => "external".to_owned(),
            Some(z) => z.target.clone().unwrap_or_else(|| "unresolved".to_owned()),
        };
        let md = match &self.md {
            None => "-".to_owned(),
            Some(m) => {
                let mut s = format!("{:?}", m.status);
                if !m.targets.is_empty() {
                    s.push(' ');
                    s.push_str(&m.targets.join(","));
                }
                if let Some(step) = m.step {
                    s.push_str(&format!(" ({step:?})"));
                }
                s
            }
        };
        format!("{}:{href} → zk {zk} / mdroots {md}", self.source())
    }
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub outcomes: Vec<Outcome>,
}

impl Report {
    pub fn count(&self, c: Category) -> usize {
        self.outcomes.iter().filter(|o| o.category == c).count()
    }

    pub fn of(&self, c: Category) -> impl Iterator<Item = &Outcome> {
        self.outcomes.iter().filter(move |o| o.category == c)
    }

    /// Counts per category, then up to `examples` examples of every
    /// non-agree category.
    pub fn table(&self, title: &str, examples: usize) -> String {
        let mut s = format!("{title}\n");
        for c in Category::ALL {
            s.push_str(&format!("  {:<18} {:>6}\n", c.name(), self.count(c)));
        }
        let sub = |f: fn(&Outcome) -> bool| self.of(Category::Disagree).filter(|o| f(o)).count();
        s.push_str(&format!(
            "  disagree: unindexed {}, ambiguous {}\n",
            sub(|o| o.unindexed),
            sub(|o| o.ambiguous)
        ));
        if examples > 0 {
            for c in Category::ALL.into_iter().filter(|c| *c != Category::Agree) {
                let mut it = self.of(c).take(examples).peekable();
                if it.peek().is_some() {
                    s.push_str(&format!("\n  {} examples:\n", c.name()));
                }
                for o in it {
                    s.push_str(&format!("    {}\n", o.describe()));
                }
            }
        }
        s
    }

    pub fn to_json(&self, zk: &ZkData, meta: Value) -> Value {
        let counts: serde_json::Map<String, Value> = Category::ALL
            .iter()
            .map(|c| (c.name().to_owned(), json!(self.count(*c))))
            .collect();
        let outcomes: Vec<Value> = self
            .outcomes
            .iter()
            .map(|o| {
                json!({
                    "category": o.category.name(),
                    "source": o.source(),
                    "ambiguous": o.ambiguous,
                    "unindexed": o.unindexed,
                    "zk": o.zk.as_ref().map(|z| json!({
                        "id": z.id, "href": z.href, "external": z.external,
                        "target": z.target, "snippet_start": z.snippet_start,
                    })),
                    "mdroots": o.md.as_ref().map(|m| json!({
                        "kind": format!("{:?}", m.kind),
                        "href": m.href, "start": m.range.start, "end": m.range.end,
                        "status": format!("{:?}", m.status), "targets": m.targets,
                        "step": m.step.map(|s| format!("{s:?}")),
                    })),
                })
            })
            .collect();
        let orphans: Vec<Value> = zk
            .orphan_rows
            .iter()
            .map(|o| json!({ "id": o.id, "source_id": o.source_id, "href": o.href, "target": o.target }))
            .collect();
        json!({
            "meta": meta,
            "counts": counts,
            "orphan_count": zk.orphans,
            "orphans": orphans,
            "outcomes": outcomes,
        })
    }
}

/// Open `<vault>/.zk/notebook.db` read-only and immutable and load every
/// note path and link row (ordered by `links.id`).
pub fn load_zk(vault: &Path) -> rusqlite::Result<ZkData> {
    let db = vault.join(".zk/notebook.db");
    let uri = format!("file:{}?mode=ro&immutable=1", db.display());
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(uri, flags)?;
    let mut notes = BTreeSet::new();
    let mut st = conn.prepare("SELECT path FROM notes")?;
    for p in st.query_map([], |r| r.get::<_, String>(0))? {
        notes.insert(p?);
    }
    let mut st = conn.prepare(
        "SELECT l.id, s.path, l.href, l.external, t.path, l.snippet_start, l.snippet_end \
         FROM links l JOIN notes s ON s.id = l.source_id \
         LEFT JOIN notes t ON t.id = l.target_id ORDER BY l.id",
    )?;
    let rows: Vec<ZkRow> = st
        .query_map([], |r| {
            Ok(ZkRow {
                id: r.get(0)?,
                source: r.get(1)?,
                href: r.get(2)?,
                external: r.get::<_, i64>(3)? != 0,
                target: r.get(4)?,
                snippet_start: r.get::<_, i64>(5)?.max(0) as usize,
                snippet_end: r.get::<_, i64>(6)?.max(0) as usize,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut st = conn.prepare(
        "SELECT l.id, l.source_id, l.href, t.path FROM links l \
         LEFT JOIN notes s ON s.id = l.source_id \
         LEFT JOIN notes t ON t.id = l.target_id \
         WHERE s.id IS NULL ORDER BY l.id",
    )?;
    let orphan_rows: Vec<OrphanRow> = st
        .query_map([], |r| {
            Ok(OrphanRow {
                id: r.get(0)?,
                source_id: r.get(1)?,
                href: r.get(2)?,
                target: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ZkData {
        notes,
        rows,
        orphans: orphan_rows.len(),
        orphan_rows,
    })
}

/// Whether `link` is compared: explicit or external, outside code,
/// comments, frontmatter and HTML, and of a kind zk could store. Markdown
/// images are included; one with no zk row counts as image-not-in-zk.
pub fn compared(link: &Link) -> bool {
    link.confidence != Confidence::Implicit
        && !matches!(
            link.context,
            Context::CodeBlock
                | Context::InlineCode
                | Context::Comment
                | Context::Frontmatter
                | Context::Html
        )
        && matches!(
            link.kind,
            LinkKind::Markdown
                | LinkKind::Reference
                | LinkKind::Image
                | LinkKind::Autolink
                | LinkKind::Url
                | LinkKind::Wiki
                | LinkKind::WikiEmbed
                | LinkKind::Org
        )
}

/// The href zk would store for `link` in `source`: markdown-style
/// destinations joined onto the source dir (Go `path.Join`), wiki targets
/// as written (left of `|`), org-in-md `[[t][d]]` as `t][d`.
pub fn comparable_href(source: &str, link: &Link) -> String {
    let raw = link.target.raw.as_str();
    match link.kind {
        LinkKind::Markdown | LinkKind::Reference | LinkKind::Image => zk_join(source, raw),
        LinkKind::Org => match &link.label {
            Some(d) => format!("{raw}][{d}"),
            None => raw.to_owned(),
        },
        _ => raw.to_owned(),
    }
}

/// zk keeps a markdown destination as written when it is a URL with a
/// host (`https://x`, `file://host/p`); otherwise it treats it as a path
/// and joins it onto the source dir (`file:///U/x` → `dir/file:/U/x`).
pub fn zk_join(source: &str, dest: &str) -> String {
    if let Some(s) = scheme(dest) {
        let rest = &dest[s.len() + 1..];
        if let Some(auth) = rest.strip_prefix("//")
            && !auth.starts_with('/')
            && !auth.is_empty()
        {
            return dest.to_owned();
        }
        if !s.eq_ignore_ascii_case("file") && !rest.starts_with('/') {
            return dest.to_owned();
        }
    }
    let dir = source.rsplit_once('/').map_or("", |(d, _)| d);
    match dir.is_empty() {
        true => go_clean(dest),
        false => go_clean(&format!("{dir}/{dest}")),
    }
}

/// Go's `path.Clean`: collapse `//`, drop `.`, resolve `..` lexically.
pub fn go_clean(p: &str) -> String {
    if p.is_empty() {
        return ".".to_owned();
    }
    let rooted = p.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => match out.last() {
                Some(&l) if l != ".." => {
                    out.pop();
                }
                _ if rooted => {}
                _ => out.push(".."),
            },
            s => out.push(s),
        }
    }
    let joined = out.join("/");
    match (rooted, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => ".".to_owned(),
        (false, false) => joined,
    }
}

/// Percent-decode, then NFC; no case folding.
pub fn normalize_href(href: &str) -> String {
    percent_decode(href).nfc().collect()
}

/// Whether zk's wikilink parser produced this kind's href: it drops
/// backslashes (`\[` → `[`, even `\n` → `n`) while mdroots keeps wiki and
/// org-in-md targets as written. Markdown destinations are unescaped by
/// both CommonMark parsers alike, so their backslashes are left alone.
pub fn wikilink_kind(kind: LinkKind) -> bool {
    matches!(kind, LinkKind::Wiki | LinkKind::WikiEmbed | LinkKind::Org)
}

/// The match key of an mdroots record: [`normalize_href`], after dropping
/// backslashes for [`wikilink_kind`]s only. zk hrefs are keyed with
/// [`normalize_href`] as stored.
pub fn md_key(m: &MdRecord) -> String {
    match wikilink_kind(m.kind) {
        true => normalize_href(&m.href.replace('\\', "")),
        false => normalize_href(&m.href),
    }
}

/// Every compared link of every note in `store`, resolved without the
/// Partial step, in source order.
pub fn md_records(store: &MemStore) -> Vec<MdRecord> {
    let mut out = Vec::new();
    for f in store.files() {
        for (l, r) in store.links(f) {
            if !compared(&l) {
                continue;
            }
            out.push(MdRecord {
                source: f.to_owned(),
                kind: l.kind,
                href: comparable_href(f, &l),
                range: l.range.clone(),
                status: r.status,
                targets: r.targets,
                step: r.step,
            });
        }
    }
    out
}

/// Classify a matched pair.
pub fn classify(zk: &ZkRow, md: &MdRecord) -> (Category, bool, bool) {
    use Category as C;
    let ambiguous = md.status == LinkStatus::Ambiguous;
    let md_ext = md.status == LinkStatus::External;
    if zk.external || md_ext {
        let c = match zk.external && md_ext {
            true => C::Agree,
            false => C::ExternalMismatch,
        };
        return (c, ambiguous, false);
    }
    let c = match (&zk.target, &md.status) {
        (Some(t), LinkStatus::Resolved | LinkStatus::Ambiguous) => {
            match md.targets.iter().any(|m| m == t) {
                true => C::Agree,
                false => C::Disagree,
            }
        }
        (Some(_), LinkStatus::Unindexed) => return (C::Disagree, ambiguous, true),
        (Some(_), _) => C::ZkOnly,
        (None, LinkStatus::Resolved | LinkStatus::Ambiguous | LinkStatus::Unindexed) => {
            C::MdrootsOnly
        }
        (None, _) => C::Agree,
    };
    (c, ambiguous, false)
}

/// Match zk rows and mdroots records per source file by normalised href in
/// occurrence order, then classify. When an href repeats, a zk row takes
/// the first unused record inside its snippet, else the first unused one.
pub fn diff(zk: &ZkData, md: &[MdRecord]) -> Report {
    type Key = (String, String);
    let mut by_key: BTreeMap<Key, Vec<(&MdRecord, bool)>> = BTreeMap::new();
    let mut outcomes = Vec::new();
    for m in md {
        if !zk.notes.contains(&m.source) {
            outcomes.push(Outcome {
                category: Category::NotInZk,
                zk: None,
                md: Some(m.clone()),
                ambiguous: m.status == LinkStatus::Ambiguous,
                unindexed: false,
            });
            continue;
        }
        by_key
            .entry((m.source.clone(), md_key(m)))
            .or_default()
            .push((m, false));
    }
    for z in &zk.rows {
        let key = (z.source.clone(), normalize_href(&z.href));
        let cands = by_key.get_mut(&key);
        let pick = cands.and_then(|c| {
            let in_snippet = c.iter().position(|(m, used)| {
                !used
                    && m.range.start >= z.snippet_start
                    && (z.snippet_end <= z.snippet_start || m.range.start < z.snippet_end)
            });
            let i = in_snippet.or_else(|| c.iter().position(|(_, used)| !used))?;
            c[i].1 = true;
            Some(c[i].0)
        });
        outcomes.push(match pick {
            Some(m) => {
                let (category, ambiguous, unindexed) = classify(z, m);
                Outcome {
                    category,
                    zk: Some(z.clone()),
                    md: Some(m.clone()),
                    ambiguous,
                    unindexed,
                }
            }
            None => Outcome {
                category: Category::UnmatchedZk,
                zk: Some(z.clone()),
                md: None,
                ambiguous: false,
                unindexed: false,
            },
        });
    }
    for (m, used) in by_key.into_values().flatten() {
        if !used {
            let category = match m.kind {
                LinkKind::Image => Category::ImageNotInZk,
                _ => Category::UnmatchedMdroots,
            };
            outcomes.push(Outcome {
                category,
                zk: None,
                md: Some(m.clone()),
                ambiguous: m.status == LinkStatus::Ambiguous,
                unindexed: false,
            });
        }
    }
    Report { outcomes }
}
