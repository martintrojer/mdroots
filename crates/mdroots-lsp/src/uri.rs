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
    let mut s = String::from("file://");
    for &b in path.to_str()?.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                s.push(b as char)
            }
            _ => s.push_str(&format!("%{b:02X}")),
        }
    }
    s.parse().ok()
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
    fn ignores_other_schemes() {
        let uri: Uri = "untitled:Untitled-1".parse().unwrap();
        assert_eq!(to_path(&uri), None);
    }
}
