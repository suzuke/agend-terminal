# Architecture Groups Audit — agend-terminal

**Date**: 2026-04-30
**Commit**: `9061a1b` (origin/main)
**Coverage tool**: `cargo llvm-cov 0.8.5` — overall **73.1%** (28247/38667 lines)
**Total src/ LOC**: ~64,665 (including inline tests)

---

## Group 1 — Agent State Classifier

**Purpose**: Detect agent state (Thinking/ToolUse/Idle/Ready/etc.) from PTY screen content via regex pattern matching + expiry logic.

**Modules**:
- `src/state.rs` (2674 LOC) — `StateTracker`, `StatePatterns`, `AgentState` enum, `classify_pty_output`, per-backend pattern tables, latched-state expiry
- `src/behavioral.rs` (404 LOC) — `BehavioralConfig` per-backend tuning (silence thresholds, cursor query support)
- `src/health.rs` (862 LOC) — `HealthTracker`, `HealthState`, `BlockedReason`, hang detection, health classification

**Boundary**:
- Callers: `agent.rs` (feeds PTY output → `state.feed()`), `daemon/supervisor.rs` (reads state for health decisions), `render.rs` (reads state for UI color), `api/handlers/query.rs` (exposes state via API)
- Calls out to: nothing external (self-contained pattern matching)

**LOC**: ~3,940
**Coverage**: state.rs 92.8%, behavioral.rs 90.8%, health.rs 93.9% — **avg ~92.5%**

**Health**: Excellent post-Sprint 34. All 5 backend thinking patterns verified against real PTY captures. ToolUse anchored with `(?m)^`. RateLimit has 5-min self-recovery. 50+ inline tests. No significant smells.

**Priority**: **L** — recently overhauled (Sprint 34), high coverage, stable.

---

## Group 2 — Agent Lifecycle & Process Management

**Purpose**: Spawn, monitor, restart, and kill agent processes. PTY management, VTerm screen tracking.

**Modules**:
- `src/agent.rs` (1244 LOC) — `AgentHandle`, `AgentCore`, `spawn_agent`, `inject_to_agent`, registry management
- `src/agent_ops.rs` (417 LOC) — `send_to`, `save_metadata`, `cleanup_working_dir`
- `src/vterm.rs` (1095 LOC) — `VTerm` wrapper around alacritty_terminal, `read_scrollback`, `tail_lines`, `dump_screen`
- `src/process.rs` (53 LOC) — `kill_process`, `killpg`, liveness check
- `src/backend.rs` (1055 LOC) — `Backend` enum, `SpawnMode`, preset configs
- `src/backend_harness.rs` (311 LOC) — PTY byte delivery tests, `verify_tcgetpgrp`

**Boundary**:
- Callers: `daemon/mod.rs` (orchestrates lifecycle), `api/handlers/instance.rs` (spawn/kill API), `mcp/handlers/instance.rs` (MCP tool dispatch)
- Calls out to: `state.rs` (state tracking), `instructions.rs` (agend.md generation), `fleet.rs` (config)

**LOC**: ~4,175
**Coverage**: agent.rs 71.6%, agent_ops.rs 93.5%, vterm.rs 77.9%, process.rs 83.0%, backend.rs 90.7%, backend_harness.rs 43.4% — **avg ~76.8%**

**Health**: agent.rs at 71.6% is the weakest — complex spawn logic with PTY fd management, subscriber broadcast, crash handling. `backend_harness.rs` at 43.4% is test infrastructure itself (lower coverage expected). vterm.rs improved with Sprint 33 `read_scrollback` + Sprint 34 trim fix.

**Priority**: **M** — agent.rs is critical path but moderately covered; vterm.rs recently improved.

---

## Group 3 — Daemon Core (Supervisor / IPC / API Server)

**Purpose**: Long-running daemon process — API socket server, agent supervision, health monitoring, cron scheduling, CI watching.

