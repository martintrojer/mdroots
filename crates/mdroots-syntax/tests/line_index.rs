//! LineIndex conversions (tables ported from ramble tests/lsp.rs).

use mdroots_syntax::LineIndex;
use mdroots_syntax::PositionEncoding::{self, Utf8, Utf16, Utf32};
use proptest::prelude::*;

#[test]
fn single_line_offset_and_col_tables() {
    // (line, character, encoding, byte)
    let cases: &[(&str, u32, PositionEncoding, usize)] = &[
        ("abc", 2, Utf16, 2),
        // é: 2 bytes, 1 utf-16 unit, 1 code point
        ("é!", 1, Utf8, 0), // inside é rounds down
        ("é!", 2, Utf8, 2),
        ("é!", 1, Utf16, 2),
        ("é!", 1, Utf32, 2),
        // 😀: 4 bytes, 2 utf-16 units, 1 code point
        ("😀x", 1, Utf16, 0), // between surrogates rounds down
        ("😀x", 2, Utf16, 4),
        ("😀x", 1, Utf32, 4),
        ("😀x", 3, Utf8, 0),
        ("😀x", 4, Utf8, 4),
        ("😀x", 3, Utf16, 5),
        // 日本: 3 bytes, 1 unit, 1 code point each
        ("日本", 1, Utf16, 3),
        ("日本", 1, Utf32, 3),
        ("日本", 6, Utf8, 6),
        // mixed
        ("aé😀日b", 5, Utf16, 10),
        ("aé😀日b", 4, Utf32, 10),
        ("aé😀日b", 10, Utf8, 10),
        // end of line and past end clamp
        ("aé", 2, Utf32, 3),
        ("aé", 99, Utf16, 3),
        ("aé", 99, Utf8, 3),
        ("", 5, Utf32, 0),
    ];
    for &(line, ch, enc, byte) in cases {
        let idx = LineIndex::new(line);
        assert_eq!(
            idx.offset(line, 0, ch, enc),
            byte,
            "offset({line:?}, {ch}, {enc:?})"
        );
    }

    // (line, byte, encoding, character)
    let back: &[(&str, usize, PositionEncoding, u32)] = &[
        ("é!", 2, Utf8, 2),
        ("é!", 2, Utf16, 1),
        ("é!", 1, Utf16, 0), // inside é rounds down
        ("😀x", 4, Utf16, 2),
        ("😀x", 4, Utf32, 1),
        ("😀x", 2, Utf16, 0),
        ("😀x", 5, Utf16, 3),
        ("日本", 3, Utf16, 1),
        ("日本", 6, Utf32, 2),
        ("aé😀日b", 10, Utf16, 5),
        ("aé😀日b", 10, Utf32, 4),
        ("aé😀日b", 11, Utf16, 6),
        ("aé", 99, Utf16, 2), // past end clamps
        ("aé", 99, Utf8, 3),
    ];
    for &(line, byte, enc, ch) in back {
        let idx = LineIndex::new(line);
        assert_eq!(
            idx.line_col(line, byte, enc),
            (0, ch),
            "line_col({line:?}, {byte}, {enc:?})"
        );
    }
}

#[test]
fn multi_line_with_crlf() {
    let src = "a😀\r\n日b\n\nlast";
    let idx = LineIndex::new(src);
    assert_eq!(idx.offset(src, 0, 3, Utf16), 5);
    // the trailing '\r' is part of line 0
    assert_eq!(idx.offset(src, 0, 4, Utf16), 6);
    assert_eq!(idx.offset(src, 0, 99, Utf16), 6);
    assert_eq!(idx.offset(src, 1, 1, Utf32), 10);
    assert_eq!(idx.offset(src, 2, 0, Utf8), 12);
    assert_eq!(idx.offset(src, 3, 2, Utf8), 15);
    // line past EOF
    assert_eq!(idx.offset(src, 9, 0, Utf8), src.len());
    assert_eq!(idx.line_col(src, 10, Utf32), (1, 1));
    assert_eq!(idx.line_col(src, src.len(), Utf16), (3, 4));
    assert_eq!(idx.line_col(src, src.len() + 5, Utf16), (3, 4));
    assert_eq!(idx.line_col(src, 3, Utf16), (0, 1)); // inside 😀
    assert_eq!(idx.line_col(src, 5, Utf8), (0, 5)); // the '\r'
    assert_eq!(idx.line_col(src, 6, Utf8), (0, 6)); // the '\n' ends line 0
    assert_eq!(idx.line_col(src, 7, Utf8), (1, 0));
}

#[test]
fn trailing_newline_opens_an_empty_line() {
    let src = "a\n";
    let idx = LineIndex::new(src);
    assert_eq!(idx.line_col(src, 2, Utf16), (1, 0));
    assert_eq!(idx.offset(src, 1, 0, Utf16), 2);
    assert_eq!(idx.offset(src, 1, 5, Utf16), 2);
    assert_eq!(idx.offset(src, 2, 0, Utf16), 2);
}

fn text() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        prop_oneof![
            Just('a'),
            Just('é'),
            Just('😀'),
            Just('日'),
            Just('\n'),
            Just('\r'),
            Just(' '),
        ],
        0..40,
    )
    .prop_map(|v| v.into_iter().collect())
}

proptest! {
    #[test]
    fn position_round_trip(src in text(), pick in any::<prop::sample::Index>()) {
        let idx = LineIndex::new(&src);
        let mut bytes: Vec<usize> = src.char_indices().map(|(i, _)| i).collect();
        bytes.push(src.len());
        let byte = bytes[pick.index(bytes.len())];
        for enc in [Utf8, Utf16, Utf32] {
            let (line, col) = idx.line_col(&src, byte, enc);
            prop_assert_eq!(idx.offset(&src, line, col, enc), byte, "{:?}", enc);
        }
    }
}
