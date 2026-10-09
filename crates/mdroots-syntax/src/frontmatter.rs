//! Frontmatter blocks: parsing and the standard-key accessors
//! (docs/specs/index.md §Frontmatter).
//!
//! YAML and TOML follow ramble's approach (ported from ramble, MIT, same
//! author, commit 5f6e7ed2b798): a line scan supplies keys in
//! source order, the source text of each value, duplicate keys and
//! per-entry line ranges; the real parser (`saphyr`, `toml`) only decides
//! each value's shape. When the parser rejects the block the scan alone is
//! used and `Frontmatter::error` says why. Unfenced headers (Logseq,
//! MultiMarkdown, JSON) are read by small line or byte scanners.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use crate::model::{
    Confidence, Context, Element, Frontmatter, FrontmatterFormat, Link, LinkKind, Tag, TagSyntax,
    Value,
};

/// Parse the frontmatter block at `range` (fences included, for YAML and
/// TOML) into its entries and the links and tags its values carry.
pub(crate) fn parse_block(
    src: &str,
    range: Range<usize>,
    format: FrontmatterFormat,
) -> (Frontmatter, Vec<Element>) {
    let range = clamp(src, range);
    let mut entries: Vec<Entry> = Vec::new();
    let error = match format {
        FrontmatterFormat::Yaml | FrontmatterFormat::Toml => {
            let kind = if format == FrontmatterFormat::Toml {
                FmKind::Toml
            } else {
                FmKind::Yaml
            };
            let inner = body(src, &range);
            let parsed = parse(&src[inner.clone()], kind);
            let mut ctx = Flatten {
                src,
                inner: inner.clone(),
                kind,
                out: &mut entries,
            };
            ctx.run(&[], parsed.entries);
            parsed.error.or_else(|| {
                (!parsed.parsed).then(|| "no `key: value` entries in the block".to_owned())
            })
        }
        FrontmatterFormat::Json => json::entries(src, range.clone(), &mut entries),
        FrontmatterFormat::Logseq => {
            line_entries(src, range.clone(), logseq_line, &mut entries);
            None
        }
        FrontmatterFormat::MultiMarkdown => {
            line_entries(src, range.clone(), mmd_line, &mut entries);
            None
        }
        _ => {
            line_entries(src, range.clone(), org_line, &mut entries);
            None
        }
    };
    let fm = Frontmatter::from_entries(
        format,
        range,
        entries
            .iter()
            .map(|e| (e.key.clone(), e.value.clone()))
            .collect(),
        error,
    );
    let elements = elements(src, &fm, &entries);
    (fm, elements)
}

/// Find an unfenced header at the top of `src` (after a BOM): Logseq
/// `key:: value` lines, a JSON object closed by a `}` line, or a
/// MultiMarkdown `Key: value` block followed by a blank line. The range
/// ends before the last line's newline, like a fenced block.
pub(crate) fn detect_unfenced(src: &str) -> Option<(Range<usize>, FrontmatterFormat)> {
    let base = if src.starts_with('\u{feff}') { 3 } else { 0 };
    let text = &src[base..];
    let lines = lines(text);
    let shift = |end: usize| Some(base..base + end);
    let run = |f: fn(&str) -> Option<(&str, &str)>| {
        lines
            .iter()
            .take_while(|(_, l)| f(l).is_some())
            .last()
            .map(|(at, l)| at + l.len())
    };
    if let Some(end) = run(logseq_line) {
        return Some((shift(end)?, FrontmatterFormat::Logseq));
    }
    if text.starts_with('{') {
        let close = lines.iter().find(|(_, l)| l.trim_end() == "}");
        return close.and_then(|(at, l)| Some((shift(at + l.len())?, FrontmatterFormat::Json)));
    }
    let end = run(mmd_line)?;
    let blank_follows = lines
        .iter()
        .find(|(at, _)| *at > end)
        .is_some_and(|(_, l)| l.trim().is_empty());
    blank_follows
        .then(|| shift(end))
        .flatten()
        .map(|r| (r, FrontmatterFormat::MultiMarkdown))
}

// ---------------------------------------------------------------------------
// Standard keys (§4.2).

const TITLE: &[&str] = &["title", "linkTitle"];
const ALIASES: &[&str] = &["aliases", "alias"];
const ID: &[&str] = &["id", "uid", "zettel-id"];
const TAGS: &[&str] = &["tags", "tag", "keywords", "categories", "filetags"];
const SUMMARY: &[&str] = &["description", "summary", "abstract", "excerpt"];
const DATES: &[&str] = &[
    "date", "created", "updated", "modified", "lastmod", "week", "year",
];
const SITE_PATH: &[&str] = &["slug", "permalink", "url"];

/// `—`, `n/a`, `TBD`, an empty string and the like: no value (§4.1).
/// `{…}` (a map item of a list, whose children are not kept) is no value
/// either.
pub(crate) fn is_placeholder(s: &str) -> bool {
    let s = s.trim();
    matches!(s, "" | "—" | "–" | "-" | "~" | "{…}")
        || ["n/a", "tbd", "none", "null"]
            .iter()
            .any(|p| s.eq_ignore_ascii_case(p))
}

fn absent(v: &Value) -> bool {
    match v {
        Value::Str(s) => is_placeholder(s),
        Value::List(items) => items.iter().all(|i| is_placeholder(i)),
        _ => true,
    }
}

fn matches(key: &str, names: &[&str]) -> bool {
    names.iter().any(|n| key.eq_ignore_ascii_case(n))
}

/// Index of the entry that carries the meaning `names`: an exact standard
/// name (in `names` order) wins, else the first case-insensitive match.
/// Placeholder values count as absent.
fn standard(entries: &[(String, Value)], names: &[&str]) -> Option<usize> {
    let present = |i: &usize| !absent(&entries[*i].1);
    names
        .iter()
        .find_map(|n| {
            (0..entries.len())
                .filter(present)
                .find(|&i| entries[i].0 == *n)
        })
        .or_else(|| {
            (0..entries.len())
                .filter(present)
                .find(|&i| matches(&entries[i].0, names))
        })
}

/// A value's first non-placeholder string.
fn first_str(v: &Value) -> Option<&str> {
    match v {
        Value::Str(s) => Some(s.trim()),
        Value::List(items) => items.iter().map(|s| s.trim()).find(|s| !is_placeholder(s)),
        _ => None,
    }
    .filter(|s| !is_placeholder(s))
}

/// Tag names in a value, as slices of it: list items or a comma, space or
/// org `:a:b:` separated string; a leading `#` stripped.
fn tag_names(v: &Value) -> Vec<&str> {
    let items: Vec<&str> = match v {
        Value::Str(s) => s.split([',', ' ', '\t']).collect(),
        Value::List(items) => items.iter().map(String::as_str).collect(),
        _ => Vec::new(),
    };
    items
        .into_iter()
        .flat_map(|t| {
            let t = t.trim();
            let colon = t.len() > 1 && t.starts_with(':') && t.ends_with(':');
            let parts: Vec<&str> = if colon {
                t.split(':').collect()
            } else {
                vec![t]
            };
            parts
        })
        .map(|t| t.strip_prefix('#').unwrap_or(t).trim())
        .filter(|t| !is_placeholder(t))
        .collect()
}

impl Frontmatter {
    fn standard_value(&self, names: &[&str]) -> Option<&Value> {
        standard(self.entries(), names).map(|i| &self.entries()[i].1)
    }

    /// From `title` or `linkTitle` (org `#+TITLE` arrives as `title`).
    pub fn title(&self) -> Option<&str> {
        self.standard_value(TITLE).and_then(first_str)
    }

    /// From `aliases` or `alias`: a list or a comma-separated string.
    pub fn aliases(&self) -> Vec<&str> {
        let items: Vec<&str> = match self.standard_value(ALIASES) {
            Some(Value::Str(s)) => s.split(',').collect(),
            Some(Value::List(items)) => items.iter().map(String::as_str).collect(),
            _ => Vec::new(),
        };
        items
            .into_iter()
            .map(str::trim)
            .filter(|s| !is_placeholder(s))
            .collect()
    }

    /// From `id`, `uid` or `zettel-id`.
    pub fn id(&self) -> Option<&str> {
        self.standard_value(ID).and_then(first_str)
    }

    /// From `tags`, `tag`, `keywords`, `categories` or org `filetags`;
    /// leading `#` stripped.
    pub fn tags(&self) -> Vec<&str> {
        self.standard_value(TAGS).map(tag_names).unwrap_or_default()
    }

    /// From `description`, `summary`, `abstract` or `excerpt`.
    pub fn summary(&self) -> Option<&str> {
        self.standard_value(SUMMARY).and_then(first_str)
    }

