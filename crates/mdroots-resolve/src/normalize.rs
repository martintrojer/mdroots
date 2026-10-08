//! Target normalisation (docs/specs/index.md §2.4): the same rules
//! produce a link's lookup key and a document's stored keys.
//!
//! `scheme`, `percent_decode` and the `file:` forms are donated from ramble
//! d394783 `src/nav.rs`.

use mdroots_syntax::{Anchor, Link, LinkKind};
use unicode_normalization::UnicodeNormalization;

/// A link target reduced to a lookup key plus what was stripped from it.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Normalized {
    /// Percent-decoded, NFC, `.md`/`.markdown`/`.org` stripped, lowercase
    /// when the root is case-insensitive, `/`-separated, no leading `./`,
    /// `/` or `~/`. `..` segments are kept for the ladder to join.
    pub key: String,
    /// The last segment's extension as written, lowercase (also set for
    /// extensions that stay in the key, e.g. `png`).
    pub had_extension: Option<String>,
    pub anchor: Option<Anchor>,
    /// The path started with `/` (after any `file:` prefix).
    pub rooted: bool,
    /// The path started with `~/`.
    pub home: bool,
    /// `"file"` (stripped), or an external scheme; then `key` is empty.
    pub scheme: Option<String>,
    /// For a wiki link with a label: the label normalised the same way
    /// (Dendron and Gollum put the target on the right).
    pub piped_alt: Option<String>,
    /// An org `id:` link: `key` is the id, resolved only by `KeyKind::Id`.
    pub id_ref: bool,
}

impl Normalized {
    /// A link to another scheme: no key, nothing to resolve.
    pub fn is_external(&self) -> bool {
        self.scheme.as_deref().is_some_and(|s| s != "file")
    }
}

/// Normalise a parsed link. Reads `link.target.path` (syntax already split
/// the anchor and org `::search`) and takes the anchor from
/// `link.target.anchor`; the scheme comes from `link.target.raw`.
pub fn normalize(link: &Link, case_sensitive: bool) -> Normalized {
    let raw = link.target.raw.as_str();
    let path = link.target.path.as_str();
    let mut n = Normalized::default();
    let rest = match link_scheme(raw) {
        Some(s) if s.eq_ignore_ascii_case("id") => {
            n.id_ref = true;
            strip_scheme(path, "id")
        }
        Some(s) if s.eq_ignore_ascii_case("file") => {
            n.scheme = Some("file".to_owned());
            strip_file(path)
        }
        Some(s) => {
            n.scheme = Some(s.to_ascii_lowercase());
            return n;
        }
        None => path,
    };
    finish_path(&mut n, rest, case_sensitive);
    n.anchor = link.target.anchor.as_ref().map(decode_anchor);
    if matches!(link.kind, LinkKind::Wiki | LinkKind::WikiEmbed)
        && let Some(label) = &link.label
    {
        n.piped_alt = Some(normalize_str(label, case_sensitive).key);
    }
    n
}

/// Normalise a destination as written: the same rules as [`normalize`],
/// splitting `#anchor` and org `::search` itself.
pub fn normalize_str(raw: &str, case_sensitive: bool) -> Normalized {
    let mut n = Normalized::default();
    let rest = match link_scheme(raw) {
        Some(s) if s.eq_ignore_ascii_case("id") => {
            n.id_ref = true;
            &raw[s.len() + 1..]
        }
        Some(s) if s.eq_ignore_ascii_case("file") => {
            n.scheme = Some("file".to_owned());
            strip_file(raw)
        }
        Some(s) => {
            n.scheme = Some(s.to_ascii_lowercase());
            return n;
        }
        None => raw,
    };
    let (path, anchor) = split_anchor(rest);
    finish_path(&mut n, path, case_sensitive);
    n.anchor = anchor.as_ref().map(decode_anchor);
    n
}

/// RFC 3986 scheme (`ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )`) before the
/// first `:`, at least two characters so `C:` drive letters stay paths.
pub fn scheme(raw: &str) -> Option<&str> {
    let (s, _) = raw.split_once(':')?;
    let mut chars = s.chars();
    let ok = s.len() >= 2
        && chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    ok.then_some(s)
}

/// Decode `%XX` escapes; a `%` not followed by two hex digits is kept, and
/// invalid UTF-8 is replaced (lossy).
pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push(h << 4 | l);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// [`scheme`], except that `x::search` (an org search on a file named
/// like `a.org`) is a path, not the scheme `x`.
fn link_scheme(raw: &str) -> Option<&str> {
    scheme(raw).filter(|s| !raw[s.len() + 1..].starts_with(':'))
}

