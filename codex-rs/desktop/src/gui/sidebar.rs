#![cfg(feature = "gtk")]

//! Sidebar: a 4-tab `GtkStack` (Files, Search, Diagnostics, Threads)
//! wrapped in an `AdwNavigationView`. The Files tab is a lazy directory
//! list of the current workspace.

use std::cell::RefCell;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::gio;
use gtk::glib;

#[derive(Clone)]
pub struct Sidebar {
    root: adw::NavigationView,
    title_label: gtk::Label,
    file_list: gtk::ListView,
    file_model: Rc<RefCell<Option<gtk::DirectoryList>>>,
    root_path: Rc<RefCell<Option<PathBuf>>>,
    on_file_activated: FileActivatedSlot,
}

type FileActivatedSlot = Rc<RefCell<Option<Box<dyn Fn(&Path)>>>>;

impl Sidebar {
    pub fn new() -> Self {
        let title_label = gtk::Label::builder()
            .label("Workspace")
            .css_classes(["title"])
            .build();

        let header = adw::HeaderBar::builder()
            .show_start_title_buttons(true)
            .show_end_title_buttons(false)
            .build();
        header.set_title_widget(Some(&title_label));

        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .vexpand(true)
            .build();

        // Files tab.
        let file_factory = gtk::SignalListItemFactory::new();
        file_factory.connect_setup(|_factory, list_item| {
            let row = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(6)
                .build();
            let icon = gtk::Image::from_icon_name("text-x-generic-symbolic");
            let label = gtk::Label::builder().xalign(0.0).build();
            row.append(&icon);
            row.append(&label);
            if let Some(item) = list_item.downcast_ref::<gtk::ListItem>() {
                item.set_child(Some(&row));
            }
        });
        file_factory.connect_bind(|_factory, list_item| {
            let Some(item) = list_item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let Some(file_info) = item.item().and_downcast::<gio::FileInfo>() else {
                return;
            };
            let Some(row) = item.child().and_downcast::<gtk::Box>() else {
                return;
            };
            let icon_widget = row.first_child().and_downcast::<gtk::Image>();
            let label_widget = row.last_child().and_downcast::<gtk::Label>();
            if let Some(label) = label_widget {
                label.set_text(&file_info.display_name());
            }
            if let Some(icon) = icon_widget {
                let is_dir = file_info.file_type() == gio::FileType::Directory;
                icon.set_icon_name(Some(if is_dir {
                    "folder-symbolic"
                } else {
                    "text-x-generic-symbolic"
                }));
            }
        });

        let file_list = gtk::ListView::builder()
            .factory(&file_factory)
            .single_click_activate(false)
            .vexpand(true)
            .build();

        let file_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .child(&file_list)
            .build();

        let search_placeholder = adw::StatusPage::builder()
            .icon_name("system-search-symbolic")
            .title("Search")
            .description("Workspace search lands in PR-E.")
            .build();
        let diagnostics_placeholder = adw::StatusPage::builder()
            .icon_name("dialog-warning-symbolic")
            .title("Diagnostics")
            .description("Lint/LSP diagnostics land in PR-D.")
            .build();
        let threads_placeholder = adw::StatusPage::builder()
            .icon_name("user-available-symbolic")
            .title("Threads")
            .description("Multi-thread agent UI lands in PR-F.")
            .build();

        stack.add_titled(&file_scroll, Some("files"), "Files");
        stack.add_titled(&search_placeholder, Some("search"), "Search");
        stack.add_titled(&diagnostics_placeholder, Some("diagnostics"), "Diagnostics");
        stack.add_titled(&threads_placeholder, Some("threads"), "Threads");

        let switcher = gtk::StackSwitcher::builder()
            .stack(&stack)
            .halign(gtk::Align::Center)
            .margin_top(4)
            .margin_bottom(4)
            .build();

        let body = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        body.append(&header);
        body.append(&switcher);
        body.append(&stack);

        let page = adw::NavigationPage::builder()
            .title("Codex")
            .child(&body)
            .build();

        let root = adw::NavigationView::new();
        root.add(&page);

        Sidebar {
            root,
            title_label,
            file_list,
            file_model: Rc::new(RefCell::new(None)),
            root_path: Rc::new(RefCell::new(None)),
            on_file_activated: Rc::new(RefCell::new(None)),
        }
    }

    pub fn root(&self) -> &adw::NavigationView {
        &self.root
    }

    /// Set the current workspace root. Populates the Files tab via a
    /// `gtk::DirectoryList` (lazy) and updates the title label.
    pub fn set_root(&self, path: &Path) {
        let display = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        self.title_label.set_text(&display);

        let file = gio::File::for_path(path);
        let dir_list = gtk::DirectoryList::new(
            Some("standard::name,standard::display-name,standard::type,standard::icon"),
            Some(&file),
        );
        dir_list.set_io_priority(glib::Priority::default());
        // Sort: directories first, then alphabetical.
        let sorter = gtk::CustomSorter::new(|a, b| {
            let a = a.downcast_ref::<gio::FileInfo>();
            let b = b.downcast_ref::<gio::FileInfo>();
            match (a, b) {
                (Some(a), Some(b)) => {
                    let a_dir = a.file_type() == gio::FileType::Directory;
                    let b_dir = b.file_type() == gio::FileType::Directory;
                    if a_dir != b_dir {
                        return if a_dir {
                            gtk::Ordering::Smaller
                        } else {
                            gtk::Ordering::Larger
                        };
                    }
                    let an = a.display_name().to_lowercase();
                    let bn = b.display_name().to_lowercase();
                    an.cmp(&bn).into()
                }
                _ => gtk::Ordering::Equal,
            }
        });
        let sort_model = gtk::SortListModel::new(Some(dir_list.clone()), Some(sorter));
        let selection = gtk::SingleSelection::new(Some(sort_model));
        self.file_list.set_model(Some(&selection));
        *self.file_model.borrow_mut() = Some(dir_list);
        *self.root_path.borrow_mut() = Some(path.to_path_buf());

        // Hook up activation: emit through the user-supplied callback.
        let cb_slot = self.on_file_activated.clone();
        let model_for_cb = self.file_model.clone();
        let selection_for_cb = selection;
        self.file_list.connect_activate(move |_view, position| {
            let model = model_for_cb.borrow();
            let _ = model; // unused but ensures the model lives at least as long as the callback
            let item = selection_for_cb.item(position);
            let Some(info) = item.and_downcast::<gio::FileInfo>() else {
                return;
            };
            // The DirectoryList reports each FileInfo with attribute
            // `standard::name`. We resolve to a path via the parent we
            // tracked in `root_path`.
            let name = info.name();
            if let Some(cb) = cb_slot.borrow().as_ref() {
                let path = name;
                cb(path.as_path());
            }
        });
    }

    /// Currently-tracked workspace root.
    pub fn current_root(&self) -> Option<PathBuf> {
        self.root_path.borrow().clone()
    }

    /// Register a callback invoked when the user activates a file row.
    /// The path is relative to the workspace root.
    pub fn on_file_activated(&self, cb: impl Fn(&Path) + 'static) {
        *self.on_file_activated.borrow_mut() = Some(Box::new(cb));
    }
}

impl Default for Sidebar {
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
    fn sidebar_root_round_trip() {
        if !ensure_gtk() {
            return;
        }
        let sidebar = Sidebar::new();
        assert!(sidebar.current_root().is_none());
        let dir = tempfile::tempdir().expect("tempdir");
        sidebar.set_root(dir.path());
        assert_eq!(sidebar.current_root().as_deref(), Some(dir.path()));
    }
}
