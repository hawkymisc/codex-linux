# Codex Linux Desktop — Architecture Plan

This document captures the architecture for `codex-desktop`, the in-tree
Linux-native Agentic IDE that wraps the Codex CLI agent (and, optionally,
Claude Code) behind a libadwaita GUI. It is the convergent finalist plan
from a 10-iteration design synthesis.

> Status: **PR-A through PR-U + PR-S2 + PR-W landed**. The IDE skeleton is now usable:
>
> * `codex-desktop` (4.2 MB release binary, 1.4 MB headless) launches an
>   AdwApplicationWindow with sidebar + GtkSourceView editor tabs +
>   virtualised GtkColumnView chat + AdwHeaderBar status pill.
> * `codex-agent` role: NDJSON JSON-RPC server speaking the protocol-
>   compatible subset (initialize / submit / interrupt / shutdown), with
>   streaming `agent/message_delta` + `agent/turn_completed`.
> * `codex-lspd` role: NDJSON dispatch + LSP `Content-Length:` framing
>   + opt-in real `rust-analyzer` spawn + initialize round-trip.
>   PR-U keeps spawned servers alive (registered in `Supervisor` by
>   `server_id`), forwards `lsp/textDocumentDid{Open,Change,Close}`
>   to the LSP server as real `textDocument/did{Open,Change,Close}`
>   notifications, and pumps `textDocument/publishDiagnostics` back
>   to the parent NDJSON channel with `server_id` tagged in params.
> * `AgentBridge` spawns the in-tree binary as a `codex-agent` child
>   via `arg0`-multiplex, drives Send → submit → streaming reply →
>   markdown rendering via `IncrementalParser`. Status pill flips
>   Idle / Thinking / Disconnected from the bridge events. PR-S2
>   adds `AgentBridge::restart()`: the disconnect-toast Reconnect
>   button respawns the agent child and reuses the same event
>   channel so existing GUI subscribers continue without re-subscribe.
> * Crash-recovery WAL: append-only CBOR + CRC-32C, fdatasync at
>   TurnCompleted / ApprovalDecision boundaries.
> * Protocol drift log (`codex-drift-log`): every notification whose
>   `method` is outside `CodexBackend`'s `KnownVariantRegistry`
>   (defaults to the agent role's `agent/message_delta` +
>   `agent/turn_completed`) gets appended to
>   `~/.local/state/codex-desktop/drift-log.jsonl` with per-method
>   counters surfaced via `AgentBridge::drift_log()`. Backs the
>   off-by-default Protocol Drift diagnostic pane (§3.3).
> * Distribution: 770 KB `.deb` (cargo-deb), 38 MB AppImage
>   (linuxdeploy + linuxdeploy-plugin-gtk + appimagetool), Flatpak
>   manifest targeting `org.gnome.Platform//46`.
> * Theme follow via `xdg-desktop-portal` (`org.freedesktop.appearance`)
>   color-scheme + accent-color.
> * Command palette (Ctrl+P / Ctrl+Shift+P): file picker + named
>   commands incl. Toggle Vim Mode.
> * Vim mode: `sourceview5::VimIMContext` swappable per editor tab.
> * Streaming markdown rendering: `IncrementalParser::push(delta)` runs
>   in O(Δ); chat widgets refresh through `md_to_widgets::block_to_
>   widget` so users see bold/code/lists growing in real time.
>
> Quality: ~200 tests green (PR-U +6, PR-S2 +1 integration, PR-W +10
> across drift-log/agent-backend/agent-bridge), clippy zero warnings,
> real `rust-analyzer 1.93.0` initialise round-trip green, AppImage
> runs out-of-the-box on Ubuntu 22.04+ with libgtk-4-1 / libadwaita-1-0
> / libgtksourceview-5-0.
> See "Phased delivery" below for the remaining roadmap (in-process Codex
> via codex-app-server-client, ClaudeBackend + full 10 conformance scenarios,
> gettext / Weblate i18n, a11y CI gate, Flatpak vendoring for Flathub, …).

## 1. Goals & non-goals

**Goals**

- Lightweight Agentic IDE for Linux: lighter than a VS Code fork, native
  feel on Ubuntu 24.04+ comparable to a SwiftUI app on macOS.
- Wraps **Codex** (via the existing `app-server` JSON-RPC) and **Claude
  Code** (via the Anthropic Agent SDK) behind a single `AgentBackend` trait.
- Built-in markdown viewer (no embedded webview by default).
- LSP / tree-sitter / linter-CLI extension surface only — no general
  plugin/extension API.
- Local-first, no telemetry. User-initiated update checks only.
- Distribution: `.deb`, AppImage, Flatpak (`org.gnome.Platform/46`).
  No Snap.

