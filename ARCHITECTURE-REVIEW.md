# Architectural Review — agend-terminal

> **HISTORICAL SNAPSHOT — 2026-06-25.** This review preserves the findings and
> measurements from its original baseline; it is not a current architecture
> reference. At `main@1d83b423` (2026-07-16) the MCP registry contains **32**
> tools and the crate version is `0.10.0`. Use
> [`docs/architecture.md`](docs/architecture.md) and
> [`docs/architecture/ARCHITECTURE-14-LEDGER.md`](docs/architecture/ARCHITECTURE-14-LEDGER.md)
> for current structure and convergence status. Counts and line references
> below intentionally remain snapshot evidence unless a paragraph explicitly
> names a newer baseline.

**Date**: 2026-06-25
**Version**: v0.9.0
**Scope**: Architectural overview (no line-by-line review)
**Codebase size**: 380 Rust source files, ~263K lines of code

## 0. Historical post-review status update (2026-06-26)

This review was written before the 2026-06-26 cleanup pass. Keep the original
findings below for historical context, but treat the following as the current
status:

- **Historical MCP tool count/docs result**: the registry had 37 tools at this
  update's baseline. It has 32 at `main@1d83b423`; the registry/docs invariant,
  rather than this dated count, is authoritative.
- **`worktree_pool.rs` split**: 6011 LOC → 642 LOC main file plus
  `worktree_pool/{workspace,gc,target_sweep,tests}.rs`.
- **`daemon/supervisor.rs` split**: 6439 LOC → 2123 LOC main file plus
  `supervisor/{usage_limit,reactions,tests}.rs`.
- **Other large test modules split**: `task_events.rs` 4457 → 2070 LOC and
  `bin/agend-git.rs` 5532 → 2488 LOC by moving tests into sibling modules.
- **Registry-lock blind spot narrowed**: daemon/runtime `AgentRegistry` locks now
  route through `agent::lock_registry`, so the self-IPC depth guard observes
  those sites. Remaining direct `.lock()` matches are test/ad-hoc/local-registry
  cases or unrelated non-agent registries.
- **MCP classification drift fixed**: timeout band, timeout side-effect behavior,
  and read-only disk-skip classification now live on `ToolEntry.class` in
  `mcp::registry`, not in three independent allowlists.
- **Worktree lease finding D/H is resolved in current code**: `LeaseError` is
  typed and the raw lease path no longer writes binding state; `bind_full`
  failure handling lives at the authoritative caller boundary.
- **`unwrap()` finding was overcounted**: the earlier "2,802 production unwraps"
  included inline `#[cfg(test)]` modules. With test code excluded and Clippy's
  `unwrap_used = "deny"` active, production unwraps are limited to documented
  const/literal-regex compile sites and non-daemon helper paths; no daemon
  hot-path remediation was needed.
- **Anti-monolith ratchet added**: `tests/src_file_size_invariant.rs` enforces a
  repo-wide 2500-LOC production-file ceiling with grandfathered can-shrink-not-grow
  ceilings for the remaining oversized files.

---

## 1. Executive Summary

agend-terminal is a multi-agent orchestration daemon that manages AI coding agents (Claude, Codex, OpenCode, Kiro, agy) with PTY isolation, a 37-tool MCP interface, Telegram bridge, git worktree management, and a Shadow Observer state-correction plane.

The codebase is **mature and defensively engineered**. Lock-tier discipline is rigorous, spawn-site rationale is documented at every site, testing is extensive, and the Shadow Observer's cycle-proof invariant is well-designed. The primary risks are **maintainability** (oversized files, god-functions) and a few **latent correctness gaps** (bare-lock self-IPC bypass, silent bind-failure swallow).

---

## 2. Overall Architecture

### 2.1 Daemon Core — Synchronous Multi-Threaded, Not Async

The daemon is a `std::thread` + `crossbeam_channel` design, **not** a Tokio application. The main loop (`src/daemon/mod.rs:1027`) is a `crossbeam_channel::select!` over three sources:

- `crash_rx` — agent exit events (Crash / CleanExit / Stage2Restart)
- `tick_rx` — 10s periodic tick (produced by a dedicated daemon_tick thread)
- `shutdown_rx` — signal handler trip

