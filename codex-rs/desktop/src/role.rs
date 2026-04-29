//! argv[0]-multiplex dispatch.
//!
//! `codex-desktop` ships as one ELF; the basename of `argv[0]` selects
//! which role the process runs as. This mirrors the pattern already used
//! by `codex-arg0` for `codex-linux-sandbox`, `apply_patch`, etc.

use std::ffi::OsString;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Desktop,
    Agent,
    Lspd,
}

impl Role {
    pub const DESKTOP_BIN: &'static str = "codex-desktop";
    pub const AGENT_BIN: &'static str = "codex-agent";
    pub const LSPD_BIN: &'static str = "codex-lspd";
}

/// Detect role from `argv[0]`. Defaults to [`Role::Desktop`] if the
/// basename is unrecognised (so plain `cargo run -p codex-desktop` always
/// runs the GUI role).
pub fn detect_role_from_argv0(argv0: &OsString) -> Role {
    let basename = Path::new(argv0)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");

    match basename {
        Role::AGENT_BIN => Role::Agent,
        Role::LSPD_BIN => Role::Lspd,
        // Default: desktop. `codex-desktop`, `codex-desktop-debug`, the
        // cargo test harness binary, etc. all land here.
        _ => Role::Desktop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::ffi::OsString;

    #[test]
    fn detects_agent_basename() {
        assert_eq!(
            detect_role_from_argv0(&OsString::from(
                "/usr/libexec/codex-desktop/codex-agent"
            )),
            Role::Agent
        );
    }

    #[test]
    fn detects_lspd_basename() {
        assert_eq!(
            detect_role_from_argv0(&OsString::from("codex-lspd")),
            Role::Lspd
        );
    }

    #[test]
    fn defaults_to_desktop_for_unknown() {
        assert_eq!(
            detect_role_from_argv0(&OsString::from("/path/to/cargo-test-bin")),
            Role::Desktop
        );
    }

    #[test]
    fn empty_argv0_defaults_to_desktop() {
        assert_eq!(
            detect_role_from_argv0(&OsString::from("")),
            Role::Desktop
        );
    }
}
