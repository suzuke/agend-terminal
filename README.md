# AgEnD Terminal

Orchestrate AI coding agents — not just run them.

> ⚠️ **Pre-alpha.** APIs, CLI flags, and `fleet.yaml` schema may change
> between minor versions. Not for production use. Pin a specific version
> and read the release notes before upgrading.

```bash
cargo install agend-terminal
agend-terminal demo    # Try it in 30 seconds
```

## What It Does

Spawns AI coding agents (Claude Code, Codex, Kiro, OpenCode, Gemini) as
long-lived PTY processes, each in its own git worktree. A built-in MCP
server lets agents talk to each other — delegate work, request info,
broadcast updates — without glue code. Crashes are survived by auto-
respawn with context handover. Drive the fleet through a multi-tab /
multi-pane TUI, a Telegram channel, or an optional system tray.

## Why Not tmux?

| | tmux + shell scripts | agend-terminal |
|---|---|---|
| Input injection | `send-keys` race conditions | Atomic PTY write |
| Output capture | Screen scraping | VTerm state tracking |
| Agent health | Manual monitoring | Auto-respawn + state detection |
| Multi-agent comms | Custom IPC | Built-in MCP tools |
| Git isolation | Manual worktrees | Auto per-agent worktree |

## Quick Start
## Installation

### Windows

**Prerequisites:**
- [Rust toolchain](https://rustup.rs/) (includes `cargo`)
- [Git for Windows](https://git-scm.com/download/win)
- One of the supported AI coding CLIs (installed and authenticated)

```powershell
# 1. Clone and build
git clone https://github.com/songsid/agend-terminal.git
cd agend-terminal
cargo build --release

# Build without Discord support:
# cargo build --release --no-default-features

# 2. Add to PATH (PowerShell)
$env:PATH += ";$(Get-Location)\target\release"

# 3. Verify
agend-terminal --version
```

> **Note:** On Windows, `agend-terminal` uses ConPTY instead of tmux.
> WSL is not required.

### Linux / macOS

```bash
# From source
git clone https://github.com/songsid/agend-terminal.git
cd agend-terminal
cargo install --path .

# Or via cargo
cargo install agend-terminal
```



```bash
# Demo (no config)
agend-terminal demo

# Interactive setup — detects backends, wires Telegram, writes fleet.yaml
agend-terminal quickstart

# Or hand-write:
cat > ~/.agend/fleet.yaml << 'YAML'
defaults:
  backend: claude
instances:
  dev:
    role: "Developer"
    working_directory: ~/my-project
  reviewer:
    role: "Code reviewer"
    working_directory: ~/my-project
YAML
agend-terminal start
```

## Backends

| Backend | Command | Status |
|---------|---------|--------|
| Claude Code | `claude` | Tested |
| Kiro CLI | `kiro-cli` | Tested |
| Codex | `codex` | Tested |
| OpenCode | `opencode` | Tested |
| Gemini CLI | `gemini` | Tested |

## Learn More

- **Commands** — [`docs/CLI.md`](docs/CLI.md) for the full subcommand reference.
- **MCP tools** — [`docs/MCP-TOOLS.md`](docs/MCP-TOOLS.md) for the 35 agent-to-agent coordination tools.
- **Architecture** — [`docs/architecture.md`](docs/architecture.md) covers git worktree isolation, health monitoring + auto-respawn, Telegram topic lifecycle, and daemon-resident design.
- **Contributing** — [`CONTRIBUTING.md`](CONTRIBUTING.md).
- **Release history** — [`CHANGELOG.md`](CHANGELOG.md).

## License

MIT
