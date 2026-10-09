//! Byte offset <-> line/column conversion in UTF-8, UTF-16 or UTF-32 units.
//!
//! Lines are split on `'\n'` only: a trailing `'\r'` is part of the line,
//! as LSP counts it.

/// The unit a column counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PositionEncoding {
    /// UTF-8 code units (bytes).
    Utf8,
    /// UTF-16 code units (the LSP default).
    Utf16,
    /// Unicode code points.
    Utf32,
}

impl PositionEncoding {
    fn width(self, c: char) -> u32 {
        match self {
            PositionEncoding::Utf8 => c.len_utf8() as u32,
            PositionEncoding::Utf16 => c.len_utf16() as u32,
            PositionEncoding::Utf32 => 1,
        }
    }
}

/// Precomputed line starts of one text. The text itself is not kept:
/// [`line_col`](Self::line_col) and [`offset`](Self::offset) take it again,
/// and it must be the text the index was built from.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LineIndex {
    /// Byte offset of the start of each line; `starts[0] == 0`.
    starts: Vec<usize>,
}

impl LineIndex {
    pub fn new(text: &str) -> Self {
        let starts = std::iter::once(0)
            .chain(text.match_indices('\n').map(|(i, _)| i + 1))
            .collect();
        LineIndex { starts }
    }

    /// Zero-based (line, column) of `offset`. The offset is clamped to the
    /// text length and rounded down to a char boundary.
    pub fn line_col(&self, text: &str, offset: usize, enc: PositionEncoding) -> (u32, u32) {
        let b = floor_char_boundary(text, offset);
        let line = self.starts.partition_point(|&s| s <= b) - 1;
        let start = self.starts[line];
        let col = text[start..b].chars().map(|c| enc.width(c)).sum();
        (line as u32, col)
    }

    /// Byte offset of (`line`, `col`). A column inside a code point rounds
    /// down to its start; past the end of the line clamps to the line end
    /// (before its `'\n'`); a line past the end gives `text.len()`.
    pub fn offset(&self, text: &str, line: u32, col: u32, enc: PositionEncoding) -> usize {
        let Some(&start) = self.starts.get(line as usize) else {
            return text.len();
        };
        let end = self
            .starts
            .get(line as usize + 1)
            .map_or(text.len(), |&next| next - 1);
        let mut units = 0u32;
        for (i, c) in text[start..end].char_indices() {
            let next = units + enc.width(c);
            if next > col {
                return start + i;
            }
            units = next;
        }
        end
    }
}

fn floor_char_boundary(s: &str, byte: usize) -> usize {
    let mut b = byte.min(s.len());
    while !s.is_char_boundary(b) {
        b -= 1;
    }
    b
}
