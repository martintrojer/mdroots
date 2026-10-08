//! Liberal scanner: link and tag candidates the structure pass does not model.
//!
//! One left-to-right pass per segment and candidate form, byte scanning for
//! ASCII markers (so every slice is on a char boundary), no backtracking.
//! Candidates that overlap an existing link, reference definition or an
//! earlier candidate are skipped; existing elements are never touched.

use std::ops::Range;

use crate::model::{
    Confidence, Context, Dialect, Element, Link, LinkKind, LinkTarget, ParseOptions, Tag, TagSyntax,
};
use crate::org::{self, Abbrevs};
use crate::regions::Regions;
use crate::structure::by_scheme;

/// Longer code spans are never paths.
const MAX_CODE_PATH: usize = 512;

/// Extensions that make a slash-less bare token a path candidate.
const BARE_EXTS: &[&str] = &[
    "md", "markdown", "org", "png", "jpg", "jpeg", "gif", "svg", "pdf", "txt",
];

/// Append candidates found in `regions` to `elements`. `abbrevs` are the
/// org `#+LINK` abbreviations (empty for markdown).
pub(crate) fn scan(
    src: &str,
    regions: &Regions,
    opts: &ParseOptions,
    abbrevs: &Abbrevs,
    elements: &mut Vec<Element>,
) {
    let base = Cover::new(
        elements
            .iter()
            .filter(|e| matches!(e, Element::Link(_) | Element::LinkDef(_)))
            .map(Element::range)
            .collect(),
    );
    // `[[…]]` outside prose: org links in org files, wikilinks elsewhere.
    let brackets = |s: &mut Seg| match opts.dialect {
        Dialect::Org => s.org_links(abbrevs),
        _ => s.wiki(),
    };
    let mut out = Vec::new();
    for (seg, ctx) in regions.segments() {
        let mut s = Seg {
            src,
            seg,
            ctx,
            base: &base,
            local: Cover::new(Vec::new()),
            out: &mut out,
        };
        match ctx {
            Context::CodeBlock | Context::Comment => s.phase(brackets),
            Context::InlineCode => {
                s.phase(brackets);
                s.phase(|s| s.code_mention(opts.dialect));
            }
            Context::Prose => {
                s.phase(Seg::templating);
                s.phase(Seg::footnotes);
                s.phase(Seg::urls);
                s.phase(|s| s.tags(opts));
                s.phase(Seg::bare_paths);
            }
            Context::Heading => {
                s.phase(Seg::urls);
                s.phase(|s| s.tags(opts));
                s.phase(Seg::bare_paths);
            }
            Context::Html => s.phase(Seg::html),
            _ => {}
        }
    }
    elements.extend(out);
}

/// Ranges sorted by start with a running maximum of ends: overlap queries
/// in O(log n) even when ranges nest.
struct Cover {
    ranges: Vec<Range<usize>>,
    max_end: Vec<usize>,
}

impl Cover {
    fn new(mut ranges: Vec<Range<usize>>) -> Self {
        ranges.sort_by_key(|r| r.start);
        let mut m = 0;
        let max_end = ranges
            .iter()
            .map(|r| {
                m = m.max(r.end);
                m
            })
            .collect();
        Cover { ranges, max_end }
    }

    /// Does any range overlap `r` (or contain its start)?
    fn hits(&self, r: &Range<usize>) -> bool {
        let n = self
            .ranges
            .partition_point(|x| x.start < r.end.max(r.start + 1));
        n > 0 && self.max_end[n - 1] > r.start
    }
}

/// One segment being scanned.
struct Seg<'a> {
    src: &'a str,
    seg: Range<usize>,
    ctx: Context,
    base: &'a Cover,
    /// Candidates emitted in this segment by earlier phases.
    local: Cover,
    out: &'a mut Vec<Element>,
}

