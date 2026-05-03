# codex-desktop — Session Handoff Brief

This document is the entry point for any session resuming work on
`codex-desktop` (the in-tree Linux GTK4 IDE wrapping Codex / Claude
Code agents). It captures **what is done, what is blocked, what comes
next, and where to look first** so the next session can be productive
in its first 100 lines instead of its first 1000.

> Anchor commit at handoff: PR #1 closed at branch
> `claude/codex-cli-linux-DSrlt`. After that PR merges to `main`,
> branch off `main` for follow-up work; this doc lives there.

## 1. First-five-minutes orientation

Read these in order:

1. **[`docs/desktop-architecture.md`](desktop-architecture.md)** — full
   architecture plan (10-iteration synthesis). Status block at the top
   tracks which PRs (A through X) have landed. §8 "Phased delivery"
   enumerates remaining v1 / v2 work.
2. **[`codex-rs/desktop/README.md`](../codex-rs/desktop/README.md)** —
   per-PR status table with concrete file paths and key types. Use
   this to find code by feature.
3. **[`codex-rs/agent-backend/src/lib.rs`](../codex-rs/agent-backend/src/lib.rs)** —
   the central `AgentBackend` trait every backend implements. Read
   this before touching either backend.
4. **[`codex-rs/desktop/src/agent_bridge.rs`](../codex-rs/desktop/src/agent_bridge.rs)** —
   the GUI ↔ tokio bridge. The `supervisor()` function is the hot path.

## 2. Workspace layout (PR-A..PR-X)

```
codex-rs/
├── agent-backend/                  # AgentBackend trait + envelope + 2 impls
│   ├── src/
│   │   ├── lib.rs                  # trait, re-exports
│   │   ├── envelope.rs             # IncomingServerNotification + KnownVariantRegistry
│   │   ├── codex.rs                # CodexBackend (NDJSON → agent role)
│   │   ├── process.rs              # ProcessBackend (generic NDJSON adapter)
│   │   ├── registry.rs             # inventory-based backend descriptor
│   │   └── ...
├── agent-backend-conformance/      # 10 TOML scenarios + runner
├── claude-backend/                 # PR-X: ClaudeBackend skeleton
│   └── src/lib.rs                  # AgentBackend impl + inventory submission
├── content-search/                 # PR-Q: ripgrep-backed find-in-files
├── desktop/                        # the binary + GUI
│   ├── src/
│   │   ├── main.rs                 # role detection (argv[0] multiplex)
│   │   ├── role.rs                 # Role enum + detect_role
│   │   ├── run.rs                  # async entry per role
│   │   ├── agent_role.rs           # PR-B: codex-agent NDJSON server
│   │   ├── lspd_role.rs            # PR-I/M/U: codex-lspd LSP supervisor
│   │   ├── agent_bridge.rs         # PR-E/F+G/S2: GUI ↔ backend bridge
│   │   ├── wal.rs                  # PR-C: CBOR + CRC32C crash recovery
│   │   ├── md_to_widgets.rs        # PR-D: MdBlock → gtk::Widget
│   │   ├── portal.rs               # PR-K: xdg-desktop-portal client
│   │   └── gui/                    # GTK4 widgets (gated on `gtk` feature)
│   ├── tests/
│   │   ├── agent_bridge_smoke.rs   # PR-E: NDJSON round-trip via the binary
│   │   └── agent_bridge_restart.rs # PR-S2: restart() lifecycle test
│   └── packaging/                  # .deb, AppImage, Flatpak
├── drift-log/                      # PR-W: append-only JSONL drift log
├── jsonrpc-framing/                # NDJSON + LSP-style framing primitives
└── markdown-ast/                   # PR-B/J: incremental MdBlock parser
```

## 3. Cheatsheet — verification commands

```bash
# All headless tests for the codex-desktop stack (the canonical CI lane).
cd codex-rs
cargo test -p codex-desktop -p codex-agent-backend -p codex-claude-backend \
           -p codex-drift-log -p codex-markdown-ast -p codex-jsonrpc-framing \
           -p codex-content-search -p codex-agent-backend-conformance \
           --no-default-features --tests

# Clippy on the same surface (must stay 0 warnings).
cargo clippy -p codex-desktop -p codex-agent-backend -p codex-claude-backend \
             -p codex-drift-log -p codex-markdown-ast -p codex-jsonrpc-framing \
             -p codex-content-search -p codex-agent-backend-conformance \
             --no-default-features --tests --no-deps

# GTK lane (requires libgtk-4-dev + libadwaita-1-dev + libgtksourceview-5-dev).
cargo test -p codex-desktop --features gtk --lib
cargo test -p codex-desktop --features gtk --test agent_bridge_smoke
cargo clippy -p codex-desktop --features gtk --tests

# Real rust-analyzer LSP smoke (requires `rustup component add rust-analyzer`).
CODEX_LSPD_REAL_SPAWN_TEST=1 cargo test -p codex-desktop --no-default-features \
    --lib lsp_start_with_real_spawn_initializes_when_on_path
```

## 4. What's done (PR-A through PR-X)

See `codex-rs/desktop/README.md` for the per-PR matrix. High-level:

