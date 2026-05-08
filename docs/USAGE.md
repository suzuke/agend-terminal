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

### `agend-mcp-bridge` — MCP server (for AI backends)

```bash
agend-mcp-bridge
```

Stdio JSON-RPC 2.0 server providing 35+ tools (task management,
decisions, messaging, CI watch, etc.). Not meant to be run manually.

Each AI backend (Claude Code, Kiro, Codex, Gemini, OpenCode) auto-
launches this as a child process based on its MCP config — the
daemon writes the bridge path into every backend's mcp.json on
every start.

> **Sprint 56 deprecation note (#531):** `agend-terminal mcp` was
> the canonical invocation through v0.6. Phase 2a of the migration
> ships `agend-mcp-bridge` in release artifacts on all 5 platforms;
> Phase 2b makes the bridge mandatory (the daemon will no longer
> fall back to the legacy `agend-terminal mcp` command). The legacy
> command remains for one Sprint with a deprecation warning;
> Sprint 57 removes it entirely. See [Phase 1 RCA](RCA-issue-531-deprecate-agend-terminal-mcp-2026-05-08.md).

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

## Channel: Telegram

Bind the fleet to a Telegram group for remote control (send messages to
agents from your phone) and outbound notifications (stall / crash / CI
alerts pushed back to the group).

### Minimum config

```yaml
channel:
  type: telegram
  bot_token_env: AGEND_BOT_TOKEN          # env var holding the bot token
  group_id: -1001234567890                # Telegram chat id of the group
  user_allowlist: [123456789]             # operator Telegram user_id(s)
```

Then export the bot token before `agend-terminal start`:

```bash
export AGEND_BOT_TOKEN="123456:abcdef..."
```

### How to get values

- **Bot token** (`AGEND_BOT_TOKEN`): create a bot via [@BotFather](https://t.me/BotFather), copy the token it returns.
- **Group id**: add your bot to the target group, then send any message and inspect the bot's `getUpdates` API (`https://api.telegram.org/bot<TOKEN>/getUpdates`) — the `chat.id` is your `group_id` (negative for groups / supergroups).
- **User id**: message [@userinfobot](https://t.me/userinfobot) on Telegram and it replies with your numeric user id. Add every operator who should be allowed to command the fleet.

### `user_allowlist` semantics (Sprint 21 fail-closed default)

| `user_allowlist` value | Inbound (sender filter) | Outbound (notification gate) |
|---|---|---|
| `[123, 456]` (≥ 1 entry) | Listed users only — others rejected | ✅ Notifications delivered |
| `[]` (empty list) | Everyone rejected | 🔇 Notifications dropped (fail-closed) |
| field absent / `null` | Legacy: everyone accepted (deprecated) | 🔇 Notifications dropped (fail-closed) |

The outbound gate landed in [PR #216](https://github.com/suzuke/agend-terminal/pull/216) (Sprint 21 Phase 1) to close the
[Sprint 20.5 cross-validation](codebase-review-2026-04-27/SYNTHESIS.md) outbound info-leak finding (40-line PTY tails were leaking to anyone added to a bound group regardless of inbound auth state). Inbound fail-closed is being landed in Phase 2.

### Migration: upgrading from < Sprint 21

If your `fleet.yaml` previously had a `channel.telegram` block **without** `user_allowlist`, the fleet still runs after upgrading but **outbound notifications now drop silently** (fail-closed). You will see:

```
WARN: telegram channel.user_allowlist is not set — any group member can command the fleet. \
      Set `user_allowlist: [123, 456]` in fleet.yaml to lock this down.
```

To restore outbound notifications, add your operator user_id(s) to `user_allowlist`. This is the **only required migration step**; bot token and group id remain unchanged.

If you previously relied on legacy "anyone in the group can command the fleet" behaviour, the inbound side still accepts all users until Phase 2 lands; configure `user_allowlist` now to close both sides simultaneously.

### `outbound_capabilities` semantics (Sprint 23 P1 — default-open)

Per-instance gate for **agent-callable** outbound MCP→Channel ops (`reply` / `react` / `edit_message` / `delegate_task` provenance). Independent of `user_allowlist` (which gates inbound + daemon-internal notifications and is still **fail-closed**).

| `outbound_capabilities` value | Behaviour |
|---|---|
| field absent | **Default-open — all ops permitted** |
| `[reply, react, edit, inject_provenance]` | Only listed ops permitted |
| `[]` (explicit empty) | All ops rejected (operator opt-out, retained) |

**Why default-open?** Single-operator threat model. The TUI is already full machine access; the cascade-attack-chain defence from Sprint 22 P0 was over-spec for the actual deployment shape. Operator explicitly accepts the security trade-off (Sprint 23 P1 reversal).

Built-in instances (`general` and any future auto-created coordinator) inherit default-open — no auto-injected list needed (was the case in Sprint 22 P0 PR #230 and is now retired).

### Restricting / opting out

Default-open is the recommended posture for single-operator deployments. Two opt-out shapes if you want the gate active:

**Restrict to a subset of ops** (e.g. allow `reply` only):

```yaml
instances:
  my-worker:
    backend: claude
    outbound_capabilities: [reply]
    # … other fields …
```

**Block all agent outbound** (relay / read-only roles):

```yaml
instances:
  my-readonly-relay:
    backend: claude
    outbound_capabilities: []                # explicit "no agent outbound"
```

See `docs/archived/MIGRATION-OUTBOUND-CAPS.md` for the full transition guide (Sprint 22 P0 fail-closed → Sprint 23 P1 default-open reversal section) and the `ChannelOpKind` enum reference.

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
