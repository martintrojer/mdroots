//! mdroots-resolve: resolution ladder, dialect detection and convention vote (no direct I/O; via `ResolveEnv`).
#![forbid(unsafe_code)]

pub mod dialect;
pub mod env;
pub mod keys;
pub mod ladder;
pub mod normalize;

pub use env::ResolveEnv;
pub use keys::{KeyKind, KeyLookup, ResolveStep, doc_keys};
pub use normalize::{Normalized, normalize, normalize_str, percent_decode, scheme};
