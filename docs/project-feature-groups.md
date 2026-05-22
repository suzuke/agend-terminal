# agend-terminal Feature Group Classification Report

> Generated: 2026-05-23 | Source: `main` branch (HEAD e822cc7+)
> Total: ~144,365 LOC across ~200 `.rs` files

---

## 1. Core Daemon (~13,200 LOC)

The central event loop, tick-based subsystem orchestration, and process supervision.

| File | LOC | Role |
|------|-----|------|
| `src/daemon/mod.rs` | 2,214 | Main daemon loop (`run`, `run_with_prepared`), `AgentConfig`, spawn paths |
| `src/daemon/supervisor.rs` | 2,080 | Agent process supervision, crash detection, respawn |
| `src/daemon/ticker.rs` | 236 | `DaemonTicker` — periodic tick scheduling |
| `src/daemon/per_tick/mod.rs` | 288 | Per-tick dispatch coordinator |
| `src/daemon/per_tick/recovery_dispatcher.rs` | 1,224 | Recovery dispatch on each tick |
| `src/daemon/per_tick/watchdog.rs` | 123 | Per-tick watchdog checks |
| `src/daemon/per_tick/hang_detection.rs` | 121 | Hang detection per tick |
| `src/daemon/per_tick/snapshot.rs` | 138 | Periodic state snapshot |
| `src/daemon/per_tick/external_liveness.rs` | 127 | External agent liveness probes |
| `src/daemon/per_tick/log_rotation.rs` | 73 | Log rotation per tick |
| `src/daemon/router.rs` | 245 | `AgentSubscription`, message routing between agents |
| `src/daemon/lifecycle.rs` | 354 | `SpawnRollback`, delete transaction, child exit wait |
| `src/daemon/restart.rs` | 179 | Supervised restart detection |
| `src/daemon/tui_bridge.rs` | 192 | TUI↔daemon bridge |
| `src/daemon/watchdog.rs` | 189 | Top-level watchdog pass |
| `src/daemon/idle_watchdog.rs` | 836 | Idle watchdog tracking |
| `src/daemon/heartbeat_pair.rs` | 392 | Heartbeat pair coordination |
| `src/daemon/anti_stall.rs` | 610 | Anti-stall detection for stuck agents |
| `src/daemon/dedup_state.rs` | 1,428 | Message deduplication state |
| `src/daemon/notification_dedup.rs` | 284 | Notification-level dedup |
| `src/daemon/utils.rs` | 56 | Daemon utility helpers |
| `src/runtime.rs` | 642 | Tokio runtime setup |
| `src/main.rs` | 1,198 | CLI arg parse → daemon/TUI/CLI dispatch |

**Key structs**: `AgentConfig`, `DaemonTicker`, `SpawnRollback`, `AgentSubscription`
**Cross-group coupling**: Agent management (spawn/respawn), Fleet/Config (fleet.yaml resolution), Communication (inbox delivery), Health (state tracking)

---

## 2. Agent Management (~12,400 LOC)

Agent lifecycle (spawn, monitor, write, dismiss), worktree pool, and backend integration.

| File | LOC | Role |
|------|-----|------|
| `src/agent.rs` | 2,943 | `AgentCore`, `AgentHandle`, `SpawnConfig`, `spawn_agent`, registry locking |
| `src/agent_ops.rs` | 819 | Metadata R/W, `send_to`, branch validation, `cleanup_working_dir` |
| `src/backend.rs` | 1,531 | `Backend` enum, `SpawnMode`, `ResumeMode`, `BackendPreset` |
| `src/backend_harness.rs` | 494 | `CapabilityMatrix`, backend capability probing |
| `src/state.rs` | 4,636 | `AgentState` enum (22 variants), `StateTracker`, PTY output classification |
| `src/worktree.rs` | 850 | `worktree_path`, worktree creation/management |
| `src/worktree_pool.rs` | 2,053 | Worktree pool allocation, lease management |
| `src/worktree_cleanup.rs` | 545 | `WorktreeEntry`, auto-cleanup, sweep from registry |
| `src/binding.rs` | 373 | Worktree binding (`bind`/`unbind`), hook installation, orphan reconciliation |
| `src/instance_monitor.rs` | 212 | Instance process monitoring |
| `src/vterm.rs` | 1,510 | `VTerm` — virtual terminal emulation, PTY write listener |
| `src/process.rs` | 182 | Low-level process helpers |
| `src/capture.rs` | 827 | `CaptureSink`, capture promote logic |
| `src/behavioral.rs` | 1,370 | Behavioral pattern detection for agents |