Each tick, the loop:
1. Reloads `runtime_config` + `operator_mode` from disk
2. Runs ~35 `PerTickHandler` implementations **sequentially on the single main-loop thread** (`src/daemon/per_tick/mod.rs:226`)
3. Dispatches crash events to respawn / clean-exit handlers

Tokio is reserved exclusively for channel integrations (Telegram/Discord), running on dedicated `current_thread` runtimes via `block_on_value` with a nested-runtime guard (`src/channel/shared_async.rs:30`).

The `'serve` loop (`src/daemon/mod.rs:1015`) wraps the tick loop to support the #1814 self-respawn recover-as-primary gate: a successor that dies during predecessor teardown triggers `continue 'serve` to re-spawn agents and resume serving.

**Strengths**: Mature design, strict lock-tier discipline, no unbounded channels, panic isolation via `catch_unwind` per handler / per subscriber / per supervisor tick.

**Concerns**: All ~35 per-tick handlers run serially on one thread. Several do synchronous file I/O (`InboxMaintenance`, `GcTick`, `WorkspaceBoundarySweep`, `LogRotation`, `EphemeralReap`, `ContextAlert`). A slow handler delays every subsequent handler on that tick. The 10s tick cadence is the only slack.

### 2.2 Three-Tier Lock Model

| Tier | Lock | Location | Purpose |
|------|------|----------|---------|
| L1 | `AgentRegistry` | `Arc<parking_lot::Mutex<HashMap<InstanceId, AgentHandle>>>` (`agent/mod.rs:126`) | Fleet-wide agent map |
| L2 | `AgentCore` | `Arc<CoreMutex<AgentCore>>` (`agent/mod.rs:72`, `sync_audit.rs:216`) | Per-agent vterm + state + health + subscribers |
| L3 | Leaf | heartbeat_pair, file flocks | Per-agent side state |

Plus lock-free mirrors for the TUI render hot path: `published_state: Arc<AtomicU8>` and `published_observed: Arc<AtomicU8>` (`agent/mod.rs:78-84`, `state/mod.rs:252-263`). These clone the state byte so `render::build_agent_state_snapshot` reads agent state **without** `core.lock()` — eliminating producer/consumer contention that froze input under boot PTY flood.

**Lock ordering** is L1 → L2, enforced structurally: the supervisor snapshots handles under registry lock, releases it, THEN takes `core.lock()` per agent (`supervisor.rs:1130-1156`) to avoid AB-BA. The router thread is structurally barred from L1/L2 (`sync_audit.rs:47-56`).

### 2.3 API Dispatch

- **Transport**: TCP loopback, NDJSON (one JSON request/response per line)
- **Auth**: 32-byte cookie (mode 0600) issued at boot (`api/mod.rs:217-258`)
- **Thread topology**: accept loop → per-connection thread (32 max via `ConnSlot` RAII atomic counter) → 5s pre-auth read timeout → cookie handshake → `pid_watcher` thread (polls peer PID liveness every 2s) → NDJSON request loop
- **Request flow**: `read_line → parse → operator_gate → request_dedup → match method → handler → response`
- **Self-IPC deadlock defense**: `api::call` refuses if calling thread holds any lock depth (`sync_audit.rs:275-297`), plus 90s read-timeout backstop

### 2.4 Agent Supervision

**Spawn chain** (`daemon/mod.rs:1838` → `agent/mod.rs:1179`):
1. `spawn_and_register_agent` — deleting-set chokepoint check, insert into configs, skills auto-install
2. `agent::spawn_agent` — build_command (env isolation, PATH prepend), `openpty`, arm `SpawnRollback` RAII, construct `AgentCore` under `CoreMutex`, register in `AgentRegistry` keyed by fleet.yaml UUID
3. Spawn `{name}_pty_read` thread (fire-and-forget) — reads 8KB chunks, `vterm.process` + `state.feed_with_lazy_fg` + `broadcast_pty_output` under `core.lock()`, on EOF → classify exit → send `AgentExitEvent`
4. Synchronous TUI listener bind + `.port` publish
5. Spawn `{name}_tui_server` thread (fire-and-forget)

