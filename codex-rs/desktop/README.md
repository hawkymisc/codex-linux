# codex-desktop

In-tree Codex Desktop wrapper for Linux. GTK4 + libadwaita + GtkSourceView 5.

## Status

| PR | Highlights |
| --- | --- |
| **PR-A** | Workspace scaffolding (`desktop` / `agent-backend` / `markdown-ast` / `jsonrpc-framing`), arg0 multiplex, plan committed to `docs/desktop-architecture.md`. |
| **PR-B** | Markdown walker, `codex-agent` role NDJSON server, `ProcessBackend`, agent-backend conformance suite (10 canonical scenarios). |
| **PR-C** | Agent-crash WAL: append-only CBOR + CRC-32C, fdatasync at TurnCompleted/ApprovalDecision, GC by retention + per-thread quota. |
| **PR-D** | GTK4 main window (AdwApplicationWindow + AdwOverlaySplitView + AdwTabView), virtualised `GtkColumnView` chat, `GtkSourceView` editor tabs, sidebar with lazy `gtk::DirectoryList`. `CodexBackend::from_async_pipe` over duplex. `md_to_widgets` renderer for the eight `MdBlock` variants. |
| **PR-E** | `AgentBridge` spawns the in-tree binary in `codex-agent` role via `Command::arg0("codex-agent")`, wires the Send button to a live submit/event round-trip, drains events into the chat pane via `glib::MainContext::spawn_local`. |
| **PR-F+G** | ChatPane row factory routes finalised assistant blocks through `codex_markdown_ast::parse_full → md_to_widgets`. AgentBridge gains a `WalSink` that records every UserOp / ServerNotification to `~/.local/state/codex-desktop/turns/<thread>/<turn>.wal` and renames `.wal → .wal.done` on TurnCompleted. |
| **PR-H** | `cargo deb -p codex-desktop --features gtk` produces a 770 KB `.deb` with `/usr/bin/codex-desktop`, `/usr/libexec/codex-desktop/codex-{agent,lspd}` shim scripts, `.desktop` entry, AppStream metainfo, hicolor SVG icon, postinst/postrm cache refreshers. |
| **PR-I** | `codex-lspd` role: NDJSON dispatch on stdio + LSP `Content-Length:` frame I/O. `lsp/start` validates language, returns `would_invoke`. Methods: initialize, lsp/start, lsp/textDocumentDid{Open,Change,Close}, shutdown. Unknown method → -32601, unknown language → -32602. |
| **PR-J** | `codex_markdown_ast::IncrementalParser::push(delta)` actually runs in O(Δ): `BlockCacheEntry { byte_range, block, fingerprint: xxh3_64, is_open }` cache, last-stable-prefix cut, conservative re-parse from earlier when the boundary flips. 50 KB stream parses in 87 ms release / 181 ms debug. TUI snapshot tests (131+300+98) stay byte-identical. |
| **PR-K** | `desktop/src/portal.rs` subscribes to `xdg-desktop-portal` `Settings.SettingChanged` for `org.freedesktop.appearance` color-scheme + accent-color, drives `adw::StyleManager::set_color_scheme` and a `@codex_accent` CSS provider. |
| **PR-L** | Ctrl+P file-picker / Ctrl+Shift+P command palette (`adw::Dialog`), `sourceview5::VimIMContext` swappable per `EditorPane`, `Toggle Vim Mode` / sidebar / chat-pane / quit commands routed through the palette. |
| **PR-M** | `codex-lspd` actually spawns the configured language server when `CODEX_LSPD_REAL_SPAWN=1` is set: full LSP `initialize` request with realistic params (processId, rootUri, capabilities subset, workspaceFolders, trace), waits up to 30 s for the matching reply, returns capabilities. Live-tested against real `rust-analyzer 1.93.0`. |
| **PR-Q** | New `codex-content-search` crate: ripgrep `grep-searcher` + `grep-regex` + `ignore` wrapped in a streaming `SearchSession` with `(case_sensitive, regex, whole_word, max_matches, max_per_file)` options. Honours `.gitignore` even outside a git repo via `WalkBuilder::require_git(false)`. |
| **PR-R** | ChatPane streaming path uses `IncrementalParser` per assistant block: bold / code / lists / tables grow in real time, not only on turn completion. `INC_PARSERS` thread-local keyed on `MessageBlock::as_ptr() as usize`; evicted on `finalise_assistant_block`. |
| **PR-O+S** | 38 MB AppImage via three-phase build (linuxdeploy `--plugin gtk` → patch wrapper → `appimagetool`). `--appimage-extract-and-run` works without FUSE. Status pill in `AdwHeaderBar` (Idle / Thinking… / Awaiting / Disconnected) driven by `BridgeEvent`. `AdwToast` "Codex agent disconnected" with a Reconnect action button. |
| **PR-T** | Flatpak manifest targeting `org.gnome.Platform//46` + `rust-stable//24.08` SDK extension, finish-args for portals + secrets + fcitx; `build-flatpak.sh` bootstrap. Vendored sources (flatpak-cargo-generator) deferred to PR-T2. |