* **Foundation (A–G):** workspace scaffolding, agent role NDJSON
  server, GTK4 main window with virtualised chat, real Codex backend
  over NDJSON pipe, WAL persistence.
* **Distribution (H, O+S, T):** 770 KB `.deb`, 38 MB AppImage, Flatpak
  manifest targeting GNOME 46.
* **LSP & search (I, M, Q, U):** `codex-lspd` role with real
  `rust-analyzer` spawn, registered server lifecycle, didChange /
  publishDiagnostics forwarding, `codex-content-search` ripgrep wrap.
* **GUI polish (K, L, R):** xdg-desktop-portal theme/accent, Ctrl+P
  command palette, Vim mode, streaming markdown rendering.
* **Reliability (S2, W):** AgentBridge restart for the Reconnect
  toast, drift log infrastructure for unknown notifications.
* **Backends (X):** Claude Code adapter skeleton with inventory
  registration. NDJSON-over-stdio path is testable; real upstream
  transport pending.

## 5. Known constraints & blockers (env-side)

The primary working environment for the recent work was a sandboxed
container without the following:

| Missing | Affects | Workaround |
|---|---|---|
| `libgtk-4-dev` / `libadwaita-1-dev` / `libgtksourceview-5-dev` | `cargo build -p codex-desktop --features gtk`; all GUI tests | Use a GTK-enabled CI runner; locally headless lanes verify everything else |
| `libcap-dev` | In-process Codex backend (would pull `codex-core`) | Track separately; AgentBridge uses subprocess CodexBackend today |
| `flatpak-builder` | `cargo deb -p codex-desktop --features gtk` followed by Flathub flow | Document install steps; not needed for `.deb`/AppImage |
| `flatpak-cargo-generator.py` | PR-T2 (vendored Cargo sources for Flatpak/Flathub) | Vendor the script under `codex-rs/desktop/packaging/flatpak/` |

## 6. Outstanding PRs — design briefs for the next session

These are sized so each maps to one self-contained PR. Listed in
descending tractability for a headless environment.

### PR-Y: CodexBackend inventory registration (small)

**Goal:** Symmetric backend discovery — `inventory::iter::<BackendDescriptor>()`
should yield both `codex` and `claude-code`.

**Where:** Add a `CodexBackend::factory` that spawns the in-tree
`codex-agent` child (the same logic currently in
`agent_bridge::AgentBridge::spawn_with`). Expose via `inventory::submit!`
in either `codex-agent-backend` or a thin `codex-codex-backend` shim
crate.

**Tests:** mirror PR-X's `inventory_submission_includes_claude_code`
and `factory_*` tests.

**Why now:** unlocks a backend picker UI without churn.

### PR-Z: `--list-backends` CLI flag (small)

**Goal:** `codex-desktop --list-backends` prints
`<id>\t<display_name>\t<status>`. Status is `ready` if the factory
returns Ok in a probe call, `unconfigured` if it returns `Closed`.

**Where:** `codex-rs/desktop/src/main.rs` (clap `--list-backends` flag,
emit-and-exit before role dispatch).

**Tests:** integration test that spawns the binary with the flag,
asserts both `codex` and `claude-code` appear (after PR-Y).

### PR-V: in-process Codex via codex-app-server-client (large, blocked)

**Goal:** Replace the `codex-agent` subprocess with an in-process
`InProcessAppServerClient` (zero-copy, no IPC, sub-ms turn latency).

**Blocker:** Pulling `codex-app-server-client` transitively depends on
`codex-core`, which needs `libcap-dev` on the build host. CI runners
need that package.

**Where:** New `feature = "in-process"` flag on `codex-agent-backend`;
`CodexBackend::start_in_process(config)` constructor wraps
`InProcessAppServerClient`; wire selectable via `AgentCommand` analog
in `agent_bridge`.

### PR-T2: Flatpak vendored Cargo sources (medium)

**Goal:** Flathub submission unblocked — `cargo build` runs offline
inside the flatpak-builder bubble.

**Where:** Vendor `flatpak-cargo-generator.py` from upstream
flatpak-builder-tools, generate `cargo-sources.json` against
`codex-rs/Cargo.lock`, switch the manifest's `cargo build` to
`--offline`. Document in `codex-rs/desktop/packaging/flatpak/README.md`.

### PR conformance against ClaudeBackend (medium)

**Goal:** Run the existing 10 conformance scenarios
(`codex-rs/agent-backend-conformance/scenarios/`) against
`ClaudeBackend` and prove parity with `CodexBackend`.

**Caveat:** Scenarios use Codex-protocol method names
(`thread/started`, `item/file_change/request_approval`,
`turn/completed`). ClaudeBackend's wire is `claude/*`. Two design
options:

