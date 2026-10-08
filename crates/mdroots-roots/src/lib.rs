//! mdroots-roots: root discovery (docs/specs/roots.md). Every filesystem
//! access goes through [`probe::Probe`], so tests can count and forbid
//! `read_dir` calls.
#![forbid(unsafe_code)]

pub mod probe;
