//! The key model shared by the ladder, the stores and the differential
//! tool: every key a document can be found by, normalised like link targets.

use mdroots_syntax::{Dialect, Document, slug};
use unicode_normalization::UnicodeNormalization;

use crate::normalize::key_of;

/// Which index a key lives in.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum KeyKind {
    /// Root-relative path without `.md`/`.markdown`/`.org`.
    Path,
    /// File name without extension.
    Stem,
    /// Frontmatter `id`, org `:ID:`, or a 12-14 digit file name prefix.
    Id,
    /// GitHub slug of the frontmatter title or the first H1.
    TitleSlug,
    Alias,
    /// Frontmatter `slug`/`permalink`/`url` without surrounding `/`.
    SitePath,
}

/// The ladder step that resolved a link (docs/specs/index.md §2.4).
/// Defined here so the ladder and the dialect vote can both use it.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResolveStep {
    FileRelative,
    RootRelative,
    SiteRooted,
    Stem,
    Id,
    Title,
    Alias,
    DialectTransform,
    Partial,
}

/// Normalised-key lookups, implemented by stores.
pub trait KeyLookup {
    /// Root-relative file paths (with extension) having `key` of `kind`, sorted.
    fn lookup(&self, kind: KeyKind, key: &str) -> Vec<String>;
    /// zk-style partial match: file name contains `needle`, then path
    /// contains it; sorted.
    fn partial(&self, needle: &str) -> Vec<String>;
}

/// Every key `doc` at `root_rel_path` can be found by. Path, Stem, Id,
/// Alias and SitePath use the link-target rules (decode, NFC, case-fold
/// when `!case_sensitive`); TitleSlug is `slug::github` of the NFC form.
/// Org heading `:ID:`s are Id keys of the file. No I/O.
/// Duplicates are removed; order is by kind, then first appearance.
pub fn doc_keys(
    root_rel_path: &str,
    doc: &Document,
    case_sensitive: bool,
) -> Vec<(KeyKind, String)> {
    let fm = doc.frontmatter();
    let facts = Facts {
        id: fm.and_then(|f| f.id()),
        // Markdown `{#id}` attributes are anchors, not document ids.
        heading_ids: match doc.dialect() {
            Dialect::Org => doc.headings().filter_map(|h| h.id.as_deref()).collect(),
            _ => Vec::new(),
        },
        title: fm.and_then(|f| f.title()),
        h1: doc
            .headings()
            .find(|h| h.level == 1)
            .map(|h| h.text.as_str()),
        aliases: fm.map(|f| f.aliases()).unwrap_or_default(),
        site_path: fm.and_then(|f| f.site_path()),
    };
    keys_from(root_rel_path, &facts, case_sensitive)
}

/// What `doc_keys` reads from a document.
#[derive(Default)]
struct Facts<'a> {
    id: Option<&'a str>,
    heading_ids: Vec<&'a str>,
    title: Option<&'a str>,
    h1: Option<&'a str>,
    aliases: Vec<&'a str>,
    site_path: Option<&'a str>,
}

fn keys_from(root_rel_path: &str, f: &Facts, case_sensitive: bool) -> Vec<(KeyKind, String)> {
    let mut out: Vec<(KeyKind, String)> = Vec::new();
    let mut push = |kind: KeyKind, key: String| {
        if !key.is_empty() && !out.iter().any(|(k, v)| *k == kind && *v == key) {
            out.push((kind, key));
        }
    };
    let key = |s: &str| key_of(s, case_sensitive).0;

    let (path, _) = key_of(root_rel_path, case_sensitive);
    let stem = path.rsplit('/').next().unwrap_or("").to_owned();
    push(KeyKind::Path, path);
    push(KeyKind::Stem, stem);

    for id in f.id.iter().chain(&f.heading_ids) {
        push(KeyKind::Id, key(id));
    }
    let file_name = root_rel_path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(root_rel_path);
    if let Some(id) = id_prefix(file_name) {
        push(KeyKind::Id, id.to_owned());
    }
    for title in [f.title, f.h1].into_iter().flatten() {
        push(KeyKind::TitleSlug, title_slug(title));
    }
    for alias in &f.aliases {
        push(KeyKind::Alias, key(alias));
    }
    if let Some(site) = f.site_path {
        push(KeyKind::SitePath, key(site.trim_matches('/')));
    }
    out
}

/// The TitleSlug key of a title or link text: `slug::github` of its NFC
/// form, so composed and decomposed spellings meet.
pub(crate) fn title_slug(s: &str) -> String {
    slug::github(&s.nfc().collect::<String>())
}

/// A leading run of 12-14 ASCII digits ended by a non-digit or the end
/// (`202101011200 title.md` → `202101011200`).
fn id_prefix(file_name: &str) -> Option<&str> {
    let n = file_name.bytes().take_while(u8::is_ascii_digit).count();
    (12..=14).contains(&n).then(|| &file_name[..n])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frontmatter facts as the syntax_frontmatter accessors return them;
    /// tests/normalize.rs covers the same keys through parse().
    #[test]
    fn keys_from_frontmatter_facts() {
        let facts = Facts {
            id: Some("Abc-1"),
            heading_ids: vec!["Sec-1", "abc-1"],
            title: Some("My Title"),
            h1: Some("Heading One"),
            aliases: vec!["Other Name", "Caf%C3%A9", "Other Name"],
            site_path: Some("/posts/My-Post/"),
        };
        let got = keys_from("Notes/202101011200 x.md", &facts, false);
        let want: Vec<(KeyKind, String)> = [
            (KeyKind::Path, "notes/202101011200 x"),
            (KeyKind::Stem, "202101011200 x"),
            (KeyKind::Id, "abc-1"),
            (KeyKind::Id, "sec-1"),
            (KeyKind::Id, "202101011200"),
            (KeyKind::TitleSlug, "my-title"),
            (KeyKind::TitleSlug, "heading-one"),
            (KeyKind::Alias, "other name"),
            (KeyKind::Alias, "caf\u{e9}"),
            (KeyKind::SitePath, "posts/my-post"),
        ]
        .into_iter()
        .map(|(k, v)| (k, v.to_owned()))
        .collect();
        assert_eq!(got, want);

        // Case-sensitive roots keep case except in the title slug.
        let got = keys_from("Notes/A.md", &facts, true);
        assert!(got.contains(&(KeyKind::Id, "Abc-1".into())));
        assert!(got.contains(&(KeyKind::TitleSlug, "my-title".into())));
        assert!(got.contains(&(KeyKind::SitePath, "posts/My-Post".into())));
    }
}