    /// Every `date`, `created`, `updated`, `modified`, `lastmod`, `week`
    /// and `year` entry
    /// as `(key, value)`, as written and in document order.
    pub fn dates(&self) -> Vec<(&str, &str)> {
        self.entries()
            .iter()
            .filter(|(k, _)| matches(k, DATES))
            .filter_map(|(k, v)| Some((k.as_str(), first_str(v)?)))
            .collect()
    }

    /// From `slug`, `permalink` or `url`.
    pub fn site_path(&self) -> Option<&str> {
        self.standard_value(SITE_PATH).and_then(first_str)
    }
}

// ---------------------------------------------------------------------------
// Links and tags from values (§4.1 comma-joined lists, §4.2 relations).

/// A flattened entry and the byte range in the document of its lines.
#[derive(Debug, Clone)]
struct Entry {
    key: String,
    value: Value,
    src: Range<usize>,
    /// False when the value's bytes could not be found: no links from it.
    located: bool,
}

const EXTENSIONS: &[&str] = &[
    "md", "markdown", "org", "png", "jpg", "jpeg", "gif", "svg", "pdf", "txt", "html", "htm",
];

fn elements(src: &str, fm: &Frontmatter, entries: &[Entry]) -> Vec<Element> {
    let mut out = Vec::new();
    let mut group = 0u32;
    let tag_entry = standard(fm.entries(), TAGS);
    for (i, e) in entries.iter().enumerate() {
        let span = clamp(src, e.src.clone());
        let from = if e.located {
            value_start(src, &span)
        } else {
            span.start
        };
        if matches(&e.key, TAGS) {
            if tag_entry == Some(i) {
                tags(src, &span, from, &e.value, &mut out);
            }
            continue;
        }
        let strings: Vec<&str> = match &e.value {
            Value::Str(s) => vec![s.as_str()],
            Value::List(items) => items.iter().map(String::as_str).collect(),
            _ => continue,
        };
        if !e.located {
            continue;
        }
        if strings.iter().any(|s| s.contains("[[")) {
            wiki_links(src, from..span.end, &mut out);
            continue;
        }
        let mut cursor = from;
        for s in strings {
            let s = s.trim();
            if is_placeholder(s) {
                continue;
            }
            if s.contains("://") || s.starts_with("mailto:") {
                if !s.chars().any(char::is_whitespace) {
                    let Some(r) = locate(src, cursor..span.end, s) else {
                        continue;
                    };
                    cursor = r.end.max(cursor);
                    let mut l =
                        Link::new(LinkKind::Url, Context::Frontmatter, Confidence::External, s);
                    l.range = r.clone();
                    l.text_range = r;
                    out.push(Element::Link(l));
                }
                continue;
            }
            let Some(parts) = path_parts(s) else { continue };
            let shared = (parts.len() > 1).then(|| {
                group += 1;
                group - 1
            });
            let mut found = Vec::new();
            for part in parts.into_iter().filter(|p| !is_placeholder(p)) {
                let Some(r) = locate(src, cursor..span.end, part) else {
                    // One part not in the source: no links from this value.
                    found.clear();
                    break;
                };
                cursor = r.end.max(cursor);
                let mut l = Link::new(
                    LinkKind::BarePath,
                    Context::Frontmatter,
                    Confidence::Implicit,
                    part,
                );
                l.range = r.clone();
                l.text_range = r;
                l.group = shared;
                found.push(Element::Link(l));
            }
            out.extend(found);
        }
    }
    out
}

/// The parts of a path-like value: split on `, `, each non-placeholder
/// part without whitespace, containing `/` or ending in a known extension.
fn path_parts(s: &str) -> Option<Vec<&str>> {
    let parts: Vec<&str> = s.split(", ").map(str::trim).collect();
    let real: Vec<&&str> = parts.iter().filter(|p| !is_placeholder(p)).collect();
    let pathy = |p: &str| {
        !p.chars().any(char::is_whitespace)
            && (p.contains('/')
                || p.rsplit_once('.').is_some_and(|(stem, ext)| {
                    !stem.is_empty() && EXTENSIONS.iter().any(|e| ext.eq_ignore_ascii_case(e))
                }))
    };
    (!real.is_empty() && real.iter().all(|p| pathy(p))).then_some(parts)
}

fn tags(src: &str, span: &Range<usize>, from: usize, v: &Value, out: &mut Vec<Element>) {
    let line_end = src[span.clone()]
        .find(['\r', '\n'])
        .map_or(span.end, |i| span.start + i);
    let mut cursor = from;
    for name in tag_names(v) {
        let range = locate(src, cursor..span.end, name).unwrap_or(span.start..line_end);
        cursor = range.end.max(cursor);
        out.push(Element::Tag(Tag {
            name: name.to_owned(),
            syntax: TagSyntax::Frontmatter,
            range,
        }));
    }
}

/// `[[target]]` / `[[target|label]]` on one line within `within`.
fn wiki_links(src: &str, within: Range<usize>, out: &mut Vec<Element>) {
    let text = &src[within.clone()];
    let mut at = 0;
    while let Some(open) = text[at..].find("[[").map(|i| at + i) {
        let inner = open + 2;
        let Some(close) = text[inner..].find("]]").map(|i| inner + i) else {
            break;
        };
        let content = &text[inner..close];
        at = close + 2;
        if content.trim().is_empty() || content.contains(['\n', '[']) {
            at = inner;
            continue;
        }
        let (target, label) = match content.split_once('|') {
            Some((t, l)) => (t, Some(l)),
            None => (content, None),
        };
        let mut l = Link::new(
            LinkKind::Wiki,
            Context::Frontmatter,
            Confidence::Explicit,
            target,
        );
        let base = within.start;
        l.range = base + open..base + close + 2;
        l.text_range = match label {
            Some(lb) => base + close - lb.len()..base + close,
            None => base + inner..base + close,
        };
        l.label = label.map(str::to_owned);
        out.push(Element::Link(l));
    }
}

/// Where the value starts in an entry's span: just past its first `:` or
/// `=` (the key separator), else the span start.
fn value_start(src: &str, span: &Range<usize>) -> usize {
    src[span.clone()]
        .find([':', '='])
        .map_or(span.start, |i| span.start + i + 1)
}

/// The first occurrence of `needle` in `src[within]` not glued to a word
/// character on either side, else the first occurrence at all.
fn locate(src: &str, within: Range<usize>, needle: &str) -> Option<Range<usize>> {
    let within = clamp(src, within);
    let hay = &src[within.clone()];
    if needle.is_empty() {
        return None;
    }
    let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '-'));
    let mut first = None;
    for (i, _) in hay.match_indices(needle) {
        first.get_or_insert(i);
        let before = hay[..i].chars().next_back();
        let after = hay[i + needle.len()..].chars().next();
        if !word(before) && !word(after) {
            first = Some(i);
            break;
        }
    }
    first.map(|i| within.start + i..within.start + i + needle.len())
}

/// `range` cut to `src` and moved onto char boundaries.
fn clamp(src: &str, range: Range<usize>) -> Range<usize> {
    let fix = |mut i: usize| {
        i = i.min(src.len());
        while !src.is_char_boundary(i) {
            i -= 1;
        }
        i
    };
    let end = fix(range.end);
    fix(range.start).min(end)..end
}

