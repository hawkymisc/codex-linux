#![cfg(feature = "gtk")]

//! GTK4 + libadwaita main window.
//!
//! For PR-A this is a deliberately minimal scaffold: opens an
//! `AdwApplicationWindow` containing a placeholder `AdwStatusPage`. The
//! real layout (AdwOverlaySplitView with sidebar / editor / chat panes)
//! lands in PR-B alongside the AgentBackend wiring.

use anyhow::Result;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

pub async fn run_main_window() -> Result<()> {
    let app = adw::Application::builder()
        .application_id("com.openai.CodexDesktop")
        .build();

    app.connect_activate(|app| {
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .default_width(1280)
            .default_height(800)
            .title("Codex")
            .build();

        let status_page = adw::StatusPage::builder()
            .icon_name("system-search-symbolic")
            .title("Codex Desktop")
            .description(
                "PR-A scaffolding. The real UI (AdwOverlaySplitView with \
                sidebar, editor, chat) lands in PR-B. See \
                docs/desktop-architecture.md.",
            )
            .build();

        let toast_overlay = adw::ToastOverlay::builder().child(&status_page).build();

        window.set_content(Some(&toast_overlay));
        window.present();
    });

    let exit_code = app.run();
    if exit_code.value() == 0 {
        Ok(())
    } else {
        anyhow::bail!("GTK application exited with status {}", exit_code.value());
    }
}
