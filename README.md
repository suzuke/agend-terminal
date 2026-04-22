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

**Run 3 Claude agents working on the same repo in parallel:**
```bash
agend-terminal start    # reads fleet.yaml, spawns agents with git worktree isolation
agend-terminal status   # see all agents and their health
agend-terminal attach dev  # watch one agent work (Ctrl+B d to detach)
```

**Agents talk to each other — no glue code:**
```
Agent A finds a bug outside its scope → delegates to Agent B via MCP tool.
Agent B fixes it → reports back with commit hash.
Agent A continues with the fix applied.
```

**Survive crashes without losing context:**
```
Agent crashes → auto-respawned with exponential backoff.
System message tells the new agent what happened.
Worktree preserves all code changes.
```

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

# 2. (Optional) Build with Discord support
cargo build --release --features discord

# 3. Add to PATH (PowerShell)
$env:PATH += ";$(Get-Location)\target\release"

# 4. Verify
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
# Try the demo (no config needed)
agend-terminal demo

# Or start with your own agents
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

## Commands

```
Get started
  agend-terminal app                   Launch multi-tab/pane TUI
  agend-terminal demo                  30-second interactive demo
  agend-terminal quickstart            Interactive setup — detect backends, wire Telegram, generate fleet.yaml
  agend-terminal doctor                Health check backends

Run a fleet
  agend-terminal start                 Start daemon with fleet.yaml
  agend-terminal daemon [name:cmd …]   Start daemon with explicit agents
  agend-terminal stop                  Stop daemon
  agend-terminal upgrade --binary <path>
                                       Hot-upgrade daemon via supervisor (Unix only)

Interact
  agend-terminal attach <name>         Attach to agent (Ctrl+B d to detach)
  agend-terminal inject <name> <text>  Send input to agent's PTY
  agend-terminal list                  List running agents
  agend-terminal status                Detailed agent status (state, health)
  agend-terminal kill <name>           Kill an agent
  agend-terminal connect <name>        Register an external agent with the running daemon
  agend-terminal fleet …               Fleet management subcommands

Integration
  agend-terminal mcp                   MCP stdio server (agent-to-agent coordination)
  agend-terminal completions <shell>   Generate shell completions (bash/zsh/fish/elvish/powershell)
  agend-terminal bugreport             One-file diagnostic export
```

## 35 MCP Tools

Agents get these tools automatically via MCP:

| Category | Tools |
|----------|-------|
| Talk to users | reply, react, edit_message, download_attachment |
| Talk to agents | send_to_instance, delegate_task, report_result, request_information, broadcast, inbox |
| Manage agents | list/create/delete/start/describe/replace_instance, set_display_name, set_description |
| Track decisions | post_decision, list_decisions, update_decision |
| Track tasks | task (create/list/claim/done/update) |
| Organize teams | create/delete/list/update_team |
| Schedule work | create/list/update/delete_schedule |
| Deploy fleets | deploy_template, teardown_deployment, list_deployments |
| Share code | checkout_repo, release_repo |

## Git Worktree Isolation

Agents pointing to git repos automatically get isolated worktrees:

```
~/my-project/               ← original repo (untouched)
~/my-project/.worktrees/
  dev/                       ← agent "dev" works here (branch agend/dev)
  reviewer/                  ← agent "reviewer" works here (branch agend/reviewer)
```

No configuration needed. `.worktrees` auto-added to `.gitignore`.

## Health Monitoring

- Auto-respawn with exponential backoff (5s → 300s)
- State detection: Idle, Thinking, ToolUse, RateLimit, Crashed, Restarting
- Crash notifications via Telegram
- 30-minute stability window prevents permanent failure from occasional crashes

## Telegram Integration

Each agent gets its own forum topic; messages route by topic. Topic lifecycle
is bidirectional:

- **Delete a pane in app → topic is deleted in Telegram** (immediate).
- **Close topic in Telegram → pane is removed in app** (immediate, via the
  `forum_topic_closed` service message).
- **Delete topic in Telegram → pane is removed in app** (lazy, on the next
  agent send to that topic). Telegram Bot API does not emit a deletion event,
  so the cleanup fires the first time a send returns
  `message thread not found`. Prefer Close if you want immediate cleanup.

## Backends

| Backend | Command | Status |
|---------|---------|--------|
| Claude Code | `claude` | Tested |
| Kiro CLI | `kiro-cli` | Tested |
| Codex | `codex` | Tested |
| OpenCode | `opencode` | Tested |
| Gemini CLI | `gemini` | Tested |

## Testing

```bash
cargo test         # 561 tests (unit + integration + MCP round-trip)
cargo clippy       # 0 errors (deny unwrap_used)
```

## License

MIT
