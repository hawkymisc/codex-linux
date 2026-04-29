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

        EditorPane {
            root,
            view,
            buffer,
            label,
            file_path: std::rc::Rc::new(std::cell::RefCell::new(None)),
        }
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
