#!/usr/bin/env bash
set -euo pipefail

# Build the codex-desktop AppImage.
#
# Requires the following on the build host:
#   - cargo (we'll invoke `cargo build --release`)
#   - libgtk-4-dev / libadwaita-1-dev / libgtksourceview-5-dev (link-time)
#   - libfuse2 for FUSE-mounted AppImage execution (`apt install libfuse2t64`).
#     If libfuse2 is unavailable we fall back to `--appimage-extract-and-run`
#     which works in sandboxed CI without FUSE.
#   - linuxdeploy + linuxdeploy-plugin-gtk: downloaded on demand into
#     ./target/appimage-tools/ if missing.
#
# Output:
#   target/appimage/Codex-Desktop-x86_64.AppImage

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
DESKTOP_CRATE_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT_DIR="$REPO_ROOT/codex-rs/target/appimage"
APPDIR="$OUT_DIR/AppDir"
TOOLS_DIR="$REPO_ROOT/codex-rs/target/appimage-tools"
mkdir -p "$OUT_DIR" "$TOOLS_DIR"

# ---------------------------------------------------------------------------
# Sandboxed CI / no-FUSE detection.
#
# linuxdeploy and linuxdeploy-plugin-gtk ship as AppImages that need libfuse2
# to mount themselves at runtime. When libfuse2 is missing we transparently
# set APPIMAGE_EXTRACT_AND_RUN=1 so the AppImages self-extract into a temp
# dir and exec their AppRun directly.
# ---------------------------------------------------------------------------
if ! ldconfig -p 2>/dev/null | grep -q 'libfuse\.so\.2'; then
  echo "==> libfuse2 not found on host; using APPIMAGE_EXTRACT_AND_RUN=1"
  export APPIMAGE_EXTRACT_AND_RUN=1
fi

# ---------------------------------------------------------------------------
# 1. Build release binary
# ---------------------------------------------------------------------------
echo "==> Building codex-desktop release binary..."
(
  cd "$REPO_ROOT/codex-rs"
  cargo build --release -p codex-desktop --features gtk
)

BIN="$REPO_ROOT/codex-rs/target/release/codex-desktop"
if [ ! -x "$BIN" ]; then
  echo "ERROR: expected release binary at $BIN" >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# 2. Stage the AppDir layout
# ---------------------------------------------------------------------------
echo "==> Staging AppDir at $APPDIR..."
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" \
         "$APPDIR/usr/share/applications" \
         "$APPDIR/usr/share/metainfo" \
         "$APPDIR/usr/share/icons/hicolor/scalable/apps" \
         "$APPDIR/usr/libexec/codex-desktop"

install -m 755 "$BIN" \
               "$APPDIR/usr/bin/codex-desktop"
install -m 644 "$DESKTOP_CRATE_ROOT/packaging/dev.codex.Desktop.desktop" \
               "$APPDIR/usr/share/applications/dev.codex.Desktop.desktop"
install -m 644 "$DESKTOP_CRATE_ROOT/packaging/dev.codex.Desktop.metainfo.xml" \
               "$APPDIR/usr/share/metainfo/dev.codex.Desktop.metainfo.xml"
install -m 644 "$DESKTOP_CRATE_ROOT/packaging/icons/hicolor/scalable/apps/dev.codex.Desktop.svg" \
               "$APPDIR/usr/share/icons/hicolor/scalable/apps/dev.codex.Desktop.svg"

# Top-level copies — linuxdeploy reads these to populate the AppImage metadata.
install -m 644 "$DESKTOP_CRATE_ROOT/packaging/dev.codex.Desktop.desktop" \
               "$APPDIR/dev.codex.Desktop.desktop"
install -m 644 "$DESKTOP_CRATE_ROOT/packaging/icons/hicolor/scalable/apps/dev.codex.Desktop.svg" \
               "$APPDIR/dev.codex.Desktop.svg"

# arg0 shim shells (mirror the .deb layout under usr/libexec/codex-desktop).
install -m 755 "$DESKTOP_CRATE_ROOT/packaging/symlinks/codex-agent" \
               "$APPDIR/usr/libexec/codex-desktop/codex-agent"
