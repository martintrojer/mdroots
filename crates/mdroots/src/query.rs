//! Note queries with combinable filters, in the shape of `zk list`: tag
//! expressions, dates, link-graph filters, sort and limit. The CLI's
//! `mdroots notes` is a thin layer over [`NoteQuery`].
//!
//! The syntax follows [zk](https://github.com/zk-org/zk) (its
//! [filtering docs](https://zk-org.github.io/zk/notes/note-filtering.html)):
//!
//! - Tags ([`TagExpr`]): `,` and ` AND ` are and; ` OR ` and `|` are or and
//!   bind tighter, so `a OR b, NOT c` is `(a or b) and not c`. A `NOT ` or
//!   `-` prefix negates; parentheses group (`(a OR b) AND NOT c`,
//!   `NOT (a, b)`). A bare negated tag cannot sit in an or group
//!   (`a OR NOT b`; zk refuses it too), but a parenthesised one can
//!   (`a OR (NOT b)`). `*` and `?` are globs. Names compare
//!   case-insensitively; a leading `#` is dropped. Keywords are upper case,
//!   so tags named `and`, `or` or `not` still work. A syntax error (an
//!   unclosed `(`, a dangling `AND`) names its column.
//! - Dates ([`parse_date`]): see there. Times without an offset are UTC
//!   (zk uses local time; mdroots has no time-zone database).
//! - Sort ([`parse_sort`]): `KEY[+|-]`, zk's keys and shortcuts. Without a
//!   sort, notes come by title A–Z, as zk's always-appended `title ASC`.
//!
//! Where mdroots differs from zk: `--related` is a filter here as in zk, so
//! the shared-neighbour score of [`Workspace::related`] is not the order;
//! titles sort case-insensitively (zk compares bytes).

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mdroots_core::memstore::counts;
use mdroots_core::{Cancel, ErrorKind};
use mdroots_resolve::ladder::LinkStatus;

use crate::{Error, Freshness, NoteSummary, Workspace};

// ---------------------------------------------------------------------------
// Tag expressions.

/// A parsed tag expression in zk's syntax: `a,b` or `a AND b` (and),
/// `a OR b` / `a|b` (or, binds tighter than and), `NOT a` / `-a` (not),
/// parentheses, globs (`year/201*`); names compare case-insensitively.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagExpr {
    root: Node,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    /// A lowercase glob pattern.
    Tag(String),
    Not(Box<Node>),
    And(Vec<Node>),
    Or(Vec<Node>),
}

impl TagExpr {
    /// Parse `s`; the error names the problem and its byte column.
    pub fn parse(s: &str) -> Result<TagExpr, String> {
        let toks = tokens(s);
        let mut p = Parser {
            toks: &toks,
            i: 0,
            end: s.len(),
        };
        let root = p.and()?;
        match p.peek() {
            None => Ok(TagExpr { root }),
            Some((col, Tok::Close)) => Err(format!("column {col}: unexpected )")),
            Some((col, _)) => Err(format!("column {col}: expected , AND or OR")),
        }
    }

    /// Whether a note with `tags` matches.
    pub fn matches(&self, tags: &[String]) -> bool {
        let lower: Vec<String> = tags.iter().map(|t| t.to_lowercase()).collect();
        eval(&self.root, &lower)
    }
}