impl Seg<'_> {
    fn text(&self) -> &str {
        &self.src[self.seg.clone()]
    }

    fn free(&self, r: &Range<usize>) -> bool {
        !self.base.hits(r) && !self.local.hits(r)
    }

    /// Run one candidate form, then let later forms see what it emitted.
    /// Every form goes through here, so no candidate overlaps an earlier one.
    fn phase(&mut self, f: impl FnOnce(&mut Self)) {
        let before = self.out.len();
        f(self);
        if self.out.len() > before {
            let mut ranges = std::mem::take(&mut self.local.ranges);
            ranges.extend(self.out[before..].iter().map(Element::range));
            self.local = Cover::new(ranges);
        }
    }

    /// The char before absolute offset `at`, anywhere in the source.
    fn before(&self, at: usize) -> Option<char> {
        self.src[..at].chars().next_back()
    }

    fn push_link(&mut self, link: Link) {
        self.out.push(Element::Link(link));
    }

    /// A non-wiki candidate with `text_range = range`. Templating, bare
    /// path and HTML targets split a `#anchor` off the path like markdown
    /// links; URLs (external), footnotes and code mentions keep `path = raw`
    /// and no anchor.
    fn plain(
        &self,
        kind: LinkKind,
        confidence: Confidence,
        raw: &str,
        range: Range<usize>,
    ) -> Link {
        let mut l = Link::new(kind, self.ctx, confidence, raw);
        if !matches!(
            kind,
            LinkKind::Templating | LinkKind::BarePath | LinkKind::Html
        ) {
            l.target = LinkTarget {
                raw: raw.to_owned(),
                path: raw.to_owned(),
                anchor: None,
                line: None,
            };
        }
        l.text_range = range.clone();
        l.range = range;
        l
    }

    /// `[[t]]`, `[[t|l]]`, `![[e]]` and org `[[t][d]]` on one line, in a
    /// segment pulldown emits no link events for.
    fn wiki(&mut self) {
        let off = self.seg.start;
        let text = self.text();
        let mut found = Vec::new();
        let mut i = 0;
        while let Some(p) = text[i..].find("[[") {
            let mut open = i + p;
            // `]]` must be on the same line (bounds the search, too).
            let line = &text[open + 2..];
            let line = &line[..line.find('\n').unwrap_or(line.len())];
            let Some(c) = line.find("]]") else {
                i = open + 2 + line.len();
                continue;
            };
            let close = open + 2 + c;
            let mut inner = &text[open + 2..close];
            // `[[a [[b]]`: the innermost opener wins.
            if let Some(q) = inner.rfind("[[") {
                open += 2 + q;
                inner = &text[open + 2..close];
            }
            i = close + 2;
            let embed = open > 0 && text.as_bytes()[open - 1] == b'!';
            let start = if embed { open - 1 } else { open };
            let range = off + start..off + close + 2;
            let t0 = off + open + 2;
            if let Some((target, desc)) = inner.split_once("][") {
                if target.trim().is_empty() {
                    continue;
                }
                let mut l = Link::new(LinkKind::Org, self.ctx, by_scheme(target), target);
                l.label = Some(desc.to_owned());
                let d0 = t0 + target.len() + 2;
                l.text_range = d0..d0 + desc.len();
                l.range = range;
                found.push(l);
                continue;
            }
            let (target, label) = match inner.split_once('|') {
                Some((t, l)) => (t, Some(l)),
                None => (inner, None),
            };
            if target.trim().is_empty() {
                continue;
            }
            let kind = if embed {
                LinkKind::WikiEmbed
            } else {
                LinkKind::Wiki
            };
            let mut l = Link::new(kind, self.ctx, by_scheme(target), target);
            l.text_range = match label {
                Some(lab) => {
                    let l0 = t0 + target.len() + 1;
                    l0..l0 + lab.len()
                }
                None => t0..t0 + target.len(),
            };
            l.label = label.map(str::to_owned);
            l.range = range;
            found.push(l);
        }
        for l in found {
            if self.free(&l.range) {
                self.push_link(l);
            }
        }
    }

    /// Org `[[t]]` / `[[t][d]]` in a segment org.rs emits no links for,
    /// classified like org.rs does in prose (`#+LINK` expansion included).
    fn org_links(&mut self, abbrevs: &Abbrevs) {
        let found = org::links_in(self.text(), self.seg.start, self.ctx, abbrevs);
        for l in found {
            if self.free(&l.range) {
                self.push_link(l);
            }
        }
    }

    /// An inline code span whose text is path-like (optionally `:LINE` or
    /// `:LINE:COL`) proposes a code mention; resolve decides if it exists.
    fn code_mention(&mut self, dialect: Dialect) {
        let Some(inner) = code_text(self.text(), dialect) else {
            return;
        };
        let start = self.seg.start + inner.start;
        let range = start..start + inner.len();
        let token = &self.src[range.clone()];
        if token.is_empty()
            || token.len() > MAX_CODE_PATH
            || token.contains(char::is_whitespace)
            || token.contains("://")
            || !self.free(&range)
        {
            return;
        }
        let (path, line) = match strip_position(token) {
            Some((p, n)) => (p, u32::try_from(n).ok()),
            None => (token, None),
        };
        if !code_path_like(path) {
            return;
        }
        let mut l = self.plain(LinkKind::CodeMention, Confidence::Implicit, token, range);
        l.target.path = path.to_owned();
        l.target.line = line;
        self.push_link(l);
    }

    /// Hugo `{{< ref "x" >}}` / `{{< relref "x" >}}` (or `{{% … %}}`) and
    /// Jekyll `{% link x %}`.
    fn templating(&mut self) {
        let off = self.seg.start;
        let text = self.text();
        let b = text.as_bytes();
        let mut found = Vec::new();
        let mut i = 0;
        while let Some(p) = text[i..].find('{') {
            let at = i + p;
            i = at + 1;
            let hit = if b[at..].starts_with(b"{{<") || b[at..].starts_with(b"{{%") {
                let close: &[u8] = if b[at + 2] == b'<' { b">}}" } else { b"%}}" };
                shortcode(text, at + 3, &["ref", "relref"], close)
            } else if b[at..].starts_with(b"{%") {
                shortcode(text, at + 2, &["link"], b"%}")
            } else {
                None
            };
            if let Some((arg, end)) = hit {
                found.push((arg, off + at..off + end));
                i = end;
            }
        }
        for (arg, range) in found {
            if self.free(&range) {
                let raw = &self.src[arg.start + off..arg.end + off];
                let l = self.plain(LinkKind::Templating, Confidence::Explicit, raw, range);
                self.push_link(l);
            }
        }
    }

    /// Footnote references `[^x]` (not definitions `[^x]:` at line start).
    fn footnotes(&mut self) {
        let off = self.seg.start;
        let text = self.text();
        let mut found = Vec::new();
        let mut i = 0;
        while let Some(p) = text[i..].find("[^") {
            let at = i + p;
            i = at + 2;
            let rest = &text[at + 2..];
            let Some(len) = rest.find(|c: char| c == ']' || c == '[' || c.is_whitespace()) else {
                break;
            };
            if len == 0 || !rest[len..].starts_with(']') {
                continue;
            }
            let end = at + 2 + len + 1;
            i = end;
            if text[end..].starts_with(':') {
                let line_start = self.src[..off + at].rfind('\n').map_or(0, |n| n + 1);
                if self.src[line_start..off + at].trim().is_empty() {
                    continue;
                }
            }
            found.push(off + at..off + end);
        }
        for range in found {
            if self.free(&range) {
                let raw = format!("^{}", &self.src[range.start + 2..range.end - 1]);
                let l = self.plain(LinkKind::Footnote, Confidence::Explicit, &raw, range);
                self.push_link(l);
            }
        }
    }

    /// Bare `http://` / `https://` URLs.
    fn urls(&mut self) {
        let off = self.seg.start;
        let text = self.text();
        let mut found = Vec::new();
        let mut i = 0;
        while let Some(p) = text[i..].find("http") {
            let at = i + p;
            i = at + 4;
            let rest = &text[at..];
            let scheme = if rest.starts_with("https://") {
                8
            } else if rest.starts_with("http://") {
                7
            } else {
                continue;
            };
            if self.before(off + at).is_some_and(char::is_alphanumeric) {
                continue;
            }
            let len = rest
                .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '`'))
                .unwrap_or(rest.len());
            let url = trim_url(&rest[..len]);
            i = at + len;
            if url.len() > scheme {
                found.push(off + at..off + at + url.len());
            }
        }
        for range in found {
            if self.free(&range) {
                let raw = &self.src[range.clone()];
                let l = self.plain(LinkKind::Url, Confidence::External, raw, range);
                self.push_link(l);
            }
        }
    }

    /// `#tag`, `#a/b`, `#multi word#` and `:colon:tags:` per `opts`.
    fn tags(&mut self, opts: &ParseOptions) {
        let off = self.seg.start;
        let text = self.text();
        let colon = opts.colon_tags && opts.dialect != Dialect::Org;
        let mut found: Vec<Tag> = Vec::new();
        let mut i = 0;
        while let Some(p) = text[i..].find(['#', ':']) {
            let at = i + p;
            i = at + 1;
            let tag_start = self
                .before(off + at)
                .is_none_or(|c| c.is_whitespace() || c == '(');
            if !tag_start {
                continue;
            }
            if text.as_bytes()[at] == b':' {
                if colon && let Some((tags, end)) = colon_tags(text, at) {
                    found.extend(tags.into_iter().map(|r| Tag {
                        name: text[r.start + 1..r.end].to_owned(),
                        syntax: TagSyntax::Colon,
                        range: off + r.start..off + r.end,
                    }));
                    i = end;
                }
                continue;
            }
            if opts.multiword_tags
                && let Some(end) = multiword(text, at)
            {
                found.push(Tag {
                    name: text[at + 1..end - 1].to_owned(),
                    syntax: TagSyntax::MultiWord,
                    range: off + at..off + end,
                });
                i = end;
                continue;
            }
            if opts.hashtags
                && let Some(end) = hashtag(text, at)
            {
                found.push(Tag {
                    name: text[at + 1..end].to_owned(),
                    syntax: TagSyntax::Hash,
                    range: off + at..off + end,
                });
                i = end;
            }
        }
        for t in found {
            if self.free(&t.range) {
                self.out.push(Element::Tag(t));
            }
        }
    }

    /// Path-like whitespace-free tokens (docs/specs/index.md §2.3 guards).
    fn bare_paths(&mut self) {
        let off = self.seg.start;
        let text = self.text();
        let mut found = Vec::new();
        for (s, tok) in tokens(text) {
            let lead = tok.len()
                - tok
                    .trim_start_matches(['(', '[', '"', '\'', '<', '*'])
                    .len();
            let tok = tok.trim_start_matches(['(', '[', '"', '\'', '<', '*']);
            let tok =
                tok.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '"', '\'', '>', '*']);
            let start = off + s + lead;
            if tok.is_empty()
                || self
                    .before(start)
                    .is_some_and(|c| matches!(c, ':' | '.' | '/') || c.is_alphanumeric())
                || !bare_path_like(tok)
            {
                continue;
            }
            found.push(start..start + tok.len());
        }
        for range in found {
            if self.free(&range) {
                let raw = &self.src[range.clone()];
                let l = self.plain(LinkKind::BarePath, Confidence::Implicit, raw, range);
                self.push_link(l);
            }
        }
    }

    /// `<a href="x">` and `<img src="x">`.
    fn html(&mut self) {
        let off = self.seg.start;
        let text = self.text();
        let mut found = Vec::new();
        let mut i = 0;
        while let Some(p) = text[i..].find('<') {
            let at = i + p;
            i = at + 1;
            if let Some((val, end)) = html_link(text, at) {
                found.push((val, off + at..off + end));
                i = end;
            }
        }
        for (val, range) in found {
            if self.free(&range) {
                let raw = &self.src[val.start + off..val.end + off];
                let l = self.plain(LinkKind::Html, by_scheme(raw), raw, range);
                self.push_link(l);
            }
        }
    }
}

