# codex-desktop

In-tree Codex Desktop wrapper for Linux. GTK4 + libadwaita + GtkSourceView 5.

## Status

| PR | Highlights |
| --- | --- |
| **PR-A** | Workspace scaffolding (`desktop` / `agent-backend` / `markdown-ast` / `jsonrpc-framing`), arg0 multiplex, plan committed to `docs/desktop-architecture.md`. |
| **PR-B** | Markdown walker, `codex-agent` role NDJSON server, `ProcessBackend`, agent-backend conformance suite (10 canonical scenarios). |
| **PR-C** | Agent-crash WAL: append-only CBOR + CRC-32C, fdatasync at TurnCompleted/ApprovalDecision, GC by retention + per-thread quota. |
| **PR-D** | GTK4 main window (AdwApplicationWindow + AdwOverlaySplitView + AdwTabView), virtualised `GtkColumnView` chat, `GtkSourceView` editor tabs, sidebar with lazy `gtk::DirectoryList`. `CodexBackend::from_async_pipe` over duplex. `md_to_widgets` renderer for the eight `MdBlock` variants. |
| **PR-E** | `AgentBridge` spawns the in-tree binary in `codex-agent` role via `Command::arg0("codex-agent")`, wires the Send button to a live submit/event round-trip, drains events into the chat pane via `glib::MainContext::spawn_local`. Smoke test: `submit "hello bridge" → agent/message_delta + agent/turn_completed` round-trips end-to-end. |

See [`../../docs/desktop-architecture.md`](../../docs/desktop-architecture.md) for the full architecture and the rollout for PR-F+ (markdown rendering in chat, lspd, packaging, ashpd portals, vim mode, command palette, …).

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
