//! Convert [`codex_markdown_ast::MdBlock`] values into [`gtk::Widget`]s.
//!
//! This is the GTK consumer that mirrors the existing TUI walker in
//! `codex-rs/tui/src/markdown_render.rs`. The module is a *pure* transform –
//! it owns no state, talks to no streaming controller, and is safe to call
//! from the GTK main loop. The streaming controller in a later PR drives
//! it (see `docs/desktop-architecture.md` §4).
//!
//! Inline runs are flattened into a single [`pango::AttrList`] applied via
//! [`gtk::Label::set_attributes`] (deliberately *not* `set_markup`) so user
//! input cannot inject Pango markup that escapes the visible-text run.

use codex_markdown_ast::{Inline, MdBlock};
use gtk::pango;
use gtk::prelude::*;

/// Render a single block to a widget. The caller is responsible for
/// parenting the returned widget into a container.
pub fn block_to_widget(block: &MdBlock) -> gtk::Widget {
    match block {
        MdBlock::Paragraph(inlines) => paragraph_to_label(inlines).upcast(),
        MdBlock::Heading { level, inlines } => heading_to_label(*level, inlines).upcast(),
        MdBlock::CodeBlock { lang, text } => code_block_to_view(lang.as_deref(), text).upcast(),
        MdBlock::List { ordered, items } => list_to_box(*ordered, items).upcast(),
        MdBlock::BlockQuote(inner) => blockquote_to_box(inner).upcast(),
        MdBlock::Table { headers, rows } => table_to_grid(headers, rows).upcast(),
        MdBlock::ThematicBreak => thematic_break_widget().upcast(),
        MdBlock::HtmlBlock(html) => html_to_label(html).upcast(),
    }
}

fn paragraph_to_label(inlines: &[Inline]) -> gtk::Label {
    let label = gtk::Label::builder()
        .wrap(true)
        .wrap_mode(pango::WrapMode::WordChar)
        .xalign(0.0)
        .selectable(true)
        .build();
    let (text, attrs) = render_inlines(inlines);
    label.set_text(&text);
    label.set_attributes(Some(&attrs));
    label
}

fn heading_to_label(level: u8, inlines: &[Inline]) -> gtk::Label {
    let label = gtk::Label::builder()
        .wrap(true)
        .wrap_mode(pango::WrapMode::WordChar)
        .xalign(0.0)
        .selectable(true)
        .build();
    let (text, attrs) = render_inlines(inlines);
    let scale = match level {
        1 => 1.6,
        2 => 1.4,
        3 => 1.2,
        _ => 1.0,
    };
    let mut scale_attr = pango::AttrFloat::new_scale(scale);
    scale_attr.set_start_index(0);
    scale_attr.set_end_index(text.len() as u32);
    attrs.insert(scale_attr);
    label.set_text(&text);
    label.set_attributes(Some(&attrs));
    let class = format!("codex-heading-{}", level.clamp(1, 6));
    label.add_css_class(&class);
    label
}

fn code_block_to_view(lang: Option<&str>, text: &str) -> gtk::Frame {
    let buffer = match lang
        .filter(|s| !s.is_empty())
        .and_then(|l| sourceview5::LanguageManager::default().language(l))
    {
        Some(language) => sourceview5::Buffer::with_language(&language),
        None => sourceview5::Buffer::new(None),
    };
    buffer.set_text(text);

    let view = sourceview5::View::with_buffer(&buffer);
    view.set_editable(false);
    view.set_cursor_visible(false);
    view.set_monospace(true);
    view.set_wrap_mode(gtk::WrapMode::None);

    let frame = gtk::Frame::builder().child(&view).build();
    frame.add_css_class("codex-codeblock");
    frame
}

fn list_to_box(ordered: bool, items: &[Vec<MdBlock>]) -> gtk::Box {
    let outer = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .build();
    for (idx, item) in items.iter().enumerate() {
        let row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();
        let marker_text = if ordered {
            format!("{}.", idx + 1)
        } else {
            "\u{2022}".to_string()
        };
        let marker = gtk::Label::builder()
            .label(&marker_text)
            .xalign(0.0)
            .yalign(0.0)
            .build();
        marker.add_css_class("codex-list-marker");
        row.append(&marker);

        let body = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .hexpand(true)
            .build();
        for block in item {
            body.append(&block_to_widget(block));
        }
        row.append(&body);
        outer.append(&row);
    }
    outer
}

fn blockquote_to_box(inner: &[MdBlock]) -> gtk::Box {
    let bq = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .build();
    bq.add_css_class("codex-blockquote");
    for block in inner {
        bq.append(&block_to_widget(block));
    }
    bq
}