**Key structs**: `AgentCore`, `AgentHandle`, `SpawnConfig`, `Backend`, `AgentState`, `StateTracker`, `VTerm`, `WorktreeEntry`
**Cross-group coupling**: Core daemon (spawn integration), Fleet/Config (instance resolution), Skill system (`install_for_agent` at spawn), Communication (agent I/O)

---

## 3. Communication (~11,800 LOC)

Inbox system, channel abstraction (Telegram, Discord), notifications, and dispatch tracking.

| File | LOC | Role |
|------|-----|------|
| `src/inbox.rs` | 4,384 | `InboxMessage`, `enqueue`/`drain`/`deliver`, sweep, thread queries |
| `src/channel/mod.rs` | 683 | `Channel` trait, `gated_notify`, `ChannelKind`, `TopicOutcome` |
| `src/channel/telegram/inbound.rs` | 703 | Telegram inbound message handling |
| `src/channel/telegram/reply.rs` | 591 | Telegram reply formatting |
| `src/channel/telegram/send.rs` | 302 | Telegram message sending |
| `src/channel/telegram/adapter.rs` | 580 | Telegram channel adapter |
| `src/channel/telegram/error.rs` | 581 | Telegram error handling |
| `src/channel/telegram/state.rs` | 320 | Telegram connection state |
| `src/channel/telegram/topic_registry.rs` | 453 | Telegram topic→agent mapping |
| `src/channel/telegram/bot_api.rs` | 150 | Bot API low-level calls |
| `src/channel/telegram/bootstrap.rs` | 206 | Telegram channel bootstrap |
| `src/channel/telegram/notify.rs` | 127 | Telegram notification formatting |
| `src/channel/telegram/ux_sink.rs` | 186 | UX event → Telegram sink |
| `src/channel/telegram/creds.rs` | 64 | Telegram credentials |
| `src/channel/telegram/mod.rs` | 32 | Telegram module root |
| `src/channel/discord.rs` | 1,457 | `DiscordChannel`, `DiscordState`, Discord binding |
| `src/channel/event.rs` | 227 | Channel event types |
| `src/channel/contract.rs` | 269 | Channel contract definitions |
| `src/channel/dedup.rs` | 442 | Channel-level message dedup |
| `src/channel/ux_event.rs` | 1,068 | UX event system |
| `src/channel/binding.rs` | 160 | Channel binding logic |
| `src/channel/auth.rs` | 106 | Channel authentication |
| `src/channel/caps.rs` | 166 | Channel capabilities |
| `src/channel/sink_registry.rs` | 181 | Sink registry for channel outputs |
| `src/notification_queue.rs` | 229 | `QueuedNotification`, composing detection, queue drain |
| `src/dispatch_tracking.rs` | 330 | Dispatch tracking for task delegation |

**Key structs**: `InboxMessage`, `Channel` (trait), `DiscordChannel`, `QueuedNotification`, `NotifySource`
**Cross-group coupling**: Core daemon (message routing), Agent management (notify/deliver to agents), Fleet/Config (channel config), MCP (send/inbox handlers)

---

## 4. TUI / Render (~7,900 LOC)

Terminal UI application, layout engine, rendering, and user interaction.