/// Lines of `text` as `(offset, line without its line ending)`.
fn lines(text: &str) -> Vec<(usize, &str)> {
    let mut at = 0;
    text.split_inclusive('\n')
        .map(|line| {
            let r = (at, line.trim_end_matches(['\n', '\r']));
            at += line.len();
            r
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Line formats: Logseq, MultiMarkdown, org keywords.

/// `key:: value` (the key has no whitespace).
fn logseq_line(line: &str) -> Option<(&str, &str)> {
    let (key, rest) = line.split_once("::")?;
    let ok = !key.is_empty()
        && !key.starts_with(['#', '-', '*'])
        && !key.chars().any(char::is_whitespace)
        && rest.chars().next().is_none_or(char::is_whitespace);
    ok.then(|| (key, rest.trim()))
}

/// `Key: value`, the key matching `[A-Za-z][\w -]*`.
fn mmd_line(line: &str) -> Option<(&str, &str)> {
    let (key, rest) = line.split_once(':')?;
    let mut cs = key.chars();
    let ok = cs.next().is_some_and(|c| c.is_ascii_alphabetic())
        && cs.all(|c| c.is_alphanumeric() || matches!(c, '_' | ' ' | '-'))
        && rest.chars().next().is_none_or(char::is_whitespace);
    ok.then(|| (key.trim_end(), rest.trim()))
}

/// `#+KEY: value`.
fn org_line(line: &str) -> Option<(&str, &str)> {
    let (key, rest) = line.strip_prefix("#+")?.split_once(':')?;
    (!key.is_empty() && !key.contains(char::is_whitespace)).then(|| (key, rest.trim()))
}

fn line_entries(
    src: &str,
    range: Range<usize>,
    f: fn(&str) -> Option<(&str, &str)>,
    out: &mut Vec<Entry>,
) {
    for (at, line) in lines(&src[range.clone()]) {
        if let Some((key, value)) = f(line) {
            let start = range.start + at;
            out.push(Entry {
                key: key.to_owned(),
                value: scalar_value(value),
                src: start..start + line.len(),
                located: true,
            });
        }
    }
}

fn scalar_value(s: &str) -> Value {
    match s {
        "" | "~" | "null" | "Null" | "NULL" => Value::Null,
        s => Value::Str(s.to_owned()),
    }
}

// ---------------------------------------------------------------------------
// YAML / TOML: the scan supplies keys and text, the parser the shapes.

/// Flattens parsed entries into `a.b` keys with byte ranges in the
/// document.
struct Flatten<'a, 'o> {
    src: &'a str,
    /// The block's text between its fences.
    inner: Range<usize>,
    kind: FmKind,
    out: &'o mut Vec<Entry>,
}

impl Flatten<'_, '_> {
    fn run(&mut self, path: &[String], entries: Vec<FmEntry>) {
        for e in entries {
            let mut path = path.to_vec();
            path.push(e.key);
            let scanned = self.inner.start + e.src.start..self.inner.start + e.src.end;
            let value = match e.value {
                FmValue::Map(children) if !children.is_empty() => {
                    self.run(&path, children);
                    continue;
                }
                FmValue::Map(_) => Value::Null,
                FmValue::List(items) => Value::List(items),
                FmValue::Scalar(s) if self.kind == FmKind::Toml && s.is_empty() => Value::Str(s),
                FmValue::Scalar(s) => scalar_value(&s),
            };
            // The scan located top-level entries and block-map children;
            // the parser's own entries (flow maps, TOML tables, dotted
            // keys) cover the whole block and are found by key path.
            let (src, located) = if scanned != self.inner {
                (scanned, true)
            } else {
                match locate_path(self.src, self.inner.clone(), &path, self.kind) {
                    Some(r) => (r, true),
                    None => (scanned, false),
                }
            };
            self.out.push(Entry {
                key: path.join("."),
                value,
                src,
                located,
            });
        }
    }
}

/// The bytes of the entry at key `path` inside `within`: from the start of
/// its last key to the end of that key's line plus any more indented
/// lines after it. Each key is searched for after the previous one, inside
/// the previous one's span; a TOML `[a.b]` header narrows to its section.
fn locate_path(
    src: &str,
    within: Range<usize>,
    path: &[String],
    kind: FmKind,
) -> Option<Range<usize>> {
    let mut span = within;
    let mut rest = path;
    if kind == FmKind::Toml
        && let Some((section, used)) = toml_section(src, span.clone(), path)
    {
        span = section;
        rest = &path[used..];
    }
    let mut entry = None;
    for key in rest {
        let (key_start, key_end) = find_key(src, span.clone(), key)?;
        let end = entry_end(src, key_start, span.end);
        entry = Some(key_start..end);
        span = key_end..end;
    }
    entry
}

/// The section of the longest `[k1.….kj]` header (j < path length) in
/// `within`, from the line after it to the next header, and `j`.
/// Split a TOML dotted key (`a."b.c".d`) on dots outside quotes; quotes are
/// removed from each part.
fn toml_key_path(h: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in h.chars() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '.') => parts.push(std::mem::take(&mut cur).trim().to_owned()),
            (_, c) => cur.push(c),
        }
    }
    parts.push(cur.trim().to_owned());
    parts
}

fn toml_section(src: &str, within: Range<usize>, path: &[String]) -> Option<(Range<usize>, usize)> {
    let ls = lines(&src[within.clone()]);
    let header = |l: &str| {
        let t = l.trim();
        (t.starts_with('[') && !t.starts_with("[["))
            .then(|| t.trim_start_matches('[').split(']').next().unwrap_or(""))
            .map(toml_key_path)
    };
    (1..path.len()).rev().find_map(|j| {
        let i = ls
            .iter()
            .position(|(_, l)| header(l).as_deref() == Some(&path[..j]))?;
        let start = within.start
            + ls.get(i + 1)
                .map_or(within.end - within.start, |(at, _)| *at);
        let end = ls[i + 1..]
            .iter()
            .find(|(_, l)| l.trim_start().starts_with('['))
            .map_or(within.end, |(at, _)| within.start + at);
        Some((start..end.max(start), j))
    })
}

/// The first `key` in `within` written as a key: after a line start,
/// whitespace, `{`, `,` or `.`, optionally quoted, and followed by `:`,
/// `=` or `.`. Returns the key's start (quote included) and end.
fn find_key(src: &str, within: Range<usize>, key: &str) -> Option<(usize, usize)> {
    let hay = &src[within.clone()];
    if key.is_empty() {
        return None;
    }
    for (i, _) in hay.match_indices(key) {
        let before = hay[..i].chars().next_back();
        let quote = before.filter(|c| matches!(c, '"' | '\''));
        let (start, before) = match quote {
            Some(_) => (i - 1, hay[..i - 1].chars().next_back()),
            None => (i, before),
        };
        let mut after = &hay[i + key.len()..];
        if let Some(q) = quote {
            match after.strip_prefix(q) {
                Some(a) => after = a,
                None => continue,
            }
        }
        let lead = before.is_none_or(|c| c.is_whitespace() || matches!(c, '{' | ',' | '.'));
        let sep = after
            .trim_start_matches([' ', '\t'])
            .starts_with([':', '=', '.']);
        if lead && sep {
            let end = hay.len() - after.len();
            return Some((within.start + start, within.start + end));
        }
    }
    None
}

/// End of the entry whose key starts at `at`: its line, plus following
/// blank or more indented lines, capped at `limit`.
fn entry_end(src: &str, at: usize, limit: usize) -> usize {
    let line_start = src[..at].rfind('\n').map_or(0, |i| i + 1);
    let indent = |l: &str| l.len() - l.trim_start_matches([' ', '\t']).len();
    let base = indent(&src[line_start..limit.max(line_start)]);
    let mut end = src[at..limit].find('\n').map_or(limit, |i| at + i);
    while end < limit {
        let next = end + 1;
        let line_end = src[next..limit].find('\n').map_or(limit, |i| next + i);
        let line = src[next..line_end].trim_end_matches('\r');
        if !line.trim().is_empty() && indent(line) <= base {
            break;
        }
        end = line_end;
    }
    end
}

// The rest of this section is donated from ramble 5f6e7ed2b798
// src/frontmatter.rs (MIT, same author), adapted: maps carry their
// flattened children, parse errors are reported, deep nesting skips the
// parser.

/// Which delimiters the block used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FmKind {
    /// `---` ... `---`
    Yaml,
    /// `+++` ... `+++`
    Toml,
}

/// A top-level value as scanned or parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FmValue {
    /// The source text, surrounding quotes stripped.
    Scalar(String),
    List(Vec<String>),
    /// A nested map and its entries (empty when unknown).
    Map(Vec<FmEntry>),
}

