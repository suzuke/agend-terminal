# Architecture Quick Start

500-word newcomer doc. Read this first; `architecture.md` for depth.

## What this is

Single-user fleet manager for AI coding agents (Claude Code / kiro-cli / codex / gemini / opencode). Spawns each agent in its own PTY, exposes them in a Ratatui TUI, mirrors traffic to Telegram, and lets agents talk to each other via an MCP server.

Threat model: **localhost-only, single operator**. Cookie file `~/.agend-terminal/run/<pid>/api.cookie` (mode 0600) gates the API; anyone who can read it already owns the user account. Defenses past that point are intentionally minimal (KISS, see §0 of `FLEET-DEV-PROTOCOL-v1.md`).

## Two binaries

- **`agend-terminal`** — the daemon + TUI + CLI. One process, no supervisor (removed Sprint 29).
- **`agend-mcp-bridge`** — zero-state stdio↔TCP relay spawned by Claude Code as its MCP server. Forwards `tools/call` to the daemon's `/mcp` API; everything else (`initialize`, `tools/list`) is local. See `src/bin/agend-mcp-bridge.rs`.

## Daemon process model

```
agend-terminal app
    ├── daemon (src/daemon/) — registry, event loop, watchdog
    ├── PTY child × N (src/agent.rs) — one per fleet.yaml instance
    │     └── shells out to agent backend (claude / kiro / codex / ...)
    ├── TUI session (src/app/) — local user view
    ├── Channel adapter (src/channel/telegram.rs) — operator + Telegram
    ├── API server (src/api/) — TCP loopback, cookie-authed
    └── MCP server (src/mcp/) — JSON-RPC over the API socket
```

PTY children are children of the daemon. Daemon dies → PTY dies → all agents lost. There's no auto-recovery; if the daemon panics you restart it manually (or wire a `launchctl` `KeepAlive`).

## How code talks to itself

| Path | Used by |
|---|---|
| **`reply` / `react`** | Agent → operator (current channel) |
| **`send`** (unified, replaces 5 old tools) | Agent → agent (kind=task/report/query/update) |
| **`inbox`** | Agent pulls own pending messages |
| **`task` / `decision` / `team` / `schedule` / `ci` / `repo` / `health` / `deployment`** | Action-based CRUD per domain |
| Direct daemon API | TUI sessions, supervisor (gone), bridges |

26 MCP tools after Sprint 30 consolidation. `src/mcp/tools.rs` is the registry; an inline invariant test `tool_definitions_count_invariant_post_sprint_30` (in the same file's `mod tests`) pins the count to catch silent drift.

## Where to start reading

- **`fleet.yaml`** schema → `src/fleet.rs`
- **Daemon entry** → `src/main.rs::run_app` → `src/daemon/mod.rs`
- **Agent spawn** → `src/agent.rs` + `src/bootstrap/agent_resolve.rs`
- **MCP tool dispatch** → `src/mcp/handlers/mod.rs` (one match arm per tool)
- **State classifier** → `src/state.rs` (regex-based, behavioral inference shadow lives in `src/behavioral.rs`)
- **TUI render** → `src/render.rs` + `src/app/`

## Process disciplines

`docs/FLEET-DEV-PROTOCOL-v1.md` is the contract:

- **§0 KISS** — every PR must answer "what real problem does this solve?"
- **§3.5.10** wire-format external fixture — never test mock-against-mock
- **§3.5.11** test-first commit order — RED commit before GREEN
- **§3.5.12** deferred-defense gate — no "we'll fix it later" without P0 trigger + SLA
- **§3.6** async pipeline — orchestrator owns `watch_ci`, impl/reviewer never block on each other

## What's deliberately absent

Removed Sprint 29 (~5 400 LOC net delete): self-healing supervisor, slow-loris timeout, RBAC outbound capability layer, hot-reload engine, working-directory symlink validation, constant-time cookie compare, frame-size env override. All confirmed unnecessary under the single-operator threat model. `docs/archived/audit-over-engineering-2026-04-28.md` documents the reasoning.