fn eval(n: &Node, tags: &[String]) -> bool {
    match n {
        Node::Tag(g) => tags.iter().any(|t| glob(g, t)),
        Node::Not(x) => !eval(x, tags),
        Node::And(xs) => xs.iter().all(|x| eval(x, tags)),
        Node::Or(xs) => xs.iter().any(|x| eval(x, tags)),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    And,
    Or,
    Not,
    Open,
    Close,
    /// A tag name (a leading `#` dropped).
    Name(String),
}

/// Split `s` into `(byte column, token)`. Separators: `,`, `|`, `(`, `)`;
/// a leading `-` negates; `AND`, `OR` and `NOT` (upper case, between
/// spaces) are keywords. Anything else up to a separator or a keyword is a
/// tag name, inner spaces included (`a and b` is one tag).
fn tokens(s: &str) -> Vec<(usize, Tok)> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    // At the start of a term, where `-` and `NOT ` negate.
    let mut term_start = true;
    while i < b.len() {
        let c = b[i];
        if c == b' ' || c == b'\t' {
            i += 1;
            continue;
        }
        let kw = |k: &str| {
            s[i..].starts_with(k)
                && s[i + k.len()..]
                    .chars()
                    .next()
                    .is_none_or(|n| n.is_whitespace() || n == '(')
        };
        let tok = match c {
            b',' => Some((Tok::And, 1)),
            b'|' => Some((Tok::Or, 1)),
            b'(' => Some((Tok::Open, 1)),
            b')' => Some((Tok::Close, 1)),
            b'-' if term_start => Some((Tok::Not, 1)),
            _ if kw("AND") => Some((Tok::And, 3)),
            _ if kw("OR") => Some((Tok::Or, 2)),
            _ if term_start && kw("NOT") => Some((Tok::Not, 3)),
            _ => None,
        };
        if let Some((t, n)) = tok {
            term_start = !matches!(t, Tok::Close);
            out.push((i, t));
            i += n;
            continue;
        }
        // A name: up to a separator or a keyword that follows a space.
        let start = i;
        let mut j = i;
        while j < b.len() {
            if matches!(b[j], b',' | b'|' | b'(' | b')') {
                break;
            }
            if b[j] == b' ' || b[j] == b'\t' {
                let rest = s[j..].trim_start();
                let next_kw = ["AND", "OR"].iter().any(|k| {
                    rest.starts_with(k)
                        && rest[k.len()..]
                            .chars()
                            .next()
                            .is_none_or(|n| n.is_whitespace() || n == '(')
                });
                if next_kw {
                    break;
                }
            }
            j += s[j..].chars().next().map_or(1, char::len_utf8);
        }
        let raw = s[start..j].trim_end();
        let name = raw.strip_prefix('#').unwrap_or(raw);
        let col = if raw.starts_with('#') {
            start + 1
        } else {
            start
        };
        out.push((col, Tok::Name(name.to_owned())));
        term_start = false;
        i = j;
    }
    out
}

/// Precedence, loosest first: and (`,`, `AND`), or (`OR`, `|`), not
/// (`NOT`, `-`), then a name or a parenthesised expression.
struct Parser<'a> {
    toks: &'a [(usize, Tok)],
    i: usize,
    /// The expression's length: the column of "end of input".
    end: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<(usize, &Tok)> {
        self.toks.get(self.i).map(|(c, t)| (*c, t))
    }

    fn col(&self) -> usize {
        self.peek().map_or(self.end, |(c, _)| c)
    }

    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek().is_some_and(|(_, x)| x == t) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn and(&mut self) -> Result<Node, String> {
        let mut xs = vec![self.or()?];
        while self.eat(&Tok::And) {
            xs.push(self.or()?);
        }
        Ok(if xs.len() == 1 {
            xs.remove(0)
        } else {
            Node::And(xs)
        })
    }

    fn or(&mut self) -> Result<Node, String> {
        let mut xs = vec![self.or_term()?];
        while self.eat(&Tok::Or) {
            xs.push(self.or_term()?);
        }
        if xs.len() == 1 {
            return Ok(xs.remove(0).1);
        }
        // zk refuses a bare negated tag in an or group (`a OR NOT b`);
        // parentheses make the intent explicit: `a OR (NOT b)`.
        if let Some((Some(col), _)) = xs.iter().find(|(bare_not, _)| bare_not.is_some()) {
            return Err(format!("column {col}: cannot negate a tag in an OR group"));
        }
        Ok(Node::Or(xs.into_iter().map(|(_, x)| x).collect()))
    }

    /// One operand of an or group, with the column of its tag when it is a
    /// bare `NOT tag` / `-tag` (not parenthesised).
    fn or_term(&mut self) -> Result<(Option<usize>, Node), String> {
        let bare_not = matches!(self.peek(), Some((_, Tok::Not)));
        let x = self.not()?;
        let tag_col = self.toks[..self.i]
            .last()
            .filter(|(_, t)| matches!(t, Tok::Name(_)))
            .map(|(c, _)| *c);
        let bare = bare_not && matches!(&x, Node::Not(inner) if matches!(**inner, Node::Tag(_)));
        Ok((if bare { tag_col } else { None }, x))
    }

    fn not(&mut self) -> Result<Node, String> {
        if self.eat(&Tok::Not) {
            return Ok(Node::Not(Box::new(self.not()?)));
        }
        self.atom()
    }

    fn atom(&mut self) -> Result<Node, String> {
        let col = self.col();
        match self.peek() {
            Some((_, Tok::Name(n))) if !n.is_empty() => {
                let n = n.to_lowercase();
                self.i += 1;
                Ok(Node::Tag(n))
            }
            Some((_, Tok::Open)) => {
                self.i += 1;
                let x = self.and()?;
                if !self.eat(&Tok::Close) {
                    return Err(format!("column {}: expected )", self.col()));
                }
                Ok(x)
            }
            _ => Err(format!("column {col}: expected a tag")),
        }
    }
}

