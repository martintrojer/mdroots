//! `file:` URIs <-> paths, without a URL crate.

use std::path::{Path, PathBuf};

use lsp_types::Uri;

/// The path of a `file:` URI, percent-decoded; `None` for other schemes
/// or a path that is not UTF-8 once decoded.
pub(crate) fn to_path(uri: &Uri) -> Option<PathBuf> {
    let scheme = uri.scheme()?;
    if !scheme.as_str().eq_ignore_ascii_case("file") {
        return None;
    }
    let path = uri.path().as_estr().decode().into_string().ok()?;
    Some(PathBuf::from(path.into_owned()))
}

/// The `file:` URI of an absolute path. Bytes outside the RFC 3986
/// unreserved set and `/` are percent-encoded.
pub(crate) fn from_path(path: &Path) -> Option<Uri> {
    with_fragment(path, None)
}

/// [`from_path`] with `#fragment` appended when `fragment` is `Some`; the
/// fragment is percent-encoded the same way (`/` kept).
pub(crate) fn with_fragment(path: &Path, fragment: Option<&str>) -> Option<Uri> {
    let mut s = String::from("file://");
    encode(&mut s, path.to_str()?);
    if let Some(f) = fragment {
        s.push('#');
        encode(&mut s, f);
    }
    s.parse().ok()
}

fn encode(out: &mut String, text: &str) {
    for &b in text.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_spaces_and_non_ascii() {
        let p = Path::new("/tmp/a b/é.md");
        let uri = from_path(p).unwrap();
        assert_eq!(uri.as_str(), "file:///tmp/a%20b/%C3%A9.md");
        assert_eq!(to_path(&uri).unwrap(), p);
    }

    #[test]
    fn encodes_the_fragment() {
        let uri = with_fragment(Path::new("/tmp/a.md"), Some("Two words^b")).unwrap();
        assert_eq!(uri.as_str(), "file:///tmp/a.md#Two%20words%5Eb");
        assert_eq!(to_path(&uri).unwrap(), Path::new("/tmp/a.md"));
    }

    #[test]
    fn ignores_other_schemes() {
        let uri: Uri = "untitled:Untitled-1".parse().unwrap();
        assert_eq!(to_path(&uri), None);
    }
}
