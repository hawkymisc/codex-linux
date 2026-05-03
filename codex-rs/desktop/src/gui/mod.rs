//! GTK4 + libadwaita main window for the codex-desktop binary.
//!
//! See `docs/desktop-architecture.md` §3 for the target widget tree. This
//! module implements the v0 layout described there:
//!
//! ```text
//! AdwToastOverlay
//! └── AdwApplicationWindow
//!     └── AdwOverlaySplitView
//!         ├── sidebar  : AdwNavigationView (Files/Search/Diagnostics/Threads)
//!         ├── content  : AdwTabView with one tab per open file (GtkSourceView)
//!         └── secondary: ChatPane (collapsible)
//! ```
//!
//! `run_main_window` is synchronous — GTK's main loop is blocking and is
//! driven on whichever thread calls into it. `crate::run::run_desktop`
//! invokes us via `tokio::task::spawn_blocking`.

pub mod app;
pub mod chat_pane;
pub mod command_palette;
pub mod editor_pane;
pub mod sidebar;
pub mod theme;

pub use app::run_main_window;
