#![cfg(feature = "gtk")]

//! Ctrl+P / Ctrl+Shift+P command palette — fuzzy command + file picker.
//!
//! Uses an `adw::Dialog` with a `gtk::SearchEntry` and a `gtk::ListView`
//! over a `gtk::StringList` model. For PR-L the matching is a simple
//! case-insensitive substring search (prefix matches first); a real
//! nucleo-backed scorer will land in a follow-up PR.

use std::cell::RefCell;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

const DIALOG_WIDTH: i32 = 600;
const DIALOG_HEIGHT: i32 = 400;
const FILE_WALK_DEPTH: usize = 5;
const MAX_FILES: usize = 1024;

#[derive(Debug, Clone)]
pub enum CommandPaletteAction {
    OpenFile(PathBuf),
    /// Toggle a named setting, e.g. `"vim-mode"`, `"sidebar"`,
    /// `"chat-pane"`, `"quit"`.
    Toggle(&'static str),
    NoOp,
}

#[derive(Clone)]
pub struct CommandPalette {
    inner: Rc<Inner>,
}

struct Inner {
    dialog: adw::Dialog,
    entry: gtk::SearchEntry,
    list: gtk::ListView,
    model: gtk::StringList,
    /// Mapping from the model index to an action. Refreshed on every
    /// query change (file mode) or on mode switch (command mode).
    rows: RefCell<Vec<CommandPaletteAction>>,
    file_provider: RefCell<Vec<PathBuf>>,
    workspace: RefCell<Option<PathBuf>>,
    /// Mode: file-picker (Ctrl+P) vs command-palette (Ctrl+Shift+P).
    mode: RefCell<PaletteMode>,
    on_action: RefCell<ActionCallback>,
}

type ActionCallback = Option<Box<dyn Fn(CommandPaletteAction)>>;

#[derive(Debug, Clone, Copy)]
enum PaletteMode {
    Files,
    Commands,
}

impl CommandPalette {
    pub fn new() -> Self {
        let entry = gtk::SearchEntry::builder()
            .placeholder_text("Type to filter…")
            .hexpand(true)
            .build();

        let model = gtk::StringList::new(&[]);
        let selection = gtk::SingleSelection::new(Some(model.clone()));
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, list_item| {
            let label = gtk::Label::builder()
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::Middle)
                .margin_start(8)
                .margin_end(8)
                .margin_top(4)
                .margin_bottom(4)
                .build();
            if let Some(item) = list_item.downcast_ref::<gtk::ListItem>() {
                item.set_child(Some(&label));
            }
        });
        factory.connect_bind(|_, list_item| {
            let Some(item) = list_item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let Some(string_obj) = item.item().and_downcast::<gtk::StringObject>() else {
                return;
            };
            let Some(label) = item.child().and_downcast::<gtk::Label>() else {
                return;
            };
            label.set_text(&string_obj.string());
        });

        let list = gtk::ListView::builder()
            .model(&selection)
            .factory(&factory)
            .single_click_activate(true)
            .vexpand(true)
            .build();

        let scroll = gtk::ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .child(&list)
            .build();

        let vbox = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_top(8)
            .margin_bottom(8)
            .margin_start(8)
            .margin_end(8)
            .build();
        vbox.append(&entry);
        vbox.append(&scroll);

        let dialog = adw::Dialog::builder()
            .content_width(DIALOG_WIDTH)
            .content_height(DIALOG_HEIGHT)
            .child(&vbox)
            .title("Command Palette")
            .build();

        let inner = Rc::new(Inner {
            dialog,
            entry,
            list,
            model,
            rows: RefCell::new(Vec::new()),
            file_provider: RefCell::new(Vec::new()),
            workspace: RefCell::new(None),
            mode: RefCell::new(PaletteMode::Files),
            on_action: RefCell::new(None),
        });