/// Whether `text` matches `pat` (`*` any run, `?` one character).
fn glob(pat: &str, text: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        match p.get(pi) {
            Some('*') => {
                star = Some((pi, ti));
                pi += 1;
            }
            Some(&c) if c == '?' || c == t[ti] => {
                pi += 1;
                ti += 1;
            }
            _ => match star {
                Some((sp, st)) => {
                    pi = sp + 1;
                    ti = st + 1;
                    star = Some((sp, st + 1));
                }
                None => return false,
            },
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

// ---------------------------------------------------------------------------
// Sort.

/// A sort key and direction (`KEY[+|-]`, zk's defaults: dates newest first,
/// path and title A–Z).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SortKey {
    Title,
    Path,
    Created,
    Modified,
}

impl SortKey {
    /// zk's intrinsic direction: `true` (ascending) for title and path.
    pub fn default_ascending(self) -> bool {
        matches!(self, SortKey::Title | SortKey::Path)
    }
}

/// Parse zk's `KEY[+|-]`: `title`/`t`, `path`/`p`, `created`/`c`,
/// `modified`/`m`; `+` ascending, `-` descending, none: the key's default.
pub fn parse_sort(s: &str) -> Result<(SortKey, bool), String> {
    let s = s.trim();
    let (name, dir) = match s.strip_suffix('+') {
        Some(n) => (n, Some(true)),
        None => match s.strip_suffix('-') {
            Some(n) => (n, Some(false)),
            None => (s, None),
        },
    };
    let key = match name.to_lowercase().as_str() {
        "title" | "t" => SortKey::Title,
        "path" | "p" => SortKey::Path,
        "created" | "c" => SortKey::Created,
        "modified" | "m" => SortKey::Modified,
        _ => return Err(format!("unknown sort key: {name}")),
    };
    Ok((key, dir.unwrap_or_else(|| key.default_ascending())))
}

// ---------------------------------------------------------------------------
// The query.

/// What [`Workspace::query`] returns notes for. All filters combine (and).
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct NoteQuery {
    /// Only notes under these paths (files or directories).
    pub paths: Vec<PathBuf>,
    /// Never notes under these paths.
    pub exclude: Vec<PathBuf>,
    pub tag: Vec<TagExpr>,
    pub tagless: bool,
    /// Full-text match (all words, as `full_text`).
    pub matching: Option<String>,
    pub created_after: Option<SystemTime>,
    pub created_before: Option<SystemTime>,
    pub modified_after: Option<SystemTime>,
    pub modified_before: Option<SystemTime>,
    pub orphan: bool,
    pub missing_backlink: bool,
    /// Notes linking to every one of these.
    pub link_to: Vec<PathBuf>,
    /// Notes linked from every one of these.
    pub linked_by: Vec<PathBuf>,
    pub related: Vec<PathBuf>,
    /// `None`: title A–Z. `bool`: ascending.
    pub sort: Option<(SortKey, bool)>,
    pub limit: Option<usize>,
}

impl NoteQuery {
    fn uses_graph(&self) -> bool {
        self.orphan
            || self.missing_backlink
            || !self.link_to.is_empty()
            || !self.linked_by.is_empty()
            || !self.related.is_empty()
    }
}

impl Workspace {
    /// The notes matching `q`, sorted and limited as it says. Graph filters
    /// on a lazy root are an error (the answer would be partial).
    ///
    /// Date bounds: `*_after` is inclusive, `*_before` exclusive; a note
    /// without the time never matches a bound on it. The created time is the
    /// frontmatter `date` (else `created`) value, else the file's creation
    /// time when known.
    pub fn query(&self, q: &NoteQuery) -> Result<Vec<NoteSummary>, Error> {
        if q.uses_graph() && self.freshness() == Freshness::Lazy {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "link-graph filters need a full index (this root is lazy)",
            ));
        }
        let include = self.prefixes(&q.paths);
        let exclude = self.prefixes(&q.exclude);
        let matched = match &q.matching {
            Some(m) => Some(self.matching_paths(m)?),
            None => None,
        };
        let orphans = match q.orphan {
            true => Some(paths_of(self.orphans()?)),
            false => None,
        };
        let missing = match q.missing_backlink {
            true => Some(
                self.missing_backlinks()?
                    .into_iter()
                    .map(|(_, to)| to)
                    .collect::<BTreeSet<_>>(),
            ),
            false => None,
        };
        let link_to = match q.link_to.is_empty() {
            true => None,
            false => Some(self.linking_to(&q.link_to)?),
        };
        let linked_by = match q.linked_by.is_empty() {
            true => None,
            false => Some(self.linked_from(&q.linked_by)?),
        };
        let related = match q.related.is_empty() {
            true => None,
            false => {
                let mut all: Option<BTreeSet<PathBuf>> = None;
                for p in &q.related {
                    let set: BTreeSet<PathBuf> =
                        self.related(p)?.into_iter().map(|(n, _)| n.path).collect();
                    all = Some(match all {
                        Some(a) => a.intersection(&set).cloned().collect(),
                        None => set,
                    });
                }
                all
            }
        };
        let within =
            |set: &Option<BTreeSet<PathBuf>>, p: &Path| set.as_ref().is_none_or(|s| s.contains(p));
        let in_range =
            |t: Option<SystemTime>, after: Option<SystemTime>, before: Option<SystemTime>| {
                if after.is_none() && before.is_none() {
                    return true;
                }
                t.is_some_and(|t| after.is_none_or(|a| t >= a) && before.is_none_or(|b| t < b))
            };

        let mut rows: Vec<(NoteSummary, Option<SystemTime>)> = Vec::new();
        for n in self.notes() {
            let p = n.path.as_path();
            if (!include.is_empty() && !include.iter().any(|i| p.starts_with(i)))
                || exclude.iter().any(|x| p.starts_with(x))
                || (q.tagless && !n.tags.is_empty())
                || !q.tag.iter().all(|e| e.matches(&n.tags))
                || !within(&matched, p)
                || !within(&orphans, p)
                || !within(&missing, p)
                || !within(&link_to, p)
                || !within(&linked_by, p)
                || !within(&related, p)
                || !in_range(n.modified, q.modified_after, q.modified_before)
            {
                continue;
            }
            let created = n.created;
            if !in_range(created, q.created_after, q.created_before) {
                continue;
            }
            rows.push((n, created));
        }

        let (key, asc) = q.sort.unwrap_or((SortKey::Title, true));
        rows.sort_by(|(a, ac), (b, bc)| {
            let primary = match key {
                SortKey::Title => directed(by_title(a, b), asc),
                SortKey::Path => directed(a.path.cmp(&b.path), asc),
                SortKey::Created => by_time(*ac, *bc, asc),
                SortKey::Modified => by_time(a.modified, b.modified, asc),
            };
            primary
                .then_with(|| by_title(a, b))
                .then_with(|| a.path.cmp(&b.path))
        });
        let mut out: Vec<NoteSummary> = rows.into_iter().map(|(n, _)| n).collect();
        if let Some(l) = q.limit {
            out.truncate(l);
        }
        Ok(out)
    }

    /// `paths` canonical where they exist (as given otherwise).
    fn prefixes(&self, paths: &[PathBuf]) -> Vec<PathBuf> {
        paths
            .iter()
            .map(|p| self.fs().canonicalize(p).unwrap_or_else(|_| p.clone()))
            .collect()
    }

    fn matching_paths(&self, m: &str) -> Result<BTreeSet<PathBuf>, Error> {
        let hits = self.full_text(m, usize::MAX, &Cancel::new())?;
        Ok(hits.into_iter().map(|h| h.path).collect())
    }

    /// Indexed notes with a counted link to every one of `targets`
    /// (self-links excluded): repeated filters combine like the others.
    fn linking_to(&self, targets: &[PathBuf]) -> Result<BTreeSet<PathBuf>, Error> {
        let mut all: Option<BTreeSet<PathBuf>> = None;
        for t in targets {
            let rel = self.rel(t)?;
            let store = self.store();
            let set: BTreeSet<PathBuf> = store
                .backlinks(&rel)
                .into_iter()
                .filter(|(from, _)| *from != rel)
                .map(|(from, _)| self.abs(&from))
                .collect();
            all = Some(match all {
                Some(a) => a.intersection(&set).cloned().collect(),
                None => set,
            });
        }
        Ok(all.unwrap_or_default())
    }

    /// Indexed notes every one of `sources` links to with a counted link (every
    /// candidate of an ambiguous link; self-links excluded).
    fn linked_from(&self, sources: &[PathBuf]) -> Result<BTreeSet<PathBuf>, Error> {
        let mut all: Option<BTreeSet<PathBuf>> = None;
        for s in sources {
            let mut out = BTreeSet::new();
            let rel = self.rel(s)?;
            let store = self.store();
            for (l, r) in store.links(&rel) {
                if !counts(&l) || !matches!(r.status, LinkStatus::Resolved | LinkStatus::Ambiguous)
                {
                    continue;
                }
                for t in r.targets {
                    if t != rel && store.document(&t).is_some() {
                        out.insert(self.abs(&t));
                    }
                }
            }
            all = Some(match all {
                Some(a) => a.intersection(&out).cloned().collect(),
                None => out,
            });
        }
        Ok(all.unwrap_or_default())
    }
}

