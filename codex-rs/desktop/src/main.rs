#![forbid(unsafe_code)]

use anyhow::Result;
use codex_desktop::role;
use codex_desktop::run;
use std::ffi::OsString;

fn main() -> Result<()> {
    init_tracing();

    let argv0: OsString = std::env::args_os().next().unwrap_or_default();
    let detected = role::detect_role_from_argv0(&argv0);

    // Build a single-threaded current-thread runtime by default; GTK has its
    // own main loop and we want the tokio runtime to live on a worker thread
    // anyway. For PR-A we use a multi-thread runtime to keep the stub roles
    // simple — the GUI binding will move to a dedicated worker pattern in
    // PR-B/C.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    runtime.block_on(run::run_role(detected))
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("codex_desktop=info,warn")),
        )
        .with_writer(std::io::stderr)
        .try_init();
}
