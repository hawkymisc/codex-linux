//! Integration tests for the byte-range-aware incremental parser.
//!
//! The load-bearing property exercised here is:
//!
//! > For every prefix of any markdown text, an `IncrementalParser` fed
//! > that prefix must produce blocks identical to `parse_full(prefix)`.
//!
//! Drift would imply the cache is reusing entries that should have
//! been invalidated. The "cut sweep" test below tries every possible
//! split between a single push and a follow-up – it is by far the
//! most likely test to surface off-by-one bugs in the boundary
//! handling.

use codex_markdown_ast::{IncrementalParser, parse_full};

#[test]
fn incremental_matches_full_on_simple_growing_paragraph() {
    let full = "hello world this is a paragraph";
    let mut inc = IncrementalParser::new();
    for ch in full.chars() {
        inc.push(&ch.to_string());
    }
    assert_eq!(inc.blocks(), parse_full(full).blocks.as_slice());
}

#[test]
fn incremental_matches_full_on_growing_code_block() {
    let stages = ["# Title\n", "```rust\n", "fn main() {\n", "}\n", "```\n"];
    let mut inc = IncrementalParser::new();
    let mut acc = String::new();
    for s in stages {
        inc.push(s);
        acc.push_str(s);
        assert_eq!(
            inc.blocks(),
            parse_full(&acc).blocks.as_slice(),
            "drift after pushing {s:?}",
        );
    }
}

#[test]
fn incremental_matches_full_on_setext_heading_promotion() {
    // a paragraph that becomes an h1 when "===" arrives
    let stages = ["Hello\n", "=====\n"];
    let mut inc = IncrementalParser::new();
    let mut acc = String::new();
    for s in stages {
        inc.push(s);
        acc.push_str(s);
        assert_eq!(inc.blocks(), parse_full(&acc).blocks.as_slice());
    }
}

#[test]
fn incremental_matches_full_on_thematic_break_and_list() {
    let full = "para1\n\n---\n\n* a\n* b\n* c\n";
    for cut in 1..full.len() {
        // Skip cuts that fall inside a multi-byte UTF-8 sequence (none
        // in this fixture, but be explicit so a future ASCII change is
        // safe).
        if !full.is_char_boundary(cut) {
            continue;
        }
        let mut inc = IncrementalParser::new();
        inc.push(&full[..cut]);
        inc.push(&full[cut..]);
        assert_eq!(
            inc.blocks(),
            parse_full(full).blocks.as_slice(),
            "drift with cut at byte {cut}",
        );
    }
}

#[test]
fn stable_block_count_grows_monotonically() {
    let stages = ["one\n\n", "two\n\n", "three"];
    let mut inc = IncrementalParser::new();
    let mut prev = 0usize;
    for s in stages {
        inc.push(s);
        assert!(
            inc.stable_block_count() >= prev,
            "stable count regressed: {prev} -> {} after {s:?}",
            inc.stable_block_count(),
        );
        prev = inc.stable_block_count();
    }
}

#[test]
fn raw_source_is_concatenation_of_pushes() {
    let mut inc = IncrementalParser::new();
    inc.push("hello ");
    inc.push("world");
    assert_eq!(inc.raw_source(), "hello world");
}

#[test]
fn empty_push_is_noop() {
    let mut inc = IncrementalParser::new();
    inc.push("hello");
    let before = inc.blocks().to_vec();
    inc.push("");
    assert_eq!(inc.blocks(), before.as_slice());
    assert_eq!(inc.raw_source(), "hello");
}

#[test]
fn incremental_is_actually_incremental() {
    use std::time::Instant;
    let big = include_str!("fixtures/large.md");
    let mut inc = IncrementalParser::new();
    let chunks: Vec<&[u8]> = big.as_bytes().chunks(64).collect();
    let t = Instant::now();
    for chunk in chunks {
        // Each chunk must be valid UTF-8; the fixture is ASCII so any
        // 64-byte boundary is safe.
        inc.push(std::str::from_utf8(chunk).expect("ascii fixture"));
    }
    let elapsed = t.elapsed();
    eprintln!(
        "[bench] incremental parse of {} bytes in {} chunks: {:?}",
        big.len(),
        big.len().div_ceil(64),
        elapsed,
    );
    assert!(
        elapsed.as_millis() < 200,
        "incremental parse took {elapsed:?}",
    );
    // Sanity: the incremental result still matches parse_full.
    assert_eq!(inc.blocks(), parse_full(big).blocks.as_slice());
}
