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
        // GTK's main loop is blocking, so run it on a dedicated blocking
        // thread to keep the tokio runtime usable. We capture the
        // current runtime handle BEFORE moving into the blocking
        // closure so the GUI side can spawn tokio tasks (the agent
        // bridge) on the same runtime.
        let rt_handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || crate::gui::run_main_window(rt_handle)).await?
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
