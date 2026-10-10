//! mdroots-syntax: parse one document into structure and liberal link candidates, plus `LineIndex` (no I/O).
#![forbid(unsafe_code)]

use std::borrow::Cow;

mod frontmatter;
mod line_index;
mod model;
mod org;
mod regions;
mod scan;
pub mod slug;
mod structure;

pub use line_index::{LineIndex, PositionEncoding};
pub use model::{
    Anchor, Confidence, Context, Dialect, Document, Element, Field, FieldValue, Frontmatter,
    FrontmatterFormat, Heading, Link, LinkDef, LinkKind, LinkTarget, ParseOptions, Tag, TagSyntax,
    Value,
};

pub use scan::strip_position;
pub use structure::scheme;

/// Parse `text` with default options for `dialect`.
pub fn parse(text: &str, dialect: Dialect) -> Document {
    parse_with(text, &ParseOptions::new(dialect))
}

/// Parse `text`. Total: never panics; every range is inside the source and
/// on char boundaries.
pub fn parse_with(text: &str, opts: &ParseOptions) -> Document {
    structure::parse_document(text.to_owned(), opts)
}

/// Decode bytes (lossy UTF-8) and parse. `None` when the first 8 KiB
/// contain a NUL byte (treated as binary).
pub fn parse_bytes(bytes: &[u8], opts: &ParseOptions) -> Option<Document> {
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return None;
    }
    let (text, lossy) = match String::from_utf8_lossy(bytes) {
        Cow::Borrowed(s) => (s.to_owned(), false),
        Cow::Owned(s) => (s, true),
    };
    let mut doc = structure::parse_document(text, opts);
    doc.lossy = lossy;
    Some(doc)
}

/// The pulldown-cmark options the structure pass uses; embedders that
/// render markdown reuse them to see the same structure.
pub fn markdown_options() -> pulldown_cmark::Options {
    use pulldown_cmark::Options as O;
    O::ENABLE_TABLES
        | O::ENABLE_FOOTNOTES
        | O::ENABLE_STRIKETHROUGH
        | O::ENABLE_TASKLISTS
        | O::ENABLE_GFM
        | O::ENABLE_WIKILINKS
        | O::ENABLE_HEADING_ATTRIBUTES
        | O::ENABLE_YAML_STYLE_METADATA_BLOCKS
        | O::ENABLE_PLUSES_DELIMITED_METADATA_BLOCKS
        | O::ENABLE_MATH
}
