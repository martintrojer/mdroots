//! parse() is total: no panics, ranges in bounds and on char boundaries,
//! elements sorted.

use mdroots_syntax::{Dialect, Element, parse};
use proptest::prelude::*;

fn check(src: &str, dialect: Dialect) -> Result<(), TestCaseError> {
    let doc = parse(src, dialect);
    let ok = |r: &std::ops::Range<usize>| {
        r.start <= r.end
            && r.end <= src.len()
            && src.is_char_boundary(r.start)
            && src.is_char_boundary(r.end)
    };
    let mut last = (0, 0);
    for el in doc.elements() {
        let r = el.range();
        prop_assert!(ok(&r), "{:?} in {:?}", el, src);
        if let Element::Link(l) = el {
            prop_assert!(ok(&l.text_range), "{:?} in {:?}", l, src);
        }
        prop_assert!((r.start, r.end) >= last, "unsorted at {:?}", el);
        last = (r.start, r.end);
    }
    if let Some(fm) = doc.frontmatter() {
        prop_assert!(ok(&fm.range));
    }
    Ok(())
}

fn fragments() -> impl Strategy<Value = String> {
    let frag = prop_oneof![
        Just("[["),
        Just("]]"),
        Just("]("),
        Just("```"),
        Just("~~~"),
        Just("---\n"),
        Just("+++\n"),
        Just("`"),
        Just("<!--"),
        Just("-->"),
        Just("#"),
        Just("$"),
        Just("\n"),
        Just("é"),
        Just("😀"),
        Just("a"),
        Just(" "),
        Just("|"),
        Just("["),
        Just("]"),
        Just(":"),
        Just("!"),
        Just("\u{feff}"),
    ];
    proptest::collection::vec(frag, 0..60).prop_map(|v| v.concat())
}

fn org_fragments() -> impl Strategy<Value = String> {
    let frag = prop_oneof![
        Just("* "),
        Just("#+begin_src\n"),
        Just("#+end_src\n"),
        Just("[["),
        Just("]["),
        Just("]]"),
        Just(":PROPERTIES:\n"),
        Just(":END:\n"),
        Just("#+LINK: "),
        Just("\n"),
        Just("a"),
        Just(":"),
        Just("~"),
        Just("="),
        Just("é"),
    ];
    proptest::collection::vec(frag, 0..60).prop_map(|v| v.concat())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn arbitrary_strings(src in any::<String>()) {
        check(&src, Dialect::Markdown)?;
        check(&src, Dialect::Org)?;
    }

    #[test]
    fn markdown_fragments(src in fragments()) {
        check(&src, Dialect::Markdown)?;
        check(&src, Dialect::Org)?;
    }

    #[test]
    fn org_fragments_total(src in org_fragments()) {
        check(&src, Dialect::Org)?;
        check(&src, Dialect::Markdown)?;
    }
}
