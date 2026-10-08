//! GitHub-style heading slugs (github-slugger rules).

use std::collections::HashSet;

use unicode_general_category::{GeneralCategory, get_general_category};

/// Unicode combining mark (general category Mn, Mc or Me). github-slugger
/// keeps these, e.g. the Devanagari virama or a decomposed accent.
fn is_mark(c: char) -> bool {
    matches!(
        get_general_category(c),
        GeneralCategory::NonspacingMark
            | GeneralCategory::SpacingMark
            | GeneralCategory::EnclosingMark
    )
}

/// GitHub heading slug: lowercase, keep alphanumerics, combining marks,
/// `-` and `_`, spaces to `-`, drop everything else.
pub fn github(text: &str) -> String {
    text.trim()
        .chars()
        .flat_map(char::to_lowercase)
        .filter_map(|c| match c {
            ' ' => Some('-'),
            '-' | '_' => Some(c),
            c if c.is_alphanumeric() || is_mark(c) => Some(c),
            _ => None,
        })
        .collect()
}

/// `base`, or `base-1`, `base-2`, … if taken; records the result in `used`.
pub fn unique(base: &str, used: &mut HashSet<String>) -> String {
    let slug = if used.contains(base) {
        (1..)
            .map(|n| format!("{base}-{n}"))
            .find(|s| !used.contains(s))
            .expect("unbounded counter")
    } else {
        base.to_owned()
    };
    used.insert(slug.clone());
    slug
}