/// The text of an inline code span relative to the segment: backtick runs
/// stripped and one space each side when both are spaces (CommonMark), or
/// org `=x=` / `~x~`.
fn code_text(seg: &str, dialect: Dialect) -> Option<Range<usize>> {
    let b = seg.as_bytes();
    let (mut s, mut e) = if b.first() == Some(&b'`') {
        let n = b.iter().take_while(|&&c| c == b'`').count();
        let m = b.iter().rev().take_while(|&&c| c == b'`').count();
        if n != m || 2 * n > b.len() {
            return None;
        }
        (n, b.len() - n)
    } else if dialect == Dialect::Org
        && b.len() >= 2
        && matches!(b[0], b'=' | b'~')
        && b[b.len() - 1] == b[0]
    {
        (1, b.len() - 1)
    } else {
        return None;
    };
    if e - s >= 2 && b[s] == b' ' && b[e - 1] == b' ' {
        s += 1;
        e -= 1;
    }
    Some(s..e)
}

/// `path:LINE:COL` or `path:LINE` (positive integers) to `(path, LINE)`.
// Donated from ramble 75b8285 src/app/codepath.rs (MIT).
fn strip_position(text: &str) -> Option<(&str, usize)> {
    let num = |s: &str| {
        (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse::<usize>().ok())
            .flatten()
            .filter(|&n| n > 0)
    };
    let (head, last) = text.rsplit_once(':')?;
    num(last)?;
    if let Some((path, line)) = head.rsplit_once(':')
        && let Some(line) = num(line)
        && !path.is_empty()
    {
        return Some((path, line));
    }
    let line = num(last)?;
    (!head.is_empty()).then_some((head, line))
}