See [`../../docs/desktop-architecture.md`](../../docs/desktop-architecture.md) for the full architecture and the remaining roadmap (real LSP didChange / publishDiagnostics forwarding, in-process Codex via codex-app-server-client, gettext, a11y CI gate).

## Build

```bash
# Headless / CI (no GUI deps required):
cargo build -p codex-desktop

# Full GUI (Ubuntu 24.04+):
sudo apt install libgtk-4-dev libadwaita-1-dev libgtksourceview-5-dev
cargo build -p codex-desktop --features gtk --release
ls -lh target/release/codex-desktop                # ~1.5 MB stripped

# Drop into ~/.local/bin so `codex app` finds it:
install -Dm755 target/release/codex-desktop ~/.local/bin/codex-desktop
codex app                                          # opens the workspace
```

## argv[0] multiplex

One ELF, three roles selected by `argv[0]` basename:

| basename | role |
|---|---|
| `codex-desktop` | GUI (default) |
| `codex-agent`   | NDJSON JSON-RPC agent over stdio |
| `codex-lspd`    | LSP/lint multiplexer (stub) |

Override for tests / development: `CODEX_DESKTOP_FORCE_ROLE=desktop|agent|lspd`.

## Driving the agent role manually

```bash
$ CODEX_DESKTOP_FORCE_ROLE=agent codex-desktop <<'EOF'
{"jsonrpc":"2.0","id":"1","method":"initialize","params":{"client_info":{"name":"smoke","version":"0"}}}
{"jsonrpc":"2.0","id":"2","method":"submit","params":{"payload":{"text":"hello"}}}
{"jsonrpc":"2.0","id":"3","method":"shutdown"}
EOF
{"id":"1","jsonrpc":"2.0","result":{"protocol_version":"0.0.0-pr-b","server_info":{"name":"codex-agent-stub","version":"0.1.0"},…}}
{"id":"2","jsonrpc":"2.0","result":{"accepted":true}}
{"jsonrpc":"2.0","method":"agent/message_delta","params":{"delta":"hello"}}
{"jsonrpc":"2.0","method":"agent/turn_completed","params":{"stop_reason":"end_turn"}}
{"id":"3","jsonrpc":"2.0","result":{"note":"goodbye","ok":true}}
```

## Features

| flag | effect |
|---|---|
| `gtk` *(off by default)* | Pulls in `gtk4 0.9`, `libadwaita 0.7`, `sourceview5 0.9` and compiles the GUI. Distribution packages (`.deb`, AppImage, Flatpak) build with this. CI lanes without `libgtk-4-dev` / `libadwaita-1-dev` build with the default feature set and exercise non-GUI logic only. |

## Testing

```bash
cargo test  -p codex-desktop --features gtk --lib
cargo test  -p codex-desktop --features gtk --test agent_bridge_smoke
cargo clippy -p codex-desktop --features gtk --tests
```

