//! Markdown parser frontends.
//!
//! Two entry points are provided:
//!
//! * [`parse_full`] – one-shot, stateless parse of an entire string.
//! * [`IncrementalParser`] – long-lived parser optimised for streaming
//!   agent replies (the same `raw_source` model used by
//!   `codex-rs/tui/src/streaming/controller.rs`).
//!
//! The walker mirrors the option set used by `tui/src/markdown_render.rs`
//! so a future TUI refactor can drop the AST in without changing
//! observable behaviour. The incremental algorithm is currently a full
//! reparse on every call; the byte-range-aware variant described in
//! `docs/desktop-architecture.md` §4 lands in PR-C.

use crate::ast::{MdBlock, MdDocument};
use crate::inline::{Inline, InlineCode, InlineImage, InlineLink};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use std::ops::Range;

/// One-shot full parse.
pub fn parse_full(src: &str) -> MdDocument {
    let opts = default_options();
    let parser = Parser::new_ext(src, opts).into_offset_iter();
    let mut peekable = parser.peekable();
    let blocks = collect_blocks(&mut peekable, None);
    MdDocument { blocks }
}

/// Default pulldown-cmark options, matching the TUI renderer's set.
fn default_options() -> Options {
    let mut o = Options::empty();
    o.insert(Options::ENABLE_STRIKETHROUGH);
    o.insert(Options::ENABLE_TABLES);
    o.insert(Options::ENABLE_TASKLISTS);
    o
}