/// One entry. `src` is the byte range in the parsed text of the lines it
/// came from (the whole text when it can't be located).
#[derive(Debug, Clone, PartialEq, Eq)]
struct FmEntry {
    key: String,
    value: FmValue,
    src: Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Parsed {
    /// In source order.
    entries: Vec<FmEntry>,
    /// False when no key could be extracted from a non-blank block.
    parsed: bool,
    /// Why the real parser rejected the block.
    error: Option<String>,
}

impl FmValue {
    /// Lists joined with `, `, maps as `{…}`.
    fn display(&self) -> String {
        match self {
            FmValue::Scalar(s) => s.clone(),
            FmValue::List(items) => items.join(", "),
            FmValue::Map(_) => "{…}".into(),
        }
    }
}

/// The text between the delimiter lines of the block at `range` in
/// `source` (the range covers the fences).
fn body(source: &str, range: &Range<usize>) -> Range<usize> {
    let block = source.get(range.clone()).unwrap_or("");
    let start = block.find('\n').map_or(block.len(), |i| i + 1);
    let end = block.rfind('\n').unwrap_or(0).max(start);
    range.start + start..range.start + end
}

/// Nesting beyond this skips the real parser (and stops map recursion).
const MAX_DEPTH: usize = 64;

/// Parse the text between the delimiters. Never fails.
fn parse(src: &str, kind: FmKind) -> Parsed {
    parse_at(src, kind, 0)
}

fn parse_at(src: &str, kind: FmKind, depth: usize) -> Parsed {
    let (mut scanned, nested) = scan(src);
    if depth < MAX_DEPTH {
        for e in &mut scanned {
            if let FmValue::Map(children) = &mut e.value {
                // Block-style children: the lines after the key line.
                let first = src[e.src.clone()].find('\n').map(|i| e.src.start + i + 1);
                if let Some(start) = first.filter(|&s| s < e.src.end) {
                    let child = parse_at(&src[start..e.src.end], kind, depth + 1);
                    *children = child
                        .entries
                        .into_iter()
                        .map(|c| shift_entry(c, start))
                        .collect();
                }
            }
        }
    }
    let real = if too_deep(src) {
        Err("frontmatter nesting too deep to parse".to_owned())
    } else {
        match kind {
            FmKind::Yaml => yaml_entries(src),
            FmKind::Toml => toml_entries(src),
        }
    };
    let (entries, error) = match real {
        Ok(real) => (
            combine(
                real.into_iter().map(|(k, v)| (k, one_line(v))).collect(),
                scanned,
                &nested,
                src,
                kind,
            ),
            None,
        ),
        Err(e) => (scanned, Some(e)),
    };
    let parsed = !entries.is_empty() || src.trim().is_empty();
    Parsed {
        entries,
        parsed,
        error,
    }
}

fn shift_entry(mut e: FmEntry, by: usize) -> FmEntry {
    e.src = e.src.start + by..e.src.end + by;
    if let FmValue::Map(children) = e.value {
        e.value = FmValue::Map(children.into_iter().map(|c| shift_entry(c, by)).collect());
    }
    e
}

/// Bracket nesting or indentation deep enough to risk the parsers' stacks.
fn too_deep(src: &str) -> bool {
    let mut depth = 0usize;
    for c in src.chars() {
        match c {
            '[' | '{' => {
                depth += 1;
                if depth > MAX_DEPTH {
                    return true;
                }
            }
            ']' | '}' => depth = depth.saturating_sub(1),
            '\n' => depth = 0,
            _ => {}
        }
    }
    src.lines()
        .any(|l| l.len() - l.trim_start().len() > 2 * MAX_DEPTH)
}

/// The parser's entries combined with the line scan's. When the scan
/// found the same top-level keys as the parser, keys, order and text come
/// from the source and the parser only decides each value's shape;
/// duplicate keys all show, in source order. Otherwise (TOML tables,
/// dotted keys, a flow mapping) the parser's entries are used, with the
/// scan's text and lines where a key matches.
fn combine(
    real: Vec<(String, FmValue)>,
    mut scanned: Vec<FmEntry>,
    nested: &[bool],
    src: &str,
    kind: FmKind,
) -> Vec<FmEntry> {
    let norm: Vec<String> = scanned
        .iter()
        .map(|e| match kind {
            FmKind::Yaml if !src[e.src.start..].trim_start().starts_with(['"', '\'']) => {
                yaml_key(&e.key)
            }
            _ => e.key.clone(),
        })
        .collect();
    // Parser keys are unique: index them once, so this stays linear.
    let (keys, shapes): (Vec<String>, Vec<FmValue>) = real.into_iter().unzip();
    let index: HashMap<&str, usize> = keys
        .iter()
        .enumerate()
        .map(|(i, k)| (k.as_str(), i))
        .collect();
    let in_scan: HashSet<&str> = norm.iter().map(String::as_str).collect();
    let covers = !scanned.is_empty()
        && norm.iter().all(|n| index.contains_key(n.as_str()))
        && keys.iter().all(|k| in_scan.contains(k.as_str()));
    if covers {
        let mut shapes: Vec<Option<FmValue>> = shapes.into_iter().map(Some).collect();
        // The last occurrence of a key is the one the parser kept.
        for (i, n) in norm.iter().enumerate().rev() {
            if let Some(shape) = shapes[index[n.as_str()]].take() {
                let value = std::mem::replace(&mut scanned[i].value, FmValue::Map(Vec::new()));
                scanned[i].value = pick(shape, value, nested[i]);
            }
        }
        return scanned;
    }
    let mut first: HashMap<&str, usize> = HashMap::new();
    for (i, n) in norm.iter().enumerate() {
        first.entry(n.as_str()).or_insert(i);
    }
    keys.iter()
        .zip(shapes)
        .map(|(key, shape)| {
            let found = first.get(key.as_str()).map(|&i| (i, &scanned[i]));
            FmEntry {
                value: match found {
                    Some((i, e)) => pick(shape, e.value.clone(), nested[i]),
                    None => shape,
                },
                src: found.map_or(0..src.len(), |(_, e)| e.src.clone()),
                key: key.clone(),
            }
        })
        .collect()
}

/// Parser text on one line: line breaks and other control characters
/// become spaces, runs of whitespace one space, ends trimmed (map
/// children too).
fn one_line(v: FmValue) -> FmValue {
    let flat = |t: String| {
        t.split(|c: char| c.is_whitespace() || c.is_control())
            .filter(|w| !w.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    };
    match v {
        FmValue::Scalar(t) => FmValue::Scalar(flat(t)),
        FmValue::List(items) => FmValue::List(items.into_iter().map(flat).collect()),
        FmValue::Map(children) => FmValue::Map(
            children
                .into_iter()
                .map(|mut c| {
                    c.value = one_line(c.value);
                    c
                })
                .collect(),
        ),
    }
}

#[cfg(test)]
thread_local! {
    /// Calls of the YAML parser from [`yaml_key`] (tests only).
    static KEY_LOADS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// An unquoted YAML key as the parser reads it (`1.10` -> `1.1`, `~` ->
/// empty), so source keys can be matched to parsed ones. Only a key that
/// could be a non-string scalar (a number, `~`, `null`, a boolean, a tag
/// or anchor) is loaded; any other key is a plain string as written.
fn yaml_key(key: &str) -> String {
    use saphyr::{LoadableYamlNode, Yaml};
    let lower = key.to_ascii_lowercase();
    let plain = key.starts_with(|c: char| c.is_alphabetic() || c == '_')
        && !matches!(lower.as_str(), "null" | "true" | "false" | ".inf" | ".nan");
    if plain {
        return key.to_string();
    }
    #[cfg(test)]
    KEY_LOADS.with(|n| n.set(n.get() + 1));
    Yaml::load_from_str(key)
        .ok()
        .and_then(|d| d.first().and_then(yaml_scalar))
        .unwrap_or_else(|| key.to_string())
}

/// The parser's shape, with the scan's text when the shapes agree.
/// Lists keep the scan's items only when the scan saw no nested list or
/// map item (`- - b`, `- src: a`) and the item counts agree.
fn pick(shape: FmValue, scanned: FmValue, nested: bool) -> FmValue {
    match (shape, scanned) {
        (FmValue::List(p), FmValue::List(t)) if nested || p.len() != t.len() => FmValue::List(p),
        (FmValue::Scalar(_), s @ FmValue::Scalar(_)) | (FmValue::List(_), s @ FmValue::List(_)) => {
            s
        }
        (FmValue::Map(p), FmValue::Map(s)) => FmValue::Map(if s.is_empty() { p } else { s }),
        (shape, _) => shape,
    }
}

// Real parsers: top-level keys and value shapes, `Err` unless a mapping.

fn yaml_entries(src: &str) -> Result<Vec<(String, FmValue)>, String> {
    use saphyr::{LoadableYamlNode, Yaml};
    let docs = Yaml::load_from_str(src).map_err(|e| e.to_string())?;
    match docs.first() {
        Some(Yaml::Mapping(map)) => Ok(map
            .iter()
            .filter_map(|(k, v)| Some((yaml_scalar(k)?, yaml_shape(v, src.len()))))
            .collect()),
        // Only comments or blank lines.
        None => Ok(Vec::new()),
        Some(_) => Err("frontmatter is not a mapping".to_owned()),
    }
}

fn yaml_scalar(y: &saphyr::Yaml) -> Option<String> {
    use saphyr::{Scalar, Yaml};
    match y {
        Yaml::Value(s) => Some(match s {
            Scalar::Null => String::new(),
            Scalar::Boolean(b) => b.to_string(),
            Scalar::Integer(i) => i.to_string(),
            Scalar::FloatingPoint(f) => f.to_string(),
            Scalar::String(s) => s.to_string(),
        }),
        Yaml::Representation(s, ..) => Some(s.to_string()),
        Yaml::Tagged(_, inner) => yaml_scalar(inner),
        _ => None,
    }
}

/// `len` is the parsed text's length: parser-only children cover it all.
fn yaml_shape(y: &saphyr::Yaml, len: usize) -> FmValue {
    use saphyr::Yaml;
    match y {
        Yaml::Mapping(map) => FmValue::Map(
            map.iter()
                .filter_map(|(k, v)| {
                    Some(FmEntry {
                        key: yaml_scalar(k)?,
                        value: yaml_shape(v, len),
                        src: 0..len,
                    })
                })
                .collect(),
        ),
        Yaml::Sequence(items) => FmValue::List(
            items
                .iter()
                .map(|i| yaml_scalar(i).unwrap_or_else(|| yaml_shape(i, len).display()))
                .collect(),
        ),
        Yaml::Tagged(_, inner) => yaml_shape(inner, len),
        y => FmValue::Scalar(yaml_scalar(y).unwrap_or_default()),
    }
}

fn toml_entries(src: &str) -> Result<Vec<(String, FmValue)>, String> {
    let table: toml::Table = src.parse().map_err(|e: toml::de::Error| e.to_string())?;
    Ok(table
        .into_iter()
        .map(|(k, v)| (k, toml_shape(&v, src.len())))
        .collect())
}

fn toml_shape(v: &toml::Value, len: usize) -> FmValue {
    match v {
        toml::Value::Table(t) => FmValue::Map(
            t.iter()
                .map(|(k, v)| FmEntry {
                    key: k.clone(),
                    value: toml_shape(v, len),
                    src: 0..len,
                })
                .collect(),
        ),
        toml::Value::Array(items) => FmValue::List(items.iter().map(toml_text).collect()),
        v => FmValue::Scalar(toml_text(v)),
    }
}

fn toml_text(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        toml::Value::Datetime(d) => d.to_string(),
        toml::Value::Array(items) => items.iter().map(toml_text).collect::<Vec<_>>().join(", "),
        toml::Value::Table(_) => "{…}".into(),
    }
}

// Line scan.

/// An entry being collected.
struct Building {
    key: String,
    /// The inline value and folded continuation lines.
    parts: Vec<String>,
    items: Vec<String>,
    /// An item opened a nested list or map (`- - b`, `- k: v`).
    nested: bool,
    map: bool,
    /// After a `|` / `>` header: every more-indented line is text.
    block: bool,
    src: Range<usize>,
}

impl Building {
    fn finish(self) -> (FmEntry, bool) {
        let nested = self.nested;
        let value = if self.block {
            FmValue::Scalar(self.parts.join(" "))
        } else if self.map {
            FmValue::Map(Vec::new())
        } else if !self.items.is_empty() {
            FmValue::List(self.items)
        } else {
            let text = self.parts.join(" ");
            if text.starts_with('[') && text.ends_with(']') {
                FmValue::List(split_inline(&text[1..text.len() - 1]))
            } else if text.starts_with('{') {
                FmValue::Map(Vec::new())
            } else {
                FmValue::Scalar(unquote(&text).to_string())
            }
        };
        let entry = FmEntry {
            key: self.key,
            value,
            src: self.src,
        };
        (entry, nested)
    }

    /// Add the `- item` line `t`.
    fn push_item(&mut self, t: &str) {
        let raw = t[1..].trim();
        self.nested |= is_item(raw) || split_entry(raw).is_some();
        self.items.push(unquote(raw).to_string());
    }

    /// No value on the key line and nothing after it yet.
    fn empty(&self) -> bool {
        self.parts.is_empty() && self.items.is_empty()
    }
}

/// Walk the block line by line: `key: value` / `key = value` at the
/// smallest indentation starts an entry; more indented lines are list
/// items, nested keys (a map) or folded continuation. Each entry comes
/// with whether a list item of it opened a nested list or map.
fn scan(src: &str) -> (Vec<FmEntry>, Vec<bool>) {
    let lines = lines(src);
    let indent = |t: &str| t.len() - t.trim_start_matches([' ', '\t']).len();
    let content = |t: &str| {
        let t = t.trim();
        !t.is_empty() && !t.starts_with('#')
    };
    let Some(base) = lines
        .iter()
        .filter(|(_, t)| content(t) && !is_item(t.trim()) && split_entry(t.trim()).is_some())
        .map(|(_, t)| indent(t))
        .min()
    else {
        return (Vec::new(), Vec::new());
    };

    let quoted = quoted_continuations(&lines, base);
    let mut out = Vec::new();
    let mut cur: Option<Building> = None;
    let start = |at: usize, t: &str, (k, v): (String, String)| {
        let block = is_block_indicator(&v);
        Building {
            key: k,
            parts: if v.is_empty() || block {
                Vec::new()
            } else {
                vec![v]
            },
            items: Vec::new(),
            nested: false,
            map: false,
            block,
            src: at..at + t.len(),
        }
    };
    for (n, &(at, line)) in lines.iter().enumerate() {
        if quoted.contains(&n)
            && let Some(c) = &mut cur
        {
            c.parts.push(line.trim().to_string());
            c.src.end = at + line.len();
            continue;
        }
        if !content(line) {
            continue;
        }
        let t = line.trim();
        let ind = indent(line);
        if ind <= base {
            match (ind == base).then(|| split_entry(t)).flatten() {
                Some(kv) if !is_item(t) => {
                    out.extend(cur.take().map(Building::finish));
                    cur = Some(start(at, line, kv));
                }
                // `key:` then `- item` at the same indentation.
                _ if is_item(t) && cur.as_ref().is_some_and(|c| c.parts.is_empty() && !c.map) => {
                    let c = cur.as_mut().expect("checked");
                    c.push_item(t);
                    c.src.end = at + line.len();
                }
                _ => out.extend(cur.take().map(Building::finish)),
            }
            continue;
        }
        let Some(c) = &mut cur else { continue };
        if c.block {
            c.parts.push(t.to_string());
        } else if is_item(t) {
            c.push_item(t);
        } else if let Some(kv) = split_entry(t).filter(|_| !c.parts.is_empty() && !c.map) {
            // Bad indentation: a key under a scalar starts its own entry.
            out.extend(cur.take().map(Building::finish));
            cur = Some(start(at, line, kv));
            continue;
        } else if split_entry(t).is_some() && (c.map || c.empty()) {
            c.map = true;
        } else if !c.map {
            c.parts.push(t.to_string());
        }
        c.src.end = at + line.len();
    }
    out.extend(cur.map(Building::finish));
    out.into_iter().unzip()
}

/// Indices of lines inside a quoted value that runs past its key line:
/// TOML `"""` / `'''` strings up to their closing fence, and YAML `"` /
/// `'` scalars continued on lines that don't look like a new entry. An
/// unclosed quote absorbs nothing.
fn quoted_continuations(lines: &[(usize, &str)], base: usize) -> HashSet<usize> {
    let mut out = HashSet::new();
    let indent = |t: &str| t.len() - t.trim_start_matches([' ', '\t']).len();
    let mut n = 0;
    while n < lines.len() {
        let line = lines[n].1;
        n += 1;
        let Some((_, v)) = split_entry(line.trim()).filter(|_| indent(line) == base) else {
            continue;
        };
        let fence = ["\"\"\"", "'''"].into_iter().find(|f| v.starts_with(f));
        let open = match fence {
            Some(f) => !v[3..].contains(f),
            None => v.starts_with(['"', '\'']) && closing_quote(&v).is_none(),
        };
        if !open {
            continue;
        }
        let close = lines[n..].iter().position(|(_, t)| match fence {
            Some(f) => t.contains(f),
            None => t.contains(&v[..1]),
        });
        let entry_before = |end: usize| {
            fence.is_none()
                && lines[n..n + end]
                    .iter()
                    .any(|(_, t)| indent(t) <= base && split_entry(t.trim()).is_some())
        };
        if let Some(end) = close.filter(|&e| !entry_before(e + 1)) {
            out.extend(n..=n + end);
            n += end + 1;
        }
    }
    out
}

fn is_item(t: &str) -> bool {
    t == "-" || t.starts_with("- ") || t.starts_with("-\t")
}

/// `|`, `>`, `|-`, `>+2` and the like: a YAML block scalar header.
fn is_block_indicator(v: &str) -> bool {
    let mut cs = v.chars();
    matches!(cs.next(), Some('|' | '>')) && cs.all(|c| matches!(c, '-' | '+' | '0'..='9'))
}

/// `key: value` or `key = value`: the key (quotes trimmed) and the value,
/// with an unquoted trailing ` # comment` removed. A `:` must be followed
/// by whitespace or the end of the line, so `http://` is not a key.
fn split_entry(t: &str) -> Option<(String, String)> {
    let (key, rest) = match t.chars().next() {
        Some(q @ ('"' | '\'')) => {
            let end = t[1..].find(q)? + 1;
            (&t[1..end], t[end + 1..].trim_start())
        }
        _ => {
            let i = t.char_indices().find_map(|(i, c)| {
                let next = t[i + c.len_utf8()..].chars().next();
                match c {
                    '=' => Some(i),
                    ':' if next.is_none_or(char::is_whitespace) => Some(i),
                    _ => None,
                }
            })?;
            (t[..i].trim(), &t[i..])
        }
    };
    let rest = rest.strip_prefix([':', '='])?;
    if key.is_empty() || key.starts_with(['#', '[', '{']) {
        return None;
    }
    let mut value = rest.trim();
    if let Some(end) = closing_quote(value) {
        let after = value[end..].trim_start();
        if after.is_empty() || after.starts_with('#') {
            value = &value[..end];
        }
    } else if let Some(i) = comment_start(value) {
        value = value[..i].trim_end();
    }
    Some((key.to_string(), value.to_string()))
}

/// Where an unquoted value's ` # comment` starts. A `#` inside `[...]` or
/// `{...}` (Obsidian's `tags: [#a, #b]`) is part of the value, and so is
/// everything after an unclosed bracket.
fn comment_start(v: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut quote = None;
    // A value that starts with `#` is kept (generous).
    let mut prev = '#';
    for (i, c) in v.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') if depth > 0 => quote = Some(c),
            (None, '[' | '{') => depth += 1,
            (None, ']' | '}') => depth = depth.saturating_sub(1),
            (None, '#') if depth == 0 && prev.is_whitespace() => return Some(i),
            _ => {}
        }
        prev = c;
    }
    None
}

