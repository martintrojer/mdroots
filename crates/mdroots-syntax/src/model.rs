//! The public document model: what `parse` returns.

use std::fmt;
use std::hash::{Hash, Hasher};
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
    /// Unfenced markdown headers at the top (Logseq `key:: value`,
    /// MultiMarkdown `Key: value`, a JSON object) are front matter
    /// (default true). When false they stay prose; fenced `---` / `+++`
    /// blocks and org keywords are unaffected.
    pub unfenced_frontmatter: bool,
}

impl ParseOptions {
    pub fn new(dialect: Dialect) -> Self {
        ParseOptions {
            dialect,
            hashtags: true,
            colon_tags: false,
            multiword_tags: false,
            unfenced_frontmatter: true,
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
#[derive(Clone)]
pub struct Link {
    pub kind: LinkKind,
    pub context: Context,
    pub confidence: Confidence,
    /// Also holds the link's label and group, which are rare.
    target: LinkTarget,
    pub range: Range<usize>,
    pub text_range: Range<usize>,
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
            range: 0..raw.len(),
            text_range: 0..raw.len(),
        }
    }

    pub fn target(&self) -> &LinkTarget {
        &self.target
    }

    /// The link text when it differs from the target (`[[t|label]]`, org
    /// `[[t][label]]`, a markdown link's text is not kept here).
    pub fn label(&self) -> Option<&str> {
        self.target.extra.as_ref()?.label.as_deref()
    }

    /// Shared by links that came from one comma-joined frontmatter list.
    pub fn group(&self) -> Option<u32> {
        self.target.extra.as_ref()?.group
    }

    /// This link pointing at `target`, keeping its label and group.
    pub fn with_target(mut self, target: LinkTarget) -> Self {
        let (label, group) = (self.label().map(Box::from), self.group());
        self.target = target;
        self.set_label(label);
        self.set_group(group);
        self
    }

    pub fn with_label(mut self, label: &str) -> Self {
        self.set_label(Some(label.into()));
        self
    }

    pub fn with_line(mut self, line: u32) -> Self {
        self.target.set_line(Some(line));
        self
    }

    pub fn with_anchor(mut self, anchor: Anchor) -> Self {
        self.target.set_anchor(Some(anchor));
        self
    }

    pub fn with_group(mut self, group: u32) -> Self {
        self.set_group(Some(group));
        self
    }

    pub(crate) fn target_mut(&mut self) -> &mut LinkTarget {
        &mut self.target
    }

    pub(crate) fn set_label(&mut self, label: Option<Box<str>>) {
        self.target.edit(|x| x.label = label);
    }

    pub(crate) fn set_group(&mut self, group: Option<u32>) {
        self.target.edit(|x| x.group = group);
    }
}

impl Link {
    #[allow(clippy::type_complexity)]
    fn key(
        &self,
    ) -> (
        LinkKind,
        Context,
        Confidence,
        &LinkTarget,
        Option<&str>,
        &Range<usize>,
        &Range<usize>,
        Option<u32>,
    ) {
        (
            self.kind,
            self.context,
            self.confidence,
            &self.target,
            self.label(),
            &self.range,
            &self.text_range,
            self.group(),
        )
    }
}

impl PartialEq for Link {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl Eq for Link {}

impl Hash for Link {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.key().hash(state);
    }
}

impl fmt::Debug for Link {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Link")
            .field("kind", &self.kind)
            .field("context", &self.context)
            .field("confidence", &self.confidence)
            .field("target", &self.target)
            .field("label", &self.label())
            .field("range", &self.range)
            .field("text_range", &self.text_range)
            .field("group", &self.group())
            .finish()
    }
}

