#![cfg(feature = "gtk")]

//! `AppState`, `MainWindow`, and the top-level `run_main_window` entry.

use std::cell::RefCell;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use anyhow::Result;
use gtk::glib;
use tokio::runtime::Handle;

use crate::agent_bridge::{AgentBridge, BridgeEvent};

use super::chat_pane::ChatPane;
use super::command_palette::CommandPalette;
use super::command_palette::CommandPaletteAction;
use super::editor_pane::EditorPane;
use super::sidebar::Sidebar;
use super::theme;

const APP_ID: &str = "dev.codex.Desktop";
const DEFAULT_WIDTH: i32 = 1280;
const DEFAULT_HEIGHT: i32 = 800;
const SIDEBAR_BREAKPOINT_PX: f64 = 720.0;

/// Mutable state shared across signal handlers.
pub struct AppState {
    pub workspace_root: PathBuf,
    pub open_tabs: Vec<EditorPane>,
    /// Handle to the agent bridge, populated once the GUI activates.
    /// `None` if the bridge failed to spawn — the GUI still runs, the
    /// chat pane just falls back to local-echo behaviour.
    pub agent_bridge: Option<Rc<AgentBridge>>,
}

impl AppState {
    pub fn new(workspace_root: PathBuf) -> Self {
        Self {
            workspace_root,
            open_tabs: Vec::new(),
            agent_bridge: None,
        }
    }
}

/// Owning handle to the top-level application window and its child
/// widgets. Cloning is cheap (each field is a refcounted GObject).
#[derive(Clone)]
pub struct MainWindow {
    pub window: adw::ApplicationWindow,
    pub toast_overlay: adw::ToastOverlay,
    pub split_view: adw::OverlaySplitView,
    pub tab_view: adw::TabView,
    pub sidebar: Sidebar,
    pub chat: ChatPane,
    pub command_palette: CommandPalette,
    /// Root container for the chat pane; we toggle its visibility via
    /// the command palette's `Toggle("chat-pane")` action.
    pub chat_root: gtk::Box,
    pub state: Rc<RefCell<AppState>>,
}