**Non-goals**

- General plugin/extension API.
- A second renderer (we ship one editor surface; a `cmlite` WebKit variant
  is the only escape hatch).
- Auto-update or background telemetry.
- Reimplementing `codex-cli` or `app-server`.

## 2. Stack

| Layer | Choice |
|---|---|
| UI toolkit | GTK4 4.14+ + libadwaita 1.4+ (1.5 features feature-detected) |
| Editor (default) | GtkSourceView 5 |
| Editor (escape) | WebKitGTK 6 + CodeMirror 6 (separate `codex-desktop-cmlite` package) |
| Agent backend (default) | Codex via `codex-app-server-client` |
| Agent backend (alt) | Claude Code via Anthropic Agent SDK adapter |
| Markdown | `pulldown-cmark` → `gtk::Widget` tree (no webview) |
| LSP | `lsp-types` + custom client over stdio |
| Linters | external CLIs (ruff, clippy, eslint, shellcheck) |
| Theme detection | `ashpd` Settings portal (`org.freedesktop.appearance`) |
| Distribution | `.deb` / AppImage / Flatpak |

The trunk decision (GTK4) was made because:

- "Native on Ubuntu 24.04" maps to "native on GNOME 46" maps to libadwaita.
- AT-SPI accessibility, IBus / fcitx5 IME support, and Adwaita HIG come
  free from GTK4 — none of these are achievable in a single quarter on a
  custom-renderer toolkit.
- Flatpak runtime `org.gnome.Platform/46` ships GTK4 and libadwaita on the
  host side, keeping the user-side delta tiny (~12 MB).

## 3. Architecture

### 3.1 Single binary, three roles via arg0 multiplex

`codex-desktop` is one ELF. Its role is selected by the basename of
`argv[0]`:

| basename | entry point |
|---|---|
| `codex-desktop` | desktop GUI main loop |
| `codex-agent` | Codex/Claude agent host |
| `codex-lspd` | LSP / lint multiplexer |

Layout on disk (after install):

```
/usr/bin/codex-desktop                          (real ELF)
/usr/libexec/codex-desktop/codex-agent          → ../../bin/codex-desktop
/usr/libexec/codex-desktop/codex-lspd           → ../../bin/codex-desktop
```

The desktop process spawns children via absolute path, setting argv[0]
explicitly with `Command::arg0`. This re-uses the existing
`codex-arg0` pattern in the workspace.

### 3.2 stdio JSON-RPC framing

Two framings live in `codex-jsonrpc-framing`:

- **NDJSON** — newline-delimited JSON-RPC 2.0. Matches the existing
  `app-server` stdio transport.
- **LSP-style** — `Content-Length: <N>\r\n\r\n<UTF-8 body>`, useful for
  embedded multi-line JSON / future tool-stream payloads.

Both are exposed as `FramedReader`/`FramedWriter` adapters on top of any
`AsyncRead`/`AsyncWrite`. The desktop binary uses NDJSON for backward
compatibility with the existing transport layer.

### 3.3 Agent backend abstraction

```rust
#[async_trait]
pub trait AgentBackend: Send + Sync + 'static {
    async fn initialize(&mut self, p: InitializeParams) -> Result<InitializeResponse>;
    async fn submit(&self, sub: Submission) -> Result<()>;
    async fn interrupt(&self, turn_id: TurnId) -> Result<()>;
    async fn shutdown(self: Box<Self>) -> Result<()>;
    fn events(&self) -> BoxStream<'static, IncomingServerNotification>;
    fn capabilities(&self) -> &BackendCapabilities;
    fn extras(&self) -> Option<&dyn AgentBackendExtras> { None }
}
```

`IncomingServerNotification` is an `#[serde(untagged)]` envelope of
`Known(ServerNotification)` and `Unknown { method, params }`. Unknown
variants survive round-trips and are surfaced in the (off-by-default)
"Protocol Drift" diagnostic pane, never silently dropped.

Backend pluggability is **static only**: third-party backends register
via cargo features and `inventory::submit!`. No `dlopen`-loaded backends.

### 3.4 Crash isolation, agent restart, WAL

The agent runs as a separate child process. UI maintains an append-only
write-ahead log per turn at
`~/.local/state/codex-desktop/turns/<thread_id>/<turn_id>.wal` (CBOR +
CRC32C, fsync at `TurnCompleted` and `ApprovalDecision` boundaries).
On agent crash mid-turn, UI replays the WAL, reissues `Initialize` +
`ResumeThread{thread_id, last_event_id}`, and unlocks the composer.