/// A link's target: `raw` as written and `path`, a part of it. Parts most
/// links lack (anchor, line, and the link's label and group) sit behind
/// one box, so a plain link costs 32 bytes plus its text.
#[non_exhaustive]
#[derive(Clone)]
pub struct LinkTarget {
    raw: Box<str>,
    /// `raw[path.0..path.1]`.
    path: (u32, u32),
    /// `None` when every part is empty, so equal targets compare equal.
    extra: Option<Box<LinkExtra>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct LinkExtra {
    label: Option<Box<str>>,
    anchor: Option<Anchor>,
    line: Option<u32>,
    group: Option<u32>,
}

impl LinkTarget {
    /// A target `raw` whose path is `raw[path]`. A `path` that is not a
    /// range of `raw` on char boundaries (or ends past 4 GiB) is a bug: it
    /// panics in debug builds and gives an empty path in release ones, so
    /// parsing stays total.
    pub fn new(raw: &str, path: Range<usize>, anchor: Option<Anchor>) -> LinkTarget {
        debug_assert!(
            raw.get(path.clone()).is_some(),
            "path {path:?} is not part of {raw:?}"
        );
        let path = match (u32::try_from(path.start), u32::try_from(path.end)) {
            (Ok(a), Ok(b)) if raw.get(path).is_some() => (a, b),
            _ => (0, 0),
        };
        let mut t = LinkTarget {
            raw: raw.into(),
            path,
            extra: None,
        };
        t.set_anchor(anchor);
        t
    }

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
        LinkTarget::new(raw, 0..path.len(), anchor)
    }

    /// As written (after `#+LINK` expansion).
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// `raw` minus the anchor or search part; not percent-decoded.
    pub fn path(&self) -> &str {
        &self.raw[self.path.0 as usize..self.path.1 as usize]
    }

    pub fn anchor(&self) -> Option<&Anchor> {
        self.extra.as_ref()?.anchor.as_ref()
    }

    /// Line from a code mention `path:LINE`.
    pub fn line(&self) -> Option<u32> {
        self.extra.as_ref()?.line
    }

    pub(crate) fn set_anchor(&mut self, anchor: Option<Anchor>) {
        self.edit(|x| x.anchor = anchor);
    }

    pub(crate) fn set_line(&mut self, line: Option<u32>) {
        self.edit(|x| x.line = line);
    }

    /// Change the boxed parts; drops the box when they end up empty.
    fn edit(&mut self, f: impl FnOnce(&mut LinkExtra)) {
        let x = self.extra.get_or_insert_with(Box::default);
        f(x);
        if **x == LinkExtra::default() {
            self.extra = None;
        }
    }
}

/// Compares what [`LinkTarget`] shows: raw, path, anchor and line (not the
/// owning link's label or group stored alongside).
impl PartialEq for LinkTarget {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl Eq for LinkTarget {}

impl Hash for LinkTarget {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.key().hash(state);
    }
}

impl LinkTarget {
    /// What equality and hashing compare: the values the accessors show,
    /// not where `path` sits inside `raw`.
    fn key(&self) -> (&str, &str, Option<&Anchor>, Option<u32>) {
        (&self.raw, self.path(), self.anchor(), self.line())
    }
}

impl fmt::Debug for LinkTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LinkTarget")
            .field("raw", &self.raw())
            .field("path", &self.path())
            .field("anchor", &self.anchor())
            .field("line", &self.line())
            .finish()
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
    fields: Vec<Field>,
    parsed: bool,
    inner: Range<usize>,
}

impl Frontmatter {
    /// A frontmatter block from flattened entries. Its [`fields`](Self::fields)
    /// are the same entries (`Value::Null` as an empty scalar), each with
    /// `range` as its range; [`parsed`](Self::parsed) is true and
    /// [`inner`](Self::inner) is `range`.
    pub fn from_entries(
        format: FrontmatterFormat,
        range: Range<usize>,
        entries: Vec<(String, Value)>,
        error: Option<String>,
    ) -> Self {
        let fields = entries
            .iter()
            .map(|(k, v)| Field::from_value(k, v, "", range.clone()))
            .collect();
        Frontmatter {
            format,
            inner: range.clone(),
            range,
            error,
            entries,
            fields,
            parsed: true,
        }
    }

    /// Replace the fields, the parsed flag and the inner range.
    pub(crate) fn with_fields(
        mut self,
        fields: Vec<Field>,
        parsed: bool,
        inner: Range<usize>,
    ) -> Self {
        self.fields = fields;
        self.parsed = parsed;
        self.inner = inner;
        self
    }

    /// Keys as written, in document order.
    pub fn entries(&self) -> &[(String, Value)] {
        &self.entries
    }

