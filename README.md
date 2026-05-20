# AgEnD Terminal

Orchestrate AI coding agents — not just run them.

> ⚠️ **Pre-alpha.** APIs, CLI flags, and `fleet.yaml` schema may change
> between minor versions. Not for production use. Pin a specific version
> and read the release notes before upgrading.

```bash
cargo install agend-terminal
agend-terminal demo    # Try it in 30 seconds
```

## ⚠️ Git Behavior Modification (Important)

agend-terminal modifies git behavior for spawned agents (PATH shim, commit
trailers, deny matrix, daemon-managed worktrees). Your own terminal is
**not** affected.

**Read [`docs/GIT-BEHAVIOR.md`](docs/GIT-BEHAVIOR.md) before starting the daemon** — what gets modified, why, the risk surface, and the opt-out paths are all documented there.

## What It Does

Spawns AI coding agents (Claude Code, Codex, Kiro, OpenCode, Gemini, Antigravity) as
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

```bash
# Demo (no config)
agend-terminal demo

# Interactive setup — detects backends, optionally wires Telegram, writes fleet.yaml
agend-terminal quickstart

# Or hand-write a minimum fleet.yaml and start the daemon:
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

For optional Telegram binding (remote control + outbound alerts), see [`docs/USAGE.md` § Channel: Telegram](docs/USAGE.md#channel-telegram).

## Backends

| Backend | Command | Status |
|---------|---------|--------|
| Claude Code | `claude` | Tested |
| Kiro CLI | `kiro-cli` | Tested |
| Codex | `codex` | Tested |
| OpenCode | `opencode` | Tested |
| Gemini CLI | `gemini` | Tested (sunsets 2026-06-18 for free/Pro/Ultra; paid Code Assist Standard/Enterprise retain access) |
| Antigravity CLI | `agy` | Tested (#987 — Gemini CLI's official successor) |

## Learn More

- **Commands** — [`docs/CLI.md`](docs/CLI.md) for the full subcommand reference.
- **MCP tools** — [`docs/MCP-TOOLS.md`](docs/MCP-TOOLS.md) for the 35 agent-to-agent coordination tools.
- **Architecture** — [`docs/architecture.md`](docs/architecture.md) covers git worktree isolation, health monitoring + auto-respawn, Telegram topic lifecycle, and daemon-resident design.
- **Contributing** — [`CONTRIBUTING.md`](CONTRIBUTING.md).
- **Release history** — [`CHANGELOG.md`](CHANGELOG.md).

## License

MIT
