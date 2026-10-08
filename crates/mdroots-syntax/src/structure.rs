//! Structure pass: one pulldown-cmark walk gives regions, headings, links,
//! reference definitions and the frontmatter block; then the liberal scan.

use std::borrow::Cow;
use std::collections::HashSet;
use std::ops::Range;
use std::sync::OnceLock;

use pulldown_cmark::{Event, LinkType, MetadataBlockKind, Parser, Tag, TagEnd};

use crate::model::{
    Confidence, Context, Dialect, Document, Element, Frontmatter, FrontmatterFormat, Heading, Link,
    LinkDef, LinkKind, ParseOptions,
};
use crate::regions::Regions;
use crate::{frontmatter, markdown_options, org, scan, slug};

/// Parse `src` into a document (no lossy flag).
pub(crate) fn parse_document(src: String, opts: &ParseOptions) -> Document {
    let (mut elements, regions, fm, abbrevs) = match opts.dialect {
        Dialect::Org => org::parse_org(&src, opts),
        _ => {
            let pass = markdown(&src);
            (
                pass.elements,
                pass.regions,
                pass.frontmatter,
                Default::default(),
            )
        }
    };
    scan::scan(&src, &regions, opts, &abbrevs, &mut elements);
    elements.sort_by_key(|e| {
        let r = e.range();
        (r.start, r.end)
    });
    Document {
        source: src,
        dialect: opts.dialect,
        elements,
        frontmatter: fm,
        line_index: OnceLock::new(),
        lossy: false,
    }
}

pub(crate) struct MarkdownPass {
    pub(crate) elements: Vec<Element>,
    pub(crate) regions: Regions,
    pub(crate) frontmatter: Option<Frontmatter>,
}

/// A link while its events are still open.
struct Open {
    link: Link,
    /// Union of the visible text events so far.
    span: Option<Range<usize>>,
    /// Concatenated visible text (the wiki label).
    text: String,
    has_pothole: bool,
    wiki: bool,
}

/// The markdown structure pass (everything but the scan).
pub(crate) fn markdown(src: &str) -> MarkdownPass {
    // A UTF-8 BOM hides a leading metadata block from pulldown: parse after it.
    let base = if src.starts_with('\u{feff}') { 3 } else { 0 };
    // pulldown reads a leading block with blank lines only between its
    // fences as rules or text: detect it and blank its fences.
    let empty = empty_front_matter(&src[base..]);
    let body = mask(&src[base..], empty.as_ref().map(|(r, _)| r.clone()));
    let body = body.as_ref();
    let shift = |r: Range<usize>| r.start + base..r.end + base;

    let mut regions = Regions::new(src.len());
    let mut elements = Vec::new();
    let mut links: Vec<Link> = Vec::new();
    let mut open: Vec<Open> = Vec::new();
    let mut heading: Option<Heading> = None;
    let mut used_slugs = HashSet::new();
    let mut html_block: Option<usize> = None;
    let mut fm_block: Option<(Range<usize>, FrontmatterFormat)> = None;
    if let Some((range, format)) = empty {
        let range = shift(range);
        regions.push(range.clone(), Context::Frontmatter);
        fm_block = Some((range, format));
    }

    let mut iter = Parser::new_ext(body, markdown_options()).into_offset_iter();
    for (event, range) in &mut iter {
        let range = shift(range);
        match event {
            Event::Start(Tag::MetadataBlock(kind)) => {
                if range.start == base && fm_block.is_none() {
                    let format = match kind {
                        MetadataBlockKind::YamlStyle => FrontmatterFormat::Yaml,
                        MetadataBlockKind::PlusesStyle => FrontmatterFormat::Toml,
                    };
                    regions.push(range.clone(), Context::Frontmatter);
                    fm_block = Some((range, format));
                }
            }
            Event::Start(Tag::Heading { level, id, .. }) => {
                regions.push(range.clone(), Context::Heading);
                heading = Some(Heading {
                    level: level as u8,
                    text: String::new(),
                    slug: String::new(),
                    range,
                    id: id.map(|s| s.to_string()),
                    custom_id: None,
                });
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some(mut h) = heading.take() {
                    // Always from the text; an explicit `{#id}` stays in `id`.
                    h.slug = slug::unique(&slug::github(&h.text), &mut used_slugs);
                    elements.push(Element::Heading(h));
                }
            }
            Event::Start(Tag::CodeBlock(_)) => regions.push(range, Context::CodeBlock),
            Event::Start(Tag::HtmlBlock) => html_block = Some(range.start),
            Event::End(TagEnd::HtmlBlock) => {
                if let Some(start) = html_block.take() {
                    let block = start..range.end;
                    let ctx = html_context(&src[block.clone()]);
                    regions.push(block, ctx);
                }
            }
            Event::Html(_) if html_block.is_none() => {
                let ctx = html_context(&src[range.clone()]);
                regions.push(range, ctx);
            }
            Event::InlineHtml(_) => {
                let ctx = html_context(&src[range.clone()]);
                regions.push(range.clone(), ctx);
                add_text(&mut open, &range, "");
            }
            Event::Code(t) => {
                regions.push(range.clone(), Context::InlineCode);
                add_heading_text(&mut heading, &t);
                add_text(&mut open, &range, &t);
            }
            Event::InlineMath(t) => {
                regions.push(range.clone(), Context::CodeBlock);
                add_heading_text(&mut heading, &t);
                add_text(&mut open, &range, &t);
            }
            Event::DisplayMath(t) => {
                regions.push(range.clone(), Context::CodeBlock);
                add_text(&mut open, &range, &t);
            }
            Event::Text(t) => {
                add_heading_text(&mut heading, &t);
                add_text(&mut open, &range, &t);
            }
            Event::SoftBreak | Event::HardBreak => add_heading_text(&mut heading, " "),
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                ..
            }) => open.push(open_link(link_type, &dest_url, range, false)),
            Event::Start(Tag::Image {
                link_type,
                dest_url,
                ..
            }) => open.push(open_link(link_type, &dest_url, range, true)),
            Event::End(TagEnd::Link | TagEnd::Image) => {
                if let Some(o) = open.pop()
                    && let Some(link) = close_link(src, o)
                {
                    links.push(link);
                }
            }
            _ => {}
        }
    }

    for (label, def) in iter.reference_definitions().iter() {
        let span = shift(def.span.clone());
        let dest = def.dest.to_string();
        elements.push(Element::LinkDef(LinkDef {
            label: label.to_owned(),
            dest: dest.clone(),
            range: span.clone(),
        }));
        let mut link = Link::new(LinkKind::Reference, Context::Prose, by_scheme(&dest), &dest);
        link.range = span.clone();
        link.text_range = span;
        links.push(link);
    }

    for mut link in links {
        link.context = regions.context_at(link.range.start);
        elements.push(Element::Link(link));
    }

    let mut fm = None;
    if let Some((range, format)) = fm_block {
        let (f, els) = frontmatter::parse_block(src, range, format);
        fm = Some(f);
        elements.extend(els);
    } else if let Some((range, format)) = frontmatter::detect_unfenced(src) {
        // pulldown saw these bytes as prose; the frontmatter pass owns them.
        elements.retain(|e| !range.contains(&e.range().start));
        regions.push(range.clone(), Context::Frontmatter);
        let (f, els) = frontmatter::parse_block(src, range, format);
        fm = Some(f);
        elements.extend(els);
    }

    MarkdownPass {
        elements,
        regions,
        frontmatter: fm,
    }
}

