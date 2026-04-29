//! Markdown parser frontends.
//!
//! Two entry points are provided:
//!
//! * [`parse_full`] – one-shot, stateless parse of an entire string.
//!   Equivalent to but slower than [`IncrementalParser`] for streaming
//!   workloads.
//! * [`IncrementalParser`] – long-lived parser optimised for streaming
//!   agent replies (the same `raw_source` model used by
//!   `codex-rs/tui/src/streaming/controller.rs`).
//!
//! The PR-A skeleton intentionally returns an empty block list from the
//! walker; the full pulldown-cmark walker mirroring
//! `codex-rs/tui/src/markdown_render.rs` lands in PR-B alongside the
//! TUI refactor.

use crate::ast::{MdBlock, MdDocument};
#[allow(unused_imports)]
use crate::inline::{Inline, InlineCode, InlineImage, InlineLink};
use pulldown_cmark::{Event, Options, Parser};
use std::ops::Range;

/// One-shot full parse. Equivalent to but slower than
/// [`IncrementalParser`] for streaming workloads.
///
/// PR-A note: the underlying walker is a stub that returns an empty
/// block list. PR-B will implement the actual AST construction.
pub fn parse_full(src: &str) -> MdDocument {
    let opts = default_options();
    let parser = Parser::new_ext(src, opts).into_offset_iter();
    let blocks = walk_blocks(parser);
    MdDocument { blocks }
}

/// Default pulldown-cmark options.
///
/// Matches the option set used by the existing TUI renderer in
/// `codex-rs/tui/src/markdown_render.rs` so that PR-B can drop in the
/// AST without changing observable behaviour.
fn default_options() -> Options {
    let mut o = Options::empty();
    o.insert(Options::ENABLE_STRIKETHROUGH);
    o.insert(Options::ENABLE_TABLES);
    o.insert(Options::ENABLE_TASKLISTS);
    o
}

/// Walks a pulldown-cmark offset-event stream into a list of
/// [`MdBlock`]s.
///
/// PR-A: returns an empty `Vec` so the rest of the crate compiles.
/// PR-B will mirror the existing `tui/src/markdown_render.rs` logic
/// (block dispatch on `Tag` / `TagEnd`, inline accumulation, table
/// header / row tracking, etc.) here.
fn walk_blocks<'a, I>(_iter: I) -> Vec<MdBlock>
where
    I: Iterator<Item = (Event<'a>, Range<usize>)>,
{
    // TODO PR-B: implement the AST walker. The full pulldown-cmark
    // walker mirrors the existing tui/src/markdown_render.rs logic and
    // lands in PR-B alongside the TUI refactor. For now this returns
    // an empty Vec so callers compile.
    Vec::new()
}

/// Snapshot used by [`IncrementalParser`] to skip already-parsed prefix
/// on streaming input.
///
/// The snapshot records *both* the byte offset of the longest "stable"
/// prefix and the cached block list for that prefix. PR-C will use
/// these together to splice newly-parsed tail blocks onto the cached
/// head without rewalking the entire source.
#[derive(Debug, Clone, Default)]
pub struct ParseSnapshot {
    /// Byte offset up to which the input has been "stably" parsed.
    ///
    /// Bytes in `0..stable_prefix_end` are guaranteed not to be
    /// reinterpreted by appending more input – e.g. they sit before a
    /// closed fenced code block, a thematic break, or a paragraph
    /// terminated by a blank line.
    pub stable_prefix_end: usize,
    /// Cached blocks for the stable prefix.
    pub blocks: Vec<MdBlock>,
}

/// Incremental markdown parser optimised for streaming agent replies.
///
/// The contract: callers append bytes to a growing [`String`] (the
/// same `raw_source` model used by
/// `codex-rs/tui/src/streaming/controller.rs`). On each call to
/// [`Self::parse`], the parser identifies the last "closed" block,
/// reparses only the open tail, and returns the new full block list
/// plus an updated snapshot.
///
/// PR-A note: the current implementation does a full reparse on every
/// call. PR-C will replace it with the byte-range-aware incremental
/// algorithm described in `docs/desktop-architecture.md` §4. The
/// public API is shaped now so neither callers nor the TUI refactor
/// (PR-B) need to change when PR-C lands.
pub struct IncrementalParser {
    snapshot: ParseSnapshot,
}

impl IncrementalParser {
    /// Construct a parser with an empty snapshot.
    pub fn new() -> Self {
        Self {
            snapshot: ParseSnapshot::default(),
        }
    }

    /// Borrow the current snapshot. Useful in tests and for callers
    /// that want to persist parse progress across processes.
    pub fn snapshot(&self) -> &ParseSnapshot {
        &self.snapshot
    }

    /// Parse `src` (the entire growing string) given the previous
    /// snapshot. Returns the full block list.
    ///
    /// PR-A implementation: simple full reparse on every call; PR-C
    /// will replace this with the byte-range-aware incremental
    /// algorithm described in `docs/desktop-architecture.md` §4.
    pub fn parse(&mut self, src: &str) -> MdDocument {
        let doc = parse_full(src);
        self.snapshot = ParseSnapshot {
            stable_prefix_end: src.len(),
            blocks: doc.blocks.clone(),
        };
        doc
    }
}

impl Default for IncrementalParser {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn parse_full_empty_returns_empty_document() {
        assert_eq!(parse_full(""), MdDocument { blocks: vec![] });
    }

    #[test]
    fn parse_full_heading_returns_empty_in_pr_a() {
        // PR-A skeleton: walker is a stub, so any input yields an empty
        // block list. The "real" assertion lives in the ignored test
        // below and will be unignored once PR-B implements the walker.
        assert_eq!(parse_full("# hello"), MdDocument { blocks: vec![] });
    }

    #[test]
    #[ignore]
    fn parse_full_heading_eventual_shape() {
        // TODO PR-B: implement walker. Once the walker is implemented,
        // remove the `#[ignore]` and this test will lock in the
        // expected shape for `# hello`.
        use crate::Inline;
        let doc = parse_full("# hello");
        assert_eq!(
            doc,
            MdDocument {
                blocks: vec![MdBlock::Heading {
                    level: 1,
                    inlines: vec![Inline::Text("hello".into())],
                }],
            }
        );
    }

    #[test]
    fn incremental_parser_updates_stable_prefix_end() {
        let mut p = IncrementalParser::new();
        let _ = p.parse("hello");
        assert_eq!(p.snapshot().stable_prefix_end, 5);
    }

    #[test]
    fn incremental_parser_default_snapshot_is_zero() {
        let p = IncrementalParser::new();
        assert_eq!(p.snapshot().stable_prefix_end, 0);
        assert!(p.snapshot().blocks.is_empty());
    }
}
