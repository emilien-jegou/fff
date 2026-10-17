//! Live grep. `grep.rs` implements the main plain-text path, the parallel
//! scan engine, and the `grep_search` entry point that picks the matcher/sink
//! for every mode in one place; `multi_pattern` and `fuzzy_grep` hold the
//! mode-specific machinery on top of the shared `prefilter`/`sink`.

#[allow(clippy::module_inception)]
mod grep;
pub use grep::*;

mod fuzzy_grep;
mod multi_pattern;
mod prefilter;
mod sink;
mod types;

pub(crate) use multi_pattern::multi_grep_search;
pub use types::*;

#[cfg(test)]
mod grep_tests;