The lib tests use a `OnceLock<bool>` guard that skips widget-construction tests when `gtk::init()` fails on a headless runner (no `DISPLAY`/`WAYLAND_DISPLAY`).

## Distribution

A real Debian package can be produced with [`cargo-deb`](https://github.com/kornelski/cargo-deb):

```bash
cargo install --locked cargo-deb            # one-time
cd codex-rs
cargo deb -p codex-desktop --features gtk
ls -lh target/debian/codex-desktop_*_amd64.deb
```

The resulting `.deb` lands at `target/debian/codex-desktop_<version>_amd64.deb`
and ships the following file layout (`dpkg-deb --contents`):

```
./
./usr/
./usr/bin/
./usr/bin/codex-desktop
./usr/libexec/
./usr/libexec/codex-desktop/
./usr/libexec/codex-desktop/codex-agent
./usr/libexec/codex-desktop/codex-lspd
./usr/share/
./usr/share/applications/
./usr/share/applications/dev.codex.Desktop.desktop
./usr/share/doc/
./usr/share/doc/codex-desktop/
./usr/share/doc/codex-desktop/README.md
./usr/share/doc/codex-desktop/architecture.md
./usr/share/doc/codex-desktop/changelog.gz
./usr/share/doc/codex-desktop/copyright
./usr/share/icons/
./usr/share/icons/hicolor/
./usr/share/icons/hicolor/scalable/
./usr/share/icons/hicolor/scalable/apps/
./usr/share/icons/hicolor/scalable/apps/dev.codex.Desktop.svg
./usr/share/metainfo/
./usr/share/metainfo/dev.codex.Desktop.metainfo.xml
```

`/usr/libexec/codex-desktop/codex-agent` and `…/codex-lspd` are tiny shell
shims that `exec -a <basename> /usr/bin/codex-desktop "$@"`, preserving the
argv[0] multiplex when the package is installed to a system that doesn't
allow same-binary symlinks across deb extraction.

The `postinst` / `postrm` maintainer scripts refresh the GTK icon cache and
the `update-desktop-database` mime cache when those tools are present.

### AppImage

A portable AppImage that bundles GTK4 + libadwaita + GtkSourceView via
`linuxdeploy-plugin-gtk`. The output runs on Ubuntu 22.04+ without
`libgtk-4-dev` / `libadwaita-1-dev` / `libgtksourceview-5-dev` installed.

```bash
sudo apt install libfuse2t64                           # for runtime FUSE
codex-rs/desktop/packaging/build-appimage.sh
ls -lh codex-rs/target/appimage/Codex-Desktop-*.AppImage
chmod +x codex-rs/target/appimage/Codex-Desktop-x86_64.AppImage
./codex-rs/target/appimage/Codex-Desktop-x86_64.AppImage   # opens the GUI
```

The script:

1. Builds `cargo build --release -p codex-desktop --features gtk`.
2. Stages an `AppDir` at `codex-rs/target/appimage/AppDir/` with the
   release binary at `usr/bin/codex-desktop`, the `.desktop` and SVG icon
   under `usr/share/{applications,icons,metainfo}/`, the arg0 shim shells
   under `usr/libexec/codex-desktop/`, and top-level `dev.codex.Desktop.{desktop,svg}`
   plus the bash `AppRun` entrypoint.
3. Downloads `linuxdeploy-x86_64.AppImage`, `linuxdeploy-plugin-gtk.sh`,
   and `appimagetool-x86_64.AppImage` (all cached under
   `codex-rs/target/appimage-tools/`) on first run.
4. Builds the AppImage in three phases:

   - **Phase A** — `linuxdeploy --plugin gtk` bundles GTK4 / libadwaita /
     GtkSourceView / their transitive deps under `usr/lib/` with
     `rpath=$ORIGIN`, installs the GTK env-setup hook
     (`apprun-hooks/linuxdeploy-plugin-gtk.sh`), and renames our AppRun
     to `AppRun.wrapped` while writing its own outer AppRun that sources
     the hook and execs the wrapped one.
   - **Phase B** — patches the plugin's outer AppRun in place so the
     trailing `exec "$this_dir"/AppRun.wrapped "$@"` becomes
     `exec -a "$(basename "$0")" "$this_dir"/AppRun.wrapped "$@"`. That
     keeps the GTK env hook on the path *and* preserves argv[0] for the
     codex-desktop / codex-agent / codex-lspd multiplex.
   - **Phase C** — `appimagetool AppDir → Codex-Desktop-x86_64.AppImage`
     seals the bundle. We run appimagetool directly (rather than letting
     linuxdeploy do it) because every `linuxdeploy --output appimage` run
     re-executes the GTK plugin and overwrites the outer AppRun — which
     would clobber the Phase B patch.

The script auto-detects missing `libfuse2` and exports
`APPIMAGE_EXTRACT_AND_RUN=1` so the linuxdeploy / appimagetool /
gtk-plugin AppImages self-extract during the build — no FUSE required
on the build host. The produced AppImage may still need `libfuse2` to
mount itself at runtime on the *user's* machine; on hosts without FUSE
it can be invoked as
`./Codex-Desktop-x86_64.AppImage --appimage-extract-and-run`.

If neither `libfuse2` nor self-extraction work on a build host, run
only steps 1–2 of the script (which leave the AppDir staged on disk)
and finalise the bundle on a machine with `libfuse2`:

```bash
linuxdeploy --appdir codex-rs/target/appimage/AppDir --plugin gtk
# patch AppDir/AppRun's trailing exec to add `-a "$(basename "$0")"`
appimagetool codex-rs/target/appimage/AppDir \
             codex-rs/target/appimage/Codex-Desktop-x86_64.AppImage
```

### Flatpak

```bash
sudo apt install flatpak flatpak-builder
flatpak remote-add --user --if-not-exists flathub https://flathub.org/repo/flathub.flatpakrepo
bash codex-rs/desktop/packaging/flatpak/build-flatpak.sh
flatpak run dev.codex.Desktop
```

The manifest (`codex-rs/desktop/packaging/flatpak/dev.codex.Desktop.yml`)
targets the GNOME 46 runtime — the same GTK4 4.14 + libadwaita 1.5 +
GtkSourceView 5 stack that ships in Ubuntu 24.04 — and pulls the Rust
toolchain from `org.freedesktop.Sdk.Extension.rust-stable//24.08`. The
`build-flatpak.sh` helper installs the runtimes from Flathub, then
invokes `flatpak-builder --user --install` so the resulting bundle lands
in the calling user's installation.

Sandbox permissions (`finish-args`):

| Permission | Why |
|---|---|
| `--socket=wayland`, `--socket=fallback-x11`, `--device=dri` | GTK4 rendering. |
| `--share=network` | LLM API access. |
| `--share=ipc` | shared memory + GTK shm fallbacks. |
| `--filesystem=home` | edit user files; tighten with portals once ashpd file-chooser lands. |
| `--talk-name=org.freedesktop.portal.*` | xdg-desktop-portal (file-chooser, secrets, notifications). |
| `--talk-name=org.freedesktop.secrets` | libsecret keyring for API tokens. |
| `--socket=fcitx`, `--env=GTK_IM_MODULE=fcitx` | IME passthrough for CJK/IME users. |
| `--env=PATH=/app/bin:/usr/bin` | so the `codex-agent` arg0 multiplex shim resolves inside the bubble. |

**Caveats / v2 work**

- The first-cut manifest does **not** vendor cargo registry sources, so
  `flatpak-builder` must be invoked with network access during the
  build (the default on most flatpak-builder versions). Production
  builds should run `flatpak-cargo-generator.py` against
  `codex-rs/Cargo.lock` and switch the `cargo build` invocation back to
  `--offline`.
- The `codex-linux-sandbox` inner sandbox is best-effort inside the
  Flatpak bubblewrap layer — nested user-namespaces work on most modern
  kernels but are not guaranteed. Security-conscious users should
  prefer the `.deb` channel where the inner sandbox is unconstrained.
