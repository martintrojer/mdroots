//! Org-mode structure pass: a line-oriented hand parser for headings,
//! links (with `#+LINK` abbreviations), keywords, property drawers, tags
//! and code regions.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use crate::model::{
    Anchor, Confidence, Context, Element, Frontmatter, FrontmatterFormat, Heading, Link, LinkKind,
    LinkTarget, Tag, TagSyntax, Value,
};
use crate::regions::Regions;
use crate::slug;

const TODO_KEYWORDS: [&str; 5] = ["TODO", "DONE", "NEXT", "WAITING", "CANCELLED"];

/// One source line: `start..end` includes the newline, `start..text_end`
/// excludes `\n` and a preceding `\r`.
struct Line {
    start: usize,
    end: usize,
    text_end: usize,
}

/// `#+LINK` abbreviations: prefix to URL template.
pub(crate) type Abbrevs = HashMap<String, String>;

/// Headings, links, regions, the keyword frontmatter and the `#+LINK`
/// abbreviations of an org document (the scan applies the latter to the
/// links it finds in code and comments).
pub(crate) fn parse_org(src: &str) -> (Vec<Element>, Regions, Option<Frontmatter>, Abbrevs) {
    let base = if src.starts_with('\u{feff}') { 3 } else { 0 };
    let lines = split_lines(src, base);
    let text = |l: &Line| &src[l.start..l.text_end];

    let mut regions = Regions::whole(src, Context::Prose);
    let mut elements = Vec::new();
    let mut abbrevs = Abbrevs::new();
    // Lines whose links org.rs emits (prose and headings), with their context.
    let mut scan_lines: Vec<(Range<usize>, Context)> = Vec::new();

    // The top block: blank lines, `#+KEY:` lines and at most one drawer.
    let mut entries: Vec<(String, Value)> = Vec::new();
    let mut fm_end: Option<usize> = None;
    let mut drawer_used = false;
    let mut i = 0;
    while i < lines.len() {
        let l = &lines[i];
        let t = text(l).trim();
        if t.is_empty() {
            if fm_end.is_some() {
                fm_end = Some(l.end);
            }
            i += 1;
        } else if let Some((key, value)) = keyword(t) {
            let key = key.to_ascii_lowercase();
            if key == "link" {
                add_abbrev(&mut abbrevs, value);
            }
            let value = if key == "filetags" {
                let tags = tag_words(src, value);
                for (name, range) in &tags {
                    elements.push(Element::Tag(Tag {
                        name: name.clone(),
                        syntax: TagSyntax::Frontmatter,
                        range: range.clone(),
                    }));
                }
                Value::List(tags.into_iter().map(|(n, _)| n).collect())
            } else {
                Value::Str(value.to_owned())
            };
            entries.push((key, value));
            fm_end = Some(l.end);
            i += 1;
        } else if !drawer_used
            && t.eq_ignore_ascii_case(":PROPERTIES:")
            && let Some(end) = drawer_end(src, &lines, i)
        {
            for (key, value) in properties(src, &lines[i + 1..end]) {
                entries.push((key.to_ascii_lowercase(), Value::Str(value.to_owned())));
            }
            drawer_used = true;
            fm_end = Some(lines[end].end);
            i = end + 1;
        } else {
            break;
        }
    }
    let frontmatter = fm_end.map(|end| {
        let range = base..end;
        regions.push(range.clone(), Context::Frontmatter);
        Frontmatter::from_entries(FrontmatterFormat::OrgKeywords, range, entries, None)
    });

    // The body.
    let mut used_slugs = HashSet::new();
    while i < lines.len() {
        let l = &lines[i];
        let t = text(l);
        let ts = t.trim_start();
        if let Some(stars) = heading_stars(t) {
            regions.push(l.start..l.end, Context::Heading);
            scan_lines.push((l.start..l.text_end, Context::Heading));
            let (mut heading, tags) = heading(src, &t[stars + 1..], stars);
            heading.range = l.start..l.end;
            heading.slug = slug::unique(&slug::github(&heading.text), &mut used_slugs);
            i += 1;
            // An optional planning line, then a property drawer.
            if i < lines.len() && is_planning(text(&lines[i])) {
                scan_lines.push((lines[i].start..lines[i].text_end, Context::Prose));
                i += 1;
            }
            if i < lines.len()
                && text(&lines[i]).trim().eq_ignore_ascii_case(":PROPERTIES:")
                && let Some(end) = drawer_end(src, &lines, i)
            {
                for (key, value) in properties(src, &lines[i + 1..end]) {
                    if key.eq_ignore_ascii_case("ID") {
                        heading.id = Some(value.to_owned());
                    } else if key.eq_ignore_ascii_case("CUSTOM_ID") {
                        heading.custom_id = Some(value.to_owned());
                    }
                }
                regions.push(lines[i].start..lines[end].end, Context::Comment);
                i = end + 1;
            }
            elements.push(Element::Heading(heading));
            elements.extend(tags.into_iter().map(Element::Tag));
        } else if let Some((name, ctx)) = block_begin(ts) {
            let close = format!("#+end_{name}");
            let mut j = i + 1;
            while j < lines.len() && !text(&lines[j]).trim().eq_ignore_ascii_case(&close) {
                j += 1;
            }
            let end = lines.get(j).map_or(src.len(), |l| l.end);
            regions.push(l.start..end, ctx);
            i = j + 1;
        } else if let Some((key, value)) = keyword(ts) {
            if key.eq_ignore_ascii_case("link") {
                add_abbrev(&mut abbrevs, value);
            }
            regions.push(l.start..l.end, Context::Comment);
            i += 1;
        } else if t == "#" || t.starts_with("# ") {
            regions.push(l.start..l.end, Context::Comment);
            i += 1;
        } else if ts == ":" || ts.starts_with(": ") {
            regions.push(l.start..l.end, Context::CodeBlock);
            i += 1;
        } else {
            scan_lines.push((l.start..l.text_end, Context::Prose));
            i += 1;
        }
    }

    for (range, ctx) in scan_lines {
        let line = &src[range.clone()];
        let links = find_links(line);
        let spans = code_spans(line, &links);
        for s in &spans {
            regions.push(
                s.start + range.start..s.end + range.start,
                Context::InlineCode,
            );
        }
        for raw in links {
            if spans.iter().any(|s| s.contains(&raw.range.start)) {
                continue;
            }
            elements.push(Element::Link(org_link(
                line,
                range.start,
                raw,
                ctx,
                &abbrevs,
            )));
        }
    }

    (elements, regions, frontmatter, abbrevs)
}

