//! The public document model: what `parse` returns.

use std::ops::Range;
use std::path::Path;
use std::sync::OnceLock;

use crate::line_index::LineIndex;

/// Source dialect of a document.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Dialect {
    #[default]
    Markdown,
    Org,
}

impl Dialect {
    /// `.org` (any case) is Org; everything else is Markdown.
    pub fn detect_from_path(p: &Path) -> Dialect {
        match p.extension() {
            Some(ext) if ext.eq_ignore_ascii_case("org") => Dialect::Org,
            _ => Dialect::Markdown,
        }
    }
}

/// Options for one parse.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ParseOptions {
    pub dialect: Dialect,
    /// `#tag` (default true).
    pub hashtags: bool,
    /// `:a:b:` colon tags (default false).
    pub colon_tags: bool,
    /// `#multi word#` tags (default false).
    pub multiword_tags: bool,
}

impl ParseOptions {
    pub fn new(dialect: Dialect) -> Self {
        ParseOptions {
            dialect,
            hashtags: true,
            colon_tags: false,
            multiword_tags: false,
        }
    }
}

impl Default for ParseOptions {
    fn default() -> Self {
        ParseOptions::new(Dialect::Markdown)
    }
}

/// One parsed document. Ranges in its elements are byte offsets into `source()`.
#[derive(Debug, Clone)]
pub struct Document {
    pub(crate) source: String,
    pub(crate) dialect: Dialect,
    pub(crate) elements: Vec<Element>,
    pub(crate) frontmatter: Option<Frontmatter>,
    pub(crate) line_index: OnceLock<LineIndex>,
    pub(crate) lossy: bool,
}

impl PartialEq for Document {
    /// Ignores the lazily built line index.
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source
            && self.dialect == other.dialect
            && self.elements == other.elements
            && self.frontmatter == other.frontmatter
            && self.lossy == other.lossy
    }
}

impl Document {
    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn dialect(&self) -> Dialect {
        self.dialect
    }

    /// All elements, sorted by `(range.start, range.end)`.
    pub fn elements(&self) -> &[Element] {
        &self.elements
    }

    pub fn headings(&self) -> impl Iterator<Item = &Heading> {
        self.elements.iter().filter_map(|e| match e {
            Element::Heading(h) => Some(h),
            _ => None,
        })
    }

    pub fn links(&self) -> impl Iterator<Item = &Link> {
        self.elements.iter().filter_map(|e| match e {
            Element::Link(l) => Some(l),
            _ => None,
        })
    }

    pub fn tags(&self) -> impl Iterator<Item = &Tag> {
        self.elements.iter().filter_map(|e| match e {
            Element::Tag(t) => Some(t),
            _ => None,
        })
    }

    pub fn link_defs(&self) -> impl Iterator<Item = &LinkDef> {
        self.elements.iter().filter_map(|e| match e {
            Element::LinkDef(d) => Some(d),
            _ => None,
        })
    }

    pub fn frontmatter(&self) -> Option<&Frontmatter> {
        self.frontmatter.as_ref()
    }

    /// Line index over the source, built on first use.
    pub fn line_index(&self) -> &LineIndex {
        self.line_index.get_or_init(|| LineIndex::new(&self.source))
    }

    /// True when the bytes were not valid UTF-8 and had to be replaced.
    pub fn is_lossy(&self) -> bool {
        self.lossy
    }
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Element {
    Heading(Heading),
    Link(Link),
    Tag(Tag),
    LinkDef(LinkDef),
}

impl Element {
    /// The element's byte range in the source.
    pub fn range(&self) -> Range<usize> {
        match self {
            Element::Heading(h) => h.range.clone(),
            Element::Link(l) => l.range.clone(),
            Element::Tag(t) => t.range.clone(),
            Element::LinkDef(d) => d.range.clone(),
        }
    }
}

/// A heading. An anchor `#x` may name it by `slug`, `id` or `custom_id`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Heading {
    pub level: u8,
    pub text: String,
    /// GitHub slug of `text`, deduplicated per document (`-1`, `-2`, …).
    pub slug: String,
    pub range: Range<usize>,
    /// Markdown `{#id}` attribute as written (not slugged or deduplicated),
    /// or org `:ID:`.
    pub id: Option<String>,
    /// Org `:CUSTOM_ID:`.
    pub custom_id: Option<String>,
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Link {
    pub kind: LinkKind,
    pub context: Context,
    pub confidence: Confidence,
    pub target: LinkTarget,
    pub label: Option<String>,
    pub range: Range<usize>,
    pub text_range: Range<usize>,
    /// Shared by links that came from one comma-joined frontmatter list.
    pub group: Option<u32>,
}

impl Link {
    /// A link to `raw`, with its anchor split off like the structure pass
    /// does (`#h` → heading, `#^b` → block). `range` and `text_range` are
    /// `0..raw.len()`.
    pub fn new(kind: LinkKind, context: Context, confidence: Confidence, raw: &str) -> Link {
        Link {
            kind,
            context,
            confidence,
            target: LinkTarget::parse(raw),
            label: None,
            range: 0..raw.len(),
            text_range: 0..raw.len(),
            group: None,
        }
    }