impl MainWindow {
    fn build(app: &adw::Application, rt_handle: Handle) -> Self {
        theme::install();

        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let state = Rc::new(RefCell::new(AppState::new(cwd.clone())));

        let title = format!("Codex Desktop — {}", cwd.display());

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .default_width(DEFAULT_WIDTH)
            .default_height(DEFAULT_HEIGHT)
            .title(&title)
            .build();

        let sidebar = Sidebar::new();
        sidebar.set_root(&cwd);

        let tab_view = adw::TabView::builder().vexpand(true).hexpand(true).build();
        let tab_bar = adw::TabBar::builder()
            .view(&tab_view)
            .autohide(false)
            .build();

        let editor_area = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        editor_area.append(&tab_bar);
        editor_area.append(&tab_view);

        // Content side: tabs on the left, chat on the right via a Flap-like
        // collapsible pane. AdwOverlaySplitView only exposes one sidebar
        // slot, so we put chat in a horizontal Paned inside the content
        // half.
        let chat = ChatPane::new();
        let chat_box = chat.root().clone();
        chat_box.set_width_request(360);

        let content_paned = gtk::Paned::builder()
            .orientation(gtk::Orientation::Horizontal)
            .start_child(&editor_area)
            .end_child(&chat_box)
            .resize_start_child(true)
            .resize_end_child(true)
            .shrink_start_child(false)
            .shrink_end_child(false)
            .position(880)
            .build();

        let split_view = adw::OverlaySplitView::builder()
            .sidebar(sidebar.root())
            .content(&content_paned)
            .min_sidebar_width(220.0)
            .max_sidebar_width(360.0)
            .sidebar_width_fraction(0.22)
            .show_sidebar(true)
            .build();

        let toast_overlay = adw::ToastOverlay::new();
        toast_overlay.set_child(Some(&split_view));

        // Breakpoint: collapse the sidebar at narrow widths.
        let breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            SIDEBAR_BREAKPOINT_PX,
            adw::LengthUnit::Px,
        ));
        breakpoint.add_setter(&split_view, "collapsed", Some(&true.into()));
        window.add_breakpoint(breakpoint);

        window.set_content(Some(&toast_overlay));

        let command_palette = CommandPalette::new();
        command_palette.set_workspace(Some(cwd));

        let main = MainWindow {
            window,
            toast_overlay,
            split_view,
            tab_view,
            sidebar,
            chat,
            command_palette,
            chat_root: chat_box,
            state,
        };
        main.wire_signals();
        main.install_shortcuts();
        main.wire_command_palette();
        main.wire_agent_bridge(rt_handle);

        // Open one empty editor tab by default.
        main.open_empty_tab();

        main
    }

    /// Wire the command palette's action callback to dispatch to the
    /// appropriate handler on this window.
    fn wire_command_palette(&self) {
        let me = self.clone();
        self.command_palette.set_action_callback(move |action| {
            me.dispatch_palette_action(action);
        });
    }

    fn dispatch_palette_action(&self, action: CommandPaletteAction) {
        match action {
            CommandPaletteAction::OpenFile(path) => {
                if let Err(err) = self.open_file_tab(&path) {
                    tracing::warn!(error = %err, path = %path.display(), "palette open_file failed");
                    self.toast(&format!("Failed to open: {}", path.display()));
                }
            }
            CommandPaletteAction::Toggle("vim-mode") => {
                // Toggle on every open editor tab: a global toggle is
                // friendlier than a per-tab one, especially when a tab
                // is just opened from the palette.
                let state = self.state.borrow();
                let next = !state
                    .open_tabs
                    .iter()
                    .any(super::editor_pane::EditorPane::vim_mode);
                for editor in &state.open_tabs {
                    editor.set_vim_mode(next);
                }
                drop(state);
                self.toast(if next { "Vim mode: ON" } else { "Vim mode: OFF" });
            }
            CommandPaletteAction::Toggle("sidebar") => {
                let split = &self.split_view;
                split.set_show_sidebar(!split.shows_sidebar());
            }
            CommandPaletteAction::Toggle("chat-pane") => {
                self.chat_root.set_visible(!self.chat_root.is_visible());
            }
            CommandPaletteAction::Toggle("quit") => {
                self.window.close();
            }
            CommandPaletteAction::Toggle(other) => {
                tracing::debug!(name = other, "palette: unknown toggle");
            }
            CommandPaletteAction::NoOp => {}
        }
    }

    /// Spawn the agent bridge and route its events into the chat pane.
    ///
    /// On bridge spawn failure we log a warning and surface a system
    /// message in the chat pane; the rest of the GUI continues to
    /// function. The send button still echoes the user's text locally.
    fn wire_agent_bridge(&self, rt_handle: Handle) {
        let bridge = match AgentBridge::spawn(rt_handle) {
            Ok(b) => Rc::new(b),
            Err(err) => {
                tracing::warn!(error = %err, "gui: failed to spawn agent bridge");
                self.chat.append_message(
                    "system",
                    "Could not start the agent backend; messages will not be processed.",
                );
                return;
            }
        };

        // Install the submit-callback into the chat pane so the Send
        // button forwards prompts to the bridge.
        let submit_bridge = Rc::clone(&bridge);
        self.chat.set_submit_callback(move |prompt| {
            submit_bridge.submit(prompt);
        });

        // Drain the bridge's event channel from the GTK main loop. We
        // hold a weak reference to the chat pane so dropping the window
        // does not keep the receiver task alive forever.
        if let Some(mut events_rx) = bridge.take_events_rx() {
            let chat = self.chat.clone();
            glib::MainContext::default().spawn_local(async move {
                while let Some(event) = events_rx.recv().await {
                    match event {
                        BridgeEvent::MessageDelta { text } => {
                            chat.start_or_extend_assistant_block(&text);
                        }
                        BridgeEvent::TurnCompleted { stop_reason } => {
                            chat.finalise_assistant_block(&stop_reason);
                        }
                        BridgeEvent::AgentClosed => {
                            chat.show_agent_disconnected();
                        }
                    }
                }
            });
        }

        self.state.borrow_mut().agent_bridge = Some(bridge);
    }

    fn wire_signals(&self) {
        let me = self.clone();
        let workspace_root = self.state.borrow().workspace_root.clone();
        self.sidebar.on_file_activated(move |relpath| {
            // Resolve to absolute path under the workspace root.
            let abs = if relpath.is_absolute() {
                relpath.to_path_buf()
            } else {
                workspace_root.join(relpath)
            };
            if abs.is_dir() {
                me.sidebar.set_root(&abs);
            } else if let Err(err) = me.open_file_tab(&abs) {
                tracing::warn!(error = %err, path = %abs.display(), "open_file_tab failed");
                me.toast(&format!("Failed to open: {}", abs.display()));
            }
        });
    }

    fn install_shortcuts(&self) {
        let controller = gtk::ShortcutController::new();
        controller.set_scope(gtk::ShortcutScope::Global);

        // Ctrl+W → close current tab.
        let me = self.clone();
        let close_tab = gtk::CallbackAction::new(move |_widget, _args| {
            if let Some(page) = me.tab_view.selected_page() {
                me.tab_view.close_page(&page);
            }
            glib::Propagation::Stop
        });
        controller.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("<Ctrl>w"),
            Some(close_tab),
        ));

        // Ctrl+T → open file dialog.
        let me = self.clone();
        let open_dialog = gtk::CallbackAction::new(move |_widget, _args| {
            me.show_open_dialog();
            glib::Propagation::Stop
        });
        controller.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("<Ctrl>t"),
            Some(open_dialog),
        ));

        // Ctrl+P → command palette in file-picker mode.
        let me = self.clone();
        let palette_files = gtk::CallbackAction::new(move |_widget, _args| {
            me.command_palette.open_files(&me.window);
            glib::Propagation::Stop
        });
        controller.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("<Ctrl>p"),
            Some(palette_files),
        ));

        // Ctrl+Shift+P → command palette in commands mode.
        let me = self.clone();
        let palette_commands = gtk::CallbackAction::new(move |_widget, _args| {
            me.command_palette.open_commands(&me.window);
            glib::Propagation::Stop
        });
        controller.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("<Ctrl><Shift>p"),
            Some(palette_commands),
        ));

        // F10 → toggle a placeholder menu (no-op toast for v0).
        let me = self.clone();
        let menu = gtk::CallbackAction::new(move |_widget, _args| {
            me.toast("Menu (F10) — not yet implemented");
            glib::Propagation::Stop
        });
        controller.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("F10"),
            Some(menu),
        ));

        // Ctrl+? → shortcuts overlay (placeholder toast).
        let me = self.clone();
        let help = gtk::CallbackAction::new(move |_widget, _args| {
            me.toast(
                "Keyboard shortcuts: Ctrl+T open, Ctrl+W close, Ctrl+P files, Ctrl+Shift+P commands",
            );
            glib::Propagation::Stop
        });
        controller.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("<Ctrl>question"),
            Some(help),
        ));

        self.window.add_controller(controller);
    }

    fn open_empty_tab(&self) {
        let editor = EditorPane::new();
        let page = self.tab_view.append(editor.root());
        page.set_title("Untitled");
        self.state.borrow_mut().open_tabs.push(editor);
    }

    /// Open `path` in a new editor tab.
    pub fn open_file_tab(&self, path: &Path) -> Result<()> {
        let editor = EditorPane::new();
        editor.open_file(path)?;
        let page = self.tab_view.append(editor.root());
        let title = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        page.set_title(&title);
        self.tab_view.set_selected_page(&page);
        self.state.borrow_mut().open_tabs.push(editor);
        Ok(())
    }

    fn show_open_dialog(&self) {
        let dialog = gtk::FileDialog::builder()
            .title("Open File")
            .modal(true)
            .build();
        let me = self.clone();
        dialog.open(
            Some(&self.window),
            None::<&gtk::gio::Cancellable>,
            move |res| match res {
                Ok(file) => {
                    if let Some(path) = file.path()
                        && let Err(err) = me.open_file_tab(&path)
                    {
                        tracing::warn!(error = %err, "FileDialog open_file_tab failed");
                        me.toast(&format!("Failed to open: {}", path.display()));
                    }
                }
                Err(err) => {
                    // User dismissal yields a cancelled error — log at debug level.
                    tracing::debug!(error = %err, "FileDialog cancelled");
                }
            },
        );
    }

    fn toast(&self, text: &str) {
        let toast = adw::Toast::builder().title(text).timeout(3).build();
        self.toast_overlay.add_toast(toast);
    }

    pub fn present(&self) {
        self.window.present();
    }
}