/// Org links in `text` (starting at `at` in the source), line by line,
/// classified exactly like prose links but with context `ctx`.
pub(crate) fn links_in(text: &str, at: usize, ctx: Context, abbrevs: &Abbrevs) -> Vec<Link> {
    let mut out = Vec::new();
    let mut start = 0;
    for line in text.split_inclusive('\n') {
        for raw in find_links(line) {
            out.push(org_link(line, at + start, raw, ctx, abbrevs));
        }
        start += line.len();
    }
    out
}

fn split_lines(src: &str, base: usize) -> Vec<Line> {
    let mut out = Vec::new();
    let mut start = base;
    for piece in src[base..].split_inclusive('\n') {
        let end = start + piece.len();
        let body = piece.strip_suffix('\n').unwrap_or(piece);
        let body = body.strip_suffix('\r').unwrap_or(body);
        out.push(Line {
            start,
            end,
            text_end: start + body.len(),
        });
        start = end;
    }
    out
}

/// Byte offset of `sub` (a subslice of `src`) in `src`.
fn offset_in(src: &str, sub: &str) -> usize {
    sub.as_ptr() as usize - src.as_ptr() as usize
}

fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.as_bytes()
        .get(..prefix.len())
        .is_some_and(|b| b.eq_ignore_ascii_case(prefix.as_bytes()))
}

/// `#+KEY: value` → `(KEY, value)`; the key has no whitespace.
fn keyword(t: &str) -> Option<(&str, &str)> {
    let rest = t.strip_prefix("#+")?;
    let (key, value) = rest.split_once(':')?;
    if key.is_empty()
        || key.contains(char::is_whitespace)
        || starts_with_ci(key, "begin_")
        || starts_with_ci(key, "end_")
    {
        return None;
    }
    Some((key, value.trim()))
}

