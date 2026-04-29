#![cfg(feature = "gtk")]

//! Chat pane: a virtualised `GtkColumnView` over a `GListStore` of
//! [`MessageBlock`] GObjects. PR-D will hook this up to the agent stream.
//! For PR-B the pane just echoes the composer text back as a "user"
//! message and seeds a single welcome message.

use std::cell::Cell;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use codex_markdown_ast::IncrementalParser;
use codex_markdown_ast::parse_full;
use gtk::glib;
use gtk::glib::Properties;
use gtk::glib::SignalHandlerId;
use gtk::glib::subclass::prelude::*;

use crate::md_to_widgets::block_to_widget;

thread_local! {
    /// Per-`ListItem` notify-handler IDs attached during `connect_bind`,
    /// keyed by the `gtk::ListItem` raw pointer. `connect_unbind`
    /// disconnects them. The list-item factory is single-threaded (GTK
    /// main loop), so a thread-local is sufficient.
    static BIND_HANDLERS: RefCell<HashMap<usize, (SignalHandlerId, SignalHandlerId)>> =
        RefCell::new(HashMap::new());

    /// Per-streaming-`MessageBlock` incremental markdown parser, keyed
    /// by the `MessageBlock` GObject pointer (cast to `usize`). The key
    /// is only ever inserted while the block is still alive in the
    /// chat-pane's `GListStore` and is evicted on
    /// [`ChatPane::finalise_assistant_block`], so we never observe
    /// pointer reuse: GLib refcounts keep the block alive across notify
    /// rebuilds, and the eviction step happens before the block is
    /// dropped from the store. GTK runs all of this on the main thread,
    /// so a thread-local keeps `Send + Sync` headaches away.
    static INC_PARSERS: RefCell<HashMap<usize, IncrementalParser>> =
        RefCell::new(HashMap::new());
}

mod imp {
    use super::*;

    #[derive(Default, Properties)]
    #[properties(wrapper_type = super::MessageBlock)]
    pub struct MessageBlock {
        #[property(get, set)]
        pub role: RefCell<String>,
        #[property(get, set)]
        pub text: RefCell<String>,
        /// `true` once an assistant block has received its terminating
        /// `agent/turn_completed` notification. Used by the streaming path
        /// to decide whether to extend the most recent block or start a
        /// new one when a new `message_delta` arrives.
        #[property(get, set)]
        pub finalised: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for MessageBlock {
        const NAME: &'static str = "CodexMessageBlock";
        type Type = super::MessageBlock;
    }