**Crash handling**: CleanExit → remove from registry (no respawn). Crash → `crash_respawn::handle_crash_respawn` (backoff + respawn worker thread). Stage2Restart → controlled restart preserving 5 health fields.

**Shutdown** (`daemon/mod.rs:1643`): drains registry, then `terminate_agents_parallel` — parallel SIGTERM → single 2s grace → per-agent `try_wait` → SIGKILL+reap only genuine holdouts (never `kill_process_tree` on an exited PID — reuse hazard, pinned by source-scan test).

---

## 3. Subsystem Architecture

### 3.1 MCP Tools (37 at the review baseline; 32 at `1d83b423`)

**Registration**: Static array registry (`registry.rs:146`), `static ALL_TOOLS: [ToolEntry; 37]`. Each entry has name, definition fn, handler fn pointer. Adding a tool = write schema + handler + append to array. Count pinned by invariant test.

**Dispatch flow**: agent → stdio JSON-RPC bridge binary → `MCP_TOOL` API method → `mcp_proxy::handle_mcp_tool` (per-tool timeout via scoped thread) → `mcp::execute_tool` → `handlers::handle_tool` (usage stats + implicit heartbeat + `dispatch::try_dispatch` linear scan) → handler returns `Value`.

**Timeout model**: FAST=5s, DEFAULT=30s, SLOW=60s. On timeout, side-effect tools return `accepted_in_progress` (suppress retry storm); read/idempotent tools return retryable error. The execution thread is NOT killed on timeout — runs to completion in background.

**Param validation**: Single chokepoint `validate_args` enforces declared `inputSchema`. Missing required → error. Present-but-null → treated as missing. Unknown key → warn only (forward-compat). Schema-vs-handler audit ensures `required[]` fields are genuinely error-on.

### 3.2 Worktree Management

**Three production bind paths** all route through `dispatch_auto_bind_lease_with_source_and_chain` (`dispatch_hook/mod.rs:348`):
1. Dispatch auto-bind (when `send kind=task` carries a `branch`)
2. `bind_self` (agent self-provision)
3. `repo action=checkout bind:true` (atomic fresh provision + bind)

**Cross-agent branch conflict prevention** (three layered guards):
1. Per-agent `BindGuard` — no concurrent binds for one agent
2. Per-(source_repo, branch) flock — serializes check-then-bind across agents
3. `scan_existing_branch_binding` — under the flock, scans all agents' `binding.json` for the same (repo, branch)

**Binding state machine**: `binding.json` (schema v1) + HMAC sidecar (`binding.json.sig`). Write path: acquire per-agent lock → read current → guard-b (reject rebind of LIVE binding to different branch) → atomic write → HMAC sign → update in-memory index → audit event-log.

**GC model**: Two kinds — `CleanRelease` (explicitly released, past 24h grace) and `ForceReclaim` (never released, agent shows no liveness, past 7d + per-agent jitter). Multi-signal liveness check: process-alive, heartbeat within 1h, PTY input within 1h, `waiting_on_since` set, ci-watch subscriber. Boot grace suspends force-reclaim for 10 min after daemon restart.

### 3.3 Task Board

**Persistence**: Append-only event log (`task_events.jsonl`, schema v2). State reconstructed via `replay_at(board)` with in-memory cache. Compaction at 20K → 10K (hysteresis). Atomic batch append (single fsync). Legacy `tasks.json` migrated at startup (idempotent, renamed to `.legacy_pre_v2` after migration).

**State machine**: `Backlog → Open → Claimed → InProgress → InReview → Verified → Done`, plus `Cancelled`, `Blocked`. Forward, skip, backward (rejection/rework), and unblock transitions defined. Dependency-derived blocking is in-memory only (computed at list-time, not persisted).

**Dispatch protocol**: `task action=create` is pure board record (zero dispatch side-effects). Dispatch is solely `send kind=task`'s job — it orchestrates: pre-checks → worktree lease + ci-watch arming → task auto-create → send → fallback delivery → dispatch tracking. The `task_id` is REQUIRED for `send kind=task` (anti-stall contract).