| File | LOC | Role |
|------|-----|------|
| `src/app/mod.rs` | 1,261 | `run()` — TUI application entry, main event loop |
| `src/app/overlay.rs` | 1,267 | Overlay dialogs (menu, rename, confirm) |
| `src/app/mouse.rs` | 1,059 | Mouse event handling |
| `src/app/session.rs` | 929 | Session management in TUI |
| `src/app/tui_events.rs` | 739 | TUI keyboard/event processing |
| `src/app/dispatch.rs` | 450 | TUI action dispatch |
| `src/app/commands.rs` | 423 | Command palette handling |
| `src/app/tui_spawn.rs` | 420 | TUI-driven agent spawn |
| `src/app/pane_factory.rs` | 589 | Pane creation factory |
| `src/app/telegram_hooks.rs` | 85 | Telegram integration hooks in TUI |
| `src/app/api_server.rs` | 137 | Embedded API server for TUI mode |
| `src/render/core_render.rs` | 928 | Core rendering logic, `state_color` |
| `src/render/panels.rs` | 708 | Task/decision panel rendering |
| `src/render/panels_fleet.rs` | 257 | Fleet view panel |
| `src/render/overlay.rs` | 428 | Overlay rendering (menus, dialogs, help) |
| `src/render/border.rs` | 264 | Border rendering with state indicators |
| `src/render/scratch.rs` | 62 | Scratch shell rendering |
| `src/render/mod.rs` | 16 | Render module root |
| `src/layout/mod.rs` | 378 | `Layout` struct, tab bar, pane resize |
| `src/layout/tab.rs` | 492 | `Tab` struct, drag-tab support |
| `src/layout/tree.rs` | 390 | `PaneNode` tree, split directions, pane swap |
| `src/layout/split.rs` | 449 | Split ratio calculations, border hit detection |
| `src/layout/pane.rs` | 186 | `Pane`, `PaneSource`, selection |
| `src/layout/preset.rs` | 122 | Layout presets |
| `src/tui.rs` | 239 | TUI attach logic, key→byte conversion |
| `src/keybinds.rs` | 374 | Keybind definitions |
| `src/mouse_forward.rs` | 88 | Mouse event forwarding |

**Key structs**: `Layout`, `Tab`, `PaneNode`, `Pane`, `VTerm`
**Cross-group coupling**: Agent management (registry for rendering), State (AgentState colors), Communication (UX events)

---

## 5. Backend Integration (~2,025 LOC)

Backend-specific spawn/resume logic and instruction generation. (Partially overlaps with Agent Management.)

| File | LOC | Role |
|------|-----|------|
| `src/backend.rs` | 1,531 | `Backend` enum (Claude/Codex/Gemini/Kiro/OpenCode/Aider), spawn commands, resume logic |
| `src/backend_harness.rs` | 494 | Capability probing (ESC-stop, byte delivery, tcgetpgrp) |

**Key structs**: `Backend`, `SpawnMode`, `ResumeMode`, `BackendPreset`, `CapabilityMatrix`
**Cross-group coupling**: Agent management (spawn_agent uses Backend), Skill system (per-backend skill install)

---

## 6. Fleet / Config (~8,500 LOC)

Fleet YAML configuration, team management, instance resolution, and bootstrap.