        let palette = CommandPalette { inner };
        palette.wire_signals();
        palette
    }

    fn wire_signals(&self) {
        // Refilter on every keystroke.
        let me = self.clone();
        self.inner.entry.connect_search_changed(move |entry| {
            me.refresh(&entry.text());
        });

        // Enter on the entry → activate the first row.
        let me = self.clone();
        self.inner.entry.connect_activate(move |_entry| {
            me.activate_selected();
        });

        // Single-click activate on the list view.
        let me = self.clone();
        self.inner.list.connect_activate(move |_list, position| {
            me.activate_index(position as usize);
        });

        // Down-arrow from the entry: focus the list. Plain key controller.
        let me_focus = self.clone();
        let key = gtk::EventControllerKey::new();
        key.connect_key_pressed(move |_ctrl, keyval, _code, _state| {
            if keyval == gtk::gdk::Key::Down {
                me_focus.inner.list.grab_focus();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        self.inner.entry.add_controller(key);
    }

    /// Update the workspace root so the file picker can list files.
    pub fn set_workspace(&self, root: Option<PathBuf>) {
        let files = match &root {
            Some(p) => collect_files(p, FILE_WALK_DEPTH, MAX_FILES),
            None => Vec::new(),
        };
        *self.inner.file_provider.borrow_mut() = files;
        *self.inner.workspace.borrow_mut() = root;
    }

    /// Open in file-picker mode (Ctrl+P).
    pub fn open_files(&self, parent: &impl IsA<gtk::Widget>) {
        *self.inner.mode.borrow_mut() = PaletteMode::Files;
        self.inner.entry.set_placeholder_text(Some("Open file by name…"));
        self.inner.entry.set_text("");
        self.refresh("");
        self.inner.dialog.present(Some(parent));
        self.inner.entry.grab_focus();
    }

    /// Open in command-palette mode (Ctrl+Shift+P).
    pub fn open_commands(&self, parent: &impl IsA<gtk::Widget>) {
        *self.inner.mode.borrow_mut() = PaletteMode::Commands;
        self.inner.entry.set_placeholder_text(Some("Run a command…"));
        self.inner.entry.set_text("");
        self.refresh("");
        self.inner.dialog.present(Some(parent));
        self.inner.entry.grab_focus();
    }

    /// Wire the action sink. Called once at construction by the host
    /// window; the closure is invoked on the GTK main thread when the
    /// user activates a row.
    pub fn set_action_callback(&self, cb: impl Fn(CommandPaletteAction) + 'static) {
        *self.inner.on_action.borrow_mut() = Some(Box::new(cb));
    }

    fn refresh(&self, query: &str) {
        let mode = *self.inner.mode.borrow();
        // Rebuild the StringList contents from the current matches.
        // gtk::StringList does not have a `clear`; splice from 0..len with [].
        let prev_len = self.inner.model.n_items();
        let (labels, actions): (Vec<String>, Vec<CommandPaletteAction>) = match mode {
            PaletteMode::Files => {
                let files = self.inner.file_provider.borrow();
                let matches = filter_files(&files, query);
                let labels = matches.iter().map(|p| p.display().to_string()).collect();
                let actions = matches
                    .into_iter()
                    .map(CommandPaletteAction::OpenFile)
                    .collect();
                (labels, actions)
            }
            PaletteMode::Commands => {
                let cmds = default_commands();
                let matches = filter_commands(&cmds, query);
                let labels = matches.iter().map(|(name, _)| (*name).to_string()).collect();
                let actions = matches.into_iter().map(|(_, a)| a).collect();
                (labels, actions)
            }
        };

        let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        self.inner.model.splice(0, prev_len, &label_refs);
        *self.inner.rows.borrow_mut() = actions;

        // Pre-select first row so Enter on the entry activates it.
        if let Some(selection) = self
            .inner
            .list
            .model()
            .and_downcast::<gtk::SingleSelection>()
            && self.inner.model.n_items() > 0
        {
            selection.set_selected(0);
        }
    }

    fn activate_selected(&self) {
        let idx = self
            .inner
            .list
            .model()
            .and_downcast::<gtk::SingleSelection>()
            .map(|s| s.selected() as usize)
            .unwrap_or(0);
        self.activate_index(idx);
    }

    fn activate_index(&self, idx: usize) {
        let action = {
            let rows = self.inner.rows.borrow();
            rows.get(idx).cloned()
        };
        if let Some(action) = action
            && let Some(cb) = self.inner.on_action.borrow().as_ref()
        {
            cb(action);
        }
        self.inner.dialog.close();
    }
}

impl Default for CommandPalette {
    fn default() -> Self {
        Self::new()
    }
}

/// The fixed list of palette commands. Built at runtime because
/// `CommandPaletteAction` is not `const`-constructible.
pub(crate) fn default_commands() -> Vec<(&'static str, CommandPaletteAction)> {
    vec![
        (
            "Toggle Vim Mode",
            CommandPaletteAction::Toggle("vim-mode"),
        ),
        (
            "Toggle Sidebar",
            CommandPaletteAction::Toggle("sidebar"),
        ),
        (
            "Toggle Chat Pane",
            CommandPaletteAction::Toggle("chat-pane"),
        ),
        (
            "Open Recent Workspace…",
            CommandPaletteAction::NoOp,
        ),
        ("Quit", CommandPaletteAction::Toggle("quit")),
    ]
}

/// Case-insensitive substring filter. Prefix matches (on the file name
/// component) come first, then other substring matches, both in original
/// order. Empty query returns the input as-is.
pub(crate) fn filter_files(files: &[PathBuf], query: &str) -> Vec<PathBuf> {
    if query.is_empty() {
        return files.to_vec();
    }
    let q = query.to_lowercase();
    let mut prefix: Vec<PathBuf> = Vec::new();
    let mut substring: Vec<PathBuf> = Vec::new();
    for f in files {
        let s = f.to_string_lossy().to_lowercase();
        let name = f
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if name.starts_with(&q) || s.starts_with(&q) {
            prefix.push(f.clone());
        } else if s.contains(&q) {
            substring.push(f.clone());
        }
    }
    prefix.extend(substring);
    prefix
}

fn filter_commands(
    cmds: &[(&'static str, CommandPaletteAction)],
    query: &str,
) -> Vec<(&'static str, CommandPaletteAction)> {
    if query.is_empty() {
        return cmds.to_vec();
    }
    let q = query.to_lowercase();
    cmds.iter()
        .filter(|(name, _)| name.to_lowercase().contains(&q))
        .cloned()
        .collect()
}

/// Walk `root` up to `max_depth` directories deep, collecting at most
/// `cap` regular files. Skips dotfiles and any path component starting
/// with `.` (covers `.git`, `.cache`, `.venv`, etc.). Sorted by relative
/// path, lexicographically.
fn collect_files(root: &Path, max_depth: usize, cap: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        if out.len() >= cap {
            break;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            if out.len() >= cap {
                break;
            }
            let name = entry.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            if name_str.starts_with('.') {
                continue;
            }
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                if depth + 1 < max_depth {
                    stack.push((path, depth + 1));
                }
            } else if ft.is_file() {
                let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
                out.push(rel);
            }
        }
    }
    out.sort();
    out
}

#[cfg(all(test, feature = "gtk"))]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn substring_filter_orders_prefix_matches_first() {
        let files = vec![
            PathBuf::from("src/lib.rs"),
            PathBuf::from("src/main.rs"),
            PathBuf::from("README.md"),
        ];
        let m = filter_files(&files, "li");
        assert_eq!(m[0], PathBuf::from("src/lib.rs"));
    }

    #[test]
    fn empty_query_returns_all() {
        let files = vec![PathBuf::from("a"), PathBuf::from("b")];
        let m = filter_files(&files, "");
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn case_insensitive_match() {
        let files = vec![PathBuf::from("README.md")];
        let m = filter_files(&files, "readme");
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn commands_list_is_non_empty() {
        assert!(default_commands().len() >= 4);
    }

    #[test]
    fn commands_filter_by_name() {
        let cmds = default_commands();
        let m = filter_commands(&cmds, "vim");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].0, "Toggle Vim Mode");
    }

    #[test]
    fn collect_files_respects_cap_and_depth() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "x").unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub").join("b.txt"), "x").unwrap();
        let files = collect_files(root, 5, 1024);
        assert_eq!(files.len(), 2);
        // Skip dotfiles
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(root.join(".git").join("HEAD"), "x").unwrap();
        let files = collect_files(root, 5, 1024);
        assert_eq!(files.len(), 2);
    }
}