/// Contains `/`, starts with `./ ../ ~/ /`, or ends in a 1-8 char
/// alphanumeric extension after a non-empty stem.
fn code_path_like(p: &str) -> bool {
    if p.is_empty() || p.contains(char::is_whitespace) {
        return false;
    }
    p.contains('/')
        || p.starts_with('~')
        || p.rsplit_once('.').is_some_and(|(stem, ext)| {
            !stem.is_empty()
                && (1..=8).contains(&ext.len())
                && ext.bytes().all(|b| b.is_ascii_alphanumeric())
        })
}

/// Spec §2.3 bare-path guards on an already trimmed token.
fn bare_path_like(t: &str) -> bool {
    let dotted = t.starts_with("./") || t.starts_with("../") || t.starts_with("~/");
    if t.contains(':') || t.starts_with('#') || !t.chars().any(char::is_alphanumeric) {
        return false;
    }
    if t.contains('/') {
        // `host.com/path` is a host, not a path.
        let first = t.split('/').next().unwrap_or("");
        return dotted || !first.contains('.');
    }
    t.rsplit_once('.').is_some_and(|(stem, ext)| {
        !stem.is_empty() && BARE_EXTS.iter().any(|e| ext.eq_ignore_ascii_case(e))
    })
}

/// Maximal runs of non-whitespace with their offsets.
fn tokens(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut rest = 0;
    std::iter::from_fn(move || {
        let s = rest + text[rest..].find(|c: char| !c.is_whitespace())?;
        let e = text[s..]
            .find(char::is_whitespace)
            .map_or(text.len(), |n| s + n);
        rest = e;
        Some((s, &text[s..e]))
    })
}