fn table_to_grid(headers: &[Vec<Inline>], rows: &[Vec<Vec<Inline>>]) -> gtk::Grid {
    let grid = gtk::Grid::builder()
        .row_spacing(2)
        .column_spacing(8)
        .build();
    grid.add_css_class("codex-table");
    for (col, cell) in headers.iter().enumerate() {
        let label = paragraph_to_label(cell);
        label.add_css_class("codex-table-header");
        grid.attach(&label, col as i32, 0, 1, 1);
    }
    for (r, row) in rows.iter().enumerate() {
        for (col, cell) in row.iter().enumerate() {
            let label = paragraph_to_label(cell);
            grid.attach(&label, col as i32, (r + 1) as i32, 1, 1);
        }
    }
    grid
}

fn thematic_break_widget() -> gtk::Separator {
    let sep = gtk::Separator::new(gtk::Orientation::Horizontal);
    sep.set_margin_top(6);
    sep.set_margin_bottom(6);
    sep
}

fn html_to_label(html: &str) -> gtk::Label {
    let label = gtk::Label::builder()
        .label(html)
        .wrap(true)
        .wrap_mode(pango::WrapMode::WordChar)
        .xalign(0.0)
        .selectable(true)
        .build();
    label.add_css_class("codex-raw-html");
    label
}

/// Build a Pango [`AttrList`] for a flat run of inlines together with the
/// concatenated UTF-8 text. Public for unit tests.
pub fn attrlist_for_inlines(inlines: &[Inline]) -> (String, pango::AttrList) {
    render_inlines(inlines)
}

fn render_inlines(inlines: &[Inline]) -> (String, pango::AttrList) {
    let attrs = pango::AttrList::new();
    let mut text = String::new();
    for inline in inlines {
        push_inline(&mut text, &attrs, inline);
    }
    (text, attrs)
}

fn push_inline(text: &mut String, attrs: &pango::AttrList, inline: &Inline) {
    match inline {
        Inline::Text(s) => text.push_str(s),
        Inline::SoftBreak => text.push(' '),
        Inline::HardBreak => text.push('\n'),
        Inline::Code(c) => {
            let start = text.len() as u32;
            text.push_str(&c.text);
            let end = text.len() as u32;
            let mut family = pango::AttrString::new_family("monospace");
            family.set_start_index(start);
            family.set_end_index(end);
            attrs.insert(family);
        }
        Inline::Emphasis(children) => {
            let start = text.len() as u32;
            for child in children {
                push_inline(text, attrs, child);
            }
            let end = text.len() as u32;
            let mut style = pango::AttrInt::new_style(pango::Style::Italic);
            style.set_start_index(start);
            style.set_end_index(end);
            attrs.insert(style);
        }
        Inline::Strong(children) => {
            let start = text.len() as u32;
            for child in children {
                push_inline(text, attrs, child);
            }
            let end = text.len() as u32;
            let mut weight = pango::AttrInt::new_weight(pango::Weight::Bold);
            weight.set_start_index(start);
            weight.set_end_index(end);
            attrs.insert(weight);
        }
        Inline::Strikethrough(children) => {
            let start = text.len() as u32;
            for child in children {
                push_inline(text, attrs, child);
            }
            let end = text.len() as u32;
            let mut strike = pango::AttrInt::new_strikethrough(true);
            strike.set_start_index(start);
            strike.set_end_index(end);
            attrs.insert(strike);
        }
        Inline::Link(link) => {
            for child in &link.children {
                push_inline(text, attrs, child);
            }
        }
        Inline::Image(img) => {
            for child in &img.alt {
                push_inline(text, attrs, child);
            }
        }
        Inline::Html(s) => text.push_str(s),
    }
}

#[cfg(all(test, feature = "gtk"))]
mod tests {
    use super::*;
    use codex_markdown_ast::{Inline, InlineCode, MdBlock};

    /// Initialise GTK on the test thread. Returns `false` when init
    /// fails (typical in headless CI without a display); tests bail out
    /// early in that case so they don't try to construct widgets.
    fn init_gtk() -> bool {
        use std::sync::OnceLock;
        static INIT: OnceLock<bool> = OnceLock::new();
        *INIT.get_or_init(|| gtk::init().is_ok())
    }

    #[test]
    fn paragraph_renders_as_label() {
        if !init_gtk() {
            return;
        }
        let inlines = vec![
            Inline::Text("hello ".into()),
            Inline::Strong(vec![Inline::Text("world".into())]),
        ];
        let block = MdBlock::Paragraph(inlines);
        let widget = block_to_widget(&block);
        let label: gtk::Label = widget.downcast().expect("paragraph should be a Label");
        assert_eq!(label.text().to_string(), "hello world");
        assert!(
            label.attributes().is_some(),
            "bold attr should be present"
        );
    }

    #[test]
    fn code_block_uses_sourceview() {
        if !init_gtk() {
            return;
        }
        let block = MdBlock::CodeBlock {
            lang: Some("rust".into()),
            text: "fn main() {}".into(),
        };
        let widget = block_to_widget(&block);
        let frame: gtk::Frame = widget.downcast().expect("Frame");
        assert!(frame.child().is_some());
        let child = frame.child().unwrap();
        assert!(child.downcast::<sourceview5::View>().is_ok());
    }