| File | LOC | Role |
|------|-----|------|
| `src/fleet.rs` | 3,759 | `FleetConfig`, `InstanceConfig`, YAML CRUD, team YAML, field merge |
| `src/teams.rs` | 1,260 | `Team` struct, create/delete/list/update, orchestrator resolution |
| `src/mcp_config.rs` | 1,213 | MCP configuration management |
| `src/instructions.rs` | 1,292 | Agent instruction generation (system prompts) |
| `src/bootstrap/mod.rs` | 824 | Bootstrap orchestration, `OwnedFleet` |
| `src/bootstrap/agent_resolve.rs` | 547 | `resolve_one` → `AgentDef`, auto-worktree detection |
| `src/bootstrap/fleet_normalize.rs` | 188 | Fleet YAML normalization |
| `src/bootstrap/canonical_hygiene.rs` | 508 | Canonical path hygiene |
| `src/bootstrap/daemon_spawn.rs` | 115 | Daemon process spawning |
| `src/bootstrap/signals.rs` | 162 | Signal handling setup |
| `src/bootstrap/attach_detect.rs` | 230 | Attach detection for existing daemons |
| `src/bootstrap/telegram_init.rs` | 284 | Telegram bootstrap init |
| `src/bootstrap/doctor.rs` | 571 | `agend doctor` health checks |
| `src/bootstrap/doctor_topics.rs` | 533 | Doctor topic-level checks |
| `src/daemon_config.rs` | 61 | Daemon configuration |
| `src/identity.rs` | 98 | Instance identity management |

**Key structs**: `FleetConfig`, `InstanceConfig`, `ResolvedInstance`, `Team`, `TeamConfig`
**Cross-group coupling**: Core daemon (agent resolution at boot), Agent management (spawn config), Communication (channel config), Skill system (skills allowlist)

---

## 7. Retention / GC (~2,300 LOC)

Garbage collection, stale data cleanup, and retention policies.

| File | LOC | Role |
|------|-----|------|
| `src/daemon/retention/worktrees.rs` | 604 | Worktree retention sweep |
| `src/daemon/retention/decisions.rs` | 311 | Decision record retention |
| `src/daemon/retention/pending_dispatches.rs` | 131 | Pending dispatch cleanup |
| `src/daemon/retention/mod.rs` | 53 | Retention module root |
| `src/daemon/boot_sweep.rs` | 558 | Boot-time state cleanup |
| `src/daemon/legacy_backfill.rs` | 722 | Legacy data migration/backfill |
| `src/branch_sweep.rs` | 875 | Git branch sweep (stale branch cleanup) |
| `src/daemon/waiting_on_stale.rs` | 249 | Stale `waiting_on` cleanup |
| `src/admin/cleanup_zombies.rs` | 611 | Zombie process cleanup |
| `src/admin/mod.rs` | 275 | Admin module root |

**Key structs**: `SweepConfig` (in task_sweep), `WorktreeEntry` (in worktree_cleanup)
**Cross-group coupling**: Core daemon (per-tick sweep), Agent management (worktree cleanup), Task system (task sweep)

---

## 8. CI/CD & PR State (~8,800 LOC)

CI watch (GitHub Actions polling), PR state tracking, and dispatch-idle nudges.

| File | LOC | Role |
|------|-----|------|
| `src/daemon/ci_watch/poller.rs` | 6,042 | CI watch poller — GitHub Actions polling, conflict alerts |
| `src/daemon/ci_watch/provider.rs` | 1,007 | CI provider abstraction |
| `src/daemon/ci_watch/sweep.rs` | 403 | CI watch entry sweep |
| `src/daemon/ci_watch/registry.rs` | 203 | CI watch registry |
| `src/daemon/ci_watch/migration.rs` | 304 | CI watch data migration |
| `src/daemon/ci_watch/watcher.rs` | 80 | Watcher entry definition |
| `src/daemon/ci_watch/mod.rs` | 44 | CI watch module root |
| `src/daemon/pr_state/mod.rs` | 2,427 | PR state tracking (open/merged/closed) |
| `src/daemon/pr_state/gh_poll.rs` | 533 | GitHub PR polling |
| `src/daemon/per_tick/ci_watch_poll.rs` | 75 | Per-tick CI poll trigger |
| `src/daemon/per_tick/pr_state_scan.rs` | 30 | Per-tick PR state scan trigger |
| `src/daemon/dispatch_idle/mod.rs` | 1,548 | Dispatch-idle detection and nudging |
| `src/daemon/dispatch_idle/fixup_nudge.rs` | 420 | Fixup-team specific idle nudge |
| `src/daemon/conflict_notify.rs` | 552 | Branch conflict notification |
| `src/daemon/decision_timeout.rs` | 650 | Decision timeout enforcement |
| `src/daemon/helper_staleness_watchdog.rs` | 328 | Helper staleness detection |

