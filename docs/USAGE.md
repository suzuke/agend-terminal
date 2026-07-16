[繁體中文](USAGE.zh-TW.md)

# Usage Guide

## Binaries

| Binary | Purpose |
|---|---|
| `agend-terminal` | Main program — all features enter through here |
| `agend-git` | Legacy kill-only compatibility helper; git interception is owned by the vendored `agentic-git` shim |
| `agend-mcp-bridge` | MCP stdio ↔ daemon bridge — spawned per agent by the AI backend, not run directly |

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
agend-terminal start [--fleet fleet.yaml] [--foreground]
```

Background service with no TUI. Reads fleet.yaml, manages agents,
auto-respawns crashed agents, runs scheduler and CI watch.

Detached (backgrounded) by **default** — the foreground process exits once the
daemon has published its run dir. Pass `--foreground` to keep it in the
foreground (legacy blocking mode, e.g. under systemd/launchd or for debugging).

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

### `agend-mcp-bridge` — MCP server (for AI backends)

```bash
agend-mcp-bridge
```

Stdio JSON-RPC 2.0 server providing 32 tools (task management,
decisions, messaging, CI watch, etc.). Not meant to be run manually.

Each supported AI backend (Claude Code, Kiro, Codex, OpenCode, Antigravity,
and Grok) auto-
launches this as a child process based on its MCP config — the
daemon writes the bridge configuration in that backend's native format.

**When to use:** You don't run this directly. It's started
automatically when an AI agent needs to talk to the daemon.

### Daemon supervision & restart

There is **no separate `agend-supervisor` binary**. Supervision is in-process:
the daemon runs its own supervisor (auto-respawn, health monitoring, hung
detection — `src/daemon/supervisor.rs`). A graceful restart / binary reload is
driven by the `restart_daemon` MCP tool (self-respawn: the daemon spawns a
successor, health-gates it, then exits — #1814), or by the OS service manager
when installed via `agend-terminal service install` (systemd / launchd / Task
Scheduler). For an external keep-alive wrapper, see `docs/MCP-DAEMON-PROXY-CONTRACT.md`.

**When to use:** run `agend-terminal service install` for OS-managed restart on
crash/boot; call `restart_daemon` to reload after upgrading the binary.

## Architecture

```
agend-terminal start (headless daemon, long-running; in-process supervisor)
  └── (optional) OS service manager / restart_daemon self-respawn for restarts
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

## Channel: Telegram

Bind the fleet to a Telegram group for remote control (send messages to
agents from your phone) and outbound notifications (stall / crash / CI
alerts pushed back to the group).

### Minimum config

```yaml
channel:
  type: telegram
  bot_token_env: AGEND_TELEGRAM_BOT_TOKEN # env var holding the bot token
  group_id: -1001234567890                # Telegram chat id of the group
  user_allowlist: [123456789]             # operator Telegram user_id(s)
```

Then export the bot token before `agend-terminal start`:

```bash
export AGEND_TELEGRAM_BOT_TOKEN="123456:abcdef..."
```

### How to get values

- **Bot token** (`AGEND_TELEGRAM_BOT_TOKEN`): create a bot via [@BotFather](https://t.me/BotFather), copy the token it returns.
- **Group id**: add your bot to the target group, then send any message and inspect the bot's `getUpdates` API (`https://api.telegram.org/bot<TOKEN>/getUpdates`) — the `chat.id` is your `group_id` (negative for groups / supergroups).
- **User id**: message [@userinfobot](https://t.me/userinfobot) on Telegram and it replies with your numeric user id. Add every operator who should be allowed to command the fleet.

### `user_allowlist` semantics (Sprint 21 fail-closed default)

| `user_allowlist` value | Inbound (sender filter) | Outbound (notification gate) |
|---|---|---|
| `[123, 456]` (≥ 1 entry) | Listed users only — others rejected | ✅ Notifications delivered |
| `[]` (empty list) | Everyone rejected | 🔇 Notifications dropped (fail-closed) |
| field absent / `null` | Everyone rejected | 🔇 Notifications dropped (fail-closed) |

Both inbound commands and outbound notifications are fail-closed when the
allowlist is absent or empty. There is no legacy accept-all fallback.

### Migration: upgrading from < Sprint 21

If your `fleet.yaml` has a Telegram channel without a non-empty
`user_allowlist`, the daemon keeps the channel disabled for inbound commands and
outbound notifications. Add the operator Telegram user IDs to restore both
directions; bot token and group ID remain unchanged.

The former per-instance `outbound_capabilities` layer has been removed. Do not
add it to new configurations; channel authorization is enforced by the channel
allowlist and operator authority gates.

## Other Commands

| Command | Purpose |
|---|---|
| `start --agents <name:cmd>...` | Start daemon with explicit agent specs (no fleet.yaml; subsumes the former `daemon` subcommand) |
| `list` / `ls` | List running agents |
| `status` | Detailed agent status (state, health) |
| `inject <name> <text>` | Inject text into an agent's PTY |
| `kill <name>` | Kill a specific agent |
| `connect <name>` | Run a backend under temporary daemon registration |
| `stop` | Stop the daemon |
| `quickstart` | Interactive setup (detect backends, configure Telegram, generate fleet.yaml) |
| `doctor` | Health check (verify installation, backends, connectivity) |
| `bugreport` | Generate diagnostic report with logs and config |
| `verify [--quick]` | Full E2E verification (subsumes the former `test` subcommand) |
| `mode <active|away|sleep>` | Set operator availability and delegation |
| `service <install|uninstall|status>` | Manage the OS service |
| `skills <action>` | Manage shared backend skills |
| `capture backend|promote` | Capture backend output or promote a fixture |
| `verify-push` | Verify a semantic push claim against a diff |
| `completions <shell>` | Generate shell completions (bash, zsh, fish, powershell) |
| `admin cleanup-branches [--yes]` | Delete local branches whose PRs were merged (dry-run by default) |
| `admin cleanup-zombies [--age <D>] [--yes]` | Kill zombie daemons holding stale `run/<pid>/` (#927; default `--age 14d`) |

## TUI Keyboard Shortcuts

All shortcuts use `Ctrl+B` as the prefix key (like tmux).

### Tab Management

| Shortcut | Action |
|---|---|
| `Ctrl+B c` | New tab (opens menu) |
| `Ctrl+B n` / `Ctrl+B p` | Next / previous tab |
| `Ctrl+B 0-9` | Go to tab by number |
| `Ctrl+B l` | Last active tab |
| `Ctrl+B &` | Close tab |
| `Ctrl+B w` | List all tabs |

### Pane Management

| Shortcut | Action |
|---|---|
| `Ctrl+B "` | Split horizontal (top/bottom) |
| `Ctrl+B %` | Split vertical (left/right) |
| `Ctrl+B arrows` | Focus pane (repeatable) |
| `Ctrl+B o` | Cycle focus (repeatable) |
| `Ctrl+B z` | Zoom/unzoom pane |
| `Ctrl+B x` | Close pane |

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
| `Ctrl+B t` / `Ctrl+B s` / `Ctrl+B m` / `Ctrl+B f` | Open task board (Tasks / Status / Monitor / Fleet view) |
| `Ctrl+B D` | Pending-decisions board |
| `Shift+Enter` | Newline without submit (requires terminal keyboard enhancement support) |
| `Alt+Enter` | Newline without submit (same as Shift+Enter) |

### Mouse

- **Click tab** — switch to tab
- **Drag tab** — reorder tabs
- **Click pane label** — focus pane
- **Drag pane label** — move pane (cross-tab supported)
- **Mouse select** — select text in pane
