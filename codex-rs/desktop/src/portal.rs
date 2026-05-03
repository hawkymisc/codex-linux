//! xdg-desktop-portal Settings client.
//!
//! See `docs/desktop-architecture.md` §6.1. Requires a running
//! xdg-desktop-portal session (always present on Ubuntu 24.04+ GNOME).
//! On systems where the portal is unavailable the helper degrades to
//! "no signal" — the GUI keeps the gsettings-polled startup state and
//! never panics.
//!
//! Two settings are surfaced from the `org.freedesktop.appearance`
//! namespace:
//!
//!  * `color-scheme`: `0` (no preference) | `1` (dark) | `2` (light) per the
//!    portal spec.
//!  * `accent-color`: optional `(f64, f64, f64)` RGB triple available on
//!    GNOME 47+ portals; older portals don't expose it. Treat absence as
//!    `None`.
//!
//! The watcher pushes [`ThemeUpdate`] messages onto an unbounded mpsc
//! channel. The GUI thread drains it via
//! `glib::MainContext::default().spawn_local` and applies via
//! [`crate::gui::theme::apply_theme`].

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{info, warn};

/// System color-scheme preference, mirroring the integer values defined
/// by the xdg-desktop-portal Settings spec.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum ColorScheme {
    /// `0` — no preference.
    #[default]
    NoPreference,
    /// `1` — prefer a dark appearance.
    Dark,
    /// `2` — prefer a light appearance.
    Light,
}

impl ColorScheme {
    /// Map a raw `u32` from the portal into a [`ColorScheme`]. Unknown
    /// values fall back to [`ColorScheme::NoPreference`].
    pub fn from_portal_u32(v: u32) -> Self {
        match v {
            1 => Self::Dark,
            2 => Self::Light,
            _ => Self::NoPreference,
        }
    }
}

/// A single snapshot of the system's appearance preferences. Emitted on
/// startup and on every `SettingChanged` signal that touches the
/// `org.freedesktop.appearance` namespace.
#[derive(Debug, Clone)]
pub struct ThemeUpdate {
    pub color_scheme: ColorScheme,
    pub accent: Option<(f64, f64, f64)>,
}

/// Spawn a tokio task that reads the initial settings via the portal,
/// then subscribes to `SettingChanged` and forwards updates. Returns the
/// receiver for the GUI thread to drain.
///
/// On error (no D-Bus, no portal present), returns `Err` and the GUI is
/// expected to log `warn!` and continue with the fallback path. The
/// returned receiver is still valid — it will simply never produce any
/// items.
pub fn spawn_theme_watcher() -> (UnboundedReceiver<ThemeUpdate>, Result<()>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let result = try_spawn(tx);
    (rx, result)
}

fn try_spawn(tx: UnboundedSender<ThemeUpdate>) -> Result<()> {
    tokio::spawn(async move {
        if let Err(e) = run_portal_loop(tx).await {
            warn!(error = %e, "portal watcher exited");
        }
    });
    Ok(())
}

async fn run_portal_loop(tx: UnboundedSender<ThemeUpdate>) -> Result<()> {
    use ashpd::desktop::settings::Settings;
    use futures::StreamExt;

    let settings = Settings::new().await?;

    // Initial snapshot.
    let scheme = settings
        .read::<u32>(APPEARANCE_NS, COLOR_SCHEME_KEY)
        .await
        .map(ColorScheme::from_portal_u32)
        .unwrap_or_default();
    let accent = settings
        .read::<(f64, f64, f64)>(APPEARANCE_NS, ACCENT_COLOR_KEY)
        .await
        .ok();
    let _ = tx.send(ThemeUpdate {
        color_scheme: scheme,
        accent,
    });

    info!(color_scheme = ?scheme, accent = ?accent, "portal: initial theme");

    // Subscribe to changes.
    let mut stream = settings.receive_setting_changed().await?;
    while let Some(change) = stream.next().await {
        if change.namespace() != APPEARANCE_NS {
            continue;
        }
        // Re-read both — simpler and cheaper than parsing the inbound
        // variant value out of `change` ourselves.
        let scheme = settings
            .read::<u32>(APPEARANCE_NS, COLOR_SCHEME_KEY)
            .await
            .map(ColorScheme::from_portal_u32)
            .unwrap_or_default();
        let accent = settings
            .read::<(f64, f64, f64)>(APPEARANCE_NS, ACCENT_COLOR_KEY)
            .await
            .ok();
        if tx
            .send(ThemeUpdate {
                color_scheme: scheme,
                accent,
            })
            .is_err()
        {
            // GUI dropped the receiver; nothing to do.
            break;
        }
    }
    Ok(())
}

const APPEARANCE_NS: &str = "org.freedesktop.appearance";
const COLOR_SCHEME_KEY: &str = "color-scheme";
const ACCENT_COLOR_KEY: &str = "accent-color";

/// Convenience installer for the GUI thread. Spawns the tokio watcher
/// and immediately registers a `glib::MainContext::spawn_local` task to
/// drain the channel and apply each [`ThemeUpdate`] via
/// [`crate::gui::theme::apply_theme`].
///
/// Must be called from the GTK main thread (after a `gdk::Display` is
/// available). Failures from the tokio side are logged and swallowed —
/// the GUI keeps running with whatever theme `theme::install` already
/// configured.
pub fn install(_window: &adw::ApplicationWindow) {
    let (mut rx, spawn_result) = spawn_theme_watcher();
    if let Err(err) = spawn_result {
        warn!(error = %err, "portal: theme watcher failed to start");
        return;
    }
    glib::MainContext::default().spawn_local(async move {
        while let Some(update) = rx.recv().await {
            crate::gui::theme::apply_theme(&update);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_scheme_from_portal_u32_maps_correctly() {
        assert!(matches!(
            ColorScheme::from_portal_u32(0),
            ColorScheme::NoPreference
        ));
        assert!(matches!(
            ColorScheme::from_portal_u32(1),
            ColorScheme::Dark
        ));
        assert!(matches!(
            ColorScheme::from_portal_u32(2),
            ColorScheme::Light
        ));
        assert!(matches!(
            ColorScheme::from_portal_u32(99),
            ColorScheme::NoPreference
        ));
    }

    #[test]
    fn theme_update_default_is_no_preference() {
        let u = ThemeUpdate {
            color_scheme: ColorScheme::default(),
            accent: None,
        };
        assert!(matches!(u.color_scheme, ColorScheme::NoPreference));
        assert!(u.accent.is_none());
    }

    #[test]
    fn color_scheme_default_is_no_preference() {
        assert_eq!(ColorScheme::default(), ColorScheme::NoPreference);
    }
}