    #[glib::derived_properties]
    impl ObjectImpl for MessageBlock {}
}

glib::wrapper! {
    /// One row in the chat transcript.
    pub struct MessageBlock(ObjectSubclass<imp::MessageBlock>);
}

impl MessageBlock {
    pub fn new(role: &str, text: &str) -> Self {
        glib::Object::builder()
            .property("role", role)
            .property("text", text)
            .build()
    }
}

impl Default for MessageBlock {
    fn default() -> Self {
        Self::new("system", "")
    }
}

/// Type alias for the optional submission callback: invoked with the
/// trimmed composer text when the user clicks Send. The pane stores it
/// behind a `RefCell<Rc<...>>` so [`ChatPane::set_submit_callback`] can
/// install the bridge after construction.
pub type SubmitCallback = Rc<dyn Fn(String) + 'static>;

/// The chat pane widget. Holds the backing `GListStore` and exposes
/// helpers for appending messages.
#[derive(Clone)]
pub struct ChatPane {
    root: gtk::Box,
    store: gtk::gio::ListStore,
    composer: sourceview5::View,
    /// Forward-the-prompt-to-the-agent hook. Set by
    /// [`ChatPane::set_submit_callback`] once the bridge is constructed.
    /// `None` until then, in which case the Send button only adds the
    /// user message locally without invoking any backend.
    submit_cb: Rc<RefCell<Option<SubmitCallback>>>,
}

impl ChatPane {
    pub fn new() -> Self {
        let header = adw::HeaderBar::builder()
            .show_start_title_buttons(false)
            .show_end_title_buttons(false)
            .build();
        let title_label = gtk::Label::builder()
            .label("Codex Desktop")
            .css_classes(["title"])
            .build();
        header.set_title_widget(Some(&title_label));

        let send_button = adw::SplitButton::builder()
            .label("Send")
            .tooltip_text("Send message (Ctrl+Enter)")
            .build();
        header.pack_end(&send_button);

        let store = gtk::gio::ListStore::new::<MessageBlock>();
        let selection = gtk::NoSelection::new(Some(store.clone()));

        let factory = gtk::SignalListItemFactory::new();
        // We deliberately defer all widget construction to `connect_bind`:
        // the row layout depends on `MessageBlock` properties that are
        // only known once the item is bound. `connect_unbind` clears the
        // child and disconnects the property listeners we attached.
        factory.connect_bind(|_factory, list_item| {
            let Some(item) = list_item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let Some(block) = item.item().and_downcast::<MessageBlock>() else {
                return;
            };
            // Build the initial row.
            let row = build_message_row(&block);
            item.set_child(Some(&row));

            // Rebuild on `text` / `finalised` notifications. We use the
            // weak `gtk::ListItem` ref so we don't keep the item alive
            // longer than its lifecycle. On `unbind` we disconnect both
            // handlers via the stash on the item's `MessageBlock`.
            let item_weak = item.downgrade();
            let block_for_text = block.clone();
            let h_text = block.connect_notify_local(Some("text"), move |b, _ps| {
                let Some(item) = item_weak.upgrade() else {
                    return;
                };
                if item.item().and_downcast::<MessageBlock>().as_ref() != Some(b) {
                    return;
                }
                let row = build_message_row(&block_for_text);
                item.set_child(Some(&row));
            });

            let item_weak = item.downgrade();
            let block_for_fin = block.clone();
            let h_fin = block.connect_notify_local(Some("finalised"), move |b, _ps| {
                let Some(item) = item_weak.upgrade() else {
                    return;
                };
                if item.item().and_downcast::<MessageBlock>().as_ref() != Some(b) {
                    return;
                }
                let row = build_message_row(&block_for_fin);
                item.set_child(Some(&row));
            });

            // Stash the handler IDs on the row's data slot so unbind can
            // disconnect them. We use `unsafe_set_data`-free storage via a
            // boxed closure attached to the list_item itself: GLib's
            // `set_data` requires `unsafe`, which is forbidden here, so
            // we store on a `RefCell<Option<...>>` keyed by the
            // `MessageBlock` GObject's qdata using safe `set_data_full`.
            // Simpler: stash IDs on the item via `unsafe_set_data` is
            // forbidden; instead drop them in a `Rc<Cell>` captured by
            // the unbind handler. We attach both IDs as item properties
            // through `glib::object::ObjectExt::set_data` — but that is
            // also `unsafe`. So we settle for the simpler design: when
            // unbind fires, we look up the block on the item and
            // disconnect ANY notify handlers we previously attached by
            // walking through `block.list_signal_handlers()`.
            //
            // Practically, blocking (rather than disconnecting) is the
            // safest cross-version GTK pattern; but `block_signal` also
            // needs the handler ID. We therefore keep the IDs in a
            // thread-local `RefCell<HashMap<...>>` keyed by the item's
            // pointer — see `BIND_HANDLERS`.
            BIND_HANDLERS.with(|cell| {
                cell.borrow_mut()
                    .insert(item.as_ptr() as usize, (h_text, h_fin));
            });
        });
        factory.connect_unbind(|_factory, list_item| {
            let Some(item) = list_item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let key = item.as_ptr() as usize;
            let handlers = BIND_HANDLERS.with(|cell| cell.borrow_mut().remove(&key));
            if let Some((h_text, h_fin)) = handlers
                && let Some(block) = item.item().and_downcast::<MessageBlock>()
            {
                block.disconnect(h_text);
                block.disconnect(h_fin);
            }
            item.set_child(gtk::Widget::NONE);
        });

        let column = gtk::ColumnViewColumn::builder()
            .title("Messages")
            .factory(&factory)
            .expand(true)
            .resizable(false)
            .build();

        let column_view = gtk::ColumnView::builder()
            .model(&selection)
            .show_column_separators(false)
            .show_row_separators(false)
            .vexpand(true)
            .build();
        column_view.append_column(&column);
        // Hide the header row — we only have one column and it has no
        // useful title.
        column_view.set_show_column_separators(false);
        if let Some(header) = column_view.first_child() {
            header.set_visible(false);
        }

        let scrolled = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .child(&column_view)
            .vexpand(true)
            .build();

        // Composer: a sourceview5::View for multi-line input.
        let composer_buffer = sourceview5::Buffer::new(None);
        let composer = sourceview5::View::with_buffer(&composer_buffer);
        composer.set_wrap_mode(gtk::WrapMode::WordChar);
        composer.set_monospace(false);
        composer.set_top_margin(6);
        composer.set_bottom_margin(6);
        composer.set_left_margin(8);
        composer.set_right_margin(8);
        composer.add_css_class("codex-chat-composer");
        composer.set_height_request(72);

        let composer_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .child(&composer)
            .min_content_height(72)
            .max_content_height(160)
            .propagate_natural_height(true)
            .build();

        let send_btn = gtk::Button::builder()
            .label("Send")
            .css_classes(["suggested-action"])
            .build();
        let stop_btn = gtk::Button::builder()
            .label("Stop")
            .css_classes(["destructive-action"])
            .sensitive(false)
            .build();

        let button_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .halign(gtk::Align::End)
            .margin_top(4)
            .margin_bottom(6)
            .margin_start(6)
            .margin_end(6)
            .build();
        button_row.append(&send_btn);
        button_row.append(&stop_btn);

        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        root.append(&header);
        root.append(&scrolled);
        root.append(&composer_scroll);
        root.append(&button_row);

        let pane = ChatPane {
            root,
            store,
            composer,
            submit_cb: Rc::new(RefCell::new(None)),
        };

        // Wire the Send button to push the composer text into the chat.
        let pane_for_send = pane.clone();
        send_btn.connect_clicked(move |_| {
            pane_for_send.send_from_composer();
        });
        let pane_for_split = pane.clone();
        send_button.connect_clicked(move |_| {
            pane_for_split.send_from_composer();
        });

        // Seed the welcome message.
        pane.append_message(
            "system",
            "Welcome to codex-desktop. The agent backend is not yet wired up — see PR-D.",
        );

        pane
    }

