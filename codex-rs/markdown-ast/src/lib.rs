#![forbid(unsafe_code)]

//! Backend-neutral markdown AST and incremental parser used by both the TUI
//! and the codex-desktop GUI.
//!
//! This crate intentionally renders nothing; consumers walk the [`MdBlock`]
//! tree to produce ratatui Lines, GTK Widgets, or any other surface.
//!
//! The parser uses [`pulldown_cmark`]'s offset iterator so we can map every
//! block back to a byte range in the source. That enables the
//! [`IncrementalParser`] to amortise streaming agent replies: a delta of
//! length Δ on top of N bytes runs in roughly O(Δ), not O(N).
//!
//! # Design intent
//!
//! * **Backend neutrality** – The types in this crate must not depend on
//!   any rendering library (ratatui, GTK, web, etc.). Each consumer owns
//!   its own walker that maps [`MdBlock`] / [`Inline`] values to the
//!   appropriate surface primitives.
//! * **Streaming friendly** – Agent replies arrive as a growing
//!   `String`. The [`IncrementalParser`] is the long-lived object that
//!   amortises reparses across calls.
//! * **Round-trippable via serde** – The AST is `Serialize` /
//!   `Deserialize` so it can be cached, sent across a process boundary
//!   (e.g. between the codex-desktop UI process and a worker), and
//!   compared in tests via JSON snapshots.
//!
//! # PR layout
//!
//! This crate lands in three pieces:
//!
//! * **PR-A** (this PR): the crate skeleton, types, and a stub
//!   [`parse_full`] / [`IncrementalParser`] that returns an empty
//!   document. Nothing downstream depends on it yet.
//! * **PR-B**: implement the pulldown-cmark walker (mirroring the
//!   existing logic in `codex-rs/tui/src/markdown_render.rs`) and switch
//!   the TUI to consume the AST. TUI snapshot tests must remain
//!   bit-identical.
//! * **PR-C**: replace the naive full reparse in
//!   [`IncrementalParser::parse`] with a byte-range-aware incremental
//!   algorithm.

pub mod ast;
pub(crate) mod incremental;
pub mod inline;
pub mod parser;

pub use ast::{MdBlock, MdDocument};
pub use inline::{Inline, InlineCode, InlineImage, InlineLink};
pub use parser::{IncrementalParser, ParseSnapshot, parse_full};
