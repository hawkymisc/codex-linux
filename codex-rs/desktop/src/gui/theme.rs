#![cfg(feature = "gtk")]

//! Theming: load the bundled `codex.css` and follow GNOME's color scheme.
//!
//! Live updates from the xdg-desktop-portal Settings interface are fed in
//! via [`apply_theme`], which is invoked from the GTK main thread by the
//! drain loop in [`crate::portal::install`].

use gtk::gdk;

/// Inline CSS for codex-specific tweaks. Kept tiny on purpose — most
/// styling comes from libadwaita.
const CODEX_CSS: &str = "
.codex-msg-user {
    padding: 12px;
    margin: 8px;
    border-radius: 8px;
    background-color: alpha(@accent_bg_color, 0.15);
    color: @window_fg_color;
}
.codex-msg-assistant {
    padding: 12px;
    margin: 8px;
    border-radius: 8px;
    background-color: alpha(@card_bg_color, 0.6);
    color: @window_fg_color;
}
.codex-msg-system {
    padding: 12px;
    margin: 8px;
    border-radius: 8px;
    color: alpha(@window_fg_color, 0.65);
    font-style: italic;
}
.codex-msg-streaming {
    opacity: 0.85;
}
.codex-msg-role {
    font-size: 0.85em;
    opacity: 0.7;
    margin-bottom: 2px;
}
.codex-streaming-tail {
    opacity: 0.6;
}
.codex-chat-composer {
    border-top: 1px solid alpha(@borders, 0.6);
    padding: 6px;
}
";

/// Install the global CSS provider and tell `AdwStyleManager` to follow
/// the system color scheme. Idempotent — safe to call from each
/// `connect_activate` callback (`gdk::Display::default()` becomes
/// available only after activation).
pub fn install() {
    if let Some(display) = gdk::Display::default() {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(CODEX_CSS);
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }

    // Follow the system light/dark preference.
    let style_manager = adw::StyleManager::default();
    style_manager.set_color_scheme(adw::ColorScheme::Default);
}

/// Apply a [`crate::portal::ThemeUpdate`] to the running app.
///
/// **GTK main thread only.** Mutates the global [`adw::StyleManager`]
/// and (when an accent is provided) installs a CSS provider on the
/// default `gdk::Display`. Calling this from a tokio worker is unsound
/// — drain the channel from `glib::MainContext::default().spawn_local`.
pub fn apply_theme(update: &crate::portal::ThemeUpdate) {
    let manager = adw::StyleManager::default();
    let scheme = match update.color_scheme {
        crate::portal::ColorScheme::Dark => adw::ColorScheme::ForceDark,
        crate::portal::ColorScheme::Light => adw::ColorScheme::ForceLight,
        crate::portal::ColorScheme::NoPreference => adw::ColorScheme::Default,
    };
    manager.set_color_scheme(scheme);

    // Accent color: stored as a CSS variable override via a CssProvider
    // attached to the default display. The portal returns RGB in
    // floating-point [0.0, 1.0]; libadwaita's CSS expects 0–255 ints.
    if let Some((r, g, b)) = update.accent {
        let css = format!(
            "@define-color codex_accent rgb({r},{g},{b});\n",
            r = (r * 255.0).round() as u8,
            g = (g * 255.0).round() as u8,
            b = (b * 255.0).round() as u8,
        );
        let provider = gtk::CssProvider::new();
        provider.load_from_string(&css);
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_USER,
            );
        }
    }
}
