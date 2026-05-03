#!/usr/bin/env bash
# Build the codex-desktop Flatpak bundle from the in-tree manifest.
#
# Outputs land under codex-rs/target/flatpak/{state,repo,app}. On success
# the bundle is installed into the calling user's Flatpak installation
# and can be launched with: flatpak run dev.codex.Desktop
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../../.." && pwd)"
MANIFEST="$REPO_ROOT/codex-rs/desktop/packaging/flatpak/dev.codex.Desktop.yml"
BUILD_DIR="$REPO_ROOT/codex-rs/target/flatpak"
STATE_DIR="$BUILD_DIR/state"
REPO_DIR="$BUILD_DIR/repo"
APP_DIR="$BUILD_DIR/app"

command -v flatpak-builder >/dev/null 2>&1 || {
    echo "flatpak-builder not found - install with: sudo apt install flatpak-builder" >&2
    exit 127
}

echo "==> Installing required Flatpak runtimes..."
flatpak install --user -y --noninteractive flathub \
    org.gnome.Platform//46 \
    org.gnome.Sdk//46 \
    org.freedesktop.Sdk.Extension.rust-stable//24.08 \
    || echo "(install steps may have failed; continuing if runtimes are already present)"

mkdir -p "$BUILD_DIR"
echo "==> Running flatpak-builder..."
flatpak-builder \
    --force-clean \
    --user --install --install-deps-from=flathub \
    --state-dir "$STATE_DIR" \
    --repo "$REPO_DIR" \
    "$APP_DIR" \
    "$MANIFEST"

echo "==> Done. Run with: flatpak run dev.codex.Desktop"