/// For a value starting with a quote, the byte just past its closing
/// quote: `\"` is escaped inside `"`, `''` inside `'`.
fn closing_quote(v: &str) -> Option<usize> {
    let q = v.chars().next().filter(|c| matches!(c, '"' | '\''))?;
    let mut chars = v.char_indices().skip(1).peekable();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' if q == '"' => {
                chars.next();
            }
            c if c == q && q == '\'' && chars.peek().is_some_and(|&(_, n)| n == q) => {
                chars.next();
            }
            c if c == q => return Some(i + 1),
            _ => {}
        }
    }
    None
}

/// Strip one pair of surrounding quotes, or a TOML `"""` / `'''` pair
/// (and the space next to it).
fn unquote(s: &str) -> &str {
    for f in ["\"\"\"", "'''"] {
        if s.len() >= 6 && s.starts_with(f) && s.ends_with(f) {
            return s[3..s.len() - 3].trim();
        }
    }
    let b = s.as_bytes();
    if b.len() >= 2 && (b[0] == b'"' || b[0] == b'\'') && b[b.len() - 1] == b[0] {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Split `a, "b, c", d` on commas outside quotes; items unquoted, empty
/// items dropped.
fn split_inline(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut quote = None;
    let mut item = String::new();
    for c in s.chars() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, ',') => {
                out.push(std::mem::take(&mut item));
                continue;
            }
            _ => {}
        }
        item.push(c);
    }
    out.push(item);
    out.iter()
        .map(|i| unquote(i.trim()).to_string())
        .filter(|i| !i.is_empty())
        .collect()
}