**Key structs**: `SweepConfig`, `TaskSweep`, CI watcher entries
**Cross-group coupling**: Communication (inbox notifications on CI events), Agent management (CI per-branch), Fleet/Config (repo settings)

---

## 9. Skill System (~1,200 LOC)

Skill management (add/remove/update/install), per-backend symlink installation, stage GC.

| File | LOC | Role |
|------|-----|------|
| `src/skills.rs` | 1,199 | `Skill`, `SkillsLock`, `install_for_agent`, `add`/`remove`/`update`/`update_all`, `cleanup_stale_stages` |

**Key structs**: `Skill`, `SkillsLock`, `SkillLockEntry`, `InstallOutcome`, `InstallMode`, `StageGcReport`
**Cross-group coupling**: Agent management (install at spawn), Fleet/Config (skills allowlist), Core daemon (install on respawn), CLI (skills subcommands)

---

## 10. MCP Layer (~14,500 LOC)

MCP tool definitions, handler implementations, and bridge.

| File | LOC | Role |
|------|-----|------|
| `src/mcp/tools.rs` | 493 | MCP tool schema definitions |
| `src/mcp/handlers/tests.rs` | 3,104 | MCP handler test suite |
| `src/mcp/handlers/dispatch_hook/tests.rs` | 2,362 | Dispatch hook tests |
| `src/mcp/handlers/ci/tests.rs` | 1,995 | CI handler tests |
| `src/mcp/handlers/dispatch_hook/mod.rs` | 1,281 | Dispatch hook logic (pre/post-dispatch validation) |
| `src/mcp/handlers/dispatch.rs` | 963 | MCP dispatch handler (send/broadcast) |
| `src/mcp/handlers/ci/mod.rs` | 971 | CI MCP handler (watch/unwatch) |
| `src/mcp/handlers/comms.rs` | 731 | Communication handlers (inbox/reply) |
| `src/mcp/handlers/worktree.rs` | 750 | Worktree MCP handlers |
| `src/mcp/handlers/force_release/gc.rs` | 626 | Force-release GC handler |
| `src/mcp/handlers/force_release/mod.rs` | 578 | Force-release handler |
| `src/mcp/handlers/binding_state.rs` | 598 | Binding state handler |
| `src/mcp/handlers/instance.rs` | 569 | Instance management handlers |
| `src/mcp/handlers/instance_lifecycle.rs` | 430 | Instance lifecycle (start/stop/delete) |
| `src/mcp/handlers/instance_spawn.rs` | 244 | Instance spawn handler |
| `src/mcp/handlers/anti_stall.rs` | 231 | Anti-stall handler |
| `src/mcp/handlers/channel.rs` | 137 | Channel handler |
| `src/mcp/handlers/sha_gate.rs` | 149 | SHA gate validation |
| `src/mcp/handlers/restart.rs` | 126 | Restart handler |
| `src/mcp/handlers/schedule.rs` | 30 | Schedule handler |
| `src/mcp/handlers/task.rs` | 78 | Task handler |
| `src/mcp/handlers/mod.rs` | 132 | Handler module root |
| `src/mcp/handlers/p0b_tests.rs` | 741 | P0b test suite |
| `src/mcp/handlers/instance_964_tests.rs` | 290 | Instance #964 regression tests |
| `src/mcp/handlers/channel_p0a_tests.rs` | 280 | Channel P0a tests |
| `src/mcp/handlers/comms_p0c_tests.rs` | 53 | Comms P0c tests |
| `src/mcp/mod.rs` | 19 | MCP module root |
| `src/bin/agend-mcp-bridge.rs` | 735 | MCP bridge binary |

