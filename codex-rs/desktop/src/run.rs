//! Per-role async entry points.

use crate::role::Role;
use anyhow::Result;
use tracing::info;

pub async fn run_role(role: Role) -> Result<()> {
    match role {
        Role::Desktop => run_desktop().await,
        Role::Agent => run_agent().await,
        Role::Lspd => run_lspd().await,
    }
}

async fn run_desktop() -> Result<()> {
    info!("codex-desktop: starting in desktop role");

    #[cfg(feature = "gtk")]
    {
        crate::gui::run_main_window().await
    }

    #[cfg(not(feature = "gtk"))]
    {
        eprintln!(
            "codex-desktop scaffolding (no GTK feature). Build with \
            `--features gtk` to launch the GUI. See \
            docs/desktop-architecture.md for the full plan."
        );
        Ok(())
    }
}

async fn run_agent() -> Result<()> {
    info!("codex-desktop: starting in agent role");
    crate::agent_role::run().await
}

async fn run_lspd() -> Result<()> {
    info!("codex-desktop: starting in lspd role (stub)");
    eprintln!(
        "codex-lspd stub. Real implementation lands in PR-D (LSP/lint \
        multiplexer). See docs/desktop-architecture.md §3.1."
    );
    Ok(())
}