/// A leading frontmatter block with nothing but blank lines between its
/// fences (`---` / `---` or `+++` / `+++`): its range, fences included
/// (not the closing newline, like pulldown's metadata blocks), and format.
fn empty_front_matter(src: &str) -> Option<(Range<usize>, FrontmatterFormat)> {
    let mut lines = src.split_inclusive('\n');
    let first = lines.next()?;
    let fence = first.trim_end();
    let format = match fence {
        "---" => FrontmatterFormat::Yaml,
        "+++" => FrontmatterFormat::Toml,
        _ => return None,
    };
    let mut at = first.len();
    for line in lines {
        let t = line.trim_end();
        if t == fence {
            return Some((0..at + fence.len(), format));
        }
        if !t.is_empty() {
            return None;
        }
        at += line.len();
    }
    None
}

/// `src` with the non-blank bytes of `range` (an empty frontmatter block,
/// ASCII only) replaced by spaces, offsets kept, so pulldown sees no rules.
fn mask(src: &str, range: Option<Range<usize>>) -> Cow<'_, str> {
    let Some(r) = range else {
        return Cow::Borrowed(src);
    };
    let mut m = src.as_bytes().to_vec();
    m[r].iter_mut()
        .filter(|b| !b.is_ascii_whitespace())
        .for_each(|b| *b = b' ');
    Cow::Owned(String::from_utf8(m).expect("ASCII replaced by ASCII"))
}

fn html_context(text: &str) -> Context {
    if text.trim_start().starts_with("<!--") {
        Context::Comment
    } else {
        Context::Html
    }
}

fn add_heading_text(heading: &mut Option<Heading>, t: &str) {
    if let Some(h) = heading {
        h.text.push_str(t);
    }
}

fn add_text(open: &mut [Open], range: &Range<usize>, t: &str) {
    for o in open {
        o.span = Some(match o.span.take() {
            None => range.clone(),
            Some(s) => s.start.min(range.start)..s.end.max(range.end),
        });
        o.text.push_str(t);
    }
}

