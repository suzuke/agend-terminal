# Architecture Quick Start

500-word newcomer doc. Read this first; `architecture.md` for depth.

## What this is

Single-user fleet manager for AI coding agents (Claude Code / Kiro CLI /
Codex / OpenCode / Antigravity CLI / Grok Build). It spawns each agent in
its own PTY, exposes the fleet in a Ratatui TUI, bridges operator traffic to
Telegram or Discord, and lets agents coordinate through the daemon's MCP
surface.

Threat model: **localhost-only, single operator**. Cookie file
`$AGEND_HOME/run/<pid>/api.cookie` (mode 0600) gates the API; anyone who can
read it already owns the user account. `$AGEND_HOME` normally resolves to
`~/.agend`, with legacy `~/.agend-terminal` fallback. Defenses past that point
are intentionally minimal (KISS, see §0 of `FLEET-DEV-PROTOCOL.md`).

## Binaries

- **`agend-terminal`** — daemon + TUI + CLI. The process does not supervise
  its own unexpected crashes; `agend-terminal service install` delegates that
  job to launchd, systemd, or Task Scheduler.
- **`agend-mcp-bridge`** — zero-state stdio↔TCP relay spawned by Claude Code as its MCP server. Forwards `tools/call` to the daemon's `/mcp` API; everything else (`initialize`, `tools/list`) is local. See `src/bin/agend-mcp-bridge.rs`.
- **`agend-git`** — PATH shim that enforces binding, protected-ref, and
  worktree policy for spawned agents.
- **`agentic-git`** — vendored, flag-gated alternative git shim.

## Daemon process model

```
agend-terminal app
    ├── daemon (src/daemon/) — registry, event loop, watchdog
    ├── PTY child × N (src/agent/) — one per fleet.yaml instance
    │     └── shells out to agent backend (claude / kiro / codex / ...)
    ├── TUI session (src/app/) — local user view
    ├── Channel adapters (src/channel/) — operator + Telegram / Discord
    ├── API server (src/api/) — TCP loopback, cookie-authed
    └── MCP server (src/mcp/) — JSON-RPC over the API socket
```

Agent crashes are detected and respawned with bounded health budgets and
session resume where the backend supports it. An unexpected daemon crash is a
different lifecycle boundary: install the OS service integration when automatic
daemon restart is required. Controlled owner-restart uses the successor-handoff
path; it is not a general crash supervisor.

## How code talks to itself

| Path | Used by |
|---|---|
| **`reply` / `react`** | Agent → operator (current channel) |
| **`send`** (unified, replaces 5 old tools) | Agent → agent (kind=task/report/query/update) |
| **`inbox`** | Agent pulls own pending messages |
| **`task` / `decision` / `team` / `schedule` / `ci` / `repo` / `health` / `deployment`** | Action-based CRUD per domain |
| Direct daemon API | TUI sessions, daemon components, bridges |

There are **32 MCP tools** at `main@1d83b423` (2026-07-16).
`src/mcp/registry.rs` is the registry; invariants in `src/mcp/tools.rs` and the
registry tests pin the count and keep `docs/MCP-TOOLS*.md` synchronized.

## Where to start reading

- **`fleet.yaml`** schema → `src/fleet/`
- **App-mode entry** → `src/main.rs` command dispatch → `src/app/mod.rs`
- **Headless daemon** → `src/daemon/mod.rs`
- **Agent spawn** → `src/agent/mod.rs` + `src/bootstrap/agent_resolve.rs`
- **MCP tool dispatch** → `src/mcp/handlers/mod.rs::handle_tool_with_runtime`
  → `src/mcp/handlers/dispatch.rs` (registry entries live in
  `src/mcp/registry.rs`)
- **State classifier** → `src/state/`, `src/backend_profile.rs`, and
  `src/behavioral.rs`
- **TUI render** → `src/render/` + `src/app/`

## Process disciplines

`docs/FLEET-DEV-PROTOCOL.md` is the contract:

- **§0 KISS** — every PR must answer "what real problem does this solve?"
- **§3.9** external fixtures — test through the real production entry point
- **§3.10** test-first commit order — RED commit before GREEN
- **§3.11** deferred-defense gate — no indefinite unevidenced deferral
- **§7 / §12** async pipeline — use `ci({action:"watch", ...})`, then perform
  the required one-shot merge-gate verification

## What's deliberately absent

There is no multi-tenant RBAC layer and no in-process crash-supervisor loop for
the daemon. The localhost, single-operator threat model keeps those concerns at
the OS service and user-account boundary. Historical removals and their original
rationale live in `docs/archived/audit-over-engineering-2026-04-28.md`; do not
use that dated audit as a map of current modules.
