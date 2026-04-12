# AgEnD Terminal

Rust-based Agent Process Manager — replaces tmux for managing AI coding agents.

Direct PTY ownership eliminates `send-keys` race conditions and enables atomic writes, output capture, and virtual terminal state tracking.

## Features

- **PTY Engine** — portable-pty + alacritty_terminal VTerm emulation
- **Attach/Detach** — Ctrl+B d to detach, agent runs in background
- **5 Backend Presets** — Claude Code, Kiro CLI, Codex, OpenCode, Gemini
- **35 MCP Tools** — reply, delegate_task, decisions, task board, teams, schedules, deployments
- **Fleet Management** — fleet.yaml config, Telegram integration
- **Health Monitoring** — auto-respawn with exponential backoff, state detection
- **Git Worktree Isolation** — auto-creates worktree per agent on git repos
- **Event Log** — append-only JSONL audit trail
- **Cron Scheduling** — inject messages on cron schedule

## Quick Start

```bash
cargo build --release
export PATH="$PWD/target/release:$PATH"

# Single agent
agend-terminal daemon shell:/bin/bash

# Fleet (with fleet.yaml)
mkdir -p ~/.agend-terminal
cp fleet.yaml ~/.agend-terminal/fleet.yaml
agend-terminal start
```

## Commands

```
agend-terminal start                     Start daemon + fleet
agend-terminal daemon [name:cmd ...]     Start daemon with agents
agend-terminal attach <name>             Attach (Ctrl+B d to detach)
agend-terminal inject <name> <text>      Send input to agent
agend-terminal list                      List running agents
agend-terminal status                    Show agent state + health
agend-terminal kill <name>               Kill an agent
agend-terminal stop                      Stop daemon
agend-terminal fleet start [config]      Start fleet from config
agend-terminal fleet stop                Stop all fleet agents
agend-terminal mcp                       Start MCP stdio server
agend-terminal doctor                    Health check
agend-terminal verify [--json]           E2E verification
```

## Fleet Configuration

```yaml
defaults:
  backend: claude-code

channel:
  type: telegram
  bot_token_env: AGEND_BOT_TOKEN
  group_id: -100123456789

instances:
  dev:
    role: "Developer"
    working_directory: ~/project    # auto worktree if git repo
    git_branch: "custom/branch"     # optional branch override
  reviewer:
    role: "Code reviewer"
    working_directory: ~/project
```

## MCP Tools (35)

| Category | Tools |
|----------|-------|
| Channel | reply, react, edit_message, download_attachment |
| Communication | send_to_instance, delegate_task, report_result, request_information, broadcast, inbox |
| Instance | list_instances, create_instance, delete_instance, start_instance, describe_instance, replace_instance, set_display_name, set_description |
| Decisions | post_decision, list_decisions, update_decision |
| Task Board | task (create/list/claim/done/update) |
| Teams | create_team, delete_team, list_teams, update_team |
| Scheduling | create_schedule, list_schedules, update_schedule, delete_schedule |
| Deployments | deploy_template, teardown_deployment, list_deployments |
| Repo | checkout_repo, release_repo |

## Git Worktree Isolation

Agents with `working_directory` pointing to a git repo automatically get their own worktree at `.worktrees/{name}/` with branch `agend/{name}`. No configuration needed.

- Worktrees are reused on respawn
- `.worktrees` auto-added to `.gitignore`
- Stale entries pruned on daemon startup
- Residual worktrees listed on shutdown

## Health Monitoring

- **States**: Starting, Ready, Idle, Thinking, ToolUse, PermissionPrompt, RateLimit, Crashed, Restarting
- **Health**: Healthy → Recovering → Unstable → Failed (with decay after 30 min stability)
- **Auto-respawn**: Exponential backoff (5s → 300s), max 5 retries
- **Hang detection**: State-aware timeouts (120s starting, 600s thinking)
- **Crash notifications**: Telegram notification on repeated crashes

## Environment

- `AGEND_TERMINAL_HOME` — Data directory (default: `~/.agend-terminal`)
- `.env` file in home directory is auto-loaded on startup

## Testing

```bash
cargo test              # 51 tests (39 unit + 5 integration + 7 MCP)
cargo clippy            # 0 errors (deny unwrap_used)
agend-terminal verify   # E2E verification suite
```

## License

MIT
