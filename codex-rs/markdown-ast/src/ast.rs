//! Block-level markdown AST.
//!
//! These types describe markdown structure at the *block* granularity –
//! headings, paragraphs, lists, and so on. Inline content (text spans,
//! emphasis, links, …) lives in [`crate::inline`] so callers can walk the
//! two layers independently.
//!
//! Every type derives `Serialize` / `Deserialize` so an AST can be
//! cached on disk or shipped across a process boundary unchanged.

use serde::{Deserialize, Serialize};

/// A parsed markdown document: an ordered list of top-level blocks.
///
/// `MdDocument` is intentionally a thin wrapper around `Vec<MdBlock>` so
/// that future fields (e.g. link reference definitions, footnote tables)
/// can be added without changing call sites that pattern-match on the
/// block list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MdDocument {
    /// Top-level blocks in source order.
    pub blocks: Vec<MdBlock>,
}

/// A single block-level markdown node.
///
/// The variants intentionally mirror the subset of pulldown-cmark events
/// already handled by `codex-rs/tui/src/markdown_render.rs`. Adding
/// CommonMark constructs (footnotes, definition lists, etc.) is a
/// follow-up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MdBlock {
    /// ATX or setext heading. `level` is 1..=6.
    Heading {
        /// Heading level. Always within `1..=6`.
        level: u8,
        /// Heading content as inline spans.
        inlines: Vec<crate::Inline>,
    },
    /// A paragraph composed of inline spans.
    Paragraph(Vec<crate::Inline>),
    /// Indented or fenced code block.
    ///
    /// `lang` is `Some` only for fenced code blocks with an info string.
    /// `text` preserves the source exactly (including trailing
    /// newlines), so callers can hand it to a syntax highlighter
    /// verbatim.
    CodeBlock {
        /// Language tag from a fenced code block, if any.
        lang: Option<String>,
        /// Verbatim code contents.
        text: String,
    },
    /// A blockquote containing nested blocks.
    BlockQuote(Vec<MdBlock>),
    /// An ordered or unordered list.
    ///
    /// Each item is itself a `Vec<MdBlock>` because a list item may
    /// contain multiple paragraphs, nested lists, code blocks, etc.
    List {
        /// Whether the list is ordered (`1.`, `2.`, …) or unordered.
        ordered: bool,
        /// One entry per item.
        items: Vec<Vec<MdBlock>>,
    },
    /// A pipe table.
    ///
    /// `headers` holds the cells of the header row; `rows` holds each
    /// body row. Each cell is a sequence of inline spans.
    Table {
        /// Cells of the header row.
        headers: Vec<Vec<crate::Inline>>,
        /// Body rows; each row has the same number of cells as `headers`.
        rows: Vec<Vec<Vec<crate::Inline>>>,
    },
    /// A raw HTML block (rendered as-is or sanitised by the caller).
    HtmlBlock(String),
    /// A horizontal rule.
    ThematicBreak,
}

impl MdBlock {
    /// Returns `true` if this block is a "terminator" – i.e. its
    /// presence at the tail of a streaming buffer is unambiguous and
    /// will not be reinterpreted by future bytes.
    ///
    /// Used by [`crate::IncrementalParser`] to determine whether the
    /// block at the end of a streaming buffer is "stable". For now the
    /// only unconditional terminator is [`MdBlock::ThematicBreak`];
    /// other heuristics (closed code fences, blank-line-terminated
    /// paragraphs, etc.) are computed at parse time from the
    /// pulldown-cmark event stream rather than from the block tree, so
    /// this method is conservative on purpose.
    pub fn is_open_terminator(&self) -> bool {
        matches!(self, Self::ThematicBreak)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inline::{Inline, InlineCode, InlineImage, InlineLink};
    use pretty_assertions::assert_eq;

    fn round_trip(block: &MdBlock) -> MdBlock {
        let json = serde_json::to_string(block).expect("serialize");
        serde_json::from_str(&json).expect("deserialize")
    }

    #[test]
    fn is_open_terminator_thematic_break() {
        assert!(MdBlock::ThematicBreak.is_open_terminator());
    }

    #[test]
    fn is_open_terminator_paragraph_is_false() {
        assert!(!MdBlock::Paragraph(vec![Inline::Text("x".into())]).is_open_terminator());
    }

    #[test]
    fn round_trip_heading() {
        let b = MdBlock::Heading {
            level: 2,
            inlines: vec![Inline::Text("Title".into())],
        };
        assert_eq!(round_trip(&b), b);
    }

    #[test]
    fn round_trip_paragraph() {
        let b = MdBlock::Paragraph(vec![
            Inline::Text("hello ".into()),
            Inline::Strong(vec![Inline::Text("world".into())]),
        ]);
        assert_eq!(round_trip(&b), b);
    }

    #[test]
    fn round_trip_code_block() {
        let b = MdBlock::CodeBlock {
            lang: Some("rust".into()),
            text: "fn main() {}\n".into(),
        };
        assert_eq!(round_trip(&b), b);
    }

    #[test]
    fn round_trip_block_quote() {
        let b = MdBlock::BlockQuote(vec![MdBlock::Paragraph(vec![Inline::Text("q".into())])]);
        assert_eq!(round_trip(&b), b);
    }

    #[test]
    fn round_trip_list() {
        let b = MdBlock::List {
            ordered: true,
            items: vec![
                vec![MdBlock::Paragraph(vec![Inline::Text("one".into())])],
                vec![MdBlock::Paragraph(vec![Inline::Text("two".into())])],
            ],
        };
        assert_eq!(round_trip(&b), b);
    }

    #[test]
    fn round_trip_table() {
        let b = MdBlock::Table {
            headers: vec![vec![Inline::Text("h1".into())], vec![Inline::Text("h2".into())]],
            rows: vec![vec![
                vec![Inline::Text("a".into())],
                vec![Inline::Text("b".into())],
            ]],
        };
        assert_eq!(round_trip(&b), b);
    }

    #[test]
    fn round_trip_html_block() {
        let b = MdBlock::HtmlBlock("<div>x</div>".into());
        assert_eq!(round_trip(&b), b);
    }

    #[test]
    fn round_trip_thematic_break() {
        let b = MdBlock::ThematicBreak;
        assert_eq!(round_trip(&b), b);
    }

    #[test]
    fn round_trip_inline_link_in_paragraph() {
        let b = MdBlock::Paragraph(vec![Inline::Link(InlineLink {
            url: "https://example.com".into(),
            title: "ex".into(),
            children: vec![Inline::Text("link".into())],
        })]);
        assert_eq!(round_trip(&b), b);
    }

    #[test]
    fn round_trip_inline_image_in_paragraph() {
        let b = MdBlock::Paragraph(vec![Inline::Image(InlineImage {
            url: "https://example.com/x.png".into(),
            title: "img".into(),
            alt: vec![Inline::Text("alt".into())],
        })]);
        assert_eq!(round_trip(&b), b);
    }

    #[test]
    fn round_trip_inline_code_in_paragraph() {
        let b = MdBlock::Paragraph(vec![Inline::Code(InlineCode { text: "x".into() })]);
        assert_eq!(round_trip(&b), b);
    }
}