## 4. Markdown & streaming pipeline

The `codex-markdown-ast` crate factors the existing
`tui/src/markdown_render.rs` AST walk into a backend-neutral
`MdBlock` enum:

```rust
pub enum MdBlock {
    Heading { level: u8, inlines: Vec<Inline> },
    Paragraph(Vec<Inline>),
    Code { lang: Option<String>, text: String },
    List { ordered: bool, items: Vec<Vec<MdBlock>> },
    BlockQuote(Vec<MdBlock>),
    Table { headers: Vec<Vec<Inline>>, rows: Vec<Vec<Vec<Inline>>> },
    HtmlBlock(String),
    ThematicBreak,
}
```

`parse_incremental(src, prior)` exploits `pulldown-cmark`'s
`into_offset_iter()` byte ranges to reparse only the open tail of a
streaming agent reply, so per-delta cost is amortized O(Δ) not O(N).

The chat surface is virtualized as `GtkColumnView<MessageBlock>`. We
never mutate a `GtkTextBuffer` character-by-character; the streaming
controller appends to a per-block `raw_source` and swaps a single row
on each newline boundary.

## 5. Editor depth

GtkSourceView 5 by default, plus closes for known feature gaps:

- Structural multi-cursor: `Vec<GtkTextMark>` shadow + custom snapshot
  overlay; all edits wrapped in `MultiCursorTxn` with one undo group.
- Code folding with gutter sync: tree-sitter `folds.scm` + LSP
  `foldingRange`; `GtkTextTag(invisible=true)` hides folded text.
- Sticky scroll: `GtkOverlay` + second read-only `GtkSourceView` driven
  by tree-sitter outer-scope queries.
- Find/Replace: non-modal Adwaita-styled toolbar; "Find in Files" via a
  new `codex-rs/content-search` crate wrapping ripgrep.
- Semantic-highlight refresh: double-buffered tag tables (no flicker).
- Inlay hints: `GtkTextChildAnchor` + line-height-locked labels.
- Vim mode: `GtkSourceVimIMContext` (built-in); Helix mode via a
  custom IM context.

Out of scope for the default surface (becomes `cmlite` territory):
structural search/replace virtualized preview, CRDT collab, remote-dev,
>200 MB log editing.

## 6. Native feel checklist

- Adwaita HIG: AdwApplicationWindow / AdwOverlaySplitView / AdwTabView /
  AdwHeaderBar / AdwToastOverlay / AdwAlertDialog (1.5; 1.4 fallback to
  GtkMessageDialog).
- Theme follow: `ashpd` `org.freedesktop.appearance` portal subscription
  for color-scheme, accent-color, monospace-font-name, text-scaling.
  Fallback to gsettings polling on Ubuntu 22.04 (AppImage glibc baseline).
- KDE Breeze override: detected via `XDG_CURRENT_DESKTOP=KDE`; swaps
  token table (4 px grid, 4 px radii, Noto Sans 10).
- IME: GTK4 IM-multicontext (IBus/fcitx5 auto-detected). Flatpak manifest
  exposes `--socket=fcitx`.
- High-contrast / reduced-motion: portal-driven, swappable token tables.
- a11y: AT-SPI 2 (free from GTK4); release-gate Orca matrix in CI.
- i18n: `gettext-rs` + Weblate; 15 target locales day-one.

## 7. Sandbox & security tiers

| Tier | Environment | Strategy |
|---|---|---|
| 0 | `.deb` / AppImage | bwrap + landlock + seccomp (full) |
| 1 | Flatpak + `org.freedesktop.Flatpak.Development` portal | `HostCommand` for host-side `codex-linux-sandbox` exec |
| 2 | Flatpak + landlock ABI ≥ 4 | in-process landlock layering + additive seccomp |
| 3 | Flatpak + landlock ABI < 4 | red banner; `.deb` recommended |
| 4 | Snap | refuses to start (exit 78) |

Updates are user-initiated only and verified against an Ed25519-signed
`releases.json`. The CLI's `--download-url` flag remains as
`--download-url-unsafe` with a warning.

## 8. Phased delivery

### MVP — 8 weeks

- W1: arg0 split, `desktop` crate skeleton, AdwApplicationWindow,
  agent-backend trait scaffold, `jsonrpc-framing` extracted.
- W2: Markdown-AST PR merged with TUI snapshot parity.
- W3: Codex backend wired end-to-end; one-turn streaming chat works.
- W4: GtkSourceView editor surface; file open/save; undo groups.
- W5: `codex-lspd` + rust-analyzer; diagnostics gutter + hover.
- W6: ashpd Settings portal; theme follow; KDE override.
- W7: WAL writer + replay; agent supervisor; conformance scenarios 1-6.
- W8: cargo-deb + AppImage CI; screenshot harness; a11y release-gate.
  **Tag v0.1.**

