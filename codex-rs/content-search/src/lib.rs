#![forbid(unsafe_code)]

//! Find-in-files search for codex-desktop.
//!
//! Wraps ripgrep's `grep-searcher` + `grep-regex` + `ignore` crates in a
//! streaming session API that mirrors `codex-file-search` (filename
//! fuzzy search). One [`SearchSession`] is alive per query; results
//! flow over a `tokio::sync::mpsc::Receiver<ContentMatch>`.

pub mod matcher;
pub mod session;

pub use session::ContentMatch;
pub use session::SearchOptions;
pub use session::SearchSession;