    #[test]
    fn ordered_list_renders_numbered_markers() {
        if !init_gtk() {
            return;
        }
        let items = vec![
            vec![MdBlock::Paragraph(vec![Inline::Text("first".into())])],
            vec![MdBlock::Paragraph(vec![Inline::Text("second".into())])],
        ];
        let block = MdBlock::List {
            ordered: true,
            items,
        };
        let widget = block_to_widget(&block);
        let outer: gtk::Box = widget.downcast().expect("Box");
        let mut count = 0;
        let mut child = outer.first_child();
        while let Some(c) = child {
            count += 1;
            child = c.next_sibling();
        }
        assert_eq!(count, 2);
    }

    #[test]
    fn unordered_list_renders_bullet_markers() {
        if !init_gtk() {
            return;
        }
        let items = vec![vec![MdBlock::Paragraph(vec![Inline::Text("only".into())])]];
        let block = MdBlock::List {
            ordered: false,
            items,
        };
        let widget = block_to_widget(&block);
        let outer: gtk::Box = widget.downcast().expect("Box");
        let row = outer.first_child().expect("row").downcast::<gtk::Box>().unwrap();
        let marker = row.first_child().expect("marker").downcast::<gtk::Label>().unwrap();
        assert_eq!(marker.text().to_string(), "\u{2022}");
    }

    #[test]
    fn heading_adds_css_class() {
        if !init_gtk() {
            return;
        }
        let block = MdBlock::Heading {
            level: 2,
            inlines: vec![Inline::Text("Title".into())],
        };
        let widget = block_to_widget(&block);
        let label: gtk::Label = widget.downcast().expect("Label");
        assert_eq!(label.text().to_string(), "Title");
        assert!(label.has_css_class("codex-heading-2"));
    }

    #[test]
    fn thematic_break_is_separator() {
        if !init_gtk() {
            return;
        }
        let widget = block_to_widget(&MdBlock::ThematicBreak);
        assert!(widget.downcast::<gtk::Separator>().is_ok());
    }

    #[test]
    fn blockquote_has_css_class() {
        if !init_gtk() {
            return;
        }
        let block = MdBlock::BlockQuote(vec![MdBlock::Paragraph(vec![Inline::Text("q".into())])]);
        let widget = block_to_widget(&block);
        let bx: gtk::Box = widget.downcast().expect("Box");
        assert!(bx.has_css_class("codex-blockquote"));
    }

    #[test]
    fn html_block_is_escaped_label() {
        if !init_gtk() {
            return;
        }
        let block = MdBlock::HtmlBlock("<b>raw</b>".into());
        let widget = block_to_widget(&block);
        let label: gtk::Label = widget.downcast().expect("Label");
        // set_label leaves the text untouched – the markup is *not*
        // interpreted (we never call set_markup), so the literal angle
        // brackets must appear in the visible string.
        assert_eq!(label.text().to_string(), "<b>raw</b>");
        assert!(label.has_css_class("codex-raw-html"));
    }

    #[test]
    fn table_renders_as_grid() {
        if !init_gtk() {
            return;
        }
        let block = MdBlock::Table {
            headers: vec![
                vec![Inline::Text("h1".into())],
                vec![Inline::Text("h2".into())],
            ],
            rows: vec![vec![
                vec![Inline::Text("a".into())],
                vec![Inline::Text("b".into())],
            ]],
        };
        let widget = block_to_widget(&block);
        let grid: gtk::Grid = widget.downcast().expect("Grid");
        let header = grid.child_at(0, 0).and_downcast::<gtk::Label>().unwrap();
        assert_eq!(header.text().to_string(), "h1");
        assert!(header.has_css_class("codex-table-header"));
        let body = grid.child_at(1, 1).and_downcast::<gtk::Label>().unwrap();
        assert_eq!(body.text().to_string(), "b");
    }

    #[test]
    fn attrlist_flattens_nested_inlines() {
        if !init_gtk() {
            return;
        }
        let inlines = vec![
            Inline::Text("a ".into()),
            Inline::Emphasis(vec![Inline::Text("b".into())]),
            Inline::Code(InlineCode { text: "c".into() }),
        ];
        let (text, attrs) = attrlist_for_inlines(&inlines);
        assert_eq!(text, "a bc");
        // Iterate the AttrList via the change iterator and tally the
        // attributes it surfaces — we expect at least the italic and
        // family attrs we attached.
        let mut iter = attrs.iterator();
        let mut seen = 0;
        loop {
            seen += iter.attrs().len();
            if !iter.next_style_change() {
                break;
            }
        }
        assert!(seen >= 2, "expected at least 2 attrs, got {seen}");
    }
}