fn paths_of(v: Vec<NoteSummary>) -> BTreeSet<PathBuf> {
    v.into_iter().map(|n| n.path).collect()
}

fn directed(o: Ordering, asc: bool) -> Ordering {
    if asc { o } else { o.reverse() }
}

fn by_title(a: &NoteSummary, b: &NoteSummary) -> Ordering {
    a.title
        .to_lowercase()
        .cmp(&b.title.to_lowercase())
        .then_with(|| a.title.cmp(&b.title))
}

/// Unknown times last in either direction.
fn by_time(a: Option<SystemTime>, b: Option<SystemTime>, asc: bool) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => directed(a.cmp(&b), asc),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

// ---------------------------------------------------------------------------
// Dates.

const DAY: i64 = 86_400;

/// Parse a date filter value: `YYYY-MM-DD`, an RFC 3339 time, or zk's
/// relative forms (`today`, `yesterday`, `last week`, `2 days ago`, …).
///
/// Absolute forms: `YYYY`, `YYYY-MM`, `YYYY-MM-DD`,
/// `YYYY-MM-DD[T ]HH:MM[:SS[.frac]]`, optionally ending in `Z` or `±HH:MM`
/// (RFC 3339). Without an offset the time is UTC. Relative forms (case
/// insensitive): `now`; `today` and `yesterday` (start of the day);
/// `N UNIT[s] ago`, `last N UNITs`, `last UNIT` with UNIT one of minute,
/// hour, day, week, month, year and N digits or `a`/`an`/`one`…`twelve`
/// (`last two weeks`); `Nd`/`Nw` (`7d`); `last WEEKDAY` (the start of the
/// latest such day before today). A time too far back to represent is an
/// error.
pub fn parse_date(s: &str, now: SystemTime) -> Result<SystemTime, String> {
    let t = s.trim();
    parse_absolute(t)
        .or_else(|| parse_relative(&t.to_lowercase(), now))
        .ok_or_else(|| format!("unrecognised date: {s:?}"))
}