**Anti-stall mechanisms** (four distinct):
1. Overdue claimed sweep — emits `Released` for Claimed tasks past `due_at`
2. Lifecycle pass — marks stale Open tasks (7d+no assignee+14d) as Cancelled, archives Done tasks (7d)
3. Operator-triggered sweep — 5 categories (validation leftovers, team disbanded, shipped, superseded, stale open), dry-run by default
4. Task stall watchdog — `eta_secs` field, emits `task_stalled` when elapsed > `eta_secs * 1.5`

Plus auto-close merged PRs via `Closes t-XXX-N` markers and orphan reconciliation for owners no longer in fleet.

### 3.4 Channel / Telegram

**Platform-neutral trait**: `Channel` trait with Telegram adapter as the only production implementation. Process-wide registry: `CHANNELS: OnceLock<RwLock<HashMap>>`.

**Inbound**: Polling supervisor on its own thread with `current_thread` tokio runtime. `handle_message` is the router: topic-closed detection → **allowlist authz gate (hoisted — was a Rank2 bug)** → status keyword → task creation → attachment extraction → topic resolution → raw-keystroke routing → message ID persistence → attachment download → length-based delivery split (short = PTY inject, long = inbox enqueue) → UxEvent emission.

**Outbound**: Single `gated_notify` chokepoint with two gates: operator mode gate (Active = all, Away = suppress Info, Sleep = suppress Info+Warn) + outbound authorization gate (fail-closed, `warn_once_user_allowlist_unconfigured`).

**Dedup**: Per-`AGEND_HOME` content-hash dedup with TTL (default 5s). Key = (channel_kind, instance_name, topic_id, content_hash). Bounded VecDeque cap 1024 with LRU eviction. `evict` rolls back the claim on terminal send failure.

**Auth model**: Inbound `is_authorized_recipient` — fail-closed (None → false, empty → false). Outbound `is_outbound_authorized` — `Some(non-empty)` only. Allowlist is the sole privilege boundary.

### 3.5 Shadow Observer (#2413)

An additive, purely observational state plane that runs beside the raw screen-scrape `agent_state` and **never rewrites it**.

**Problem solved**: Hook delivery is best-effort, so a closing event (`Stop`/`PostToolUse`) can DROP — leaving an episode/tool span open forever and the agent stuck reporting `Active`/`ToolUse` while actually idle.

**Three evidence planes**:
1. **Hook plane** (claude, agy) — `Authority::Hook`, `Confidence::Confirmed`. Unix-socket ingest.
2. **Stream plane** (codex rollout, kiro session, opencode SSE) — `Authority::Stream`, `Confidence::Strong`. File/TCP tail.
3. **Screen + liveness backstop** — `Authority::Screen`/`ProcessHeuristic`/`Inferred`. Folded into reducer at observe-time.

**The reducer** (`reducer.rs:182-302`) is a pure state machine over (accumulator, screen, liveness, now). Precedence: dead child > rate-limited > waiting-for-user > active family (mid-API false-idle beat: screen==Idle && observer_fresh && api_in_flight → keep Active) > idle (dropped-hook recovery via `reconcile_to_idle`).

**Confidence gating** (`gate.rs:91-114`): `gated_override` promotes observed state ONLY when ALL four conditions hold: (a) authority is Hook/Stream, (b) confidence is Confirmed/Strong, (c) raw screen is NOT a gate screen (Approval/RateLimited), (d) observed state genuinely DISAGREES with raw at coarse level.

**Cycle-proof invariant**: The reducer's screen input is vterm-only. The gated override writes `published_observed` atomic + `snapshot.json`'s `agent_state` — NEVER `State::current`. So a promoted state can't feed back into the screen-scrape classifier.

**Two consumers**: (A) pane badge via lock-free `published_observed` atomic (pixels only), (B) operated snapshot state for dispatch deciders (behavior). Independent kill-switches: `AGEND_SHADOW_OBSERVER=0` (whole plane), `AGEND_OBSERVED_DISPATCH=0` (promotion only).

---