**Key structs**: Tool schemas (JSON), handler functions
**Cross-group coupling**: Nearly all groups — MCP is the primary API surface for agents. Delegates to inbox, tasks, teams, worktree, CI, health, etc.

---

## 11. Shared Infrastructure & Other (~9,600 LOC)

Task/event persistence, API layer, diagnostics, and utilities.

| File | LOC | Role |
|------|-----|------|
| `src/tasks.rs` | 4,698 | `Task` CRUD, health response, orphan reconciliation |
| `src/task_events.rs` | 2,118 | Task event log (append-only event sourcing) |
| `src/daemon/task_sweep.rs` | 1,210 | Task sweep (stale task cleanup) |
| `src/daemon/task_progress.rs` | 473 | Task progress tracking |
| `src/decisions.rs` | 577 | Decision record CRUD |
| `src/schedules.rs` | 951 | Cron-style schedule management |
| `src/deployments.rs` | 2,132 | Deployment template management |
| `src/api/mod.rs` | 1,769 | Internal API server (`spawn_one`, HTTP handlers) |
| `src/api/handlers/messaging.rs` | 2,163 | API messaging handlers |
| `src/api/handlers/instance.rs` | 985 | API instance handlers (spawn/start/replace) |
| `src/api/handlers/team.rs` | 242 | API team handlers |
| `src/api/handlers/query.rs` | 77 | API query handlers |
| `src/api/handlers/external.rs` | 58 | External API handlers |
| `src/api/handlers/verify_push.rs` | 27 | Push verification |
| `src/api/handlers/mcp_proxy.rs` | 109 | MCP proxy handler |
| `src/api/handlers/mod.rs` | 119 | API handler module root |
| `src/api/request_dedup.rs` | 941 | API request deduplication |
| `src/store.rs` | 554 | JSON store utilities (`load`/`save`/`atomic_write`) |
| `src/logging.rs` | 713 | Logging infrastructure |
| `src/event_log.rs` | 215 | Event log management |
| `src/health.rs` | 1,428 | `HealthTracker`, `HealthState`, `BlockedReason` |
| `src/status_summary.rs` | 308 | Status summary generation |
| `src/claim_verifier.rs` | 1,569 | Claim verification logic |
| `src/verify.rs` | 769 | Verification utilities |
| `src/sync_audit.rs` | 226 | Sync audit checks |
| `src/snapshot.rs` | 200 | State snapshot |
| `src/error.rs` | 47 | Error types |
| `src/types.rs` | 59 | Shared type definitions |
| `src/paths.rs` | 23 | Path utilities |
| `src/lib.rs` | 20 | Library root |
| `src/framing.rs` | 212 | Message framing |
| `src/ipc.rs` | 240 | IPC socket management |
| `src/connect.rs` | 182 | Connection helpers |
| `src/bridge_client.rs` | 92 | MCP bridge client |
| `src/protocol.rs` | 102 | Protocol constants |
| `src/display_time.rs` | 152 | Time display formatting |
| `src/git_helpers.rs` | 90 | Git helper functions |
| `src/github_token.rs` | 371 | GitHub token management |
| `src/auth_cookie.rs` | 387 | Auth cookie handling |
| `src/sync.rs` | 18 | Sync primitives |
| `src/thread_census.rs` | 104 | Thread census for diagnostics |
| `src/daemon/canonical_drift.rs` | 123 | Canonical path drift detection |
| `src/daemon/mcp_registry_watcher.rs` | 262 | MCP registry file watcher |
| `src/daemon/poll_reminder.rs` | 263 | Poll reminder system |
| `src/daemon/per_tick/check_schedules.rs` | 78 | Schedule check per tick |
| `src/daemon/per_tick/inbox_maintenance.rs` | 147 | Inbox maintenance per tick |
| `src/daemon/per_tick/poll_reminder.rs` | 65 | Poll reminder per tick |
| `src/daemon/cron_tick.rs` | 405 | Cron job tick execution |
| `src/daemon/auto_release.rs` | 549 | Auto-release logic |