### v1 — +8 weeks

ClaudeBackend + full 10 conformance scenarios, command palette, structural
multi-cursor + find-in-files, code folding + sticky scroll, Flatpak
submission, rename/code-actions polish, cmlite WebKit variant, IME full
matrix green, RTL.

### v2 — +12 weeks

Inline ghost-text, workspace settings UI, integrated linter CLIs, voice
input (whisper.cpp), AT-SPI editor caret tracking, accent-color portal
(GNOME 47+).

## 9. Falsifiable commitments

| Metric | Value | Hardware |
|---|---|---|
| Cold start (binary→first frame) | ≤350 ms p50, ≤600 ms p99 | UHD 620 |
| 5k-line file open | ≤180 ms p50 | UHD 620 |
| RSS, 5k-line + 200 chat msgs | ≤220 MB | UHD 620 |
| Buffer overhead | ≤1.4 KB/line | — |
| Stream frame time | median ≤8 ms / p99 ≤12 ms / max ≤25 ms / 0 dropped | UHD 620, 250 KB md @ 60 tok/s |
| Stream RSS growth (60 s) | ≤30 MB | UHD 620 |
| Installed footprint (.deb, stripped+LTO) | ≤48 MB | — |
| AppImage size | ≤55 MB | x86_64 |
| 10 conformance scenarios | 100% pass for both backends | nightly |
| a11y matrix | 5/5 pass | weston + orca |
| IME matrix | 6/6 (ibus×4 + fcitx5×2) | xdotool harness |

CI gates a +5 % regression vs main on perf metrics; merge blocked unless
the commit subject contains `[bench-regression-ack: <reason>]`.

## 10. The 10 conformance scenarios

1. File edit round-trip (FileChange → approve → on-disk hash matches).
2. Bash exec with approval round-trip.
3. Plan turn (multi-step checklist; partial updates).
4. Web search refusal under `network_disabled=true`.
5. Approval round-trip (agent stalls → decision → resume <200 ms).
6. Interrupt mid-stream (Esc → `Op::Interrupt` → `TurnAborted` <500 ms).
7. Large markdown reply (250 KB at ≥55 fps on UHD 620).
8. Attachment upload (4 MB PNG drag-drop with content hash).
9. Network-disabled run (`strace` shows zero DNS calls).
10. Sandbox-violation recovery (workspace-out write denied → re-plan).

## 11. New crates introduced (PR-A scaffolding)

| Crate | Purpose |
|---|---|
| `codex-jsonrpc-framing` | NDJSON + LSP-style framing for stdio JSON-RPC |
| `codex-agent-backend` | `AgentBackend` trait + forward-compat envelope |
| `codex-markdown-ast` | Backend-neutral markdown AST + incremental parse |
| `codex-desktop` | Desktop binary skeleton with arg0 multiplex; GTK behind feature flag |

Subsequent PRs have shipped `codex-content-search` (PR-Q),
`codex-agent-backend-conformance` (PR-B), and `codex-drift-log` (PR-W);
`codex-lspd` lives inside `codex-desktop` as an arg0-multiplexed role
rather than a sibling crate (PR-I/M/U). Still outstanding:
`codex-claude-backend`, `codex-desktop-theme`, `codex-desktop-a11y`,
`codex-host-portal`.

## 12. Honest residual risks

- First-frame hitch on 250 KB cold parse (~25-30 ms) — not solved.
- Semantic protocol drift (same schema, changed meaning) — not detectable
  at the wire layer; behavioral fixtures are v2 work.
- Trusted-publisher crate compromise — `cargo vet` cannot catch it.
- Refreshable Braille displays — BRLTTY routes outside our test harness;
  "should work via AT-SPI" is honestly disclosed but unverified.
- GtkSourceView shadow-multi-cursor will feel second-class for one
  release cycle; cmlite escape mitigates the long tail.

## 13. References

This plan synthesizes:

- `codex-rs/app-server-client/src/lib.rs` — JSON-RPC client
- `codex-rs/app-server-protocol/src/protocol/v2.rs` — `ServerNotification`
- `codex-rs/arg0/src/lib.rs` — argv[0]-multiplex pattern
- `codex-rs/tui/src/markdown_render.rs` — AST walker to factor
- `codex-rs/tui/src/streaming/controller.rs` — source-retention model
- `codex-rs/cli/src/desktop_app/linux.rs` — launcher detection (already
  recognises in-tree `codex-desktop` binary on `$PATH`).