## 4. Key Findings

### 4.1 Real Issues

#### A. Direct `registry.lock()` bypasses self-IPC deadlock guard

**Severity**: Medium-High (latent deadlock)

94 sites call `registry.lock()` directly (e.g. `src/daemon/crash_respawn.rs:142`, `src/agent/mod.rs:1883,1898,2212,2297`) instead of going through `agent::lock_registry` which bumps `REGISTRY_LOCK_DEPTH`. The self-IPC guard (`assert_no_registry_lock_for_self_ipc`) only sees depth bumped by the tracked wrapper. A self-IPC call on a thread that took the registry via a bare `.lock()` would NOT trip the guard and could deadlock.

**Mitigation**: 90s `api::call` read-timeout backstop converts the deadlock into a recoverable error after 90s rather than a permanent freeze. But this is a 90-second freeze, not prevention.

**Recommendation**: Migrate remaining 94 direct `registry.lock()` calls to the tracked wrapper, or enforce wrapper-only access via a compile-time lint.

#### B. File size invariant violated in ~25 files

**Severity**: Medium (maintainability)

The project enforces a 750-LOC `file_size_invariant`, but 25 production files exceed it. Worst offenders:

| Lines | File |
|-------|------|
| 6154 | `src/daemon/supervisor.rs` |
| 5943 | `src/worktree_pool.rs` |
| 4893 | `src/bin/agend-git.rs` |
| 4457 | `src/task_events.rs` |
| 3971 | `src/daemon/dispatch_idle/mod.rs` |
| 3457 | `src/app/mod.rs` |
| 3428 | `src/daemon/pr_state/mod.rs` |
| 3239 | `src/api/handlers/messaging.rs` |
| 3224 | `src/daemon/mod.rs` |
| 3190 | `src/agent/mod.rs` |

Tests are re-homed to `review_repro_*.rs` sibling files via `#[path]` attributes, but production code is monolithic. `worktree_pool.rs` (5943 lines) mixes lease, release, GC, liveness, workspace-B, and branch-cleanup in one file — all separable concerns.

**Recommendation**: Decompose `worktree_pool.rs` into lease/release/gc/liveness modules. Prioritize the top 5 offenders.

#### C. 2,802 `unwrap()` calls in production code

**Severity**: Medium (panic risk)

Distribution by file (non-test, >5 calls):

| Count | File |
|-------|------|
| 207 | `src/task_events.rs` |
| 115 | `src/worktree_pool.rs` |
| 83 | `src/skills.rs` |
| 82 | `src/bin/agend-git.rs` |
| 72 | `src/deployments.rs` |
| 70 | `src/binding.rs` |
| 57 | `src/daemon/retention/worktrees.rs` |
| 57 | `src/daemon/dispatch_idle/mod.rs` |

