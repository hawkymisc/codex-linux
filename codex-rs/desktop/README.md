# codex-desktop

Scaffolding crate for the in-tree Codex Desktop GUI on Linux (GTK4 +
libadwaita + GtkSourceView 5).

This is the PR-A skeleton: it builds with no GTK system libraries
installed. The full UI, agent wiring, LSP multiplexer, and packaging
land in subsequent PRs. See
[`docs/desktop-architecture.md`](../../docs/desktop-architecture.md)
for the full plan.

## Features

- `gtk` *(off by default)* — pulls in `gtk4`, `libadwaita`, and
  `sourceview5` and compiles the actual GUI. Build the distribution
  packages (`.deb`, AppImage, Flatpak) with `--features gtk`. CI
  environments without `libgtk-4-dev` / `libadwaita-1-dev` build with
  the default feature set.

## argv[0] multiplex

A single ELF dispatches to one of three roles based on the basename of
`argv[0]`:

| basename         | role        |
| ---------------- | ----------- |
| `codex-desktop`  | GUI (default) |
| `codex-agent`    | Agent worker (stub in PR-A) |
| `codex-lspd`     | LSP/lint daemon (stub in PR-A) |

Distributions install hardlinks/symlinks named `codex-agent` and
`codex-lspd` next to the real `codex-desktop` binary.
