#![cfg(feature = "gtk")]

//! Chat pane: a virtualised `GtkColumnView` over a `GListStore` of
//! [`MessageBlock`] GObjects. PR-D will hook this up to the agent stream.
//! For PR-B the pane just echoes the composer text back as a "user"
//! message and seeds a single welcome message.

use std::cell::Cell;
use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use gtk::glib::Properties;
use gtk::glib::subclass::prelude::*;

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
        factory.connect_setup(|_factory, list_item| {
            let label = gtk::Label::builder()
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .xalign(0.0)
                .selectable(true)
                .build();
            if let Some(item) = list_item.downcast_ref::<gtk::ListItem>() {
                item.set_child(Some(&label));
            }
        });
        factory.connect_bind(|_factory, list_item| {
            let Some(item) = list_item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let Some(message) = item.item().and_downcast::<MessageBlock>() else {
                return;
            };
            let Some(label) = item.child().and_downcast::<gtk::Label>() else {
                return;
            };
            label.set_text(&message.text());
            // Reset the role-specific class.
            for cls in [
                "codex-msg-user",
                "codex-msg-assistant",
                "codex-msg-system",
            ] {
                label.remove_css_class(cls);
            }
            let cls = format!("codex-msg-{}", message.role());
            label.add_css_class(&cls);
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

    /// Append `delta` to the most recent unfinalised assistant block.
    ///
    /// If the most recent block is not an assistant block — or it has
    /// already been finalised — a fresh `assistant` row is appended. The
    /// streaming controller in PR-F will replace the in-place text
    /// concatenation with rich markdown rendering; for now we just keep a
    /// running plain-text buffer.
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
                block.set_text(combined);
                // ColumnView listens to property notifications, so this
                // re-binds the row label automatically.
                return;
            }
        }
        let block = MessageBlock::new("assistant", delta);
        self.store.append(&block);
    }

    /// Mark the most recent assistant block as finalised. Optionally
    /// appends a small footer indicating the stop reason if it's not a
    /// boring `end_turn`.
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
}