---

## 12. CLI / Quickstart (~2,000 LOC)

User-facing CLI subcommands and guided setup.

| File | LOC | Role |
|------|-----|------|
| `src/cli.rs` | 807 | CLI subcommands: doctor, capture, skills, doctor-topics |
| `src/quickstart.rs` | 1,411 | Guided quickstart wizard |
| `src/bugreport.rs` | 286 | Bug report generation |

**Cross-group coupling**: Fleet/Config (doctor checks), Skills (CLI subcommands), Backend (capture)

---

## 13. System Service & Tray (~1,300 LOC)

OS-level service registration and system tray icon.

| File | LOC | Role |
|------|-----|------|
| `src/service/mod.rs` | 562 | Service management (install/uninstall/start/stop) |
| `src/service/macos.rs` | 117 | macOS launchd integration |
| `src/service/linux.rs` | 118 | Linux systemd integration |
| `src/service/windows.rs` | 125 | Windows service integration |
| `src/tray/mod.rs` | 353 | System tray icon and menu |
| `src/tray/config.rs` | 54 | Tray configuration |
| `src/tray/icon.rs` | 6 | Tray icon asset |
| `src/tray/autostart/mod.rs` | 40 | Autostart module root |
| `src/tray/autostart/macos.rs` | 169 | macOS autostart (login items) |
| `src/tray/autostart/linux.rs` | 89 | Linux autostart (XDG) |
| `src/tray/autostart/windows.rs` | 119 | Windows autostart (registry) |
| `src/tray/terminal/mod.rs` | 62 | Terminal launcher module |
| `src/tray/terminal/macos.rs` | 120 | macOS terminal launch |
| `src/tray/terminal/linux.rs` | 147 | Linux terminal launch |
| `src/tray/terminal/windows.rs` | 66 | Windows terminal launch |

**Cross-group coupling**: Core daemon (service wraps daemon), CLI (service subcommand)

---

## 14. Binaries (~2,580 LOC)

Standalone executables beyond `main`.

| File | LOC | Role |
|------|-----|------|
| `src/bin/agend-git.rs` | 1,848 | Git hook proxy (pre-push verification, push interception) |
| `src/bin/agend-mcp-bridge.rs` | 735 | MCP stdio bridge for agent↔daemon communication |

---

## Cross-Group Dependency Map

```
                    ┌─────────────┐
                    │ Core Daemon │
                    └──────┬──────┘
           ┌───────────────┼───────────────┐
           ▼               ▼               ▼
    ┌──────────────┐ ┌───────────┐ ┌──────────────┐
    │    Agent     │ │  Fleet /  │ │   CI / CD    │
    │  Management  │ │  Config   │ │   PR State   │
    └──────┬───────┘ └─────┬─────┘ └──────┬───────┘
           │               │              │
           ▼               ▼              ▼
    ┌──────────────────────────────────────────────┐
    │              MCP Layer (API surface)          │
    └──────────────────────┬───────────────────────┘
           ┌───────────────┼───────────────┐
           ▼               ▼               ▼
    ┌─────────────┐ ┌────────────┐ ┌──────────────┐
    │Communication│ │  TUI /     │ │   Shared     │
    │  (Inbox,    │ │  Render    │ │   Infra      │
    │  Channels)  │ │            │ │  (Tasks,     │
    └─────────────┘ └────────────┘ │  Store, etc) │
                                   └──────────────┘
```

**Heaviest modules** (>4,000 LOC): `ci_watch/poller.rs` (6,042), `tasks.rs` (4,698), `state.rs` (4,636), `inbox.rs` (4,384)

**Highest fan-out**: `daemon/mod.rs` touches Agent, Fleet, Skills, Health, Communication. `api/mod.rs` (`spawn_one`) similarly bridges Agent + Fleet + Skills.

**Test concentration**: ~8,825 LOC in dedicated test files under `src/mcp/handlers/` — nearly all MCP handler tests.