    pub fn with_label(mut self, label: &str) -> Self {
        self.label = Some(label.to_owned());
        self
    }

    pub fn with_line(mut self, line: u32) -> Self {
        self.target.line = Some(line);
        self
    }

    pub fn with_anchor(mut self, anchor: Anchor) -> Self {
        self.target.anchor = Some(anchor);
        self
    }

    pub fn with_group(mut self, group: u32) -> Self {
        self.group = Some(group);
        self
    }
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LinkTarget {
    /// As written (after `#+LINK` expansion).
    pub raw: String,
    /// `raw` minus the anchor or search part; not percent-decoded.
    pub path: String,
    pub anchor: Option<Anchor>,
    /// Line from a code mention `path:LINE`.
    pub line: Option<u32>,
}

impl LinkTarget {
    /// Split `raw` at the first `#`: `#^x` is a block anchor, `#x` a
    /// heading anchor, an empty fragment no anchor.
    pub(crate) fn parse(raw: &str) -> LinkTarget {
        let (path, anchor) = match raw.split_once('#') {
            None => (raw, None),
            Some((path, frag)) => {
                let anchor = match frag.strip_prefix('^') {
                    Some(b) => Some(Anchor::Block(b.to_owned())),
                    None if frag.is_empty() => None,
                    None => Some(Anchor::Heading(frag.to_owned())),
                };
                (path, anchor)
            }
        };
        LinkTarget {
            raw: raw.to_owned(),
            path: path.to_owned(),
            anchor,
            line: None,
        }
    }
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Anchor {
    Heading(String),
    Block(String),
    CustomId(String),
    Search(String),
}

#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LinkKind {
    Markdown,
    Reference,
    Autolink,
    Image,
    Wiki,
    WikiEmbed,
    Org,
    BarePath,
    Url,
    CodeMention,
    Html,
    Templating,
    Footnote,
}

/// Where a candidate sits; decides what features treat it as a link.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Context {
    Prose,
    Heading,
    Frontmatter,
    Html,
    CodeBlock,
    InlineCode,
    Comment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Confidence {
    /// A link form that is always a link.
    Explicit,
    /// A link only if the target exists.
    Implicit,
    /// Outside the notebook; never diagnosed.
    External,
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Tag {
    pub name: String,
    pub syntax: TagSyntax,
    pub range: Range<usize>,
}

#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TagSyntax {
    Hash,
    MultiWord,
    Colon,
    Frontmatter,
    OrgHeading,
}

/// A markdown reference definition `[label]: dest`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LinkDef {
    pub label: String,
    pub dest: String,
    pub range: Range<usize>,
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Frontmatter {
    pub format: FrontmatterFormat,
    pub range: Range<usize>,
    pub error: Option<String>,
    entries: Vec<(String, Value)>,
}

impl Frontmatter {
    pub fn from_entries(
        format: FrontmatterFormat,
        range: Range<usize>,
        entries: Vec<(String, Value)>,
        error: Option<String>,
    ) -> Self {
        Frontmatter {
            format,
            range,
            error,
            entries,
        }
    }

    /// Keys as written, in document order.
    pub fn entries(&self) -> &[(String, Value)] {
        &self.entries
    }

    /// Value of the first entry whose key is exactly `key`.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
}

#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrontmatterFormat {
    Yaml,
    Toml,
    Json,
    Logseq,
    MultiMarkdown,
    OrgKeywords,
}

/// A frontmatter value; nested maps are flattened to `a.b` keys.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Value {
    Null,
    Str(String),
    List(Vec<String>),
}
