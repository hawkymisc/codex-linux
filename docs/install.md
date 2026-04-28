## Installing & building

### System requirements

| Requirement                 | Details                                                         |
| --------------------------- | --------------------------------------------------------------- |
| Operating systems           | macOS 12+, Ubuntu 20.04+/Debian 10+, or Windows 11 **via WSL2** |
| Git (optional, recommended) | 2.23+ for built-in PR helpers                                   |
| RAM                         | 4-GB minimum (8-GB recommended)                                 |

### DotSlash

The GitHub Release also contains a [DotSlash](https://dotslash-cli.com/) file for the Codex CLI named `codex`. Using a DotSlash file makes it possible to make a lightweight commit to source control to ensure all contributors use the same version of an executable, regardless of what platform they use for development.

### Build from source

```bash
# Clone the repository and navigate to the root of the Cargo workspace.
git clone https://github.com/openai/codex.git
cd codex/codex-rs

# Install the Rust toolchain, if necessary.
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
rustup component add rustfmt
rustup component add clippy
# Install helper tools used by the workspace justfile:
cargo install just
# Optional: install nextest for the `just test` helper
cargo install --locked cargo-nextest

# Build Codex.
cargo build

# Launch the TUI with a sample prompt.
cargo run --bin codex -- "explain this codebase to me"

# After making changes, use the root justfile helpers (they default to codex-rs):
just fmt
just fix -p <crate-you-touched>

# Run the relevant tests (project-specific is fastest), for example:
cargo test -p codex-tui
# If you have cargo-nextest installed, `just test` runs the test suite via nextest:
just test
# Avoid `--all-features` for routine local runs because it increases build
# time and `target/` disk usage by compiling additional feature combinations.
# If you specifically want full feature coverage, use:
cargo test --all-features
```

## Codex Desktop on Linux (`codex app`)

`codex app` opens a workspace in Codex Desktop. macOS and Windows builds are
shipped by OpenAI; OpenAI does not yet publish an official Linux build, so on
Linux the command launches an **in-tree desktop wrapper that you build
yourself** from this repository (no third-party redistribution required).

When invoked on Linux, `codex app` looks for an installed launcher in the
following locations and runs it with the workspace path as the first argument:

| Path                                           | Source                          |
| ---------------------------------------------- | ------------------------------- |
| `codex-desktop` on `$PATH`                     | preferred — built from this repo|
| `/usr/local/bin/codex-desktop`                 | system-wide install             |
| `/usr/bin/codex-desktop`                       | distro packaging                |
| `~/.local/bin/codex-desktop`                   | rootless install                |
| `~/.cargo/bin/codex-desktop`                   | `cargo install` install         |
| `~/Applications/Codex.AppImage` (and variants) | self-built AppImage drops       |
| `~/.local/bin/Codex.AppImage`                  | self-built AppImage drops       |

### Ubuntu 24.04+ quickstart

1. Install the Codex CLI itself (`npm i -g @openai/codex` or download the
   `codex-x86_64-unknown-linux-musl.tar.gz` release artifact).
2. Build the in-tree desktop wrapper from source:
   ```bash
   git clone <this repo>
   cd codex-rs
   sudo apt install -y build-essential pkg-config libcap-dev curl
   cargo build --release -p codex-desktop
   install -Dm755 target/release/codex-desktop ~/.local/bin/codex-desktop
   ```
3. Run `codex app` from any project directory; it detects the binary in
   `$PATH` and launches it on the workspace.

If you have a self-built AppImage you trust, pass it explicitly and Codex CLI
will download it to `~/.local/bin/Codex.AppImage`, mark it executable, and
launch it:

```bash
codex app --download-url https://example.com/path/to/Codex.AppImage
```

> AppImages on Ubuntu 24.04+ require `libfuse2t64`
> (`sudo apt install libfuse2t64`). Wayland sessions typically need
> `--ozone-platform-hint=auto`.

To be notified when OpenAI ships an official Linux build, sign up at
<https://openai.com/form/codex-app/>.

## Tracing / verbose logging

Codex is written in Rust, so it honors the `RUST_LOG` environment variable to configure its logging behavior.

The TUI defaults to `RUST_LOG=codex_core=info,codex_tui=info,codex_rmcp_client=info` and log messages are written to `~/.codex/log/codex-tui.log` by default. For a single run, you can override the log directory with `-c log_dir=...` (for example, `-c log_dir=./.codex-log`).

```bash
tail -F ~/.codex/log/codex-tui.log
```

By comparison, the non-interactive mode (`codex exec`) defaults to `RUST_LOG=error`, but messages are printed inline, so there is no need to monitor a separate file.

See the Rust documentation on [`RUST_LOG`](https://docs.rs/env_logger/latest/env_logger/#enabling-logging) for more information on the configuration options.
