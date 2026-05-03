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

/// Environment variable that overrides the argv[0]-based role detection.
///
/// Recognised values: `"desktop"`, `"agent"`, `"lspd"`. Anything else is
/// ignored and the argv[0] basename is used instead. The override exists so
/// integration tests (and developer overrides) can spawn a single binary in
/// a specific role without renaming it on disk.
pub const FORCE_ROLE_ENV: &str = "CODEX_DESKTOP_FORCE_ROLE";

/// Resolve the role to run.
///
/// Honours [`FORCE_ROLE_ENV`] first; if set to a recognised value, that role
/// wins regardless of `argv[0]`. Otherwise falls through to
/// [`detect_role_from_argv0`].
pub fn detect_role(argv0: &OsString) -> Role {
    let forced = std::env::var(FORCE_ROLE_ENV).ok();
    detect_role_with_env(argv0, forced.as_deref())
}

/// Pure helper used by [`detect_role`] and unit tests: resolves the role
/// from the `argv[0]` value and an optional override string.
///
/// Splitting this out keeps the env-reading concerns out of the table-driven
/// unit tests (mutating `std::env` from concurrent tests is unsound under
/// `#![forbid(unsafe_code)]`).
pub fn detect_role_with_env(argv0: &OsString, forced: Option<&str>) -> Role {
    if let Some(value) = forced {
        match value {
            "desktop" => return Role::Desktop,
            "agent" => return Role::Agent,
            "lspd" => return Role::Lspd,
            _ => {
                // Unrecognised value: fall through to argv0 detection.
            }
        }
    }
    detect_role_from_argv0(argv0)
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

    #[test]
    fn force_role_env_agent_wins_over_argv0() {
        let got =
            detect_role_with_env(&OsString::from("/usr/bin/codex-desktop"), Some("agent"));
        assert_eq!(got, Role::Agent);
    }

    #[test]
    fn force_role_env_desktop_wins_over_argv0() {
        let got = detect_role_with_env(
            &OsString::from("/usr/libexec/codex-desktop/codex-agent"),
            Some("desktop"),
        );
        assert_eq!(got, Role::Desktop);
    }

    #[test]
    fn force_role_env_lspd_recognised() {
        let got = detect_role_with_env(&OsString::from("/usr/bin/codex-desktop"), Some("lspd"));
        assert_eq!(got, Role::Lspd);
    }

    #[test]
    fn force_role_env_unknown_value_falls_through() {
        // Unrecognised env value should fall back to argv0-based detection.
        let got = detect_role_with_env(&OsString::from("codex-agent"), Some("garbage"));
        assert_eq!(got, Role::Agent);
    }

    #[test]
    fn force_role_env_unset_uses_argv0() {
        let got = detect_role_with_env(&OsString::from("/path/to/codex-lspd"), None);
        assert_eq!(got, Role::Lspd);
    }
}