/// Strip trailing punctuation from a URL; `)` only while unbalanced.
fn trim_url(mut u: &str) -> &str {
    loop {
        let Some(c) = u.chars().next_back() else {
            return u;
        };
        let strip = match c {
            '.' | ',' | ';' | ':' | '!' | '?' | '[' | ']' | '*' | '\'' => true,
            ')' => u.matches('(').count() < u.matches(')').count(),
            _ => false,
        };
        if !strip {
            return u;
        }
        u = &u[..u.len() - 1];
    }
}

fn tag_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '/')
}

/// `#name` at `at`: end of the tag (exclusive), trailing `/` dropped.
fn hashtag(text: &str, at: usize) -> Option<usize> {
    let rest = &text[at + 1..];
    if !rest.chars().next()?.is_alphabetic() {
        return None;
    }
    let len = rest.find(|c: char| !tag_char(c)).unwrap_or(rest.len());
    if rest[len..].starts_with('#') {
        return None;
    }
    let name = rest[..len].trim_end_matches('/');
    Some(at + 1 + name.len())
}

/// `#multi word#` at `at` on one line: end after the closing `#`.
fn multiword(text: &str, at: usize) -> Option<usize> {
    let rest = &text[at + 1..];
    let len = rest.find(['#', '\n'])?;
    if !rest[len..].starts_with('#') {
        return None;
    }
    let name = &rest[..len];
    let first = name.chars().next()?;
    let last = name.chars().next_back()?;
    if !first.is_alphabetic() || last.is_whitespace() {
        return None;
    }
    let end = at + 1 + len + 1;
    let after = text[end..].chars().next();
    (!after.is_some_and(char::is_alphanumeric)).then_some(end)
}

/// `:a:b:` at `at`: one `:name` range per tag and the end of the group.
fn colon_tags(text: &str, at: usize) -> Option<(Vec<Range<usize>>, usize)> {
    let mut tags = Vec::new();
    let mut i = at;
    loop {
        let rest = &text[i + 1..];
        let len = rest.find(|c: char| !tag_char(c)).unwrap_or(rest.len());
        if len == 0 || !rest[len..].starts_with(':') {
            break;
        }
        tags.push(i..i + 1 + len);
        i += 1 + len;
        if !text[i + 1..].starts_with(tag_char) {
            break;
        }
    }
    let end = i + 1;
    let ok = !tags.is_empty()
        && text[end.min(text.len())..]
            .chars()
            .next()
            .is_none_or(char::is_whitespace);
    ok.then_some((tags, end))
}