    /// Value of the first entry whose key is exactly `key`.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// The top-level entries as written, in source order, before nested
    /// maps are flattened: duplicate keys all show, scalars keep their
    /// source text (`null` stays `"null"`), nested maps keep their
    /// children. For YAML and TOML, keys the parser alone found (TOML
    /// tables and dotted keys, flow maps) come as the parser reports them.
    /// JSON entries are flattened to `a.b` keys.
    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    /// False when the block is not blank but no key could be read from it.
    pub fn parsed(&self) -> bool {
        self.parsed
    }

    /// The bytes between the fences: after the opening fence line and
    /// before the closing one. For unfenced formats, `range`.
    pub fn inner(&self) -> Range<usize> {
        self.inner.clone()
    }
}

/// A frontmatter entry as written, from [`Frontmatter::fields`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Field {
    pub key: String,
    pub value: FieldValue,
    /// The entry's lines in [`Document::source`]. When they can't be
    /// found: the block's inner range for a top-level field, the parent's
    /// range for a nested one.
    pub range: Range<usize>,
}

impl Field {
    pub(crate) fn from_value(key: &str, v: &Value, null: &str, range: Range<usize>) -> Field {
        let value = match v {
            Value::Null => FieldValue::Scalar(null.to_owned()),
            Value::Str(s) => FieldValue::Scalar(s.clone()),
            Value::List(items) => FieldValue::List(items.clone()),
        };
        Field {
            key: key.to_owned(),
            value,
            range,
        }
    }
}

/// The value of a [`Field`], text as written on one line.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FieldValue {
    Scalar(String),
    List(Vec<String>),
    /// A nested map and its entries (empty when unknown).
    Map(Vec<Field>),
}

impl FieldValue {
    /// The value on one line: a scalar as is, list items joined with `, `,
    /// a map as `{…}`.
    pub fn display(&self) -> String {
        match self {
            FieldValue::Scalar(s) => s.clone(),
            FieldValue::List(items) => items.join(", "),
            FieldValue::Map(_) => "{…}".into(),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every parsed link lives as long as the workspace: 50,000 links at
    /// 10,000 notes. Rare parts (label, anchor, line, group) sit behind
    /// one box so the common link stays small.
    #[test]
    fn link_sizes() {
        assert!(size_of::<LinkTarget>() <= 32, "{}", size_of::<LinkTarget>());
        assert!(size_of::<Link>() <= 72, "{}", size_of::<Link>());
    }

    fn link(raw: &str) -> Link {
        Link::new(LinkKind::Wiki, Context::Prose, Confidence::Explicit, raw)
    }

    #[test]
    fn rare_parts_box_only_when_set() {
        let plain = link("a#h");
        assert_eq!(plain.target().path(), "a");
        assert!(plain.target().extra.is_some(), "anchor is boxed");
        assert!(link("a").target().extra.is_none());
        let mut l = link("a").with_label("L").with_group(2);
        assert_eq!((l.label(), l.group()), (Some("L"), Some(2)));
        l.set_label(None);
        l.set_group(None);
        assert!(l.target().extra.is_none(), "emptied box is dropped");
        assert_eq!(l, link("a"));
    }

    #[test]
    fn target_equality_ignores_link_parts() {
        let a = link("a").with_label("L");
        assert_eq!(a.target(), link("a").target());
        assert_ne!(a, link("a"));
        let moved = link("b")
            .with_label("L")
            .with_group(1)
            .with_target(LinkTarget::new("a", 0..1, None));
        assert_eq!((moved.label(), moved.group()), (Some("L"), Some(1)));
        assert_eq!(moved.target(), link("a").target());
    }

    #[test]
    fn target_equality_compares_the_path_text_not_its_offset() {
        use std::hash::{DefaultHasher, Hash, Hasher};
        let (a, b) = (
            LinkTarget::new("aa", 0..1, None),
            LinkTarget::new("aa", 1..2, None),
        );
        assert_eq!((a.path(), b.path()), ("a", "a"));
        assert_eq!(a, b);
        let h = |t: &LinkTarget| {
            let mut s = DefaultHasher::new();
            t.hash(&mut s);
            s.finish()
        };
        assert_eq!(h(&a), h(&b));
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "is not part of")]
    fn target_path_must_be_part_of_raw() {
        let _ = LinkTarget::new("ab", 1..3, None);
    }
}