/// `#+LINK: ABBR TEMPLATE`.
fn add_abbrev(abbrevs: &mut Abbrevs, value: &str) {
    if let Some((abbr, template)) = value.split_once(char::is_whitespace) {
        let template = template.trim();
        if !template.is_empty() {
            abbrevs.insert(abbr.to_owned(), template.to_owned());
        }
    }
}

/// `:a:b:` (or space-separated) words with their ranges in `src`.
fn tag_words(src: &str, value: &str) -> Vec<(String, Range<usize>)> {
    value
        .split(|c: char| c == ':' || c.is_whitespace())
        .filter(|w| !w.is_empty())
        .map(|w| {
            let at = offset_in(src, w);
            (w.to_owned(), at..at + w.len())
        })
        .collect()
}

/// Index of the `:END:` line of a drawer opened at `lines[open]`; `None`
/// if a heading or EOF comes first.
fn drawer_end(src: &str, lines: &[Line], open: usize) -> Option<usize> {
    for (k, l) in lines.iter().enumerate().skip(open + 1) {
        let t = &src[l.start..l.text_end];
        if t.trim().eq_ignore_ascii_case(":END:") {
            return Some(k);
        }
        if heading_stars(t).is_some() {
            return None;
        }
    }
    None
}

/// `:KEY: value` lines of a drawer body.
fn properties<'a>(src: &'a str, lines: &[Line]) -> Vec<(&'a str, &'a str)> {
    lines
        .iter()
        .filter_map(|l| {
            let t = src[l.start..l.text_end].trim();
            let (key, value) = t.strip_prefix(':')?.split_once(':')?;
            (!key.is_empty() && !key.contains(char::is_whitespace)).then_some((key, value.trim()))
        })
        .collect()
}

/// Number of stars of a `^\*+ ` heading line.
fn heading_stars(t: &str) -> Option<usize> {
    let stars = t.bytes().take_while(|&b| b == b'*').count();
    (stars > 0 && t.as_bytes().get(stars) == Some(&b' ')).then_some(stars)
}

fn is_planning(t: &str) -> bool {
    let t = t.trim_start();
    ["SCHEDULED:", "DEADLINE:", "CLOSED:"]
        .iter()
        .any(|p| t.starts_with(p))
}

/// Heading from the text after `* `: TODO keyword and priority stripped,
/// trailing `:tags:` split off. Range and slug are filled by the caller.
fn heading(src: &str, title: &str, stars: usize) -> (Heading, Vec<Tag>) {
    let mut rest = title.trim_start();
    for kw in TODO_KEYWORDS {
        if let Some(r) = rest.strip_prefix(kw)
            && (r.is_empty() || r.starts_with(' '))
        {
            rest = r.trim_start();
            break;
        }
    }
    let b = rest.as_bytes();
    if b.len() >= 4
        && rest.starts_with("[#")
        && b[2].is_ascii_alphanumeric()
        && b[3] == b']'
        && (b.len() == 4 || b[4] == b' ')
    {
        rest = rest[4..].trim_start();
    }
    let rest = rest.trim_end();
    let mut tags = Vec::new();
    let mut title = rest;
    if rest.ends_with(':') {
        let k = rest.rfind([' ', '\t']).map_or(0, |p| p + 1);
        let token = &rest[k..];
        let inner = token.get(1..token.len() - 1).unwrap_or("");
        if token.starts_with(':')
            && !inner.is_empty()
            && inner
                .split(':')
                .all(|w| !w.is_empty() && !w.contains(char::is_whitespace))
        {
            for w in inner.split(':') {
                let at = offset_in(src, w);
                tags.push(Tag {
                    name: w.to_owned(),
                    syntax: TagSyntax::OrgHeading,
                    range: at..at + w.len(),
                });
            }
            title = rest[..k].trim_end();
        }
    }
    let heading = Heading {
        level: stars.min(u8::MAX as usize) as u8,
        text: render_links(title),
        slug: String::new(),
        range: 0..0,
        id: None,
        custom_id: None,
    };
    (heading, tags)
}

/// `title` with each `[[t][d]]` replaced by `d` and `[[t]]` by `t`.
fn render_links(title: &str) -> String {
    let mut out = String::new();
    let mut at = 0;
    for l in find_links(title) {
        out.push_str(&title[at..l.range.start]);
        out.push_str(&title[l.desc.clone().unwrap_or(l.target.clone())]);
        at = l.range.end;
    }
    out.push_str(&title[at..]);
    out
}

