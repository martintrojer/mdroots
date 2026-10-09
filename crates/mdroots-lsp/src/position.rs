//! LSP positions <-> byte offsets in the negotiated encoding
//! (docs/specs/library.md §3.1).

use std::ops::Range;

use lsp_types::{Position, PositionEncodingKind};
use mdroots::syntax::{LineIndex, PositionEncoding};

/// UTF-8 when the client offers it, else the LSP default UTF-16.
pub(crate) fn negotiate(offered: Option<&[PositionEncodingKind]>) -> PositionEncoding {
    match offered {
        Some(v) if v.contains(&PositionEncodingKind::UTF8) => PositionEncoding::Utf8,
        _ => PositionEncoding::Utf16,
    }
}

pub(crate) fn kind(enc: PositionEncoding) -> PositionEncodingKind {
    match enc {
        PositionEncoding::Utf8 => PositionEncodingKind::UTF8,
        PositionEncoding::Utf16 => PositionEncodingKind::UTF16,
        PositionEncoding::Utf32 => PositionEncodingKind::UTF32,
    }
}

/// `index` is over `text`.
pub(crate) fn position(
    index: &LineIndex,
    text: &str,
    offset: usize,
    enc: PositionEncoding,
) -> Position {
    let (line, character) = index.line_col(text, offset, enc);
    Position { line, character }
}

pub(crate) fn range(
    index: &LineIndex,
    text: &str,
    r: Range<usize>,
    enc: PositionEncoding,
) -> lsp_types::Range {
    lsp_types::Range {
        start: position(index, text, r.start, enc),
        end: position(index, text, r.end, enc),
    }
}

/// Byte offset of `pos`; used by the request handlers that take a position.
pub(crate) fn offset(index: &LineIndex, text: &str, pos: Position, enc: PositionEncoding) -> usize {
    index.offset(text, pos.line, pos.character, enc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiates_utf8_only_when_offered() {
        let both = [PositionEncodingKind::UTF16, PositionEncodingKind::UTF8];
        assert_eq!(negotiate(Some(&both)), PositionEncoding::Utf8);
        let utf16 = [PositionEncodingKind::UTF16];
        assert_eq!(negotiate(Some(&utf16)), PositionEncoding::Utf16);
        assert_eq!(negotiate(None), PositionEncoding::Utf16);
    }

    #[test]
    fn round_trips_in_each_encoding() {
        // "é" is 2 bytes and 1 UTF-16 unit; "😀" is 4 bytes and 2 units.
        let text = "a\né😀x\n";
        let idx = LineIndex::new(text);
        let x = text.find('x').unwrap();
        for (enc, col) in [(PositionEncoding::Utf8, 6), (PositionEncoding::Utf16, 3)] {
            let p = position(&idx, text, x, enc);
            assert_eq!((p.line, p.character), (1, col), "{enc:?}");
            assert_eq!(offset(&idx, text, p, enc), x, "{enc:?}");
        }
    }
}