**Modules**:
- `src/daemon/mod.rs` (1145 LOC) — `run_core`, main daemon loop, agent event handling
- `src/daemon/supervisor.rs` (222 LOC) — agent health supervision, restart decisions
- `src/daemon/lifecycle.rs` (178 LOC) — `delete_transaction`, instance teardown
- `src/daemon/heartbeat_pair.rs` (85 LOC) — MCP heartbeat tracking
- `src/daemon/watchdog.rs` (113 LOC) — daemon self-monitoring
- `src/daemon/ticker.rs` (92 LOC) — periodic tick driver
- `src/daemon/poll_reminder.rs` (178 LOC) — inbox poll reminders
- `src/daemon/cron_tick.rs` (250 LOC) — cron schedule execution
- `src/daemon/ci_watch.rs` (1744 LOC) — GitHub Actions CI monitoring
- `src/daemon/task_sweep.rs` (372 LOC) — PR merge → task done sweep
- `src/daemon/legacy_backfill.rs` (747 LOC) — migration backfill logic
- `src/daemon/tui_bridge.rs` (97 LOC) — TUI↔daemon bridge
- `src/api/mod.rs` (1563 LOC) — API socket server, method dispatch, `spawn_one`
- `src/api/handlers/` (1732 LOC total) — per-method handlers (instance, messaging, team, query, external, mcp_proxy)
- `src/ipc.rs` (151 LOC) — Unix socket IPC
- `src/framing.rs` (145 LOC) — NDJSON framing protocol

**Boundary**:
- Callers: `main.rs` (daemon entry), `cli.rs` (CLI commands)
- Calls out to: agent lifecycle, fleet config, inbox, tasks, decisions, channel layer

**LOC**: ~8,814
**Coverage**: daemon/mod.rs 77.1%, supervisor 72.1%, lifecycle 92.1%, heartbeat_pair 98.8%, watchdog 100%, ticker 96.7%, poll_reminder 97.2%, cron_tick 89.6%, ci_watch 92.6%, task_sweep 65.6%, legacy_backfill 53.5%, tui_bridge 14.4%, api/mod.rs 91.4%, framing 97.2%, ipc 85.4% — **avg ~81.5%**

**Health**: `legacy_backfill.rs` at 53.5% is migration code that may be removable. `tui_bridge.rs` at 14.4% is thin glue. `task_sweep.rs` at 65.6% handles PR-merge automation — moderate risk. `daemon/mod.rs` at 77.1% is the core loop — complex but reasonably covered.

**Priority**: **M** — large surface, mostly well-covered, but legacy_backfill and task_sweep are debt pockets.

---

## Group 4 — MCP Layer (Tools / Handlers / Inbox)

**Purpose**: MCP protocol implementation — tool definitions, handler dispatch, inbox message delivery, unified send routing.

**Modules**:
- `src/mcp/tools.rs` (326 LOC) — tool definitions JSON, count invariant
- `src/mcp/handlers/mod.rs` (90 LOC) — tool dispatch router
- `src/mcp/handlers/comms.rs` (700 LOC) — unified send, broadcast, inbox, request_information
- `src/mcp/handlers/instance.rs` (564 LOC) — create/delete/replace/interrupt/pane_snapshot
- `src/mcp/handlers/task.rs` (58 LOC) — task/team tool handlers
- `src/mcp/handlers/schedule.rs` (21 LOC) — schedule tool handler
- `src/mcp/handlers/channel.rs` (39 LOC) — channel/reply tool handler
- `src/mcp/handlers/ci.rs` (105 LOC) — CI watch tool handler
- `src/mcp/handlers/tests.rs` (1776 LOC) — comprehensive handler tests
- `src/mcp/mod.rs` (235 LOC) — MCP server entry, NDJSON session
- `src/mcp_config.rs` (1132 LOC) — MCP server config, tool filtering
- `src/inbox.rs` (2408 LOC) — inbox JSONL storage, delivery, drain, threading
- `src/bin/agend-mcp-bridge.rs` (228 LOC) — MCP bridge binary

**Boundary**:
- Callers: `api/mod.rs` (MCP_TOOL dispatch), `agend-mcp-bridge` binary (standalone MCP server)
- Calls out to: `api::call()` for daemon operations, `inbox` for message storage, `agent_ops` for send fallback

**LOC**: ~7,682
**Coverage**: tools 98.2%, handlers/mod 98.9%, comms 86.9%, instance 47.5%, task 62.1%, schedule 28.6%, channel 10.3%, ci 32.4%, mcp/mod 90.2%, mcp_config 94.9%, inbox 95.0%, bridge 73.7% — **avg ~73.2%**

**Health**: `handlers/instance.rs` at 47.5% is concerning — it's the MCP handler for create/delete/replace/pane_snapshot. `handlers/channel.rs` at 10.3% and `handlers/schedule.rs` at 28.6% are very low. `handlers/ci.rs` at 32.4% is the CI watch tool. These low-coverage handlers are the main debt in this group.