/// `#+begin_NAME` of a block whose contents are not prose: the lowercase
/// name and the context of the whole block.
fn block_begin(ts: &str) -> Option<(String, Context)> {
    if !starts_with_ci(ts, "#+begin_") {
        return None;
    }
    let name = ts[8..]
        .split(char::is_whitespace)
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let ctx = match name.as_str() {
        "src" | "example" | "export" => Context::CodeBlock,
        "comment" => Context::Comment,
        _ => return None,
    };
    Some((name, ctx))
}

/// A `[[target]]` or `[[target][desc]]` in one line; ranges are relative.
struct RawLink {
    range: Range<usize>,
    target: Range<usize>,
    desc: Option<Range<usize>>,
}

fn find_links(s: &str) -> Vec<RawLink> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(p) = s[i..].find("[[") {
        let open = i + p;
        let t0 = open + 2;
        let Some(q) = s[t0..].find(']') else { break };
        let t1 = t0 + q;
        i = open + 1;
        if t1 == t0 || s[t0..t1].contains('[') {
            continue;
        }
        let after = &s[t1..];
        if after.starts_with("]]") {
            out.push(RawLink {
                range: open..t1 + 2,
                target: t0..t1,
                desc: None,
            });
            i = t1 + 2;
        } else if after.starts_with("][") {
            let d0 = t1 + 2;
            if let Some(r) = s[d0..].find("]]") {
                let d1 = d0 + r;
                out.push(RawLink {
                    range: open..d1 + 2,
                    target: t0..t1,
                    desc: Some(d0..d1),
                });
                i = d1 + 2;
            }
        }
    }
    out
}