    pub fn root(&self) -> &gtk::Box {
        &self.root
    }

    /// Append a message to the transcript.
    pub fn append_message(&self, role: &str, text: &str) {
        tracing::info!(role, len = text.len(), "chat: append_message");
        let block = MessageBlock::new(role, text);
        self.store.append(&block);
    }

    /// Number of messages currently in the transcript. Useful for tests.
    pub fn message_count(&self) -> u32 {
        self.store.n_items()
    }

    /// Install the submission callback. Replaces any previously installed
    /// callback; passing a fresh closure is the supported way to reset.
    ///
    /// The pane invokes `cb` with the trimmed composer text whenever the
    /// user clicks Send, after appending a "user" `MessageBlock` locally.
    pub fn set_submit_callback<F>(&self, cb: F)
    where
        F: Fn(String) + 'static,
    {
        *self.submit_cb.borrow_mut() = Some(Rc::new(cb));
    }

    /// Return the most recent `assistant` MessageBlock in the transcript,
    /// if any. Used by the incremental streaming path (and by tests) to
    /// look up the current row.
    #[allow(dead_code)]
    pub(crate) fn last_assistant_block(&self) -> Option<MessageBlock> {
        let n = self.store.n_items();
        for i in (0..n).rev() {
            if let Some(item) = self.store.item(i)
                && let Some(block) = item.downcast_ref::<MessageBlock>()
                && block.role() == "assistant"
            {
                return Some(block.clone());
            }
        }
        None
    }

    /// Append `delta` to the most recent unfinalised assistant block.
    ///
    /// If the most recent block is not an assistant block — or it has
    /// already been finalised — a fresh `assistant` row is appended.
    ///
    /// In addition to growing the cumulative `text` property, this routine
    /// pushes the delta into the per-block [`IncrementalParser`] in the
    /// `INC_PARSERS` thread-local cache *before* `set_text` triggers the
    /// `text-notify` signal. The list-item factory rebuilds the row on
    /// that notify and reads the parser's `blocks()` snapshot, so the
    /// "push first, notify second" order keeps the rendered widgets in
    /// sync with the cumulative source.
    pub fn start_or_extend_assistant_block(&self, delta: &str) {
        let n = self.store.n_items();
        if n > 0 {
            let last_idx = n - 1;
            if let Some(item) = self.store.item(last_idx)
                && let Some(block) = item.downcast_ref::<MessageBlock>()
                && block.role() == "assistant"
                && !block.finalised()
            {
                let mut combined = block.text();
                combined.push_str(delta);
                let key = block.as_ptr() as usize;
                INC_PARSERS.with(|cell| {
                    let mut map = cell.borrow_mut();
                    let parser = map.entry(key).or_default();
                    let _ = parser.push(delta);
                });
                block.set_text(combined);
                // ColumnView listens to property notifications, so this
                // re-binds the row automatically; the rebuild reads the
                // parser snapshot we just advanced above.
                return;
            }
        }
        let block = MessageBlock::new("assistant", "");
        let key = block.as_ptr() as usize;
        INC_PARSERS.with(|cell| {
            let mut map = cell.borrow_mut();
            let parser = map.entry(key).or_default();
            let _ = parser.push(delta);
        });
        // Now set the text so the bind path observes a parser entry.
        block.set_text(delta);
        self.store.append(&block);
    }