/// The day `s` names (see [`parse_date`]) as a `[start, end)` range of one
/// UTC day, for zk's `--created DAY` / `--modified DAY`.
pub fn day_range(s: &str, now: SystemTime) -> Result<(SystemTime, SystemTime), String> {
    let bad = || format!("unrecognised date: {s:?}");
    let t = to_secs(parse_date(s, now)?);
    let start = t.checked_sub(t.rem_euclid(DAY)).ok_or_else(bad)?;
    let end = start.checked_add(DAY).ok_or_else(bad)?;
    Ok((
        from_secs(start, 0).ok_or_else(bad)?,
        from_secs(end, 0).ok_or_else(bad)?,
    ))
}

/// `t` as RFC 3339 in UTC, rounded down to whole seconds:
/// `2024-02-29T13:05:00Z`. For years 0000–9999 [`parse_date`] reads it
/// back; other years, which RFC 3339 cannot write, come out with more
/// digits or a sign (`10000-01-01T00:00:00Z`, `-001-12-31T00:00:00Z`)
/// and do not parse.
pub fn format_rfc3339(t: SystemTime) -> String {
    let secs = to_secs(t);
    let (y, m, d) = from_days(secs.div_euclid(DAY));
    let tod = secs.rem_euclid(DAY);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        tod / 3600,
        tod % 3600 / 60,
        tod % 60
    )
}