// ---------------------------------------------------------------------------
// JSON: a top-level object of scalars, arrays of scalars and objects.

mod json {
    use super::{Entry, Value};
    use std::ops::Range;

    const MAX_DEPTH: usize = 32;

    /// Entries of the object at `range`; on a syntax error the entries
    /// read so far and the message.
    pub(super) fn entries(src: &str, range: Range<usize>, out: &mut Vec<Entry>) -> Option<String> {
        let mut p = P {
            s: &src[..range.end],
            at: range.start,
        };
        p.object("", 0, out).err()
    }

    struct P<'a> {
        s: &'a str,
        at: usize,
    }

    impl P<'_> {
        fn ws(&mut self) {
            let rest = &self.s[self.at..];
            self.at += rest.len() - rest.trim_start().len();
        }

        fn peek(&mut self) -> Option<char> {
            self.ws();
            self.s[self.at..].chars().next()
        }

        fn eat(&mut self, c: char) -> Result<(), String> {
            if self.peek() == Some(c) {
                self.at += 1;
                Ok(())
            } else {
                Err(format!("json: expected `{c}` at byte {}", self.at))
            }
        }

        fn object(
            &mut self,
            prefix: &str,
            depth: usize,
            out: &mut Vec<Entry>,
        ) -> Result<(), String> {
            if depth > MAX_DEPTH {
                return Err("json: nesting too deep".into());
            }
            self.eat('{')?;
            if self.peek() == Some('}') {
                self.at += 1;
                return Ok(());
            }
            loop {
                let start = {
                    self.ws();
                    self.at
                };
                let key = self.string()?;
                let key = if prefix.is_empty() {
                    key
                } else {
                    format!("{prefix}.{key}")
                };
                self.eat(':')?;
                match self.peek() {
                    Some('{') => self.object(&key, depth + 1, out)?,
                    Some('[') => {
                        let items = self.array(depth + 1)?;
                        out.push(Entry {
                            key,
                            value: Value::List(items),
                            src: start..self.at,
                            located: true,
                        });
                    }
                    _ => {
                        let (text, string) = self.scalar()?;
                        let value = match text.as_str() {
                            "null" if !string => Value::Null,
                            _ => Value::Str(text),
                        };
                        out.push(Entry {
                            key,
                            value,
                            src: start..self.at,
                            located: true,
                        });
                    }
                }
                match self.peek() {
                    Some(',') => self.at += 1,
                    _ => return self.eat('}'),
                }
            }
        }

        fn array(&mut self, depth: usize) -> Result<Vec<String>, String> {
            if depth > MAX_DEPTH {
                return Err("json: nesting too deep".into());
            }
            self.eat('[')?;
            let mut items = Vec::new();
            if self.peek() == Some(']') {
                self.at += 1;
                return Ok(items);
            }
            loop {
                match self.peek() {
                    Some('[') => items.push(self.array(depth + 1)?.join(", ")),
                    Some('{') => {
                        self.object("", depth + 1, &mut Vec::new())?;
                        items.push("{…}".into());
                    }
                    _ => items.push(self.scalar()?.0),
                }
                match self.peek() {
                    Some(',') => self.at += 1,
                    _ => return self.eat(']').map(|_| items),
                }
            }
        }

        /// A string (unescaped) or a literal as written; `true` if a string.
        fn scalar(&mut self) -> Result<(String, bool), String> {
            if self.peek() == Some('"') {
                return Ok((self.string()?, true));
            }
            let rest = &self.s[self.at..];
            let n = rest
                .find(|c: char| c.is_whitespace() || matches!(c, ',' | '}' | ']'))
                .unwrap_or(rest.len());
            if n == 0 {
                return Err(format!("json: expected a value at byte {}", self.at));
            }
            self.at += n;
            Ok((rest[..n].to_owned(), false))
        }

        fn string(&mut self) -> Result<String, String> {
            self.eat('"')?;
            let mut out = String::new();
            let mut chars = self.s[self.at..].char_indices();
            while let Some((i, c)) = chars.next() {
                match c {
                    '"' => {
                        self.at += i + 1;
                        return Ok(out);
                    }
                    '\\' => match chars.next().map(|(_, c)| c) {
                        Some('n') => out.push('\n'),
                        Some('t') => out.push('\t'),
                        Some('r') => out.push('\r'),
                        Some('u') => {
                            let hex: String = (0..4)
                                .filter_map(|_| chars.next())
                                .map(|(_, c)| c)
                                .collect();
                            let c = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32);
                            out.push(c.unwrap_or('\u{fffd}'));
                        }
                        Some(c) => out.push(c),
                        None => break,
                    },
                    c => out.push(c),
                }
            }
            Err("json: unterminated string".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kv(fm: &Parsed) -> Vec<(&str, FmValue)> {
        fm.entries
            .iter()
            .map(|e| (e.key.as_str(), e.value.clone()))
            .collect()
    }

    /// Keys of a map value (`None` if not a map).
    fn map_keys(v: &FmValue) -> Option<Vec<&str>> {
        match v {
            FmValue::Map(c) => Some(c.iter().map(|e| e.key.as_str()).collect()),
            _ => None,
        }
    }

    fn is_map(v: &FmValue) -> bool {
        matches!(v, FmValue::Map(_))
    }

    fn s(v: &str) -> FmValue {
        FmValue::Scalar(v.into())
    }

    fn l(v: &[&str]) -> FmValue {
        FmValue::List(v.iter().map(|s| s.to_string()).collect())
    }

    /// Entries with maps replaced by a marker, for comparing.
    fn shapes(fm: &Parsed) -> Vec<(&str, FmValue)> {
        kv(fm)
            .into_iter()
            .map(|(k, v)| (k, if is_map(&v) { s("{map}") } else { v }))
            .collect()
    }

    #[test]
    fn valid_yaml_scalars_lists_maps_quotes_dates() {
        let src = "title: \"Hello: world\"\ndate: 2024-01-05\nversion: 1.10\n\
                   tags: [rust, 'tui']\nauthors:\n  - Ann\n  - Bob\n\
                   meta:\n  a: 1\n  b: 2\nempty:\nflag: yes\n";
        let fm = parse(src, FmKind::Yaml);
        assert!(fm.parsed);
        assert_eq!(fm.error, None);
        assert_eq!(
            shapes(&fm),
            [
                ("title", s("Hello: world")),
                ("date", s("2024-01-05")),
                ("version", s("1.10")),
                ("tags", l(&["rust", "tui"])),
                ("authors", l(&["Ann", "Bob"])),
                ("meta", s("{map}")),
                ("empty", s("")),
                ("flag", s("yes")),
            ]
        );
        assert_eq!(map_keys(&fm.entries[5].value), Some(vec!["a", "b"]));
        let authors = &fm.entries[4];
        assert_eq!(&src[authors.src.clone()], "authors:\n  - Ann\n  - Bob");
        let FmValue::Map(meta) = &fm.entries[5].value else {
            unreachable!()
        };
        assert_eq!(&src[meta[1].src.clone()], "  b: 2");
    }

    #[test]
    fn yaml_block_scalar_and_compact_list() {
        let src = "desc: >\n  one\n  two\ntags:\n- a\n- b\n";
        let fm = parse(src, FmKind::Yaml);
        assert_eq!(kv(&fm), [("desc", s("one two")), ("tags", l(&["a", "b"]))]);
    }

    #[test]
    fn broken_yaml_bad_indentation() {
        let fm = parse("title: X\n author: Y\ntags: [a]\n", FmKind::Yaml);
        assert!(fm.error.is_some());
        assert_eq!(
            kv(&fm),
            [("title", s("X")), ("author", s("Y")), ("tags", l(&["a"]))]
        );
    }

    #[test]
    fn broken_yaml_tab() {
        let fm = parse("tags:\n\t- a\n\t- b\ntitle: T\n", FmKind::Yaml);
        assert_eq!(kv(&fm), [("tags", l(&["a", "b"])), ("title", s("T"))]);
    }

    #[test]
    fn broken_yaml_unclosed_quote() {
        let fm = parse("title: \"Hello\nauthor: Ann\n", FmKind::Yaml);
        assert_eq!(kv(&fm), [("title", s("\"Hello")), ("author", s("Ann"))]);
    }

    #[test]
    fn broken_yaml_duplicate_key() {
        let src = "a: 1\nb: 2\na: 3\n";
        let fm = parse(src, FmKind::Yaml);
        assert!(fm.parsed);
        assert_eq!(kv(&fm), [("a", s("1")), ("b", s("2")), ("a", s("3"))]);
        let lines: Vec<&str> = fm.entries.iter().map(|e| &src[e.src.clone()]).collect();
        assert_eq!(lines, ["a: 1", "b: 2", "a: 3"]);
    }

    #[test]
    fn non_string_yaml_keys_keep_their_source_text_and_lines() {
        let src = "1.10: y\n007: z\nnull: x\n~: n\ntags:\n  - a\n";
        let fm = parse(src, FmKind::Yaml);
        assert_eq!(
            kv(&fm),
            [
                ("1.10", s("y")),
                ("007", s("z")),
                ("null", s("x")),
                ("~", s("n")),
                ("tags", l(&["a"])),
            ]
        );
        let lines: Vec<&str> = fm.entries.iter().map(|e| &src[e.src.clone()]).collect();
        assert_eq!(
            lines,
            ["1.10: y", "007: z", "null: x", "~: n", "tags:\n  - a"]
        );
    }

    #[test]
    fn scalars_stay_as_written() {
        let fm = parse("a: 007\nb: 0x1F\nc: True\n", FmKind::Yaml);
        assert_eq!(
            kv(&fm),
            [("a", s("007")), ("b", s("0x1F")), ("c", s("True"))]
        );
    }

    #[test]
    fn toml_tables_and_dotted_keys_use_the_parser_keys() {
        let fm = parse("a.b = 1\n[t]\nk = 2\n", FmKind::Toml);
        let keys: Vec<_> = fm.entries.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, ["a", "t"]);
        assert_eq!(map_keys(&fm.entries[0].value), Some(vec!["b"]));
        assert_eq!(map_keys(&fm.entries[1].value), Some(vec!["k"]));
    }

    #[test]
    fn toml_front_matter() {
        let src = "title = \"T\"\ndate = 2024-01-05\ntags = [\"rust\", \"tui\"]\n\
                   nums = [\n  1,\n  2,\n]\n[extra]\nk = 1\n";
        let fm = parse(src, FmKind::Toml);
        assert_eq!(
            shapes(&fm),
            [
                ("title", s("T")),
                ("date", s("2024-01-05")),
                ("tags", l(&["rust", "tui"])),
                ("nums", l(&["1", "2"])),
                ("extra", s("{map}")),
            ]
        );
    }

    #[test]
    fn broken_toml_falls_back_to_key_value_lines() {
        let fm = parse("title = \"T\nauthor = Ann\nx = [1, 2\n", FmKind::Toml);
        assert!(fm.error.is_some());
        assert_eq!(
            kv(&fm),
            [("title", s("\"T")), ("author", s("Ann")), ("x", s("[1, 2"))]
        );
    }

    #[test]
    fn empty_block() {
        for kind in [FmKind::Yaml, FmKind::Toml] {
            let fm = parse("", kind);
            assert!(fm.parsed);
            assert!(fm.entries.is_empty());
            assert!(parse("\n  \n", kind).parsed);
        }
    }

    #[test]
    fn only_comments_or_stray_text_is_unparsed() {
        for src in ["# a comment\n# another\n", "just some text\nmore text\n"] {
            for kind in [FmKind::Yaml, FmKind::Toml] {
                let fm = parse(src, kind);
                assert!(!fm.parsed, "{src:?} {kind:?}");
                assert!(fm.entries.is_empty());
            }
        }
    }

    #[test]
    fn stray_lines_and_comments_are_skipped() {
        let fm = parse(
            "# c\ntitle: A # note\nstray text\nurl: http://x.y\n",
            FmKind::Yaml,
        );
        assert_eq!(kv(&fm), [("title", s("A")), ("url", s("http://x.y"))]);
    }

    #[test]
    fn quoted_keys_and_inline_list_with_quoted_commas() {
        let fm = parse("\"a: b\": 1\nt: [\"x, y\", z,]\n", FmKind::Yaml);
        assert_eq!(kv(&fm), [("a: b", s("1")), ("t", l(&["x, y", "z"]))]);
    }

    #[test]
    fn a_comment_after_a_quoted_value_is_dropped() {
        let fm = parse(
            "q: \"x\" # c\nc2: '#000' # trailing\ne: \"a\\\"b\" # c\n",
            FmKind::Yaml,
        );
        assert_eq!(
            kv(&fm),
            [("q", s("x")), ("c2", s("#000")), ("e", s("a\\\"b"))]
        );
        let fm = parse("s = 'x' # c\nt = \"#y\" # d\n", FmKind::Toml);
        assert_eq!(kv(&fm), [("s", s("x")), ("t", s("#y"))]);
        // Through the line scan alone (broken YAML), too.
        let fm = parse("q: 'x' # c\n bad: [\n", FmKind::Yaml);
        assert_eq!(kv(&fm)[0], ("q", s("x")));
        assert_eq!(closing_quote("'it''s' # x"), Some(7));
        assert_eq!(closing_quote("\"a\\\"b\" # x"), Some(6));
        assert_eq!(closing_quote("'open"), None);
    }

    #[test]
    fn multi_line_quoted_strings_are_joined_and_cover_all_their_lines() {
        let src = "multi = \"\"\"\nline1\nline2\"\"\"\nlit = '''\na\nb'''\nz = 1\n";
        let fm = parse(src, FmKind::Toml);
        assert_eq!(
            kv(&fm),
            [
                ("multi", s("line1 line2")),
                ("lit", s("a b")),
                ("z", s("1"))
            ]
        );
        assert_eq!(
            &src[fm.entries[0].src.clone()],
            "multi = \"\"\"\nline1\nline2\"\"\""
        );
        // A YAML double-quoted scalar continued on an unindented line.
        let src = "title: \"a\nb\"\nz: 1\n";
        let fm = parse(src, FmKind::Yaml);
        assert_eq!(kv(&fm), [("title", s("a b")), ("z", s("1"))]);
        assert_eq!(&src[fm.entries[0].src.clone()], "title: \"a\nb\"");
        // An indented continuation that looks like a key is still text.
        let fm = parse("title: \"a\n  k: v\"\nz: 1\n", FmKind::Yaml);
        assert_eq!(kv(&fm), [("title", s("a k: v")), ("z", s("1"))]);
        // Broken YAML keeps the scan's text.
        let fm = parse("title: \"open\nz: 1\n", FmKind::Yaml);
        assert_eq!(kv(&fm), [("title", s("\"open")), ("z", s("1"))]);
        // ... even when a later line has a quote.
        let fm = parse("title: \"open\nz: 1\nw: \"x\"\n", FmKind::Yaml);
        assert_eq!(
            kv(&fm),
            [("title", s("\"open")), ("z", s("1")), ("w", s("x"))]
        );
    }

    #[test]
    fn hashes_inside_an_inline_list_are_values() {
        let fm = parse("tags: [#a, #b] # c\nm: {k: #v}\nt: x #y\n", FmKind::Yaml);
        assert_eq!(
            shapes(&fm),
            [("tags", l(&["#a", "#b"])), ("m", s("{map}")), ("t", s("x")),]
        );
        // Unclosed bracket: generous, keep the rest.
        let fm = parse("tags: [#a, #b\n", FmKind::Yaml);
        assert_eq!(kv(&fm), [("tags", s("[#a, #b"))]);
        // A bracket inside a quoted item doesn't close the list.
        let fm = parse("t: [\"x ] #\", #y] # c\nh: #h\n", FmKind::Yaml);
        assert_eq!(kv(&fm), [("t", l(&["x ] #", "#y"])), ("h", s("#h"))]);
    }

    #[test]
    fn crlf_lines() {
        let src = "title: T\r\ntags:\r\n  - a\r\nz: 1\r\n";
        let fm = parse(src, FmKind::Yaml);
        assert_eq!(
            kv(&fm),
            [("title", s("T")), ("tags", l(&["a"])), ("z", s("1"))]
        );
        assert_eq!(&src[fm.entries[1].src.clone()], "tags:\r\n  - a");
        // Through the scan alone (a tab makes it invalid YAML).
        let fm = parse("a: x\r\n\tb: y\r\n", FmKind::Yaml);
        assert_eq!(kv(&fm), [("a", s("x")), ("b", s("y"))]);
    }

    #[test]
    fn tagged_yaml_values_take_the_inner_shape() {
        let fm = parse(
            "t: !!str 5\nl: !custom [a, b]\nm: !x {k: 1}\n",
            FmKind::Yaml,
        );
        assert_eq!(
            shapes(&fm),
            [
                ("t", s("!!str 5")),
                ("l", l(&["a", "b"])),
                ("m", s("{map}")),
            ]
        );
        assert_eq!(map_keys(&fm.entries[2].value), Some(vec!["k"]));
    }

    #[test]
    fn block_scalars_are_text_even_when_lines_look_like_keys_or_items() {
        for h in ["|", ">", "|-", ">+"] {
            let src = format!("desc: {h}\n  a: b\n  c\nlist: {h}\n  - a\n  - b\nz: 1\n");
            let fm = parse(&src, FmKind::Yaml);
            assert_eq!(
                kv(&fm),
                [("desc", s("a: b c")), ("list", s("- a - b")), ("z", s("1"))],
                "{h}"
            );
            assert_eq!(
                &src[fm.entries[0].src.clone()],
                "desc: |\n  a: b\n  c".replace('|', h)
            );
            // Scan only (a tab elsewhere makes the YAML invalid).
            let fm = parse(&format!("{src}\tbad\n"), FmKind::Yaml);
            assert_eq!(
                kv(&fm)[..2],
                [("desc", s("a: b c")), ("list", s("- a - b"))],
                "{h}"
            );
        }
        // Text that looks like a flow list or map, or is quoted, stays text.
        let fm = parse(
            "a: |\n  [x, y]\nb: >\n  {k: v}\nc: |\n  \"q\"\nd: [\n",
            FmKind::Yaml,
        );
        assert_eq!(
            kv(&fm),
            [
                ("a", s("[x, y]")),
                ("b", s("{k: v}")),
                ("c", s("\"q\"")),
                ("d", s("["))
            ]
        );
    }

    #[test]
    fn parser_scalar_text_never_has_line_breaks() {
        // The parser's text is used when the scan can't place the key.
        let fm = parse("{a: \"x\\ny\", b: 1}\n", FmKind::Yaml);
        assert_eq!(kv(&fm), [("a", s("x y")), ("b", s("1"))]);
        let fm = parse("a.b = \"x\\ny\"\n[t]\nk = \"p\\tq\"\n", FmKind::Toml);
        assert_eq!(map_keys(&fm.entries[0].value), Some(vec!["b"]));
        let FmValue::Map(a) = &fm.entries[0].value else {
            unreachable!()
        };
        assert_eq!(a[0].value, s("x y"));
        let FmValue::Map(t) = &fm.entries[1].value else {
            unreachable!()
        };
        assert_eq!(t[0].value, s("p q"));
        let fm = parse("x = \"\"\"\nl1\nl2\n\"\"\"\ny.z = 1\n", FmKind::Toml);
        assert_eq!(kv(&fm)[0], ("x", s("l1 l2")));
    }

    fn many_keys(n: usize) -> String {
        (0..n).map(|i| format!("key{i}: value {i}\n")).collect()
    }

    /// Fastest of three runs, so a busy machine doesn't fail the test.
    fn time(src: &str) -> std::time::Duration {
        (0..3)
            .map(|_| {
                let t = std::time::Instant::now();
                assert!(parse(src, FmKind::Yaml).parsed);
                t.elapsed()
            })
            .min()
            .unwrap()
    }

    #[test]
    fn parse_time_is_linear_in_the_number_of_keys() {
        let (small, big) = (many_keys(5_000), many_keys(20_000));
        let (ts, tb) = (time(&small), time(&big));
        // Linear: 4x the keys, about 4x the time; quadratic would be 16x.
        assert!(tb < ts * 8, "5k keys {ts:?}, 20k keys {tb:?}");
    }

    #[test]
    fn plain_keys_skip_the_per_key_yaml_load() {
        KEY_LOADS.with(|n| n.set(0));
        let fm = parse(&many_keys(1_000), FmKind::Yaml);
        assert_eq!(fm.entries.len(), 1_000);
        assert_eq!(KEY_LOADS.with(|n| n.get()), 0);
        let fm = parse("1.10: a\n~: b\nNull: c\ntrue: d\nx: e\n", FmKind::Yaml);
        assert_eq!(KEY_LOADS.with(|n| n.get()), 4);
        let keys: Vec<&str> = fm.entries.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, ["1.10", "~", "Null", "true", "x"]);
    }

    #[test]
    fn many_keys_with_duplicates_stay_in_source_order() {
        let mut src = many_keys(3_000);
        src.push_str("key7: again\nkey0: last\n");
        let fm = parse(&src, FmKind::Yaml);
        assert_eq!(fm.entries.len(), 3_002);
        let tail: Vec<_> = kv(&fm)[2_999..].to_vec();
        assert_eq!(
            tail,
            [
                ("key2999", s("value 2999")),
                ("key7", s("again")),
                ("key0", s("last"))
            ]
        );
        assert_eq!(kv(&fm)[0], ("key0", s("value 0")));
        assert_eq!(kv(&fm)[7], ("key7", s("value 7")));
    }

    #[test]
    fn nested_list_items_take_the_parser_items() {
        let fm = parse("l:\n  - a\n  - - b\n    - c\n", FmKind::Yaml);
        assert_eq!(kv(&fm), [("l", l(&["a", "b, c"]))]);
        let fm = parse(
            "resources:\n- src: a.jpg\n  title: A\n- src: b.jpg\n",
            FmKind::Yaml,
        );
        assert_eq!(kv(&fm), [("resources", l(&["{…}", "{…}"]))]);
        // One item, so the counts agree: the nesting alone decides.
        let fm = parse("r:\n- src: a.jpg\n  title: A\n", FmKind::Yaml);
        assert_eq!(kv(&fm), [("r", l(&["{…}"]))]);
        // A nested flow list: the scan splits it into more items.
        let fm = parse("l: [a, [b, c]]\n", FmKind::Yaml);
        assert_eq!(kv(&fm), [("l", l(&["a", "b, c"]))]);
        // Same items: the scan's text still wins.
        let fm = parse("l:\n  - '01'\n  - 1.10\n  - \"a: b\"\n", FmKind::Yaml);
        assert_eq!(kv(&fm), [("l", l(&["01", "1.10", "a: b"]))]);
    }

    #[test]
    fn body_strips_the_delimiter_lines() {
        let src = "---\na: 1\n---\n\n# H\n";
        let r = 0..src.find("---\n\n").unwrap() + 3;
        assert_eq!(&src[body(src, &r)], "a: 1");
        let src = "---\n---\n";
        assert_eq!(body(src, &(0..7)), 4..4);
    }

    #[test]
    fn org_filetags_and_title_map_to_standard_keys() {
        let fm = Frontmatter::from_entries(
            FrontmatterFormat::OrgKeywords,
            0..0,
            vec![
                ("title".into(), Value::Str("T".into())),
                ("filetags".into(), Value::Str(":a:b:".into())),
            ],
            None,
        );
        assert_eq!(fm.title(), Some("T"));
        assert_eq!(fm.tags(), ["a", "b"]);
    }

    #[test]
    fn deep_nesting_does_not_reach_the_parser() {
        let src = format!("a: {}\n", "[".repeat(10_000));
        let fm = parse(&src, FmKind::Yaml);
        assert!(fm.error.is_some());
        let src: String = (0..500)
            .map(|i| format!("{}k{i}:\n", " ".repeat(i)))
            .collect();
        let _ = parse(&src, FmKind::Yaml);
    }
}
