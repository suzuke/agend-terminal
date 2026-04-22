# Usage Guide

## Binaries

| Binary | Purpose |
|---|---|
| `agend-terminal` | Main program — all features enter through here |
| `agend-supervisor` | Frozen supervisor for daemon hot-upgrade + crash recovery (Unix only) |

## Startup Modes

### `agend-terminal app` — Interactive TUI workbench

```bash
agend-terminal app [--fleet fleet.yaml]
```

Full multi-tab/pane terminal UI built with ratatui. Manages agents
locally: spawn, kill, respawn, drag-and-drop panes between tabs.

If a daemon is already running, connects to it (attached mode).
Otherwise starts its own local fleet (owned mode).

**When to use:** Day-to-day development. Interactive multi-agent work.

### `agend-terminal start` — Headless daemon

```bash
agend-terminal start [--fleet fleet.yaml] [--detached]
```

Background service with no TUI. Reads fleet.yaml, manages agents,
auto-respawns crashed agents, runs scheduler and CI watch.

Use `--detached` to fork into background.

**When to use:** Server deployments. CI/CD. Unattended fleet operation.

### `agend-terminal attach <name>` — Thin terminal client

```bash
agend-terminal attach at-dev-2
```

Minimal raw-mode terminal connecting to a single agent's PTY
through the daemon's API socket. No panes, no tabs — just one
agent's terminal stream.

Detach with `Ctrl+B d`.

**When to use:** SSH into a remote machine and inspect one agent.
Lightweight debugging. Reading agent output without the full TUI.

### `agend-terminal tray` — System tray resident

```bash
agend-terminal tray   # requires: cargo build --features tray
```

Menu-bar icon (macOS / Linux). Color-coded daemon status:
gray = offline, amber = idle, green = active.

Automatically starts the daemon if not running. Click "Open App"
to launch the full TUI.

**When to use:** Background monitoring. Launch-at-login. Quick
access without keeping a terminal open.

### `agend-terminal mcp` — MCP server (for AI backends)

```bash
agend-terminal mcp
```

Stdio JSON-RPC 2.0 server providing 35+ tools (task management,
decisions, messaging, CI watch, etc.). Not meant to be run manually.

Each AI backend (Claude Code, Kiro, Codex, Gemini) auto-launches
this as a child process based on its MCP config.

**When to use:** You don't run this directly. It's started
automatically when an AI agent needs to talk to the daemon.

### `agend-supervisor` — Hot-upgrade supervisor

```bash
agend-supervisor [--home ~/.agend-terminal]
```

Sits above the daemon. Manages daemon lifecycle: start, crash
recovery, and zero-downtime binary upgrades.

Upgrade flow: stage new binary → self-test → swap → monitor
stability window → commit or rollback.

**When to use:** Production environments where the daemon must
survive binary upgrades without dropping agent sessions.

## Architecture

```
agend-supervisor (frozen binary, rarely upgraded)
  └── agend-terminal start/daemon (headless, long-running)
        ├── Agent PTYs (managed by daemon)
        ├── MCP servers (one per agent, started by AI backends)
        ├── Telegram polling
        ├── Scheduler (cron + one-shot)
        └── API socket
              └── agend-terminal attach <name> (thin clients connect here)

agend-terminal app (standalone TUI)
  ├── Daemon running → attached mode (connects to existing daemon)
  └── No daemon → owned mode (manages its own local fleet)

agend-terminal tray (menu-bar resident)
  └── Auto-starts daemon → click "Open App" → launches TUI
```

## Other Commands

| Command | Purpose |
|---|---|
| `daemon <name:cmd>...` | Start daemon with explicit agent specs (no fleet.yaml) |
| `list` / `ls` | List running agents |
| `status` | Detailed agent status (state, health) |
| `inject <name> <text>` | Inject text into an agent's PTY |
| `kill <name>` | Kill a specific agent |
| `connect <name>` | Connect an external agent to the daemon |
| `fleet start/stop` | Batch start/stop from fleet.yaml |
| `stop` | Stop the daemon |
| `quickstart` | Interactive setup (detect backends, configure Telegram, generate fleet.yaml) |
| `demo` | 30-second interactive demo of multi-agent orchestration |
| `doctor` | Health check (verify installation, backends, connectivity) |
| `bugreport` | Generate diagnostic report with logs and config |
| `upgrade` | Trigger hot-upgrade (requires supervisor) |
| `verify` | Full E2E verification |
| `test [suite]` | Run built-in tests (mcp, attach, inbox, api, all) |
| `capture` | Capture backend output (debugging) |
| `completions <shell>` | Generate shell completions (bash, zsh, fish, powershell) |

## TUI Keyboard Shortcuts

All shortcuts use `Ctrl+B` as the prefix key (like tmux).

### Tab Management

| Shortcut | Action |
|---|---|
| `Ctrl+B n` | New tab (opens menu) |
| `Ctrl+B 1-9` | Go to tab by number |
| `Ctrl+B Tab` | Next tab |
| `Ctrl+B Shift+Tab` | Previous tab |
| `Ctrl+B l` | Last active tab |
| `Ctrl+B w` | List all tabs |

### Pane Management

| Shortcut | Action |
|---|---|
| `Ctrl+B \|` | Split vertical |
| `Ctrl+B -` | Split horizontal |
| `Ctrl+B arrows` | Focus pane (repeatable) |
| `Ctrl+B o` | Cycle focus (repeatable) |
| `Ctrl+B z` | Zoom/unzoom pane |
| `Ctrl+B x` | Close pane |
| `Ctrl+B X` | Close tab |

### Scrolling

| Shortcut | Action |
|---|---|
| Mouse wheel | Scroll focused pane |
| `Ctrl+B [` | Scroll mode (exit with Esc) |
| `Ctrl+B PageUp/Down` | Page scroll |

### Other

| Shortcut | Action |
|---|---|
| `Ctrl+B ~` | Scratch shell overlay |
| `Ctrl+B :` | Command palette |
| `Ctrl+B ?` | Show keybindings help |
| `Ctrl+B d` | Detach (exit TUI, daemon keeps running) |
| `Ctrl+B m` | Toggle mirror mute (future: TUI channel mirror) |
| `Shift+Enter` | Newline without submit (requires terminal keyboard enhancement support) |
| `Alt+Enter` | Newline without submit (same as Shift+Enter) |

### Mouse

- **Click tab** — switch to tab
- **Drag tab** — reorder tabs
- **Click pane label** — focus pane
- **Drag pane label** — move pane (cross-tab supported)
- **Mouse select** — select text in pane