**Priority**: **H** — MCP is the primary agent-facing interface; low coverage in instance/channel/schedule/ci handlers is a production risk.

---

## Group 5 — Fleet Config & Instance Management

**Purpose**: Fleet configuration (fleet.yaml), team management (teams.json), instance metadata, instructions generation.

**Modules**:
- `src/fleet.rs` (1494 LOC) — `FleetConfig`, instance config, fleet.yaml parsing
- `src/teams.rs` (504 LOC) — team CRUD, `find_team_for`, team isolation support
- `src/instructions.rs` (1174 LOC) — `agend.md` steering file generation
- `src/identity.rs` (46 LOC) — `Sender` newtype for identity validation
- `src/instance_monitor.rs` (141 LOC) — instance metrics collection
- `src/fleet_broadcast.rs` (removed in Sprint 35 PR-7) — N/A

**Boundary**:
- Callers: `daemon/mod.rs`, `api/handlers/`, `mcp/handlers/`, `bootstrap/`
- Calls out to: `store.rs` (persistence), `agent.rs` (registry queries)

**LOC**: ~3,359
**Coverage**: fleet.rs 96.8%, teams.rs 92.9%, instructions.rs 98.1%, identity.rs 100%, instance_monitor.rs 97.2% — **avg ~97.0%**

**Health**: Excellent. Highest average coverage of any group. Fleet config parsing is well-tested. Team isolation gate (Sprint 37) added with 8 fixture tests. Instructions generation at 98.1% is thorough.

**Priority**: **L** — stable, high coverage, recently validated.

---

## Group 6 — Persistence & Audit Layer

**Purpose**: On-disk state management — tasks, decisions, schedules, deployments, event log, store abstraction.

**Modules**:
- `src/tasks.rs` (2170 LOC) — task board CRUD, lifecycle
- `src/task_events.rs` (1722 LOC) — task event JSONL stream
- `src/decisions.rs` (386 LOC) — decision panel CRUD
- `src/schedules.rs` (925 LOC) — cron schedule management
- `src/deployments.rs` (737 LOC) — deployment template management
- `src/event_log.rs` (139 LOC) — audit event log (append-only JSONL)
- `src/store.rs` (257 LOC) — generic JSON store with schema versioning
- `src/dispatch_tracking.rs` (178 LOC) — review dispatch tracking
- `src/snapshot.rs` (143 LOC) — fleet snapshot for status summary
- `src/status_summary.rs` (239 LOC) — human-readable status builder

**Boundary**:
- Callers: `mcp/handlers/` (task/decision/schedule tools), `daemon/` (task_sweep, cron_tick)
- Calls out to: filesystem (JSONL files), `store.rs` (generic persistence)

**LOC**: ~6,896
**Coverage**: tasks 95.7%, task_events 95.7%, decisions 94.0%, schedules 92.8%, deployments 92.7%, event_log 84.2%, store 96.9%, dispatch_tracking 97.2%, snapshot 98.6%, status_summary 74.1% — **avg ~92.2%**

**Health**: Very good. All core persistence modules >92%. `status_summary.rs` at 74.1% is the weakest — it's a read-only summary builder, low risk. `event_log.rs` at 84.2% is simple append-only.

**Priority**: **L** — stable, high coverage, well-tested persistence layer.

---

## Group 7 — TUI / App Layer

**Purpose**: Terminal UI — ratatui rendering, pane layout, mouse handling, keyboard events, overlay panels.

**Modules**:
- `src/app/mod.rs` (1039 LOC) — `run_app`, main TUI event loop
- `src/app/overlay.rs` (1196 LOC) — overlay panels (help, search, meta)
- `src/app/tui_events.rs` (697 LOC) — keyboard/mouse event dispatch
- `src/app/mouse.rs` (439 LOC) — mouse click/drag handling
- `src/app/pane_factory.rs` (395 LOC) — pane creation from config
- `src/app/session.rs` (288 LOC) — session save/restore
- `src/app/commands.rs` (244 LOC) — TUI command parsing
- `src/app/dispatch.rs` (242 LOC) — TUI event dispatch
- `src/app/api_server.rs` (84 LOC) — embedded API server for TUI mode
- `src/app/telegram_hooks.rs` (54 LOC) — telegram integration hooks
- `src/render.rs` (2385 LOC) — ratatui rendering, pane borders, tab bar, status bar
- `src/layout.rs` (2106 LOC) — pane layout tree, split/merge, tab management
- `src/tui.rs` (140 LOC) — terminal setup/teardown
- `src/keybinds.rs` (173 LOC) — keybinding configuration