/// The absolute forms of [`parse_date`], untrimmed; also the frontmatter
/// date parser.
pub(crate) fn parse_absolute(s: &str) -> Option<SystemTime> {
    let b = s.as_bytes();
    let year = num(b, 0, 4)?;
    if b.len() == 4 {
        return from_secs(civil(year, 1, 1)? * DAY, 0);
    }
    if b.get(4) != Some(&b'-') {
        return None;
    }
    let month = num(b, 5, 2)?;
    if b.len() == 7 {
        return from_secs(civil(year, month, 1)? * DAY, 0);
    }
    if b.get(7) != Some(&b'-') {
        return None;
    }
    let day = num(b, 8, 2)?;
    let days = civil(year, month, day)?;
    if b.len() == 10 {
        return from_secs(days * DAY, 0);
    }
    if !matches!(b.get(10), Some(b'T' | b't' | b' ')) || b.get(13) != Some(&b':') {
        return None;
    }
    let (h, m) = (num(b, 11, 2)?, num(b, 14, 2)?);
    let mut i = 16;
    let mut sec = 0;
    let mut nanos = 0u32;
    if b.get(i) == Some(&b':') {
        sec = num(b, i + 1, 2)?;
        i += 3;
        if b.get(i) == Some(&b'.') {
            let start = i + 1;
            i = start;
            while b.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
            if i == start {
                return None;
            }
            let frac = &s[start..i.min(start + 9)];
            nanos = frac.parse::<u32>().ok()? * 10u32.pow(9 - frac.len() as u32);
        }
    }
    if h > 23 || m > 59 || sec > 60 {
        return None;
    }
    let offset = match &b[i..] {
        [] | [b'Z' | b'z'] => 0,
        [sign @ (b'+' | b'-'), _, _, b':', _, _] => {
            let (oh, om) = (num(b, i + 1, 2)?, num(b, i + 4, 2)?);
            if oh > 23 || om > 59 {
                return None;
            }
            let o = oh * 3600 + om * 60;
            if *sign == b'+' { o } else { -o }
        }
        _ => return None,
    };
    from_secs(days * DAY + h * 3600 + m * 60 + sec - offset, nanos)
}

