#![cfg(feature = "gtk")]

//! Theming: load the bundled `codex.css` and follow GNOME's color scheme.

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