fn hex(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|d| d as u8)
}

/// Key of a document path or a frontmatter value (id, alias, site path):
/// decode, NFC, `\` → `/`, clean segments, strip `.md`/`.markdown`/`.org`,
/// case-fold. Also returns the lowercase extension.
pub(crate) fn key_of(s: &str, case_sensitive: bool) -> (String, Option<String>) {
    let mut n = Normalized::default();
    finish_path(&mut n, s, case_sensitive);
    (n.key, n.had_extension)
}

/// `path` minus a leading `name:` (case-insensitive), if present.
fn strip_scheme<'a>(path: &'a str, name: &str) -> &'a str {
    match scheme(path) {
        Some(s) if s.eq_ignore_ascii_case(name) => &path[s.len() + 1..],
        _ => path,
    }
}

/// Strip `file:` in its forms `file:///abs`, `file://localhost/abs`,
/// `file:/abs` and `file:rel`; a path without the prefix is returned as is.
fn strip_file(path: &str) -> &str {
    let after = strip_scheme(path, "file");
    if after.len() == path.len() {
        return path;
    }
    match after.strip_prefix("//") {
        Some(s) => s.strip_prefix("localhost").unwrap_or(s),
        None => after,
    }
}

/// Split at the first `#` or org `::`, whichever comes first.
fn split_anchor(s: &str) -> (&str, Option<Anchor>) {
    let hash = s.find('#');
    let search = s.find("::");
    match (hash, search) {
        (Some(h), Some(c)) if c < h => org_search(s, c),
        (None, Some(c)) => org_search(s, c),
        (Some(h), _) => {
            let frag = &s[h + 1..];
            let anchor = match frag.strip_prefix('^') {
                Some(b) => Some(Anchor::Block(b.to_owned())),
                None if frag.is_empty() => None,
                None => Some(Anchor::Heading(frag.to_owned())),
            };
            (&s[..h], anchor)
        }
        (None, None) => (s, None),
    }
}

/// Org `path::*Heading`, `path::#custom-id` or `path::text` at `at`.
fn org_search(s: &str, at: usize) -> (&str, Option<Anchor>) {
    let q = &s[at + 2..];
    let anchor = if let Some(h) = q.strip_prefix('*') {
        Some(Anchor::Heading(h.to_owned()))
    } else if let Some(c) = q.strip_prefix('#') {
        Some(Anchor::CustomId(c.to_owned()))
    } else if q.is_empty() {
        None
    } else {
        Some(Anchor::Search(q.to_owned()))
    };
    (&s[..at], anchor)
}

fn decode_anchor(a: &Anchor) -> Anchor {
    match a {
        Anchor::Heading(s) => Anchor::Heading(percent_decode(s)),
        Anchor::Block(s) => Anchor::Block(percent_decode(s)),
        Anchor::CustomId(s) => Anchor::CustomId(percent_decode(s)),
        Anchor::Search(s) => Anchor::Search(percent_decode(s)),
        other => other.clone(),
    }
}

/// Fill `key`, `had_extension`, `rooted` and `home` from a path with no
/// scheme or anchor left.
fn finish_path(n: &mut Normalized, path: &str, case_sensitive: bool) {
    let decoded = percent_decode(path).replace('\\', "/");
    let mut s: String = decoded.nfc().collect();
    if !case_sensitive {
        s = s.to_lowercase();
    }
    let mut rest = s.as_str();
    if let Some(r) = rest.strip_prefix("~/") {
        n.home = true;
        rest = r;
    } else if rest.starts_with('/') {
        n.rooted = true;
    }
    // Drop empty and `.` segments (leading "./", "a/./b", "a//b", trailing
    // "/"); keep `..` for the ladder to join against the source dir.
    let segs: Vec<&str> = rest
        .split('/')
        .filter(|seg| !seg.is_empty() && *seg != ".")
        .collect();
    let mut key = segs.join("/");
    if let Some(last) = segs.last()
        && let Some(dot) = last.rfind('.')
        && dot > 0
        && dot + 1 < last.len()
    {
        let ext = last[dot + 1..].to_lowercase();
        if matches!(ext.as_str(), "md" | "markdown" | "org") {
            key.truncate(key.len() - (last.len() - dot));
        }
        n.had_extension = Some(ext);
    }
    n.key = key;
}