/// After `{{<`, `{{%` or `{%` at `i`: one of `words`, an argument (quoted
/// or bare) and `close`. Returns the argument range and the end offset.
fn shortcode(
    text: &str,
    mut i: usize,
    words: &[&str],
    close: &[u8],
) -> Option<(Range<usize>, usize)> {
    let b = text.as_bytes();
    let ws = |i: &mut usize| {
        while *i < b.len() && matches!(b[*i], b' ' | b'\t') {
            *i += 1;
        }
    };
    if b.get(i) == Some(&b'-') {
        i += 1;
    }
    ws(&mut i);
    let w0 = i;
    while i < b.len() && (b[i].is_ascii_alphabetic() || b[i] == b'_') {
        i += 1;
    }
    if !words.contains(&&text[w0..i]) {
        return None;
    }
    let gap = i;
    ws(&mut i);
    if i == gap {
        return None;
    }
    let arg = if matches!(b.get(i), Some(b'"' | b'\'')) {
        let q = b[i];
        let s = i + 1;
        let n = b[s..].iter().position(|&c| c == q || c == b'\n')?;
        if b[s + n] != q {
            return None;
        }
        i = s + n + 1;
        s..s + n
    } else {
        let s = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && !close.starts_with(&b[i..i + 1]) {
            i += 1;
        }
        s..i
    };
    ws(&mut i);
    if b.get(i) == Some(&b'-') {
        i += 1;
    }
    (!arg.is_empty() && b[i..].starts_with(close)).then_some((arg, i + close.len()))
}

