#![cfg(feature = "gtk")]

//! Editor pane: a `sourceview5::View` inside a `GtkScrolledWindow`,
//! with file-open/save plumbing.

use std::path::Path;
use std::path::PathBuf;

use adw::prelude::*;
use anyhow::Context;
use anyhow::Result;
use sourceview5::prelude::BufferExt as _;
use sourceview5::prelude::ViewExt as _;

#[derive(Clone)]
pub struct EditorPane {
    root: gtk::Box,
    view: sourceview5::View,
    buffer: sourceview5::Buffer,
    label: gtk::Label,
    file_path: std::rc::Rc<std::cell::RefCell<Option<PathBuf>>>,
    /// `EventControllerKey` whose `im_context` is swapped between
    /// `VimIMContext` (vim mode) and `IMMulticontext` (default) by
    /// [`EditorPane::set_vim_mode`].
    vim_controller: gtk::EventControllerKey,
    vim_enabled: std::rc::Rc<std::cell::Cell<bool>>,
}

impl EditorPane {
    pub fn new() -> Self {
        let buffer = sourceview5::Buffer::new(None);
        let view = sourceview5::View::with_buffer(&buffer);
        view.set_show_line_numbers(true);
        view.set_highlight_current_line(true);
        view.set_monospace(true);
        view.set_tab_width(4);
        view.set_auto_indent(true);
        view.set_smart_backspace(true);
        view.set_wrap_mode(gtk::WrapMode::None);

        let scroll = gtk::ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .child(&view)
            .build();

        // Placeholder for diff/lint overlay (per architecture doc).
        let placeholder = adw::Bin::builder().build();

        let paned = gtk::Paned::builder()
            .orientation(gtk::Orientation::Vertical)
            .start_child(&scroll)
            .end_child(&placeholder)
            .resize_start_child(true)
            .resize_end_child(false)
            .shrink_start_child(false)
            .shrink_end_child(true)
            .position(640)
            .build();

        let label = gtk::Label::new(Some("Untitled"));

        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        root.append(&paned);

        // Install a key controller on the view so we can swap its IM
        // context between VimIMContext and the default multicontext at
        // runtime via [`set_vim_mode`].
        let vim_controller = gtk::EventControllerKey::new();
        vim_controller.set_im_context(Some(&gtk::IMMulticontext::new()));
        view.add_controller(vim_controller.clone());

        EditorPane {
            root,
            view,
            buffer,
            label,
            file_path: std::rc::Rc::new(std::cell::RefCell::new(None)),
            vim_controller,
            vim_enabled: std::rc::Rc::new(std::cell::Cell::new(false)),
        }
    }

    /// Returns `true` if vim-style modal editing is currently active for
    /// this pane.
    pub fn vim_mode(&self) -> bool {
        self.vim_enabled.get()
    }

    /// Enable or disable vim-style modal editing. When enabled, installs
    /// a `sourceview5::VimIMContext` on the view's key controller; when
    /// disabled, restores a plain `gtk::IMMulticontext`.
    pub fn set_vim_mode(&self, enabled: bool) {
        if enabled == self.vim_enabled.get() {
            return;
        }
        if enabled {
            let vim = sourceview5::VimIMContext::new();
            // Connect the IM context to the view as required for cursor
            // movement, scrolling, and the `:` command bar to function.
            vim.set_client_widget(Some(&self.view));
            self.vim_controller.set_im_context(Some(&vim));
        } else {
            self.vim_controller
                .set_im_context(Some(&gtk::IMMulticontext::new()));
        }
        self.vim_enabled.set(enabled);
        tracing::info!(enabled, "editor: vim mode toggled");
    }

    pub fn root(&self) -> &gtk::Box {
        &self.root
    }

    pub fn label(&self) -> &gtk::Label {
        &self.label
    }

    /// Load `path` into the editor buffer. Detects the language via
    /// `LanguageManager::guess_language`.
    pub fn open_file(&self, path: &Path) -> Result<()> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let text = String::from_utf8_lossy(&bytes);
        self.buffer.set_text(&text);
        let lang = sourceview5::LanguageManager::default()
            .guess_language(Some(path), None);
        self.buffer.set_language(lang.as_ref());
        let display_name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        self.label.set_text(&display_name);
        *self.file_path.borrow_mut() = Some(path.to_path_buf());
        tracing::info!(path = %path.display(), "editor: opened file");
        Ok(())
    }

    /// Path of the currently-open file, if any.
    pub fn file_path(&self) -> Option<PathBuf> {
        self.file_path.borrow().clone()
    }

    pub fn view(&self) -> &sourceview5::View {
        &self.view
    }
}

impl Default for EditorPane {
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
    fn editor_starts_empty() {
        if !ensure_gtk() {
            return;
        }
        let pane = EditorPane::new();
        assert!(pane.file_path().is_none());
    }

    #[test]
    fn editor_opens_file() {
        if !ensure_gtk() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("hello.rs");
        std::fs::write(&p, "fn main() {}\n").expect("write");
        let pane = EditorPane::new();
        pane.open_file(&p).expect("open");
        assert_eq!(pane.file_path().as_deref(), Some(p.as_path()));
    }
}
