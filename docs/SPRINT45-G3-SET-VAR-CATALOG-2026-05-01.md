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
| R1 | `src/inbox.rs:522` | `AGEND_POINTER_ONLY_INJECT` | Feature flag: pointer-only inbox injection | Replace with `DaemonConfig.pointer_only_inject: bool` |
| R2 | `src/main.rs:80+` | `AGEND_HOME` | Home directory override | Keep — process-wide, read once at startup |
| R3 | `src/identity.rs:25` | `AGEND_INSTANCE_NAME` | Agent identity for MCP bridge | Keep — set by parent process for child, read once |
| R4 | `src/mcp/mod.rs:267` | `AGEND_HOME` (comment) | Comment noting env var race risk | No action — comment only |

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
1. Swap P2 (`AGEND_DAEMON_PID`) to use `DaemonConfig`
2. Swap R1 (`AGEND_POINTER_ONLY_INJECT`) to use `DaemonConfig`
3. Keep P1 (`.env` loader) and R2/R3 (startup-only reads) as-is
4. Migrate test fixtures to explicit-param where feasible