    /// Mark the most recent assistant block as finalised. Optionally
    /// appends a small footer indicating the stop reason if it's not a
    /// boring `end_turn`. Also evicts the block's incremental parser
    /// from the `INC_PARSERS` cache so memory grows only with the
    /// number of in-flight streams, not the lifetime of the session.
    pub fn finalise_assistant_block(&self, stop_reason: &str) {
        let n = self.store.n_items();
        if n == 0 {
            return;
        }
        let last_idx = n - 1;
        let Some(item) = self.store.item(last_idx) else {
            return;
        };
        let Some(block) = item.downcast_ref::<MessageBlock>() else {
            return;
        };
        if block.role() != "assistant" {
            return;
        }
        block.set_finalised(true);
        if !stop_reason.is_empty() && stop_reason != "end_turn" {
            let mut text = block.text();
            text.push_str(&format!("\n(stop: {stop_reason})"));
            block.set_text(text);
        }
        let key = block.as_ptr() as usize;
        INC_PARSERS.with(|cell| {
            cell.borrow_mut().remove(&key);
        });
    }

    /// Append a system block stating that the agent has disconnected.
    pub fn show_agent_disconnected(&self) {
        self.append_message("system", "Agent disconnected.");
    }

    fn send_from_composer(&self) {
        let buffer = self.composer.buffer();
        let (start, end) = buffer.bounds();
        let text = buffer.text(&start, &end, false).to_string();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        let owned = trimmed.to_string();
        self.append_message("user", &owned);
        buffer.set_text("");
        // Forward to the bridge if one is installed. Cloning the inner
        // `Rc` keeps the borrow short-lived.
        let cb_opt = self.submit_cb.borrow().as_ref().map(Rc::clone);
        if let Some(cb) = cb_opt {
            cb(owned);
        }
    }
}

impl Default for ChatPane {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the visual representation of a `MessageBlock` for rendering in
/// the chat transcript.
///
/// The shape is:
/// - Outer = vertical `gtk::Box` carrying the role-specific CSS class
///   (`codex-msg-user|assistant|system`) and, while streaming, the
///   `codex-msg-streaming` modifier.
/// - First child = a small role label header rendered with Pango markup
///   (safe — role display names are hardcoded constants).
/// - Body = either a single `gtk::Label` (user input, system messages,
///   or pre-first-delta streaming assistant text), one widget per
///   `MdBlock` produced by [`crate::md_to_widgets::block_to_widget`] for
///   finalised assistant blocks, or — for *streaming* assistant blocks
///   that already have a parser entry in `INC_PARSERS` — a vertical
///   `gtk::Box` containing one widget per `MdBlock` in the parser's
///   current snapshot followed by a faint streaming-cursor marker.
fn build_message_row(block: &MessageBlock) -> gtk::Widget {
    let role = block.role();
    let outer = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .build();
    outer.add_css_class(&format!("codex-msg-{role}"));
    if !block.finalised() && role == "assistant" {
        outer.add_css_class("codex-msg-streaming");
    }

    // Role label header.
    let header_markup = match role.as_str() {
        "user" => "<b>You</b>",
        "assistant" => "<b>Codex</b>",
        // Anything else falls under "system" styling.
        _ => "<b>system</b>",
    };
    let header = gtk::Label::builder().xalign(0.0).build();
    header.set_use_markup(true);
    header.set_markup(header_markup);
    header.add_css_class("codex-msg-role");
    outer.append(&header);

    // Body.
    let text = block.text();
    let is_streaming_assistant = role == "assistant" && !block.finalised();

    if is_streaming_assistant {
        // Streaming assistant: prefer the incremental parser snapshot if
        // we have one. The parser is keyed by the GObject pointer; we
        // copy the blocks out under the borrow so we don't keep the
        // RefCell borrowed across widget construction.
        let key = block.as_ptr() as usize;
        let blocks_opt = INC_PARSERS.with(|cell| {
            cell.borrow()
                .get(&key)
                .map(|p| p.blocks().to_vec())
        });
        match blocks_opt {
            Some(md_blocks) if !md_blocks.is_empty() => {
                let body = gtk::Box::builder()
                    .orientation(gtk::Orientation::Vertical)
                    .spacing(4)
                    .build();
                for md_block in &md_blocks {
                    body.append(&block_to_widget(md_block));
                }
                let cursor = gtk::Label::new(Some("\u{258D}"));
                cursor.add_css_class("codex-streaming-cursor");
                cursor.set_xalign(0.0);
                body.append(&cursor);
                outer.append(&body);
            }
            _ => {
                // Pre-first-delta or empty parser: keep the simple-Label
                // shape so the row still has a body child.
                let body = gtk::Label::builder()
                    .label(&text)
                    .wrap(true)
                    .wrap_mode(gtk::pango::WrapMode::WordChar)
                    .xalign(0.0)
                    .selectable(true)
                    .build();
                outer.append(&body);
            }
        }
    } else if role == "user" || role == "system" {
        let body = gtk::Label::builder()
            .label(&text)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .xalign(0.0)
            .selectable(true)
            .build();
        outer.append(&body);
    } else {
        // Finalised assistant: parse markdown and render each block.
        let doc = parse_full(&text);
        if doc.blocks.is_empty() {
            // Empty / whitespace-only — fall back to an empty label so
            // the row still has a body child.
            let body = gtk::Label::builder()
                .label(&text)
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .xalign(0.0)
                .selectable(true)
                .build();
            outer.append(&body);
        } else {
            for md_block in &doc.blocks {
                outer.append(&block_to_widget(md_block));
            }
        }
    }

    outer.upcast()
}

#[cfg(all(test, feature = "gtk"))]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    static INIT: OnceLock<bool> = OnceLock::new();