fn open_link(link_type: LinkType, dest: &str, range: Range<usize>, image: bool) -> Open {
    let (kind, has_pothole, wiki) = match (link_type, image) {
        (LinkType::WikiLink { has_pothole }, false) => (LinkKind::Wiki, has_pothole, true),
        (LinkType::WikiLink { has_pothole }, true) => (LinkKind::WikiEmbed, has_pothole, true),
        (_, true) => (LinkKind::Image, false, false),
        (LinkType::Inline, _) => (LinkKind::Markdown, false, false),
        (LinkType::Autolink | LinkType::Email, _) => (LinkKind::Autolink, false, false),
        // Reference, Collapsed, Shortcut and their (never produced) *Unknown variants.
        _ => (LinkKind::Reference, false, false),
    };
    let confidence = if kind == LinkKind::Autolink {
        Confidence::External
    } else {
        by_scheme(dest)
    };
    let mut link = Link::new(kind, Context::Prose, confidence, dest);
    link.range = range.clone();
    link.text_range = range.start..range.start;
    Open {
        link,
        span: None,
        text: String::new(),
        has_pothole,
        wiki,
    }
}

/// Finish an open link; `None` drops it (a wikilink spanning lines).
fn close_link(src: &str, o: Open) -> Option<Link> {
    let mut link = o.link;
    link.text_range = o.span.unwrap_or_else(|| {
        // `[](x)` / `![](x)`: empty text just inside the opening bracket.
        let open = src[link.range.clone()].find('[').map_or(0, |i| i + 1);
        let at = (link.range.start + open).min(link.range.end);
        at..at
    });
    if !o.wiki {
        return Some(link);
    }
    let text = &src[link.range.clone()];
    if text.contains('\n') {
        return None;
    }
    if let Some((target, desc)) = link.target.raw.clone().split_once("][") {
        // pulldown reads org `[[t][d]]` as a wikilink to `t][d`.
        let confidence = by_scheme(target);
        let mut org = Link::new(LinkKind::Org, Context::Prose, confidence, target);
        org.label = Some(desc.to_owned());
        org.range = link.range.clone();
        org.text_range = link.text_range.clone();
        if let Some(open) = text.find("[[") {
            let start = link.range.start + open + 2 + target.len() + 2;
            let end = start + desc.len();
            if end <= link.range.end && src.get(start..end) == Some(desc) {
                org.text_range = start..end;
            }
        }
        return Some(org);
    }
    if o.has_pothole {
        link.label = Some(o.text);
    }
    Some(link)
}

/// External when `dest` starts with a URL scheme other than `file`.
pub(crate) fn by_scheme(dest: &str) -> Confidence {
    match scheme(dest) {
        Some(s) if !s.eq_ignore_ascii_case("file") => Confidence::External,
        _ => Confidence::Explicit,
    }
}

/// RFC 3986 scheme of at least two chars (so `C:\x` is not one).
fn scheme(dest: &str) -> Option<&str> {
    let (s, _) = dest.split_once(':')?;
    let mut chars = s.chars();
    let ok = s.len() >= 2
        && chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    ok.then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(src: &str, needle: &str) -> Context {
        let at = src.find(needle).expect("needle");
        markdown(src).regions.context_at(at)
    }

    #[test]
    fn contexts() {
        assert_eq!(ctx("```\n[x](y)\n```\n", "[x]"), Context::CodeBlock);
        assert_eq!(ctx("~~~~\n[[w]]\n~~~~\n", "[[w]]"), Context::CodeBlock);
        assert_eq!(ctx("a\n\n    [[w]]\n", "[[w]]"), Context::CodeBlock);
        assert_eq!(ctx("a `[[w]]` b", "[[w]]"), Context::InlineCode);
        assert_eq!(ctx("a <!-- c --> b", " c "), Context::Comment);
        assert_eq!(ctx("<!-- a\nb --> x\n", "b -->"), Context::Comment);
        assert_eq!(ctx("a <span>x</span> b", "<span>"), Context::Html);
        assert_eq!(ctx("<div>\n[[w]]\n</div>\n", "[[w]]"), Context::Html);
        assert_eq!(ctx("a $x$ b", "x"), Context::CodeBlock);
        assert_eq!(ctx("$$\nx\n$$\n", "x"), Context::CodeBlock);
        assert_eq!(ctx("# a `c` d\n", "c"), Context::InlineCode);
        assert_eq!(ctx("# a `c` d\n", "d"), Context::Heading);
        assert_eq!(ctx("---\nk: v\n---\nbody", "k"), Context::Frontmatter);
        assert_eq!(ctx("---\nk: v\n---\nbody", "body"), Context::Prose);
        assert_eq!(ctx("text\n\n---\nk: v\n---\n", "k"), Context::Prose);
        assert_eq!(ctx("---\n---\nbody", "---"), Context::Frontmatter);
        assert_eq!(ctx("---\n---\nbody", "body"), Context::Prose);
        assert_eq!(ctx("+++\n\n+++\n", "+++"), Context::Frontmatter);
        assert_eq!(ctx("\u{feff}---\n---\n", "---"), Context::Frontmatter);
    }

    #[test]
    fn segments_partition_source() {
        let src = "# h `c`\n\ntext <!-- x -->\n```\ncode\n```\n";
        let regions = markdown(src).regions;
        let mut at = 0;
        for (r, c) in regions.segments() {
            assert_eq!(r.start, at);
            assert!(r.end > r.start);
            assert_eq!(regions.context_at(r.start), c);
            at = r.end;
        }
        assert_eq!(at, src.len());
    }
}