/// Run the GTK main loop. **Synchronous** — GTK takes over the calling
/// thread. `crate::run::run_desktop` invokes us inside
/// `tokio::task::spawn_blocking` and threads its tokio runtime handle
/// through so the agent bridge can spawn tasks on the same runtime.
pub fn run_main_window(rt_handle: Handle) -> Result<()> {
    tracing::info!("gui: building adw::Application id={APP_ID}");

    let app = adw::Application::builder().application_id(APP_ID).build();

    app.connect_activate(move |app| {
        let main = MainWindow::build(app, rt_handle.clone());
        main.present();
        crate::portal::install(&main.window);
    });

    // Pass an empty argv so GTK doesn't try to interpret the codex CLI's
    // own arguments.
    let argv: [&str; 0] = [];
    let exit_code = app.run_with_args(&argv);
    if exit_code.value() == 0 {
        Ok(())
    } else {
        anyhow::bail!("GTK application exited with status {}", exit_code.value());
    }
}

#[cfg(all(test, feature = "gtk"))]
mod tests {
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
    fn app_state_initialises() {
        if !ensure_gtk() {
            return;
        }
        let s = super::AppState::new(std::path::PathBuf::from("/tmp"));
        assert_eq!(s.workspace_root, std::path::PathBuf::from("/tmp"));
        assert!(s.open_tabs.is_empty());
    }
}