/// `~code~` and `=verbatim=` spans (markers included) under org emphasis
/// boundaries; markers inside `links` are ignored.
fn code_spans(s: &str, links: &[RawLink]) -> Vec<Range<usize>> {
    let b = s.as_bytes();
    let mut in_link = vec![false; b.len()];
    for l in links {
        in_link[l.range.clone()].iter_mut().for_each(|x| *x = true);
    }
    let is_ws = |k: usize| b[k].is_ascii_whitespace();
    let closes = |k: usize| {
        k > 0
            && !in_link[k]
            && !is_ws(k - 1)
            && (k + 1 == b.len() || is_ws(k + 1) || b"-.,:;!?')}\"".contains(&b[k + 1]))
    };
    let close_at =
        |m: u8| -> Vec<usize> { (0..b.len()).filter(|&k| b[k] == m && closes(k)).collect() };
    let (close_eq, close_tilde) = (close_at(b'='), close_at(b'~'));
    let mut spans = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let m = b[i];
        let opens = (m == b'=' || m == b'~')
            && !in_link[i]
            && (i == 0 || is_ws(i - 1) || b"-({'\"".contains(&b[i - 1]))
            && i + 1 < b.len()
            && !is_ws(i + 1);
        if opens {
            let closers = if m == b'=' { &close_eq } else { &close_tilde };
            let c = closers.partition_point(|&k| k < i + 2);
            if let Some(&j) = closers.get(c) {
                spans.push(i..j + 1);
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    spans
}

/// Classify a raw link found in `line` (which starts at `at` in the source).
fn org_link(line: &str, at: usize, raw: RawLink, ctx: Context, abbrevs: &Abbrevs) -> Link {
    let written = &line[raw.target.clone()];
    let mut confidence = Confidence::Explicit;
    let mut raw_s = written.to_owned();
    let (path, anchor) = if let Some(rest) = written.strip_prefix("file:") {
        split_search(rest)
    } else if let Some(id) = written.strip_prefix("id:") {
        (id.to_owned(), None)
    } else if written.starts_with('*') || written.starts_with('#') {
        split_search(&format!("::{written}"))
    } else if let Some((prefix, rest)) = written.split_once(':')
        && is_prefix(prefix)
        && !rest.starts_with(':')
    {
        confidence = Confidence::External;
        if let Some(template) = abbrevs.get(prefix) {
            raw_s = if template.contains("%s") {
                template.replace("%s", rest)
            } else {
                format!("{template}{rest}")
            };
        }
        (raw_s.clone(), None)
    } else if written.contains("::") {
        split_search(written)
    } else {
        (written.to_owned(), None)
    };
    let shift = |r: Range<usize>| r.start + at..r.end + at;
    Link {
        kind: LinkKind::Org,
        context: ctx,
        confidence,
        target: LinkTarget {
            raw: raw_s,
            path,
            anchor,
            line: None,
        },
        label: raw.desc.clone().map(|d| line[d].to_owned()),
        range: shift(raw.range),
        text_range: shift(raw.desc.unwrap_or(raw.target)),
        group: None,
    }
}

/// `path::*Heading` / `path::#custom` / `path::text`.
fn split_search(s: &str) -> (String, Option<Anchor>) {
    let Some((path, search)) = s.split_once("::") else {
        return (s.to_owned(), None);
    };
    let anchor = if let Some(h) = search.strip_prefix('*') {
        Some(Anchor::Heading(h.to_owned()))
    } else if let Some(c) = search.strip_prefix('#') {
        Some(Anchor::CustomId(c.to_owned()))
    } else if search.is_empty() {
        None
    } else {
        Some(Anchor::Search(search.to_owned()))
    };
    (path.to_owned(), anchor)
}

/// A link-type prefix: a letter, then letters, digits, `+`, `-`, `.`, `_`.
fn is_prefix(p: &str) -> bool {
    let mut chars = p.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.' | '_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(src: &str, needle: &str) -> Context {
        let at = src.find(needle).expect("needle");
        parse_org(src).1.context_at(at)
    }

    #[test]
    fn contexts() {
        let src = "#+begin_src rust\nlet x = [[a]];\n#+end_src\nafter\n";
        assert_eq!(ctx(src, "[[a]]"), Context::CodeBlock);
        assert_eq!(ctx(src, "#+end_src"), Context::CodeBlock);
        assert_eq!(ctx(src, "after"), Context::Prose);
        assert_eq!(ctx("#+BEGIN_SRC\nx\n#+END_SRC\n", "x"), Context::CodeBlock);
        assert_eq!(ctx("#+begin_example\nx\n", "x"), Context::CodeBlock);
        assert_eq!(
            ctx("#+begin_comment\nx\n#+end_comment\n", "x"),
            Context::Comment
        );
        assert_eq!(ctx("a\n: fixed [[x]]\nb\n", "[[x]]"), Context::CodeBlock);
        assert_eq!(ctx("a\n# note [[x]]\n", "[[x]]"), Context::Comment);
        assert_eq!(ctx("a\n#tag\n", "#tag"), Context::Prose);
        assert_eq!(ctx("a ~code~ b", "code"), Context::InlineCode);
        assert_eq!(ctx("a =verb= b", "verb"), Context::InlineCode);
        assert_eq!(ctx("a=b= c", "b"), Context::Prose);
        assert_eq!(ctx("* H ~c~ d\n", "c"), Context::InlineCode);
        assert_eq!(ctx("* H ~c~ d\n", "d"), Context::Heading);
        assert_eq!(ctx("#+TITLE: t\n\nbody\n", "t\n"), Context::Frontmatter);
        assert_eq!(ctx("#+TITLE: t\n\nbody\n", "body"), Context::Prose);
        assert_eq!(
            ctx("x\n#+LINK: T https://t/%s\n", "https"),
            Context::Comment
        );
        assert_eq!(
            ctx("* H\n:PROPERTIES:\n:ID: [[x]]\n:END:\n", "[[x]]"),
            Context::Comment
        );
    }

    #[test]
    fn tildes_in_links_are_not_code() {
        let src = "[[file:~/a]] and [[file:~/b]]\n";
        let (els, regions, _, _) = parse_org(src);
        let links = els.iter().filter(|e| matches!(e, Element::Link(_))).count();
        assert_eq!(links, 2);
        assert!(regions.segments().all(|(_, c)| c != Context::InlineCode));
    }

    #[test]
    fn link_in_inline_code_is_left_to_scan() {
        let (els, regions, _, _) = parse_org("a =x [[y]] z= b\n");
        assert!(els.iter().all(|e| !matches!(e, Element::Link(_))));
        assert_eq!(regions.context_at(5), Context::InlineCode);
    }
}