/// `<a … href=…>` or `<img … src=…>` at `at`: value range and tag end.
fn html_link(text: &str, at: usize) -> Option<(Range<usize>, usize)> {
    let b = text.as_bytes();
    let mut i = at + 1;
    let n0 = i;
    while i < b.len() && b[i].is_ascii_alphanumeric() {
        i += 1;
    }
    let name = &text[n0..i];
    let want = if name.eq_ignore_ascii_case("a") {
        "href"
    } else if name.eq_ignore_ascii_case("img") {
        "src"
    } else {
        return None;
    };
    if !b.get(i).is_some_and(|c| c.is_ascii_whitespace()) {
        return None;
    }
    let mut hit = None;
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        match b.get(i) {
            None => return None,
            Some(b'>') => break,
            Some(b'/') => {
                i += 1;
                continue;
            }
            _ => {}
        }
        let a0 = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && !matches!(b[i], b'=' | b'>' | b'/') {
            i += 1;
        }
        let attr = &text[a0..i];
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if b.get(i) != Some(&b'=') {
            continue;
        }
        i += 1;
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let val = match b.get(i) {
            Some(&q @ (b'"' | b'\'')) => {
                let s = i + 1;
                let n = b[s..].iter().position(|&c| c == q)?;
                i = s + n + 1;
                s..s + n
            }
            _ => {
                let s = i;
                while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'>' {
                    i += 1;
                }
                s..i
            }
        };
        if hit.is_none() && attr.eq_ignore_ascii_case(want) {
            hit = Some(val);
        }
    }
    let val = hit.filter(|v| !v.is_empty())?;
    Some((val, i + 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::structure::markdown;
    use proptest::prelude::*;

    /// The inputs of tests/scan.rs, plus mixed-context lines.
    const INPUTS: &[&str] = &[
        "```\nsee [[note#h]] and ![[img.png]]\n```\n",
        "a `[[x|lab]]` b",
        "a <!-- [[c]] --> b",
        "```\n[[{{filename-stem}}]]\n```\n",
        "```\n[[]] [[a\nb]]\n```\n",
        "```\n[[t][d]]\n```\n",
        "`[t](x.md)`",
        "`[[x/y]]` and `[[a.md]]` and `[[t][d/e]]`",
        "open `src/main.rs:12` now",
        "`x.rs:3:1`",
        "`~/notes/a`",
        "`foo bar`",
        "`https://x.com/a`",
        "`a.rs:99999999999`",
        "[`a/b.rs`](x)",
        "see notes/foo.md.",
        "(./a.png) x ../up/a b read me.md, ok",
        "# Head a/b `c/d` e/f #t\n",
        "see host.com/path mode:mode/fast/x https://x.com/a/b",
        "go to https://x.com/a?b=1. And (http://y.org/p_(q)).",
        "# Heading\n\nsome #rust and #a/b, (#paren) x#no #1no\n",
        "[t](#anchor) #rust",
        "x :a:b: y #multi word# z",
        "text[^1] more.\n\n[^1]: the note\n",
        "see {{< ref \"x.md\" >}} and {% link z.md %}",
        "<a href=\"./a.md\">x</a> and <img alt=\"\" src='https://x/p.png'>\n",
        "<div>\n<a href=b.md>b</a>\n</div>\n",
        "é#tag 😀 notes/é.md. `é/ü.rs:2` https://é.x/ü",
        "https://x.com/#tag/a.md {{< ref \"https://y/#t\" >}}",
    ];

    /// Every element the scan adds lies inside one segment whose context
    /// it records (tags: Prose or Heading), and overlaps neither an
    /// existing link/definition nor another scanned candidate.
    fn check(src: &str, opts: &ParseOptions) -> Result<(), String> {
        let pass = markdown(src);
        let mut elements = pass.elements.clone();
        let n = elements.len();
        scan(src, &pass.regions, opts, &Abbrevs::new(), &mut elements);
        let segs: Vec<_> = pass.regions.segments().collect();
        let mut taken: Vec<Range<usize>> = pass
            .elements
            .iter()
            .filter(|e| matches!(e, Element::Link(_) | Element::LinkDef(_)))
            .map(Element::range)
            .collect();
        for e in &elements[n..] {
            let r = e.range();
            let seg = segs
                .iter()
                .find(|(s, _)| s.start <= r.start && r.end <= s.end && r.start < s.end);
            let Some((_, ctx)) = seg else {
                return Err(format!("{e:?} spans segments {segs:?} in {src:?}"));
            };
            let ok = match e {
                Element::Link(l) => l.context == *ctx,
                Element::Tag(_) => matches!(ctx, Context::Prose | Context::Heading),
                _ => false,
            };
            if !ok {
                return Err(format!("{e:?} in a {ctx:?} segment of {src:?}"));
            }
            if let Some(t) = taken.iter().find(|t| t.start < r.end && r.start < t.end) {
                return Err(format!("{e:?} overlaps {t:?} in {src:?}"));
            }
            taken.push(r);
        }
        Ok(())
    }

    fn all_opts() -> [ParseOptions; 2] {
        let mut o = ParseOptions::new(Dialect::Markdown);
        o.colon_tags = true;
        o.multiword_tags = true;
        [ParseOptions::new(Dialect::Markdown), o]
    }

    #[test]
    fn candidates_stay_in_their_segment() {
        for src in INPUTS {
            for opts in &all_opts() {
                check(src, opts).unwrap();
            }
        }
    }

    fn fragments() -> impl Strategy<Value = String> {
        let frag = prop_oneof![
            Just("[["),
            Just("]]"),
            Just("]["),
            Just("[[a/b]]"),
            Just("|"),
            Just("![["),
            Just("[^"),
            Just("]"),
            Just(":"),
            Just("#"),
            Just("#a"),
            Just("/"),
            Just("./"),
            Just(".md"),
            Just("`"),
            Just("```\n"),
            Just("# "),
            Just("<!--"),
            Just("-->"),
            Just("{{< ref "),
            Just(" >}}"),
            Just("{% link "),
            Just(" %}"),
            Just("\""),
            Just("<a href="),
            Just("<img src="),
            Just(">"),
            Just("https://"),
            Just("("),
            Just(")"),
            Just("$"),
            Just("\n"),
            Just(" "),
            Just("é"),
            Just("😀"),
            Just("a"),
            Just("1"),
        ];
        proptest::collection::vec(frag, 0..50).prop_map(|v| v.concat())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(1024))]
        #[test]
        fn fragments_stay_in_their_segment(src in fragments()) {
            for opts in &all_opts() {
                if let Err(e) = check(&src, opts) {
                    prop_assert!(false, "{}", e);
                }
            }
        }
    }
}