Many are in post-success patterns (e.g. `serde_json::to_value(x).unwrap()` after serialization that can't fail), but the sheer count is concerning for daemon-grade code where a panic downs the entire fleet.

**Recommendation**: Audit the top 10 files. At minimum, replace `unwrap()` on fallible I/O / lock acquisitions with `?` or explicit error handling.

#### D. `worktree_pool::lease` swallows `bind_full` failure

**Severity**: Medium-High (silent failure)

At `worktree_pool.rs:65-69`:
```rust
if let Err(e) = binding::bind_full(...) {
    tracing::warn!(...);  // logged but not returned Err
}
```

If `bind_full` fails, the worktree is created but the binding (which the git shim needs to authorize pushes) is absent. The agent gets a worktree but can't push. No rollback of the worktree on bind failure at this layer.

The dispatch path (`dispatch_hook/mod.rs:546-574`) DOES roll back on `bind_full` failure, but the raw `lease` path does not.

**Recommendation**: Roll back the worktree on `bind_full` failure in the raw `lease` path, or return an error.

### 4.2 Architectural Smells

#### E. `comms.rs::handle_delegate_task` is a god-function

**Severity**: Medium (coupling)

253 lines orchestrating: pre-checks, worktree lease, ci-watch arming, task auto-create, send, fallback delivery, dispatch tracking, UX events. Directly imports 9 modules (`dispatch_hook`, `tasks`, `agent`, `teams`, `api`, `dispatch_tracking`, `agent_ops`, `comms_gates`, `comms_inbox`). This is the single most coupled site in the codebase.

**Recommendation**: Extract into phase functions (pre-checks → lease → dispatch → tracking).

#### F. Three overlapping tool classifications must be manually synced

**Severity**: Medium (drift risk)

Three independent classifications of "tool nature":
- `is_read_only_tool` (`handlers/mod.rs:178`) — controls heartbeat disk-write skip
- `is_side_effect_tool` (`mcp_proxy.rs:96`) — controls timeout response (`accepted_in_progress` vs retryable)
- `FAST_TOOLS` / `SLOW_TOOLS` (`mcp_proxy.rs:31,50`) — controls per-tool timeout

A new tool added without updating all three gets inconsistent timeout/heartbeat/retry behavior. The `is_read_only_tool` allowlist is especially fragile — a new action-based tool with a read action (e.g. `task list`) keeps the full disk path because coupling to per-action semantics was deliberately avoided.

**Recommendation**: Unify into a single enum-driven trait for read-only / side-effect / timeout classification.

#### G. Two parallel outbound paths for Telegram notifications

**Severity**: Low-Medium (fragile duplication)

- `notify_telegram` (re-loads FleetConfig, fresh `Bot::new`) — used by supervisor/health paths
- `TelegramChannel::notify` (uses shared `TelegramState`) — adapter impl

The dedup-aware topic-deleted recovery is re-implemented in `notify.rs` rather than reused from the adapter. A fix to one path may not propagate to the other.

**Recommendation**: Consolidate into a single outbound path, or extract shared recovery logic.

#### H. Stringly-typed error classification at the worktree lease boundary

**Severity**: Low-Medium (leaky abstraction)

`worktree::create` returns `Option<WorktreeInfo>` — all failure modes collapse to `None`. Then `dispatch_auto_bind_lease` classifies via string-contains: `msg.contains("E4.5")` (`dispatch_hook/mod.rs:496`). The `map_lease_err` closure explicitly acknowledges: "the lease layer returns an anyhow string; this is the SINGLE boundary that classifies it into the typed ErrorCode."

**Recommendation**: Introduce a typed error enum at the lease layer.

#### I. `active_channel()` returns `None` with ≥2 channels

**Severity**: Low (footgun, mitigated for P0)

`mod.rs:150` returns `Some` ONLY when exactly one channel is registered. `notify_all_escalation_channels` handles P0s, but any non-P0 caller using `active_channel()` silently no-ops in a multi-channel fleet.

### 4.3 Well-Defended Areas

- **Lock ordering**: L1 → L2, structurally enforced (supervisor snapshots handles, releases registry, then takes core.lock per agent). Router thread barred from L1/L2.
- **No unbounded channels** in the daemon hot path. All channels bounded (`crash_rx` 64, `tick_rx` 1, PTY subscribers 1024 with `try_send`, router reg 64). Unbounded channels only in TUI rendering (consumer is the render loop).
- **Spawn-site rationale**: `// fire-and-forget: <reason>` present at every major spawn site. Phase 5b invariant test enforces.
- **Shadow Observer cycle-proof invariant**: reducer's screen input is vterm-only; promoted state writes atomics + snapshot.json, never `State::current`.
- **Binding HMAC**: defense-in-depth (not a security boundary, but prevents accidental agent self-rewriting).
- **Task event log**: schema versioned, forward-compat fail-closed for unknown variants.
- **Telegram inbound authz**: allowlist gate hoisted BEFORE all content side-effects (Rank2 bug fixed).
- **Test coverage**: extensive — `tasks/tests.rs` (3909 lines), `mcp/handlers/tests.rs` (3509 lines), `inbox/tests.rs` (4051 lines). Invariant tests pin tool count, tool routing, schema-vs-handler alignment, binding schema version, GC idempotency, lifecycle idempotency.
- **PID-reuse hazard** in `terminate_agents_parallel`: defended by `try_wait` (not `is_pid_alive`), `kill_process_tree` only on genuine holdouts, pinned by source-scan test.

---

## 5. Security Notes

- **Telegram allowlist is a binary gate** — no per-command scope, no 2FA, no audit trail beyond `tracing::warn!` on rejection. A compromised operator account = full fleet command authority.
- **HMAC binding signature is defense-in-depth, not a boundary** — same-uid agent can read the key + re-sign. True sealing needs OS isolation (#1653 parked).
- **No command injection** — inbound text never reaches a shell. It either lands in typed `InboxMessage` or is injected via API `INJECT` as data.
- **Bot token** is read from env var per notify call (`notify.rs:52-95`); a leaked env var on the daemon host = full bot control. Standard for bot tokens.

---

## 6. Concurrency Summary

| Metric | Count |
|--------|-------|
| `tokio::spawn` / `thread::spawn` sites | 92 |
| `unsafe` blocks | 86 (mostly in `src/process.rs:19` — FFI for process management) |
| `Arc<Mutex>` / `Arc<RwLock>` declarations | ~20 files |
| Total `.lock()` calls | 772 |

Spawn-site rationale comments are present at every major site. The `DaemonTicker` primitive stores `JoinHandle` for forward-compat graceful join but `Drop` is intentionally a no-op (shutdown flag is the exit signal).

The 86 `unsafe` blocks are concentrated in `src/process.rs` (19 — FFI for process management), `src/daemon/per_tick/recovery_dispatcher.rs` (10), `src/bootstrap/signals.rs` (7), `src/admin/cleanup_zombies.rs` (7). These are platform-specific process/signal handling — expected for a process manager.

---

## 7. Recommendations (Prioritized)

| Priority | Recommendation | Effort |
|----------|----------------|--------|
| **High** | Migrate 94 direct `registry.lock()` calls to tracked wrapper or enforce via lint | Medium |
| **High** | Split `worktree_pool.rs` (5943 lines) into lease/release/gc/liveness modules | Medium-High |
| **Med-High** | Fix `bind_full` failure swallow in raw lease path (`worktree_pool.rs:65`) — rollback worktree on bind failure | Small |
| **Medium** | Unify tool classifications (read-only / side-effect / timeout) into single enum-driven trait | Medium |
| **Medium** | Introduce typed error enum at worktree lease boundary (replace `Option<WorktreeInfo>` + string matching) | Medium |
| **Medium** | Refactor `handle_delegate_task` god-function into phase functions | Medium |
| **Medium** | Audit 2,802 `unwrap()` calls in production code, especially in daemon paths | Large |
| **Low-Med** | Consolidate Telegram outbound paths or extract shared topic-deleted recovery | Small-Medium |
| **Low-Med** | Remove vestigial `TaskStore` / `tasks.json` code (bridge-phase, post-PR3 cutover) | Small |
| **Low** | Address `active_channel()` multi-channel footgun (introduce `primary_channel()` concept) | Small |
| **Low** | Remove stray artifacts: `positive-threshold` (empty file), `graphify-out/` (build artifact) | Trivial |

---

## 8. Conclusion

agend-terminal is a **well-engineered, defensively-designed** multi-agent orchestration daemon. Its strengths include rigorous lock-tier discipline, lock-free state mirrors for the render hot path, comprehensive panic isolation, extensive test coverage with invariant pinning, and the Shadow Observer's cycle-proof additive state plane. The spawn-site rationale discipline and the self-IPC deadlock guard (with timeout backstop) demonstrate mature concurrency reasoning.

The primary risks are **maintainability** (25 files exceeding the 750-LOC invariant, monolithic `worktree_pool.rs` and `supervisor.rs`, a 253-line god-function in `comms.rs`) and a few **latent correctness gaps** (bare `registry.lock()` bypass of the self-IPC depth counter, silent `bind_full` failure swallow in the raw lease path, 2,802 `unwrap()` calls). None of these are live deadlocks or data-corruption bugs today, but they represent fragility that compounds as the codebase grows.

The codebase would benefit most from: (1) closing the bare-lock bypass, (2) decomposing the oversized files, and (3) introducing typed errors at the worktree lease boundary.