**Boundary**:
- Callers: `main.rs` (TUI entry point)
- Calls out to: `agent.rs` (registry), `daemon/` (via API), `vterm.rs` (screen content)

**LOC**: ~9,482
**Coverage**: app/mod 15.3%, overlay 41.5%, tui_events 29.1%, mouse 47.6%, pane_factory 23.8%, session 0%, commands 0%, dispatch 0%, api_server 0%, telegram_hooks 0%, render 40.0%, layout 75.5%, tui 20.0%, keybinds 68.8% — **avg ~25.8%**

**Health**: **Worst coverage in the codebase.** 5 files at 0% (session, commands, dispatch, api_server, telegram_hooks). app/mod at 15.3% is the main event loop — critical but untested. render.rs at 40% is the largest rendering file. Layout at 75.5% is the best in this group. TUI code is inherently hard to test (requires terminal emulation), but the 0% files indicate no attempt at unit testing even for pure-logic portions.

**Priority**: **H** — lowest coverage, largest LOC, critical user-facing surface. However, TUI testing ROI is debatable — many of these are thin glue over ratatui.

---

## Group 8 — Channel Layer (Telegram / Discord)

**Purpose**: External messaging channel integration — Telegram bot, Discord bot, channel abstraction.

**Modules**:
- `src/channel/telegram.rs` (4205 LOC) — Telegram bot integration, topic routing, message handling
- `src/channel/discord.rs` (1456 LOC) — Discord bot integration (not in coverage — likely behind feature flag)
- `src/channel/mod.rs` (214 LOC) — channel trait, active channel selection
- `src/channel/ux_event.rs` (1050 LOC) — UX event types, action selection
- `src/channel/sink_registry.rs` (66 LOC) — event sink registry
- `src/channel/binding.rs` (77 LOC) — channel binding references
- `src/channel/auth.rs` (44 LOC) — user authentication
- `src/channel/caps.rs` (41 LOC) — channel capabilities
- `src/channel/contract.rs` (127 LOC) — channel contract tests
- `src/channel/event.rs` (64 LOC) — channel event types

**Boundary**:
- Callers: `daemon/mod.rs` (channel init), `app/telegram_hooks.rs` (TUI hooks)
- Calls out to: `inbox.rs` (message delivery), `agent.rs` (inject), teloxide/serenity crates

**LOC**: ~7,344
**Coverage**: telegram 62.1%, mod 70.1%, ux_event 97.8%, sink_registry 100%, binding 92.2%, auth 100%, caps 100%, contract 98.4%, event 100% — **avg ~80.1%** (excluding discord.rs which is not in coverage output)

**Health**: telegram.rs at 62.1% is the largest file in the codebase (4205 LOC) and moderately covered. The channel abstraction layer (ux_event, contract, caps, auth) is excellently tested. discord.rs appears to be behind a feature flag and not measured.

**Priority**: **M** — telegram.rs is large and moderately covered; channel abstraction is solid.

---

## Group 9 — CLI / Entry Points / Bootstrap

**Purpose**: CLI argument parsing, daemon/TUI/MCP entry points, bootstrap sequence.

**Modules**:
- `src/main.rs` (724 LOC) — main entry, subcommand dispatch
- `src/cli.rs` (381 LOC) — clap CLI definition
- `src/connect.rs` (136 LOC) — `agend-terminal connect` subcommand
- `src/quickstart.rs` (324 LOC) — quickstart wizard
- `src/bugreport.rs` (188 LOC) — bug report generator
- `src/bootstrap/mod.rs` (234 LOC) — bootstrap sequence
- `src/bootstrap/agent_resolve.rs` (129 LOC) — agent binary resolution
- `src/bootstrap/daemon_spawn.rs` (47 LOC) — daemon process spawning
- `src/bootstrap/fleet_normalize.rs` (120 LOC) — fleet config normalization
- `src/bootstrap/doctor.rs` (114 LOC) — system health check
- `src/bootstrap/signals.rs` (44 LOC) — signal handler setup
- `src/bootstrap/telegram_init.rs` (19 LOC) — telegram initialization
- `src/bridge_client.rs` (35 LOC) — MCP bridge client
- `src/admin.rs` (200 LOC) — admin commands
- `src/auth_cookie.rs` (252 LOC) — authentication cookie management
- `src/notification_queue.rs` (108 LOC) — compose-aware notification queue
- `src/verify.rs` (731 LOC) — fleet verification / invariant checks
- `src/worktree.rs` (355 LOC) — git worktree management
- `src/worktree_cleanup.rs` (250 LOC) — worktree cleanup on delete
- `src/thread_census.rs` (83 LOC) — thread tracking