install -m 755 "$DESKTOP_CRATE_ROOT/packaging/symlinks/codex-lspd" \
               "$APPDIR/usr/libexec/codex-desktop/codex-lspd"

# AppRun: argv[0]-preserving entrypoint, installed pre-linuxdeploy too — some
# linuxdeploy versions skip writing AppRun if one already exists.
install -m 755 "$DESKTOP_CRATE_ROOT/packaging/AppRun" "$APPDIR/AppRun"

# ---------------------------------------------------------------------------
# 3. Fetch linuxdeploy + plugin if missing
# ---------------------------------------------------------------------------
LINUXDEPLOY="$TOOLS_DIR/linuxdeploy-x86_64.AppImage"
LINUXDEPLOY_GTK="$TOOLS_DIR/linuxdeploy-plugin-gtk.sh"
if [ ! -x "$LINUXDEPLOY" ]; then
  echo "==> Downloading linuxdeploy..."
  curl -fL -o "$LINUXDEPLOY" \
    "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage"
  chmod +x "$LINUXDEPLOY"
fi
if [ ! -x "$LINUXDEPLOY_GTK" ]; then
  echo "==> Downloading linuxdeploy-plugin-gtk..."
  curl -fL -o "$LINUXDEPLOY_GTK" \
    "https://raw.githubusercontent.com/linuxdeploy/linuxdeploy-plugin-gtk/master/linuxdeploy-plugin-gtk.sh"
  chmod +x "$LINUXDEPLOY_GTK"
fi

# ---------------------------------------------------------------------------
# 4. Run linuxdeploy in two phases so we can fix up the GTK plugin's AppRun
#    before the AppImage is sealed.
#
# Phase A: linuxdeploy --plugin gtk (no --output) bundles GTK libs and writes
# its own AppRun that sources `apprun-hooks/linuxdeploy-plugin-gtk.sh` (sets
# GTK_PATH / GIO_MODULE_DIR / fontconfig / gdk-pixbuf loaders) and then execs
# our original AppRun, renamed to `AppRun.wrapped`.
#
# Phase B: We patch the plugin's outer AppRun so its trailing
#   exec "$this_dir"/AppRun.wrapped "$@"
# becomes
#   exec -a "$(basename "$0")" "$this_dir"/AppRun.wrapped "$@"
# which is what makes argv[0] arrive at our AppRun.wrapped intact (so symlinks
# named codex-agent / codex-lspd dispatch to the right role).
#
# Phase C: linuxdeploy --output appimage seals the AppDir into the .AppImage.
# ---------------------------------------------------------------------------
export PATH="$TOOLS_DIR:$PATH"
OUTPUT_APPIMAGE="$OUT_DIR/Codex-Desktop-x86_64.AppImage"

echo "==> Phase A: linuxdeploy --plugin gtk (bundle libs, write wrapped AppRun)..."
(
  cd "$OUT_DIR"
  ARCH=x86_64 \
  "$LINUXDEPLOY" --appdir "$APPDIR" --plugin gtk
)

PLUGIN_APPRUN="$APPDIR/AppRun"
if [ -f "$APPDIR/AppRun.wrapped" ] && [ -f "$PLUGIN_APPRUN" ]; then
  echo "==> Phase B: patching linuxdeploy-plugin-gtk AppRun to preserve argv[0]..."
  sed -i \
    -e 's|^exec "$this_dir"/AppRun\.wrapped "$@"$|exec -a "$(basename "$0")" "$this_dir"/AppRun.wrapped "$@"|' \
    "$PLUGIN_APPRUN"
fi

echo "==> Phase C: linuxdeploy --output appimage (seal the AppImage)..."
(
  cd "$OUT_DIR"
  ARCH=x86_64 \
  OUTPUT="$OUTPUT_APPIMAGE" \
  "$LINUXDEPLOY" --appdir "$APPDIR" --output appimage
)

ls -lh "$OUTPUT_APPIMAGE"
echo "==> AppImage ready: $OUTPUT_APPIMAGE"
