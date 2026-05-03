//! Inline (span-level) markdown AST.
//!
//! `Inline` represents the leaves of a parsed markdown document: text
//! runs, emphasis, links, code spans, raw HTML, and so on. Block-level
//! constructs ([`crate::ast::MdBlock`]) own `Vec<Inline>` for their
//! content.
//!
//! These types are deliberately backend-neutral – they describe
//! *structure*, not styling. A consumer crate (TUI, GTK, web) is
//! expected to walk the inline tree and emit the appropriate primitives
//! for its surface.

use serde::{Deserialize, Serialize};

/// A single inline (span-level) markdown node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Inline {
    /// A run of plain text.
    Text(String),
    /// A soft line break (newline in source that should usually render
    /// as a space).
    SoftBreak,
    /// A hard line break (e.g. trailing two spaces or `\` in source).
    HardBreak,
    /// A backtick-delimited code span.
    Code(InlineCode),
    /// `*emph*` / `_emph_` content.
    Emphasis(Vec<Inline>),
    /// `**strong**` / `__strong__` content.
    Strong(Vec<Inline>),
    /// `~~strikethrough~~` content (GFM extension).
    Strikethrough(Vec<Inline>),
    /// `[label](url)` link.
    Link(InlineLink),
    /// `![alt](url)` image.
    Image(InlineImage),
    /// Raw inline HTML (e.g. `<kbd>`).
    Html(String),
}

/// A code span. Held as a struct so callers can attach more metadata
/// (e.g. detected language) in future without breaking the enum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlineCode {
    /// Verbatim contents of the code span (without surrounding
    /// backticks).
    pub text: String,
}

/// A hyperlink.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlineLink {
    /// Resolved destination URL (or local file path for local-file
    /// links).
    pub url: String,
    /// Optional title attribute (often empty).
    pub title: String,
    /// The link's visible label, parsed as inline content.
    pub children: Vec<Inline>,
}

/// An image reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlineImage {
    /// Resolved image URL.
    pub url: String,
    /// Optional title attribute (often empty).
    pub title: String,
    /// Alt text, parsed as inline content (so callers can render
    /// emphasis inside alt text if desired).
    pub alt: Vec<Inline>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn round_trip(i: &Inline) -> Inline {
        let json = serde_json::to_string(i).expect("serialize");
        serde_json::from_str(&json).expect("deserialize")
    }

    #[test]
    fn round_trip_text() {
        let i = Inline::Text("hi".into());
        assert_eq!(round_trip(&i), i);
    }

    #[test]
    fn round_trip_soft_break() {
        let i = Inline::SoftBreak;
        assert_eq!(round_trip(&i), i);
    }

    #[test]
    fn round_trip_hard_break() {
        let i = Inline::HardBreak;
        assert_eq!(round_trip(&i), i);
    }

    #[test]
    fn round_trip_code() {
        let i = Inline::Code(InlineCode { text: "x".into() });
        assert_eq!(round_trip(&i), i);
    }

    #[test]
    fn round_trip_emphasis() {
        let i = Inline::Emphasis(vec![Inline::Text("e".into())]);
        assert_eq!(round_trip(&i), i);
    }

    #[test]
    fn round_trip_strong() {
        let i = Inline::Strong(vec![Inline::Text("s".into())]);
        assert_eq!(round_trip(&i), i);
    }

    #[test]
    fn round_trip_strikethrough() {
        let i = Inline::Strikethrough(vec![Inline::Text("s".into())]);
        assert_eq!(round_trip(&i), i);
    }

    #[test]
    fn round_trip_link() {
        let i = Inline::Link(InlineLink {
            url: "https://example.com".into(),
            title: "t".into(),
            children: vec![Inline::Text("label".into())],
        });
        assert_eq!(round_trip(&i), i);
    }

    #[test]
    fn round_trip_image() {
        let i = Inline::Image(InlineImage {
            url: "https://example.com/x.png".into(),
            title: "t".into(),
            alt: vec![Inline::Text("alt".into())],
        });
        assert_eq!(round_trip(&i), i);
    }

    #[test]
    fn round_trip_html() {
        let i = Inline::Html("<kbd>k</kbd>".into());
        assert_eq!(round_trip(&i), i);
    }
}