**Boundary**:
- Callers: OS (binary entry point)
- Calls out to: daemon, TUI, MCP server, bootstrap sequence

**LOC**: ~4,524
**Coverage**: main 30.9%, cli 0%, connect 0%, quickstart 25.6%, bugreport 28.7%, bootstrap/mod 90.2%, agent_resolve 79.1%, daemon_spawn 0%, fleet_normalize 74.2%, doctor 91.2%, signals 9.1%, telegram_init 0%, bridge_client 0%, admin 83.0%, auth_cookie 94.8%, notification_queue 88.9%, verify 34.8%, worktree 80.8%, worktree_cleanup 98.8%, thread_census 94.0% — **avg ~52.8%**

**Health**: Mixed. CLI entry points (cli.rs, connect.rs, daemon_spawn.rs) at 0% are expected — they're thin wrappers. `verify.rs` at 34.8% is the fleet invariant checker — important but under-tested. `quickstart.rs` at 25.6% is the wizard — low priority. `auth_cookie.rs` at 94.8% and `worktree_cleanup.rs` at 98.8% are well-tested.

**Priority**: **L** — entry points are inherently hard to unit test; the important logic modules (auth_cookie, worktree, bootstrap) are well-covered.

---

## Summary Table

| # | Group | LOC | Avg Coverage | Priority | Key Risk |
|---|-------|-----|-------------|----------|----------|
| 1 | Agent State Classifier | 3,940 | 92.5% | L | Recently overhauled (Sprint 34) |
| 2 | Agent Lifecycle & Process | 4,175 | 76.8% | M | agent.rs 71.6% — complex spawn logic |
| 3 | Daemon Core | 8,814 | 81.5% | M | legacy_backfill 53.5%, task_sweep 65.6% |
| 4 | MCP Layer | 7,682 | 73.2% | **H** | instance handler 47.5%, channel/schedule/ci <33% |
| 5 | Fleet Config & Management | 3,359 | 97.0% | L | Stable, high coverage |
| 6 | Persistence & Audit | 6,896 | 92.2% | L | Stable, well-tested |
| 7 | TUI / App Layer | 9,482 | 25.8% | **H** | 5 files at 0%, app/mod 15.3% |
| 8 | Channel Layer | 7,344 | 80.1% | M | telegram.rs 62.1% (4205 LOC) |
| 9 | CLI / Entry Points | 4,524 | 52.8% | L | Entry points inherently hard to test |

---

## Recommended Processing Order

1. **Group 4 — MCP Layer** (H) — Primary agent-facing interface. Low coverage in instance/channel/schedule/ci handlers is a production risk. MCP is the wire surface that all agents interact with; bugs here affect every fleet operation. Unblocks confidence in Groups 2 and 3 which depend on MCP handlers.

2. **Group 7 — TUI / App Layer** (H) — Lowest coverage in the codebase, but ROI is debatable. Recommend focusing on pure-logic portions (layout.rs, pane_factory.rs, session.rs) rather than rendering code. The 0% files (commands, dispatch, session) likely have extractable logic worth testing.

3. **Group 8 — Channel Layer** (M) — telegram.rs at 4205 LOC / 62.1% is the single largest file. Refactoring into smaller modules + adding coverage would reduce incident surface. Channel abstraction layer is already solid.

4. **Group 2 — Agent Lifecycle** (M) — agent.rs at 71.6% is the core spawn/monitor/inject path. Improving coverage here directly reduces production incident risk.

5. **Group 3 — Daemon Core** (M) — Large but mostly well-covered. Focus on legacy_backfill.rs (removable?) and task_sweep.rs (65.6%).

6. **Groups 1, 5, 6, 9** (L) — Stable, well-covered, or inherently hard to test. Address opportunistically during related sprints.
