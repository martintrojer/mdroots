//! Byte regions of a document tagged with the [`Context`] they sit in.

use std::cell::OnceCell;
use std::ops::Range;

use crate::model::Context;

/// Context marks pushed by a structure pass, resolved into a sorted
/// partition of `0..len`. Bytes covered by no mark are `Prose`. Where marks
/// nest, the innermost (latest-starting, then shortest) wins for its bytes.
#[derive(Debug, Clone)]
pub(crate) struct Regions {
    len: usize,
    marks: Vec<(Range<usize>, Context)>,
    segments: OnceCell<Vec<(Range<usize>, Context)>>,
}

impl Regions {
    /// No marks: all of `0..len` is prose.
    pub(crate) fn new(len: usize) -> Self {
        Regions {
            len,
            marks: Vec::new(),
            segments: OnceCell::new(),
        }
    }

    /// All of `src` in one context.
    pub(crate) fn whole(src: &str, ctx: Context) -> Self {
        let mut r = Regions::new(src.len());
        r.push(0..src.len(), ctx);
        r
    }

    /// Mark `range` (clamped to the source) as `ctx`. Empty ranges are ignored.
    pub(crate) fn push(&mut self, range: Range<usize>, ctx: Context) {
        let end = range.end.min(self.len);
        let start = range.start.min(end);
        if start < end {
            self.marks.push((start..end, ctx));
            self.segments = OnceCell::new();
        }
    }

    /// Sorted, gapless partition of `0..len`; adjacent equal contexts merged.
    pub(crate) fn segments(&self) -> impl Iterator<Item = (Range<usize>, Context)> + '_ {
        self.resolved().iter().cloned()
    }

    /// Context of the byte at `off`; offsets at or past the end take the
    /// context of the last byte.
    pub(crate) fn context_at(&self, off: usize) -> Context {
        let segs = self.resolved();
        let i = segs.partition_point(|(r, _)| r.end <= off);
        segs.get(i)
            .or(segs.last())
            .map_or(Context::Prose, |(_, c)| *c)
    }

    fn resolved(&self) -> &[(Range<usize>, Context)] {
        self.segments.get_or_init(|| self.resolve())
    }

    fn resolve(&self) -> Vec<(Range<usize>, Context)> {
        // Outer marks first: by start, then longest, then push order.
        let mut order: Vec<usize> = (0..self.marks.len()).collect();
        order.sort_by_key(|&i| {
            let r = &self.marks[i].0;
            (r.start, std::cmp::Reverse(r.end), i)
        });
        let mut bounds: Vec<usize> = self
            .marks
            .iter()
            .flat_map(|(r, _)| [r.start, r.end])
            .chain([0, self.len])
            .collect();
        bounds.sort_unstable();
        bounds.dedup();

        let mut out: Vec<(Range<usize>, Context)> = Vec::new();
        let mut stack: Vec<usize> = Vec::new();
        let mut next = 0;
        for w in bounds.windows(2) {
            let (a, b) = (w[0], w[1]);
            while next < order.len() && self.marks[order[next]].0.start <= a {
                stack.push(order[next]);
                next += 1;
            }
            while stack.last().is_some_and(|&i| self.marks[i].0.end <= a) {
                stack.pop();
            }
            let ctx = stack.last().map_or(Context::Prose, |&i| self.marks[i].1);
            match out.last_mut() {
                Some((r, c)) if *c == ctx && r.end == a => r.end = b,
                _ => out.push((a..b, ctx)),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partition_with_nesting() {
        let mut r = Regions::new(10);
        r.push(2..8, Context::Heading);
        r.push(4..6, Context::InlineCode);
        let segs: Vec<_> = r.segments().collect();
        assert_eq!(
            segs,
            vec![
                (0..2, Context::Prose),
                (2..4, Context::Heading),
                (4..6, Context::InlineCode),
                (6..8, Context::Heading),
                (8..10, Context::Prose),
            ]
        );
        assert_eq!(r.context_at(5), Context::InlineCode);
        assert_eq!(r.context_at(6), Context::Heading);
        assert_eq!(r.context_at(99), Context::Prose);
        for (range, ctx) in segs {
            for off in range {
                assert_eq!(r.context_at(off), ctx);
            }
        }
    }

    #[test]
    fn empty_source() {
        let r = Regions::new(0);
        assert_eq!(r.segments().count(), 0);
        assert_eq!(r.context_at(0), Context::Prose);
    }

    #[test]
    fn comment_block_is_one_region() {
        let src = "<!--\na [[x]]\n-->\n";
        let regions = crate::structure::markdown(src).regions;
        let at = src.find("[[x]]").unwrap();
        assert_eq!(regions.context_at(at), Context::Comment);
    }
}