    fn ensure_gtk() -> bool {
        // gtk::init() may segfault in headless CI without a display
        // server. Detect headless env BEFORE calling init().
        *INIT.get_or_init(|| {
            if std::env::var_os("DISPLAY").is_none()
                && std::env::var_os("WAYLAND_DISPLAY").is_none()
            {
                return false;
            }
            if gtk::init().is_err() {
                return false;
            }
            if !adw::is_initialized() {
                let _ = adw::init();
            }
            true
        })
    }

    #[test]
    fn message_block_properties_round_trip() {
        if !ensure_gtk() {
            return;
        }
        let m = MessageBlock::new("assistant", "hello");
        assert_eq!(m.role(), "assistant");
        assert_eq!(m.text(), "hello");
        m.set_role("user");
        m.set_text("world");
        assert_eq!(m.role(), "user");
        assert_eq!(m.text(), "world");
    }

    #[test]
    fn chat_pane_seeds_welcome() {
        if !ensure_gtk() {
            return;
        }
        let pane = ChatPane::new();
        assert!(pane.message_count() >= 1);
        pane.append_message("user", "hi");
        assert!(pane.message_count() >= 2);
    }

    /// Walk the immediate children of `bx` into a `Vec<gtk::Widget>` so
    /// tests can index by position. GTK doesn't expose a children API on
    /// `gtk::Box`; iterating via `first_child` / `next_sibling` is the
    /// supported pattern.
    fn box_children(bx: &gtk::Box) -> Vec<gtk::Widget> {
        let mut out = Vec::new();
        let mut child = bx.first_child();
        while let Some(c) = child {
            child = c.next_sibling();
            out.push(c);
        }
        out
    }

    #[test]
    fn assistant_finalised_renders_markdown_widgets() {
        if !ensure_gtk() {
            return;
        }
        let block = MessageBlock::new("assistant", "# Hello\n\nworld");
        block.set_finalised(true);
        let row = build_message_row(&block);
        let bx: gtk::Box = row.downcast().expect("row is a Box");
        assert!(bx.has_css_class("codex-msg-assistant"));
        assert!(!bx.has_css_class("codex-msg-streaming"));
        let children = box_children(&bx);
        // Children: header + heading + paragraph (>= 3). The assertion
        // in the spec is "at least 2 children" for the markdown body
        // (heading + paragraph), so total should be >= 3.
        assert!(
            children.len() >= 3,
            "expected header + >=2 markdown widgets, got {}",
            children.len()
        );
    }