/// Collect blocks from `iter` until either the iterator is exhausted or
/// an `End(stop)` event is observed. The matching `End` is consumed.
fn collect_blocks<'a, I>(iter: &mut std::iter::Peekable<I>, stop: Option<TagEnd>) -> Vec<MdBlock>
where
    I: Iterator<Item = (Event<'a>, Range<usize>)>,
{
    let mut blocks = Vec::new();
    while let Some((event, _range)) = iter.next() {
        match event {
            Event::Start(tag) => {
                if let Some(block) = consume_block(tag, iter) {
                    blocks.push(block);
                }
            }
            Event::End(end) => {
                if Some(end) == stop {
                    return blocks;
                }
                // Stray End at the top level – ignore. pulldown-cmark
                // should not emit these in well-formed input.
            }
            Event::Rule => blocks.push(MdBlock::ThematicBreak),
            Event::Html(s) => blocks.push(MdBlock::HtmlBlock(s.into_string())),
            Event::Text(_)
            | Event::Code(_)
            | Event::SoftBreak
            | Event::HardBreak
            | Event::FootnoteReference(_)
            | Event::TaskListMarker(_)
            | Event::InlineHtml(_) => {
                // Top-level inline events – wrap in a synthetic paragraph
                // for graceful degradation.
                let mut inlines = Vec::new();
                push_inline_event(&mut inlines, event);
                blocks.push(MdBlock::Paragraph(inlines));
            }
        }
    }
    blocks
}

/// Build a single [`MdBlock`] from a `Start(tag)` event already consumed
/// from the iterator. Returns `None` if the tag should be skipped (e.g.
/// the never-emitted top-level Item).
fn consume_block<'a, I>(tag: Tag<'a>, iter: &mut std::iter::Peekable<I>) -> Option<MdBlock>
where
    I: Iterator<Item = (Event<'a>, Range<usize>)>,
{
    match tag {
        Tag::Heading { level, .. } => {
            let inlines = collect_inlines(iter, TagEnd::Heading(level));
            Some(MdBlock::Heading {
                level: heading_level_to_u8(level),
                inlines,
            })
        }
        Tag::Paragraph => {
            let inlines = collect_inlines(iter, TagEnd::Paragraph);
            Some(MdBlock::Paragraph(inlines))
        }
        Tag::CodeBlock(kind) => {
            let lang = match kind {
                CodeBlockKind::Indented => None,
                CodeBlockKind::Fenced(lang) => {
                    let s = lang.into_string();
                    if s.is_empty() { None } else { Some(s) }
                }
            };
            let mut text = String::new();
            for (event, _r) in iter.by_ref() {
                match event {
                    Event::Text(s) => text.push_str(&s),
                    Event::End(TagEnd::CodeBlock) => break,
                    _ => {
                        // Pulldown-cmark only emits Text inside code blocks
                        // for fenced/indented; defensive ignore.
                    }
                }
            }
            Some(MdBlock::CodeBlock { lang, text })
        }
        Tag::BlockQuote => {
            let inner = collect_blocks(iter, Some(TagEnd::BlockQuote));
            Some(MdBlock::BlockQuote(inner))
        }
        Tag::List(start) => {
            let ordered = start.is_some();
            let mut items: Vec<Vec<MdBlock>> = Vec::new();
            loop {
                match iter.next() {
                    Some((Event::Start(Tag::Item), _)) => {
                        items.push(collect_item_blocks(iter));
                    }
                    Some((Event::End(TagEnd::List(_)), _)) => break,
                    Some(_) => continue, // strays
                    None => break,
                }
            }
            Some(MdBlock::List { ordered, items })
        }
        Tag::Item => {
            // Should be consumed inside Tag::List above; if reached at
            // top level, treat the body as a one-item list.
            let inner = collect_item_blocks(iter);
            Some(MdBlock::List {
                ordered: false,
                items: vec![inner],
            })
        }
        Tag::Table(_alignments) => {
            let mut headers: Vec<Vec<Inline>> = Vec::new();
            let mut rows: Vec<Vec<Vec<Inline>>> = Vec::new();
            loop {
                match iter.next() {
                    Some((Event::Start(Tag::TableHead), _)) => {
                        loop {
                            match iter.next() {
                                Some((Event::Start(Tag::TableCell), _)) => {
                                    headers.push(collect_inlines(iter, TagEnd::TableCell));
                                }
                                Some((Event::End(TagEnd::TableHead), _)) => break,
                                Some(_) => continue,
                                None => break,
                            }
                        }
                    }
                    Some((Event::Start(Tag::TableRow), _)) => {
                        let mut row: Vec<Vec<Inline>> = Vec::new();
                        loop {
                            match iter.next() {
                                Some((Event::Start(Tag::TableCell), _)) => {
                                    row.push(collect_inlines(iter, TagEnd::TableCell));
                                }
                                Some((Event::End(TagEnd::TableRow), _)) => break,
                                Some(_) => continue,
                                None => break,
                            }
                        }
                        rows.push(row);
                    }
                    Some((Event::End(TagEnd::Table), _)) => break,
                    Some(_) => continue,
                    None => break,
                }
            }
            Some(MdBlock::Table { headers, rows })
        }
        // Inline-only tags reaching this point are degenerate; wrap in
        // a paragraph so we don't lose data.
        Tag::Emphasis
        | Tag::Strong
        | Tag::Strikethrough
        | Tag::Link { .. }
        | Tag::Image { .. } => {
            let mut inlines = Vec::new();
            push_inline_start(&mut inlines, tag, iter);
            Some(MdBlock::Paragraph(inlines))
        }
        // Tag variants we don't model (footnotes, html blocks via Tag,
        // metadata blocks, etc.) — fall through to a no-op so the parser
        // keeps walking the surrounding structure.
        _ => None,
    }
}

/// Collect all blocks belonging to a single list item until its
/// matching `End(Item)`.
fn collect_item_blocks<'a, I>(iter: &mut std::iter::Peekable<I>) -> Vec<MdBlock>
where
    I: Iterator<Item = (Event<'a>, Range<usize>)>,
{
    collect_blocks(iter, Some(TagEnd::Item))
}

fn heading_level_to_u8(level: pulldown_cmark::HeadingLevel) -> u8 {
    use pulldown_cmark::HeadingLevel as H;
    match level {
        H::H1 => 1,
        H::H2 => 2,
        H::H3 => 3,
        H::H4 => 4,
        H::H5 => 5,
        H::H6 => 6,
    }
}

/// Drain inline events from the iterator until the matching `End(stop)`.
fn collect_inlines<'a, I>(iter: &mut std::iter::Peekable<I>, stop: TagEnd) -> Vec<Inline>
where
    I: Iterator<Item = (Event<'a>, Range<usize>)>,
{
    let mut out: Vec<Inline> = Vec::new();
    while let Some((event, _r)) = iter.next() {
        match event {
            Event::End(end) if end == stop => return out,
            Event::Start(tag) => push_inline_start(&mut out, tag, iter),
            other => push_inline_event(&mut out, other),
        }
    }
    out
}

fn push_inline_event(out: &mut Vec<Inline>, event: Event<'_>) {
    match event {
        Event::Text(s) => out.push(Inline::Text(s.into_string())),
        Event::Code(s) => out.push(Inline::Code(InlineCode { text: s.into_string() })),
        Event::SoftBreak => out.push(Inline::SoftBreak),
        Event::HardBreak => out.push(Inline::HardBreak),
        Event::Html(s) | Event::InlineHtml(s) => out.push(Inline::Html(s.into_string())),
        Event::Rule
        | Event::FootnoteReference(_)
        | Event::TaskListMarker(_)
        | Event::Start(_)
        | Event::End(_) => {
            // Ignore — these either signal block transitions handled by
            // the outer walker, or features not yet in the AST surface.
        }
    }
}

fn push_inline_start<'a, I>(out: &mut Vec<Inline>, tag: Tag<'a>, iter: &mut std::iter::Peekable<I>)
where
    I: Iterator<Item = (Event<'a>, Range<usize>)>,
{
    match tag {
        Tag::Emphasis => {
            let inner = collect_inlines(iter, TagEnd::Emphasis);
            out.push(Inline::Emphasis(inner));
        }
        Tag::Strong => {
            let inner = collect_inlines(iter, TagEnd::Strong);
            out.push(Inline::Strong(inner));
        }
        Tag::Strikethrough => {
            let inner = collect_inlines(iter, TagEnd::Strikethrough);
            out.push(Inline::Strikethrough(inner));
        }
        Tag::Link {
            dest_url, title, ..
        } => {
            let children = collect_inlines(iter, TagEnd::Link);
            out.push(Inline::Link(InlineLink {
                url: dest_url.into_string(),
                title: title.into_string(),
                children,
            }));
        }
        Tag::Image {
            dest_url, title, ..
        } => {
            let alt = collect_inlines(iter, TagEnd::Image);
            out.push(Inline::Image(InlineImage {
                url: dest_url.into_string(),
                title: title.into_string(),
                alt,
            }));
        }
        // A nested block tag inside an inline context is malformed –
        // skip to the matching End to stay synchronised.
        _ => {
            let mut depth: u32 = 1;
            for (event, _r) in iter.by_ref() {
                match event {
                    Event::Start(_) => depth += 1,
                    Event::End(_) => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Snapshot used by [`IncrementalParser`] to skip already-parsed prefix.
#[derive(Debug, Clone, Default)]
pub struct ParseSnapshot {
    pub stable_prefix_end: usize,
    pub blocks: Vec<MdBlock>,
}

/// Incremental markdown parser.
///
/// PR-B implementation: full reparse on every `parse` call but with a
/// real walker so callers receive correctly-shaped `MdBlock` trees.
/// PR-C will replace the body of `parse` with the byte-range-aware
/// algorithm in `docs/desktop-architecture.md` §4.
pub struct IncrementalParser {
    snapshot: ParseSnapshot,
}

impl IncrementalParser {
    pub fn new() -> Self {
        Self {
            snapshot: ParseSnapshot::default(),
        }
    }

    pub fn snapshot(&self) -> &ParseSnapshot {
        &self.snapshot
    }

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
    use crate::Inline;
    use pretty_assertions::assert_eq;

    fn paragraph_text(text: &str) -> MdBlock {
        MdBlock::Paragraph(vec![Inline::Text(text.into())])
    }

    #[test]
    fn parse_full_empty_returns_empty_document() {
        assert_eq!(parse_full(""), MdDocument { blocks: vec![] });
    }

    #[test]
    fn walks_simple_paragraph() {
        assert_eq!(
            parse_full("hello world"),
            MdDocument {
                blocks: vec![paragraph_text("hello world")],
            }
        );
    }

    #[test]
    fn walks_atx_heading() {
        assert_eq!(
            parse_full("# Title"),
            MdDocument {
                blocks: vec![MdBlock::Heading {
                    level: 1,
                    inlines: vec![Inline::Text("Title".into())],
                }],
            }
        );
    }

    #[test]
    fn walks_heading_levels() {
        for level in 1..=6u8 {
            let src = format!("{} h", "#".repeat(level as usize));
            let doc = parse_full(&src);
            assert_eq!(doc.blocks.len(), 1, "level {level} src {src:?}");
            match &doc.blocks[0] {
                MdBlock::Heading { level: l, .. } => assert_eq!(*l, level),
                other => panic!("expected heading, got {other:?}"),
            }
        }
    }

    #[test]
    fn walks_thematic_break() {
        assert_eq!(
            parse_full("---"),
            MdDocument {
                blocks: vec![MdBlock::ThematicBreak],
            }
        );
    }

    #[test]
    fn walks_fenced_code_block_with_lang() {
        let src = "```rust\nfn main() {}\n```";
        let doc = parse_full(src);
        assert_eq!(doc.blocks.len(), 1);
        match &doc.blocks[0] {
            MdBlock::CodeBlock { lang, text } => {
                assert_eq!(lang.as_deref(), Some("rust"));
                assert_eq!(text, "fn main() {}\n");
            }
            other => panic!("expected code block, got {other:?}"),
        }
    }

    #[test]
    fn walks_fenced_code_block_without_lang() {
        let src = "```\nplain\n```";
        let doc = parse_full(src);
        match &doc.blocks[0] {
            MdBlock::CodeBlock { lang, text } => {
                assert_eq!(*lang, None);
                assert_eq!(text, "plain\n");
            }
            other => panic!("expected code block, got {other:?}"),
        }
    }

    #[test]
    fn walks_indented_code_block() {
        let src = "    indented\n    code\n";
        let doc = parse_full(src);
        match &doc.blocks[0] {
            MdBlock::CodeBlock { lang, text } => {
                assert_eq!(*lang, None);
                assert!(text.contains("indented"));
            }
            other => panic!("expected code block, got {other:?}"),
        }
    }

    #[test]
    fn walks_unordered_list() {
        let src = "- a\n- b\n";
        let doc = parse_full(src);
        match &doc.blocks[0] {
            MdBlock::List { ordered, items } => {
                assert!(!ordered);
                assert_eq!(items.len(), 2);
            }
            other => panic!("expected list, got {other:?}"),
        }
    }

    #[test]
    fn walks_ordered_list() {
        let src = "1. a\n2. b\n";
        let doc = parse_full(src);
        match &doc.blocks[0] {
            MdBlock::List { ordered, items } => {
                assert!(*ordered);
                assert_eq!(items.len(), 2);
            }
            other => panic!("expected list, got {other:?}"),
        }
    }

    #[test]
    fn walks_blockquote() {
        let src = "> quoted\n";
        let doc = parse_full(src);
        match &doc.blocks[0] {
            MdBlock::BlockQuote(inner) => {
                assert_eq!(inner.len(), 1);
                assert!(matches!(inner[0], MdBlock::Paragraph(_)));
            }
            other => panic!("expected blockquote, got {other:?}"),
        }
    }

    #[test]
    fn walks_inline_emphasis() {
        let doc = parse_full("*hi*");
        match &doc.blocks[0] {
            MdBlock::Paragraph(inlines) => {
                assert_eq!(inlines.len(), 1);
                assert!(matches!(inlines[0], Inline::Emphasis(_)));
            }
            other => panic!("expected paragraph, got {other:?}"),
        }
    }

    #[test]
    fn walks_inline_strong() {
        let doc = parse_full("**hi**");
        match &doc.blocks[0] {
            MdBlock::Paragraph(inlines) => {
                assert!(matches!(inlines[0], Inline::Strong(_)));
            }
            other => panic!("expected paragraph, got {other:?}"),
        }
    }

    #[test]
    fn walks_inline_strikethrough() {
        let doc = parse_full("~~gone~~");
        match &doc.blocks[0] {
            MdBlock::Paragraph(inlines) => {
                assert!(matches!(inlines[0], Inline::Strikethrough(_)));
            }
            other => panic!("expected paragraph, got {other:?}"),
        }
    }

    #[test]
    fn walks_inline_code() {
        let doc = parse_full("`x`");
        match &doc.blocks[0] {
            MdBlock::Paragraph(inlines) => {
                assert!(matches!(&inlines[0], Inline::Code(InlineCode { text }) if text == "x"));
            }
            other => panic!("expected paragraph, got {other:?}"),
        }
    }

    #[test]
    fn walks_link() {
        let doc = parse_full("[lbl](http://x)");
        match &doc.blocks[0] {
            MdBlock::Paragraph(inlines) => match &inlines[0] {
                Inline::Link(link) => {
                    assert_eq!(link.url, "http://x");
                    assert_eq!(link.children.len(), 1);
                }
                other => panic!("expected link, got {other:?}"),
            },
            other => panic!("expected paragraph, got {other:?}"),
        }
    }

    #[test]
    fn walks_image() {
        let doc = parse_full("![alt](url)");
        match &doc.blocks[0] {
            MdBlock::Paragraph(inlines) => match &inlines[0] {
                Inline::Image(img) => {
                    assert_eq!(img.url, "url");
                }
                other => panic!("expected image, got {other:?}"),
            },
            other => panic!("expected paragraph, got {other:?}"),
        }
    }

    #[test]
    fn walks_table_two_by_two() {
        let src = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let doc = parse_full(src);
        match &doc.blocks[0] {
            MdBlock::Table { headers, rows } => {
                assert_eq!(headers.len(), 2);
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].len(), 2);
            }
            other => panic!("expected table, got {other:?}"),
        }
    }

    #[test]
    fn walks_softbreak_in_paragraph() {
        let src = "line one\nline two";
        let doc = parse_full(src);
        match &doc.blocks[0] {
            MdBlock::Paragraph(inlines) => {
                assert!(inlines.iter().any(|i| matches!(i, Inline::SoftBreak)));
            }
            other => panic!("expected paragraph, got {other:?}"),
        }
    }

    #[test]
    fn walks_html_block() {
        let src = "<div>raw</div>\n";
        let doc = parse_full(src);
        // Pulldown-cmark may emit an HtmlBlock or wrap it differently
        // depending on the input; assert we got at least one HTML
        // surface in the output.
        assert!(
            doc.blocks
                .iter()
                .any(|b| matches!(b, MdBlock::HtmlBlock(_)))
        );
    }

    #[test]
    fn incremental_parser_default_snapshot_is_zero() {
        let p = IncrementalParser::new();
        assert_eq!(p.snapshot().stable_prefix_end, 0);
        assert!(p.snapshot().blocks.is_empty());
    }

    #[test]
    fn incremental_parser_updates_stable_prefix_end() {
        let mut p = IncrementalParser::new();
        let _ = p.parse("hello");
        assert_eq!(p.snapshot().stable_prefix_end, 5);
    }

    #[test]
    fn incremental_parser_appending_extends_paragraph() {
        let mut p = IncrementalParser::new();
        let doc1 = p.parse("hello");
        assert_eq!(doc1.blocks.len(), 1);
        let doc2 = p.parse("hello world");
        match &doc2.blocks[0] {
            MdBlock::Paragraph(inlines) => match &inlines[0] {
                Inline::Text(t) => assert_eq!(t, "hello world"),
                other => panic!("expected text, got {other:?}"),
            },
            other => panic!("expected paragraph, got {other:?}"),
        }
    }
}