fn parse_relative(s: &str, now: SystemTime) -> Option<SystemTime> {
    let now_s = to_secs(now);
    // The start of the day `back` days before today. Checked: `now` may
    // be any representable time.
    let day = now_s.div_euclid(DAY);
    let start = |back: i64| from_secs(day.checked_sub(back)?.checked_mul(DAY)?, 0);
    let words: Vec<&str> = s.split_whitespace().collect();
    match words.as_slice() {
        ["now"] => return Some(now),
        ["today"] => return start(0),
        ["yesterday"] => return start(1),
        ["last", w] if weekday(w).is_some() => {
            let want = weekday(w)?;
            let cur = (day.rem_euclid(7) + 4) % 7; // 1970-01-01: Thursday
            let back = match (cur - want).rem_euclid(7) {
                0 => 7,
                n => n,
            };
            return start(back);
        }
        _ => {}
    }
    let (n, unit) = match words.as_slice() {
        [n, unit, "ago"] => (count(n)?, *unit),
        ["last", n, unit] => (count(n)?, *unit),
        ["last", unit] => (1, *unit),
        [w] => {
            let (digits, unit) = match w.strip_suffix('d') {
                Some(d) => (d, "day"),
                None => (w.strip_suffix('w')?, "week"),
            };
            (digits.parse().ok()?, unit)
        }
        _ => return None,
    };
    let unit = unit.strip_suffix('s').unwrap_or(unit);
    let secs = match unit {
        "minute" => 60,
        "hour" => 3600,
        "day" => DAY,
        "week" => 7 * DAY,
        "month" => return months_back(now_s, n, now),
        "year" => return months_back(now_s, n.checked_mul(12)?, now),
        _ => return None,
    };
    from_secs(now_s.checked_sub(n.checked_mul(secs)?)?, sub_nanos(now))
}

/// `now` moved back `n` calendar months, the day clamped to the month.
fn months_back(now_s: i64, n: i64, now: SystemTime) -> Option<SystemTime> {
    let days = now_s.div_euclid(DAY);
    let tod = now_s.rem_euclid(DAY);
    let (y, m, d) = from_days(days);
    let total = (y * 12 + (m - 1)).checked_sub(n)?;
    let (y, m) = (total.div_euclid(12), total.rem_euclid(12) + 1);
    let d = d.min(month_len(y, m));
    from_secs(
        civil(y, m, d)?.checked_mul(DAY)?.checked_add(tod)?,
        sub_nanos(now),
    )
}

fn weekday(w: &str) -> Option<i64> {
    let days = [
        "sunday",
        "monday",
        "tuesday",
        "wednesday",
        "thursday",
        "friday",
        "saturday",
    ];
    days.iter().position(|d| *d == w).map(|i| i as i64)
}

fn count(w: &str) -> Option<i64> {
    let words = [
        "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven",
        "twelve",
    ];
    match w {
        "a" | "an" => Some(1),
        _ => words
            .iter()
            .position(|x| *x == w)
            .map(|i| i as i64 + 1)
            .or_else(|| w.parse().ok()),
    }
}

/// The `len` ASCII digits at `at`.
fn num(b: &[u8], at: usize, len: usize) -> Option<i64> {
    let d = b.get(at..at + len)?;
    d.iter()
        .all(u8::is_ascii_digit)
        .then(|| d.iter().fold(0i64, |acc, c| acc * 10 + i64::from(c - b'0')))
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn month_len(y: i64, m: i64) -> i64 {
    match m {
        2 if is_leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 of a valid civil date (proleptic Gregorian;
/// Howard Hinnant's `days_from_civil`), `None` if it overflows.
fn civil(y: i64, m: i64, d: i64) -> Option<i64> {
    if !(1..=12).contains(&m) || d < 1 || d > month_len(y, m) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era.checked_mul(146_097)?.checked_add(doe - 719_468)
}

/// The civil date of a day count since 1970-01-01 (the inverse of
/// [`civil`]).
fn from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// `secs` seconds plus `nanos` after the epoch, if `SystemTime` can hold
/// it.
fn from_secs(secs: i64, nanos: u32) -> Option<SystemTime> {
    let base = if secs >= 0 {
        UNIX_EPOCH.checked_add(Duration::from_secs(secs.unsigned_abs()))
    } else {
        UNIX_EPOCH.checked_sub(Duration::from_secs(secs.unsigned_abs()))
    };
    base?.checked_add(Duration::from_nanos(u64::from(nanos)))
}

/// Whole seconds since the epoch, rounded down.
fn to_secs(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(e) => {
            let d = e.duration();
            let s = -i128::from(d.as_secs()) - i128::from(d.subsec_nanos() > 0);
            i64::try_from(s).unwrap_or(i64::MIN)
        }
    }
}

fn sub_nanos(t: SystemTime) -> u32 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.subsec_nanos(),
        Err(e) => (1_000_000_000 - e.duration().subsec_nanos()) % 1_000_000_000,
    }
}