* **A.** Add a translation shim in the test harness so claude/* events
  are renamed to the conformance scenario shape before the runner sees
  them. Cleanest for parity but complicated.
* **B.** Author a parallel `claude-scenarios/` directory with
  claude-namespaced expected events. Simpler but doubles maintenance.

Recommend **A** — single source of truth for "what an agent must do".

**Where:** New tests in `codex-rs/agent-backend-conformance/tests/`
that build a duplex-driven mock claude-code host and run all 10
fixtures.

### PR Backend picker UI (medium, GTK)

**Goal:** Dropdown in the AdwHeaderBar that lists registered backends.
Selecting one calls `AgentBridge::restart_with(backend_id)`.

**Where:** `codex-rs/desktop/src/gui/app.rs`. Requires GTK build; see
known constraints.

### PR Protocol Drift diagnostic pane (small, GTK)

**Goal:** Sidebar tab showing `bridge.drift_log().summary()` — table of
`method × count`, with a "Reveal log file" button.

**Where:** `codex-rs/desktop/src/gui/sidebar.rs` (the diagnostics tab
is currently a `StatusPage` placeholder). Drives off
`AgentBridge::drift_log()` (added in PR-W).

### PR-S2 GUI follow-ups (small)

* On `BridgeEvent::AgentClosed` immediately after `restart()`, suppress
  the second toast (currently the dying old supervisor's AgentClosed
  causes a duplicate disconnect-toast). One option: introduce a
  `BridgeEvent::AgentRestarting` variant emitted from `restart()` so
  the GUI can distinguish.

### PR i18n (large)

`gettext-rs` + Weblate, 15 target locales. Architecture doc §6.

### PR a11y CI gate (large)

AT-SPI 2 + Orca matrix in CI. Architecture doc §6 / §9. Needs a GTK
runner with virtual display and Orca.

## 7. Patterns established (reuse these)

* **Backend pattern.** Each backend lives in its own crate, implements
  `AgentBackend`, exposes `from_async_pipe<R, W>` for testing via
  `tokio::io::duplex`, and registers via `inventory::submit!`. See
  `codex-rs/claude-backend/src/lib.rs` (PR-X) — it's the canonical
  template.
* **Drift-aware backends.** Every backend ships its own
  `default_registry()` returning a `KnownVariantRegistry`. Backend-
  specific because the same method name can mean different things
  across protocols. AgentBridge writes any `is_unknown()` notification
  to `DriftLog` so the diagnostic pane can surface them.
* **Restart-friendly bridge.** Long-lived state (events_tx, drift_log,
  cmd_spec) lives on `AgentBridge`; per-supervisor state lives in a
  swappable `SupervisorSlot`. Replace the slot to restart; subscribers
  see continuous events. See `agent_bridge::restart()` in PR-S2.
* **No `MutexGuard` across `.await`.** Workspace clippy lint
  `await-holding-invalid-types` forbids it. When async I/O needs
  exclusive access (e.g. per-server LSP stdin), use a per-resource
  writer task draining an mpsc, not a `Mutex<AsyncWrite>`. See
  `lspd_role::spawn_server_writer` in PR-U.
* **Single mpsc → stdout writer.** When multiple producers (response
  dispatch + async notification pumps) write to the same outbound
  stream, funnel them through one `mpsc::UnboundedSender<Value>` and a
  single writer task. Frames cannot interleave that way. See
  `lspd_role::run` in PR-U.

## 8. Inconsistencies / minor debts to clean up

These are not blockers but worth noting on first re-read:

* `codex-rs/agent-backend/src/process.rs` (`ProcessBackend`) still
  hard-wraps notifications in `Unknown(UnknownNotification {...})`
  rather than going through `IncomingServerNotification::classify`.
  Fix when a `ProcessBackend` user actually needs Known classification
  (none today).
* `agent-backend/src/codex.rs:121` — comment says "Wraps a fully-fledged
  in-process app-server" referring to a deferred PR-E variant. Update
  when PR-V lands.
* `agent_bridge::tests::spawn_with_opens_drift_log_at_custom_path`
  uses `/bin/cat` as the spawned binary; the supervisor will try to
  send NDJSON and `cat` will echo it back, producing log noise but no
  test failure. Acceptable tradeoff for headless verification.
* The `LspMessage` helper in `lspd_role.rs` retains a `_request`
  associated function (leading underscore) for a follow-up that hasn't
  landed. Either use it (PR-V LSP request multiplexing) or delete it
  in a cleanup PR.
* Tests like `wal_sink_records_user_op_and_notification` write under
  the user's `~/.local/state/` if `HOME` is set during `cargo test` —
  the WAL sink uses `resolve_home()`. Tests that need isolation
  override it via tempdir. Worth auditing all callers when adding new
  state-writing features.

## 9. How to start your next PR

```bash
# Branch off main (after PR #1 merges).
git fetch origin main
git checkout -b claude/<your-handle>-<topic> origin/main

# Verify baseline:
cd codex-rs && cargo test -p codex-desktop --no-default-features --tests

# Implement; for each commit, mirror the cheatsheet in §3.

# Push and open a PR:
git push -u origin claude/<your-handle>-<topic>
```

Commit-message convention (used throughout PR-A..PR-X):

```
feat(<crate-or-area>): PR-<letter> — <one-line summary>

<detailed body explaining what landed, what's deferred, and the
verification you ran>
```

End each commit body with a blank line. The harness appends a
`https://claude.ai/code/session_<id>` trailer automatically.