    #[test]
    fn user_block_renders_as_label_no_markdown() {
        if !ensure_gtk() {
            return;
        }
        let block = MessageBlock::new("user", "# not a heading");
        // User blocks should not be markdown-rendered regardless of
        // finalisation state.
        block.set_finalised(true);
        let row = build_message_row(&block);
        let bx: gtk::Box = row.downcast().expect("row is a Box");
        assert!(bx.has_css_class("codex-msg-user"));
        let children = box_children(&bx);
        // header + body label.
        assert_eq!(children.len(), 2, "expected exactly 2 children");
        let body: gtk::Label = children[1]
            .clone()
            .downcast()
            .expect("body child should be a Label");
        assert_eq!(body.text().to_string(), "# not a heading");
    }

    #[test]
    fn streaming_block_keeps_simple_label_shape() {
        if !ensure_gtk() {
            return;
        }
        // No parser entry exists for this bare block — the streaming
        // path must fall back to the single-Label shape (header + Label).
        // Once an `IncrementalParser` has been advanced for the block
        // (see `streaming_assistant_block_renders_incremental_widgets`),
        // the body becomes a Box of MdBlock widgets instead.
        let block = MessageBlock::new("assistant", "# streaming...");
        assert!(!block.finalised());
        let row = build_message_row(&block);
        let bx: gtk::Box = row.downcast().expect("row is a Box");
        assert!(bx.has_css_class("codex-msg-assistant"));
        assert!(bx.has_css_class("codex-msg-streaming"));
        let children = box_children(&bx);
        assert_eq!(
            children.len(),
            2,
            "pre-first-delta streaming row should be header + single Label"
        );
        // Body is either a Label (no parser) or a Box (parser cached).
        // For this test no parser entry was inserted, so we assert the
        // simple-Label shape.
        let body: gtk::Label = children[1]
            .clone()
            .downcast()
            .expect("streaming body should be a Label when no parser is cached");
        assert_eq!(body.text().to_string(), "# streaming...");
    }

    #[test]
    fn streaming_assistant_block_renders_incremental_widgets() {
        if !ensure_gtk() {
            return;
        }
        let pane = ChatPane::new();
        pane.start_or_extend_assistant_block("**hello** ");
        pane.start_or_extend_assistant_block("world");
        let block = pane
            .last_assistant_block()
            .expect("assistant block should exist after two deltas");
        let row = build_message_row(&block);
        let outer: gtk::Box = row.downcast().expect("row is a Box");
        assert!(outer.has_css_class("codex-msg-assistant"));
        assert!(outer.has_css_class("codex-msg-streaming"));
        // Walk children — at minimum we expect a role header label AND
        // the content body (a Box of MdBlock widgets ending in the
        // streaming cursor).
        let mut count = 0;
        let mut child = outer.first_child();
        while let Some(c) = child {
            count += 1;
            child = c.next_sibling();
        }
        assert!(count >= 2, "row should contain header + body, got {count}");
        // The body should be a Box (because a parser is cached and has
        // produced at least one MdBlock).
        let children = box_children(&outer);
        let body: gtk::Box = children[1]
            .clone()
            .downcast()
            .expect("streaming body should be a Box once the parser has parsed content");
        // Body should contain at least one MdBlock widget plus the
        // streaming cursor at the end.
        let body_children = box_children(&body);
        assert!(
            body_children.len() >= 2,
            "body should contain MdBlock widget(s) + cursor, got {}",
            body_children.len()
        );
        let last = body_children
            .last()
            .expect("body has a final cursor child")
            .clone();
        let cursor: gtk::Label = last
            .downcast()
            .expect("final body child should be the cursor Label");
        assert!(cursor.has_css_class("codex-streaming-cursor"));
    }

    #[test]
    fn finalising_assistant_block_evicts_parser() {
        if !ensure_gtk() {
            return;
        }
        let pane = ChatPane::new();
        pane.start_or_extend_assistant_block("**hello**");
        let block = pane
            .last_assistant_block()
            .expect("assistant block should exist after a delta");
        let key = block.as_ptr() as usize;
        // Sanity check: parser entry exists prior to finalise.
        let present_before =
            INC_PARSERS.with(|cell| cell.borrow().contains_key(&key));
        assert!(present_before, "parser must be cached during streaming");
        pane.finalise_assistant_block("end_turn");
        INC_PARSERS.with(|cell| {
            assert!(
                !cell.borrow().contains_key(&key),
                "parser must be evicted on finalise"
            );
        });
    }
}
