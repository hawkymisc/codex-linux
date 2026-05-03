//! Byte-range-aware incremental block cache used by
//! [`crate::IncrementalParser`].
//!
//! See `docs/desktop-architecture.md` §4.2 / Gen 5 mutation pass for the
//! algorithm specification. The short version:
//!
//! 1. Find the cut: last entry where `is_open == false` ⇒ end byte `S`.
//! 2. Re-parse only `src[S..N]` via pulldown-cmark, mapping every event
//!    range back into the full document by adding `S`.
//! 3. Fingerprint each new block via xxhash3; tail match-and-replace
//!    against the existing cache.
//! 4. Track `is_open` for unclosed fences / unfinished setext headings.
//! 5. Append-only invalidation – the raw source only grows.
//!
//! The cache is *private*; the public surface is the methods on
//! [`crate::IncrementalParser`]. Tests live in
//! `markdown-ast/tests/incremental.rs`.
//!
//! # Conservativeness
//!
//! Detecting "this block can still grow / be reinterpreted" is hard in
//! the general case (lazy continuation, setext headings, fenced code).
//! We use a simple rule that is conservative but provably correct: the
//! **last** top-level block is always `is_open == true`; every earlier
//! block is `is_open == false`. Empirically this still gives big wins
//! on streaming inputs because the cache lets us skip re-parsing the
//! large stable prefix on every push.

use std::ops::Range;

use crate::ast::MdBlock;

/// One cached top-level block plus the metadata needed to decide
/// whether a re-parse can reuse it.
#[derive(Debug, Clone)]
pub(crate) struct BlockCacheEntry {
    /// Byte range of the block in the full source.
    pub(crate) byte_range: Range<usize>,
    /// The parsed block.
    pub(crate) block: MdBlock,
    /// xxhash3 of the source bytes covered by `byte_range`. Surfaced
    /// to consumers via [`crate::IncrementalParser::block_fingerprints`]
    /// so callers can validate cache identity across a process
    /// boundary without serialising the full AST.
    pub(crate) fingerprint: u64,
    /// `true` if the block could still grow / be reinterpreted by
    /// future bytes (the trailing block of any partial source is
    /// always treated as open).
    pub(crate) is_open: bool,
}

/// In-memory cache of already-parsed top-level blocks.
#[derive(Debug, Clone, Default)]
pub(crate) struct BlockCache {
    pub(crate) entries: Vec<BlockCacheEntry>,
    /// Highest byte offset covered by a `is_open == false` entry. The
    /// next re-parse can safely start here.
    pub(crate) parsed_up_to: usize,
}

impl BlockCache {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Return the byte offset where the next re-parse should start. The
    /// algorithm walks backwards from the end and stops at the last
    /// `is_open == false` entry – that entry's `byte_range.end` is the
    /// safe cut point because everything after it might need to be
    /// reinterpreted.
    pub(crate) fn cut_byte(&self) -> usize {
        for entry in self.entries.iter().rev() {
            if !entry.is_open {
                return entry.byte_range.end;
            }
        }
        0
    }

    /// Truncate the cache so that no entry overlaps the byte range
    /// `[from..)`. Used right before appending freshly-parsed entries
    /// from a re-parse of the tail.
    pub(crate) fn truncate_at_byte(&mut self, from: usize) {
        self.entries.retain(|e| e.byte_range.end <= from);
        self.parsed_up_to = self
            .entries
            .iter()
            .filter(|e| !e.is_open)
            .map(|e| e.byte_range.end)
            .max()
            .unwrap_or(0);
    }

    /// Snapshot view of the parsed blocks, in source order.
    pub(crate) fn blocks_view(&self) -> Vec<MdBlock> {
        self.entries.iter().map(|e| e.block.clone()).collect()
    }

    /// Number of stable (`is_open == false`) entries.
    pub(crate) fn stable_count(&self) -> usize {
        self.entries.iter().filter(|e| !e.is_open).count()
    }
}

/// Compute the streaming fingerprint of a slice of source bytes.
pub(crate) fn fingerprint(bytes: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_64(bytes)
}
