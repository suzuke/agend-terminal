# Sprint 45 G3 — `std::env::set_var` Callsite Catalog

**Date**: 2026-05-01
**Purpose**: Machine-readable catalog of all `set_var` usage in production code. Test-only callsites excluded (they use `fleet_test_guard()` mutex or explicit-param patterns per Sprint 44 lessons).

## Production `set_var` callsites

| # | File:Line | Env Var | Purpose | Replacement Strategy |
|---|---|---|---|---|
| P1 | `src/main.rs:127` | `*` (from .env) | Load `.env` file key-value pairs into process env | Keep — `.env` loader is intentional process-wide config; runs once at startup before any threads |
| P2 | `src/daemon/mod.rs:224` | `AGEND_DAEMON_PID` | Publish daemon PID for child processes | Replace with `DaemonConfig.pid` field; children read config instead of env |

## Production env var *readers* (candidates for config injection)

| # | File:Line | Env Var | Purpose | Replacement Strategy |
|---|---|---|---|---|
| R1 | `src/inbox.rs:522` | `AGEND_POINTER_ONLY_INJECT` | Feature flag: pointer-only inbox injection | Replace with `DaemonConfig.pointer_only_inject` |
| R2 | `src/main.rs:82` | `AGEND_HOME` | Home directory override | Keep — process-wide, read once at startup |
| R3 | `src/identity.rs:22` | `AGEND_INSTANCE_NAME` | Agent identity for MCP bridge | Keep — set by parent process for child, read once |
| R4 | `src/mcp/mod.rs:247` | `AGEND_DAEMON_PID` | Check if running inside daemon process | Replace with `DaemonConfig.daemon_pid` (P2 writer's reader) |
| R5 | `src/mcp/mod.rs:268` | `AGEND_TEST_ISOLATION` | Skip daemon API calls in tests | Keep — test-only guard, not prod config |
| R6 | `src/daemon/mod.rs:931` | `AGEND_SPAWN_STAGGER_MS` | Stagger agent spawn timing | Keep — low-frequency startup config, env var acceptable |
| R7 | `src/daemon/watchdog.rs:9` | `AGEND_WATCHDOG_DRY_RUN` | Watchdog dry-run mode | Keep — debug/test flag, not prod config |
| R8 | `src/worktree_cleanup.rs:13` | `AGEND_WORKTREE_AUTO_CLEANUP` | Auto-cleanup worktrees flag | Keep — opt-in feature flag, env var acceptable |
| R9 | `src/agent.rs:603` | `AGEND_DEBUG_PTY_READ` | Debug PTY read logging | Keep — debug flag |
| R10 | `src/mcp/mod.rs:36,40` | `AGEND_MCP_TOOLS_ALLOW/DENY` | MCP tool allow/deny lists | Keep — startup config, env var acceptable |

## Test-only `set_var` callsites (excluded from prod scope)

| File | Count | Env Vars | Pattern |
|---|---|---|---|
| `src/mcp/handlers/tests.rs` | ~30 | `AGEND_HOME`, `AGEND_TEST_ISOLATION` | Uses `fleet_test_guard()` mutex |
| `src/worktree_cleanup.rs` | 5 | `AGEND_WORKTREE_AUTO_CLEANUP` | Test fixtures |
| `src/inbox.rs` | 2 | `AGEND_POINTER_ONLY_INJECT` | Test fixtures |
| `src/daemon/watchdog.rs` | 2 | `AGEND_WATCHDOG_DRY_RUN` | Test fixtures |
| `src/daemon/ci_watch.rs` | 8 | `GITLAB_TOKEN`, `BITBUCKET_TOKEN`, `HOME` | Test fixtures |
| `src/channel/telegram.rs` | 4 | `PR57_*`, `SPRINT23_*` | Test fixtures |
| `src/identity.rs` | 1 | `AGEND_INSTANCE_NAME` | Test fixture |

## PR-3 plan

PR-3 will:
1. Swap P2 (`AGEND_DAEMON_PID` writer in `daemon/mod.rs`) to use `DaemonConfig.daemon_pid`
2. Swap R1 (`AGEND_POINTER_ONLY_INJECT` reader in `inbox.rs`) to use `DaemonConfig.pointer_only_inject`
3. Swap R4 (`AGEND_DAEMON_PID` reader in `mcp/mod.rs`) to use `DaemonConfig.daemon_pid`
4. Keep P1 (`.env` loader) and R2/R3/R5-R10 (startup-only reads, debug flags) as-is
5. Migrate test fixtures to explicit-param where feasible
