#![forbid(unsafe_code)]

//! Library crate for the codex-desktop binary.
//!
//! Exposes a small public API that the binary `main.rs` orchestrates:
//!  * [`role`] — argv[0] dispatch logic that picks one of `codex-desktop`,
//!    `codex-agent`, `codex-lspd`.
//!  * [`run`] — async entry points for each role.
//!
//! The actual GUI code lives behind the `gtk` cargo feature so that the
//! workspace can build in CI environments without GTK development headers.

pub mod agent_role;
pub mod role;
pub mod run;

#[cfg(feature = "gtk")]
pub mod gui;

#[cfg(test)]
mod tests {
    use crate::role::{Role, detect_role_from_argv0};
    use pretty_assertions::assert_eq;

    #[test]
    fn lib_exposes_role_module() {
        assert_eq!(detect_role_from_argv0(&"codex-agent".into()), Role::Agent);
    }
}
