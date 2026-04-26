# Track B audit — `src/daemon/` + `src/agent.rs` + `src/health.rs`

**Audit metadata**

- `audit_mode`: codebase_audit
- `audit_head`: f6a465e + Sprint 19 cleanup merges (HEAD `1485e85` of `origin/main` at audit start)
- `surface`: `src/daemon/{mod.rs, supervisor.rs, ci_watch.rs, watchdog.rs, poll_reminder.rs}` + `src/agent.rs` + `src/health.rs` + cross-grep into `src/api/handlers/instance.rs`, `src/app/mod.rs`, `src/backend.rs`, `src/instance_monitor.rs` (cross-area touchpoints only)
- `methodology`: `git log --diff-filter=A --follow` per file (comfort-zone first-pass), `rg` for spawn / lock / atomic patterns, targeted `Read` of every spawn site + lifecycle entry point, no test runs (audit-only PR)
- `time_box`: 2h hard cap, started 21:08 UTC, written ~22:00 UTC
- `peer_pass`: TODO — appended after dev-impl-1 pushes `CHANNEL.md` (Track A). Reads Track A and adds 1 paragraph blindspot critique.
- `tier_breakdown`:
  - **Tier-1 hot (~70% time)**: `supervisor.rs` (286), `mod.rs` spawn/replace/delete flow (lines 230-710, 1015-1140), `agent.rs` (1033, focus on `spawn_agent` lines 325-480), `ci_watch.rs` spawn site (lines 395-432; rest skim only — large file, mostly poll provider logic out of lifecycle scope)
  - **Tier-2 control**: `health.rs` (594), `watchdog.rs` (189) — invariant + dry-run path semantic walkthrough only
  - **Tier-3 peripheral grep**: `poll_reminder.rs` (248) — grepped for spawn / loop / external state mutation, no findings worth quoting

---

## 1. Findings

Critical = path-keyword auto-Critical (daemon spawn / signal propagation / lifecycle invariant) per dispatch §3.

### Critical

**F1. `spawn_agent` partial-failure leaves orphan PID or phantom registry entry** [Critical, lifecycle]
- File: `src/agent.rs:325-480`
- Failure window: `pty_system.openpty` (line 344) and `pair.slave.spawn_command` (line 353) execute **before** registry insertion (line 379). If `take_writer` (line 361), `try_clone_reader` (line 366), or the `pty_read_loop` thread spawn (line 430) errors, the child process has already been started but is never registered. **Result: orphan PID** — child runs in the OS but no `AgentHandle` references it; nothing knows to kill it.
- Symmetric phantom case: if registry insertion succeeds (line 379-400) but the `pty_read_loop` thread spawn fails (line 430, propagates via `?`), `spawn_agent` returns Err but the registry entry is **not rolled back** — the agent appears in `list_instances` but VTerm never gets PTY output, freeze visible to operator.
- `spawn_and_register_agent` in `mod.rs:1037-1075` partially compensates by removing the `configs` entry on `spawn_agent` failure (line 1073), but it does **not** call `agent::spawn_agent` rollback for the registry — and the TUI server thread spawn at line 1080 has the same partial-state risk.
- Why Critical: invariant violation for "registry agrees with running children". Operator must manually `kill` orphan PID + restart daemon to sync.

**F2. `delete_instance` does not wait for child exit before removing registry entry** [Critical, lifecycle, signal]
- File: `src/api/handlers/instance.rs:84-128`
- Sequence: `kill_process_tree(pid)` → `child.kill()` → `reg.remove(name)` → `configs.remove(name)`. None of these wait for the child to actually transition to exited. On a busy box the kernel may take milliseconds-to-seconds to reap the process group; during that window the PID is still alive but the registry says it's gone.
- Two failure modes: (a) PID re-use — if the OS rapidly recycles the freed PID for an unrelated process, any later `kill_process_tree(pid)` call (e.g. from a re-issued delete or a watchdog) targets the wrong process. (b) Concurrent `spawn` of the same name (line 130 `handle_spawn`) — guard at line 139 `contains_key(name)` returns false because we just removed, but the dying process is still holding the working directory / PTY device file, so spawn succeeds at the registry level but the new child collides with the dying one on shared resources.
- Compounding: no cleanup of `metadata/{name}.json`, `inbox/{name}.jsonl`, or worktree state in the same critical section. Stale metadata can survive a re-spawn of the same name and feed wrong `last_heartbeat` to `supervisor::tick`.
- Why Critical: signal propagation invariant — a delete must be observable as "PID gone" before the registry mutation is visible. Currently the registry mutates first; PID death is asynchronous.

**F3. `kill_agent` (app mode) leaves registry entry intact and does no metadata cleanup** [Critical, lifecycle]
- File: `src/app/mod.rs:859-866`
- Sends `child.kill()` and removes from registry. **Does not** call `kill_process_tree`, so child subprocesses (kiro-cli's bun/mcp/acp tree) survive — known issue from Sprint 9 PR-T `interrupt` work that PR `delete_instance` already addresses, but app-mode `kill_agent` regressed to leader-only kill.
- No `event_log::log` entry (compare `handle_kill` line 68 which logs).
- Asymmetry vs API delete: app-mode `kill_agent` lacks the IPC port / config / fleet-broadcast cleanup. App mode currently leaks every kill into `configs` map.
- Why Critical: cross-backend signal propagation regression — kiro-cli + Codex spawn child trees and app-mode kill orphans them.

**F4. `pty_read_loop` thread has no shutdown signal observation at spawn site** [Critical, lifecycle]
- File: `src/agent.rs:420-434`
- The reader thread receives a `shutdown_for_reaper: Option<Arc<AtomicBool>>` via `PtyReadContext` but the spawn site doesn't make the actual `pty_read_loop` shutdown semantics visible. If the loop blocks on `pty_reader.read(...)` — it does, by design, see the reader.read syscall — the shutdown flag won't be checked until read returns. On clean daemon shutdown, the only thing that wakes the read is the child dying (PTY EOF). If for any reason the child outlives the daemon (e.g. shutdown short-circuits before kill propagates), the reader thread leaks.
- This pattern is mostly correct because the typical shutdown path kills the child first, which closes the PTY, which returns EOF from read. But the implicit assumption "shutdown will always close PTY before we exit" is undocumented and brittle. Recommend either explicit doc-comment at the spawn site OR a `pty_reader.set_read_timeout(Some(short))` so the loop can observe shutdown.

### High

**F5. `spawn_and_register_agent` TUI server spawn has no rollback path** [High, lifecycle]
- File: `src/daemon/mod.rs:1080-1082`
- Same shape as F1 phantom: the `?` on the `serve_agent_tui` thread spawn means the function returns Err (caller may rollback configs) but `agent::spawn_agent` already inserted into the registry **and** spawned a child + reader thread. Even if the caller drops the configs entry on Err, the orphan child + registry entry survive.
- Same issue exists at the respawn site: `mod.rs:690` spawns the same TUI server thread inside the respawn block; on spawn err it `tracing::warn!` and continues, so the respawned agent is registered without an attachable TUI socket. operator can't `attach`.

**F6. Supervisor + main loop tick overlap on heartbeat read** [High, race]
- Files: `src/daemon/supervisor.rs:67-69` (reads heartbeat → `core.state.update_heartbeat(age)`) and `src/daemon/mod.rs:380-389` (main loop holds core lock for hang detection). Both run on independent timers (supervisor 10s, daemon main 10s). They lock the registry sequentially but never coordinate. A heartbeat-update race between supervisor and main loop can produce visibly inconsistent state (supervisor sees "heartbeat fresh", main loop sees "heartbeat stale") in a single tick window.
- Mitigated in practice because both read from the same metadata file and only one writes (MCP tool calls), so the worst case is "both ticks drop the conclusion they would have made anyway, redo next tick". Still, the dual-tick contract is undocumented.
- Why High not Critical: outcomes converge eventually; no resource leak.

**F7. Multiple `save_metadata` calls in `clear_waiting_on_if_stale` are not atomic** [High, lifecycle invariant]
- File: `src/daemon/supervisor.rs:211-213`
- `save_metadata("waiting_on", null)` and `save_metadata("waiting_on_since", null)` are two separate disk operations. A daemon crash between them leaves `waiting_on=null + waiting_on_since=set` on disk. On restart, `set_waiting_on`'s freshness logic interprets a non-null `since` as "agent is currently waiting" without a `waiting_on` value — divergent state.
- Fix shape: extend `save_metadata` to accept a multi-field write OR use a single transactional patch.

**F8. Respawn flow restores HealthTracker after registry insert, opening a brief window of fresh-tracker state** [High, race]
- File: `src/daemon/mod.rs:629-668`
- Respawn thread runs: `spawn_agent` → newly inserted `AgentHandle` has `HealthTracker::new()` (line 374 of agent.rs) → respawn restores `saved_health` AFTER (line 663). Between these two steps (microseconds, but real), any concurrent reader of the agent's health (e.g. supervisor tick, MCP `describe_instance`) sees a freshly-zeroed crash counter. False "Healthy" report for a chronically crashing agent.
- Window is small but observable; auditing tools (Sprint 18 PR-AZ Instance Monitor) would surface this.

### Medium

**F9. `daemon_tick` thread loses the tick if the channel is full** [Medium, robustness]
- File: `src/daemon/mod.rs:334-346`
- `bounded(1)` channel + `if tx.send(()).is_err() { break; }`. If the main loop is busy processing a crash burst (`crash_rx` has many pending), the tick channel can be full when the next tick fires. `crossbeam::channel::Sender::send` on a full bounded channel **blocks** by default — the existing `is_err()` check is for the receiver-dropped case, not full-channel. So the tick thread blocks until the main loop drains. In practice this just delays maintenance; not a leak. But the comment "every 10s for health/schedule/session maintenance" can become every 20-30s during crash storms.
- Fix shape: `try_send` with debug log on dropped tick.

**F10. `dismiss` thread (auto-dialog dismissal) is unnamed** [Medium, observability]
- File: `src/agent.rs:772`
- `std::thread::spawn(move || ...)` — no `Builder::new().name(...)`. In thread dumps and tracing, this thread shows as `thread-N` with no agent attribution. The closure does emit a `tracing::debug!` with `agent = %agent` but only on completion — during the 300ms sleep + lock wait, no observability.
- Trivial fix: wrap in `Builder::new().name(format!("{n}_dismiss"))`.

**F11. `instance_monitor::spawn_monitor_tick` + `supervisor::spawn` are fire-and-forget without explicit rationale at call site** [Medium, observability]
- File: `src/daemon/mod.rs:305-306`
- Both calls discard JoinHandle silently. supervisor.rs documents the rationale in its module doc; instance_monitor (out of Track B scope but called from Track B file) does not. Dispatch §11 wants explicit rationale at every spawn site.
- Recommendation: add 1-line `// fire-and-forget: shutdown signal is process exit` comment at both call sites for consistency.

### Low

**F12. `consume_upgrade_marker` thread silently swallows JSON parse errors** [Low, observability]
- File: `src/daemon/mod.rs:1116`
- `serde_json::from_str(&raw).unwrap_or_default()` — a malformed marker becomes empty `Value`, then yields `from = "(unknown)"` / `to = "(unknown)"`. Operator sees the cosmetic notice but never learns that the marker was corrupt. Unrelated to lifecycle correctness; but pattern violates the silent-absorb convention from Sprint 17 PR-AE3.
- Trivial fix: log an `warn!` if `from_str` errs.

### Track B specific deliverable: JoinHandle inventory

Each `std::thread` spawn site within Track B scope; columns: file:line, name, JoinHandle handling, rationale documented (Y/N), shutdown awareness inside loop (Y/N).

| # | File:line | Thread name | JoinHandle | Rationale doc | Shutdown-aware loop |
|---|---|---|---|---|---|
| 1 | `daemon/mod.rs:257` | `api_server` | `?` propagated then dropped | N at site | N (`api::serve` runs forever) |
| 2 | `daemon/mod.rs:336` | `daemon_tick` | `.ok()` discard | partial — comment says "every 10s for maintenance" | Y (`if tx.send(()).is_err() { break; }`) |
| 3 | `daemon/mod.rs:629` | `<n>_respawn` | `if let Err(e)` then warn | partial — docstring on respawn flow | Y (line 634 `shutdown_for_respawn.load(...)`) |
| 4 | `daemon/mod.rs:690` | `<n>_tui_server` (respawn-time) | `if let Err(e)` then warn | N | unknown (depends on `serve_agent_tui` impl, out of scope) |
| 5 | `daemon/mod.rs:1080` | `<n>_tui_server` (startup) | `?` propagated then dropped | N | unknown |
| 6 | `daemon/mod.rs:1105` | `upgrade_marker` | `let _ =` discard | partial — docstring above fn explains cosmetic | Y (short-lived, no loop) |
| 7 | `daemon/supervisor.rs:29` | `supervisor` | `let _ =` discard | **Y — explicit module-doc rationale at lines 7-8** | N (infinite loop, no shutdown check; relies on process exit) |
| 8 | `agent.rs:430` | `<n>_pty_read` | `?` propagated then dropped | N at site (F4) | partial — `shutdown` flag passed via `PtyReadContext` but read syscall doesn't observe it |
| 9 | `agent.rs:497` | `<n>_instr_boot` | spawn_result captured but its handling not visible in shown context | partial — docstring on fn | Y (lines 502-505 `s.load(...)`) |
| 10 | `agent.rs:772` | unnamed (`thread::spawn`) | dropped | N (F10) | Y (short-lived, no loop) |
| 11 | `daemon/ci_watch.rs:401` | `ci_check` | `.unwrap_or_else()` returns dummy | N | N (one-shot per spawn) |

**Inventory summary**: 11 spawn sites in Track B scope. 1 has explicit rationale doc (supervisor). 4 have shutdown-aware loops. 0 have stored JoinHandles for graceful join on daemon stop. **No spawn site joins on shutdown** — the daemon relies entirely on process exit for thread cleanup. This is acceptable for the current architecture but should be made explicit in `daemon/mod.rs` shutdown flow doc.

### Track B specific deliverable: lifecycle event partial-failure trace

For each lifecycle event, the multi-step mutation chain and the failure mode at each step.

#### `spawn_agent` (agent.rs:325-480)

| Step | Operation | Failure mode if this step fails |
|---|---|---|
| 1 | `pty_system.openpty(...)` (line 344) | clean Err return — no state mutation yet, **safe** |
| 2 | `pair.slave.spawn_command(cmd)` (line 353) | child PID does not exist; clean Err, **safe** |
| 3 | `drop(pair.slave)` (line 357) | infallible |
| 4 | `pair.master.take_writer()` (line 361) | **child PID exists, no registry entry** — **orphan PID** (F1) |
| 5 | `pair.master.try_clone_reader()` (line 366) | **same as step 4** — orphan PID |
| 6 | `reg.insert(name, AgentHandle)` (line 379-400) | infallible (Mutex lock + HashMap insert) |
| 7 | `Builder::new()...spawn(pty_read_loop)` (line 430) | **child PID exists + registry entry exists** — **phantom registry entry** (F1) |

Total exposure: 2 orphan windows + 1 phantom window. None have rollback.

#### `handle_delete` (api/handlers/instance.rs:84-128)

| Step | Operation | Failure mode if process crashes between steps |
|---|---|---|
| 1 | external registry remove (line 91-95) | clean — early return path |
| 2 | `kill_process_tree(pid)` (line 101) | kernel-issued, no rollback semantics |
| 3 | `child.kill()` (line 103) | best-effort — fires signal, returns immediately |
| 4 | `reg.remove(name)` (line 106) | registry now says agent gone, but PID may still be alive |
| 5 | `configs.remove(name)` (line 108) | crash here → registry clean, configs has stale entry → restart will not respawn (correct), but stale entry leaks until next config write |
| 6 | `ipc::remove_port(...)` (line 109) | crash here → registry+configs clean, but IPC port file leaked on disk |
| 7 | `event_log::log(...)` + `notifier.notify(InstanceDeleted)` (line 110-116) | crash here → upstream subscribers never see the event |
| 8 | `fleet_broadcast::broadcast(InstanceDeleted)` (line 120-126) | crash here → other agents never get the InstanceDeleted update; their fleet view stays stale until next ad-hoc sync |

Total exposure: steps 4-8 all have partial-state crash windows. No transaction; no recovery on restart that detects "config exists but registry doesn't".

#### `respawn` flow (daemon/mod.rs:620-708)

| Step | Operation | Failure mode |
|---|---|---|
| 1 | snapshot `saved_health` (line 623-627) | clean — read-only |
| 2 | `Builder::new()...spawn(respawn_thread)` (line 629) | spawn err → no respawn, agent stays crashed; warn logged |
| 3 | inside thread: `sleep(delay)` then check `shutdown` (line 632-637) | clean — early return on shutdown |
| 4 | `agent::spawn_agent(...)` (line 641) | inherits all of `spawn_agent`'s partial-failure modes (F1) |
| 5 | restore `saved_health` (line 663-665) | **F8 race window**: between step 4 register and step 5 restore, observers see fresh tracker |
| 6 | `core.health.respawn_ok()` (line 666) | clean |
| 7 | sleep 2s + `write_to_agent` system message (line 671-682) | best-effort cosmetic — agent missing message is acceptable |
| 8 | spawn `<n>_tui_server` thread (line 690) | spawn err → respawned agent has no TUI socket, F5 phantom |

#### `replace_instance` flow

Not separately implemented as a daemon function — operator achieves "replace" via `delete_instance` followed by `spawn_instance` (two API calls). This means the partial-failure window of `delete` (F2) compounds with the partial-failure window of `spawn` (F1). Recommendation: add a daemon-side `replace_instance` that holds the registry lock across delete + spawn, gating concurrent ops.

### Track B specific deliverable: Backend × capability matrix (cross-area with Track A)

Per-backend semantics for spawn-related capabilities. Cells: ✅ verified by code; ⚠️ inferred from preset; ❓ unverified (existing backlog).

| Backend | PID discovery | Process-tree kill | Signal propagation (SIGINT/ESC) | Heartbeat write | Instructions inject path |
|---|---|---|---|---|---|
| ClaudeCode | ✅ via `child.process_id()` | ✅ `kill_process_tree(pid)` (unix-only) | ❓ inferred OK; existing backlog `t-20260425040356199333-6` flags ESC unverified | depends on agent calling MCP heartbeat tool (no implicit write) | claude-code auto-reads `CLAUDE.md` |
| KiroCli | ✅ same | ✅ same; child tree spans bun/mcp/acp | ❓ same backlog | same | `inject_instructions_on_ready: true` → `spawn_instructions_bootstrap` polls Ready then injects file content |
| Codex | ✅ same | ✅ same | ❓ same backlog | same | inject via TUI typed input |
| Gemini | ✅ same | ✅ same | ❓ same backlog | same | similar to Codex per preset |

**Cross-area finding** (label: B + A intersect): the matrix shows uniform PID discovery + tree kill but **non-uniform signal semantics** (all ❓). PR #159 ESC integration claimed cross-backend support without per-backend test; PR-X transport-only verification left semantics Unverified. This audit confirms the gap is still systemic in Track B's lifecycle code — no backend-specific signal verification at the spawn or kill site.

**Recommendation**: capability matrix entry per backend, asserted in code via the `BackendPreset` struct (e.g. `signal_semantics_verified: bool`), so reviewers can refuse PRs that claim cross-backend signal behavior without per-backend evidence (per protocol §3.5.8).

### Path-keyword auto-Critical mapping

Findings that match dispatch §3 path-keywords (`daemon spawn / signal propagation / lifecycle invariant`):

- F1, F2, F3, F4 — all hit "lifecycle invariant" + "daemon spawn"
- F2, F3 also hit "signal propagation"

All 4 already labeled Critical above. No path-keyword finding under-classified.

---

## 2. Praise (3 sub-buckets)

### 2.1 Pattern adoption

- **Lock + drop discipline** in `supervisor::tick` (lines 46-51, 53-121): registry lock taken to snapshot handles, dropped before per-agent core lock; per-agent core lock taken only for the mutation block, dropped before the Telegram spawn. The comment at line 44-45 explicitly names the deadlock scenario being avoided. Pattern is repeated (with the same explanatory comment shape) in the main loop's hang detection block (`mod.rs:378-440`). This is exactly the discipline that prevents deadlocks against MCP handlers that take the locks in different orders.
- **`crash_tx` bounded(64)** (line 234) with `try_send` on the producer side, with rationale comment: "every fleet member crashing at once" sets the upper bound. The bounded + try_send choice prevents a stuck consumer from blocking PTY close handlers — exactly the Sprint pre-Sprint 1 P2-2 review finding the comment cites.
- **Snapshot-then-process** in `supervisor::tick` extends to the action enum `NoticeAction` (lines 147-153): the lock-held block produces an enum, then the post-lock block dispatches. This is the right pattern for "decide while holding lock, side-effect after release", and it makes the lock-window auditable from the enum branches alone.

### 2.2 Defensive comments / rationale documented

- `supervisor.rs` module doc lines 7-8 explicitly document the fire-and-forget rationale ("Shutdown is implicit: when the hosting process exits, this thread dies with it"). This is the cleanest example of the explicit-rationale standard the audit is checking — every other spawn site should match.
- `health.rs` lines 18-37 explain why `AWAITING_OP_SILENCE` is 30s, why only `Starting` is considered, what the threshold trades off, and which alternative checks (pattern-based InteractivePrompt, `check_hang`) cover the other regimes. This is the kind of constant-doc that lets future-you change the value safely.
- `agent.rs:439-475` (`spawn_instructions_bootstrap` content read at spawn time): comment explicitly closes the file-mutation-window attack: "Read the instructions body here — while we hold the spawn context and before the `Ready` poll window starts — so an external process mutating the file between write and bootstrap cannot inject a different prompt."

### 2.3 Tests that lock contracts (not just behavior)

- `supervisor::tests::waiting_on_cleared_when_heartbeat_stale` (line 235-285) tests both the stale (clear) and fresh (do not clear) branches. Locks the §4.4 design contract directly, not the implementation. Future refactor that flips the polarity will fail this test loudly.
- `watchdog::tests::test_watchdog_dry_run_env_logs_to_event_log` (line 62-) verifies the dry-run mode via *observed side-effects* (event_log content) rather than via a method-call counter. The shape rules out a "dry_run flag was passed but ignored" regression.

---

## 3. Coverage

What was audited and how. (Strict 5 sub-section: "(none observed — audit complete)" not applicable here since coverage is positive enumeration.)

| Surface | Depth | What I checked | What I did NOT check |
|---|---|---|---|
| `supervisor.rs` | Full | Every fn body, both spawn + tick + 2 helpers; tests | none |
| `daemon/mod.rs` lifecycle blocks | Targeted: lines 230-710 (startup + main loop + crash respawn), 1015-1140 (spawn_and_register + upgrade_marker) | every spawn site, the `select!` arms, registry insert/remove pairs | hot-reload diff + apply (`apply_fleet_reload`), schedule check (`check_schedules`) — Track D MCP overlap |
| `agent.rs` | Targeted: spawn_agent (325-480), spawn_instructions_bootstrap (489-549), pty_read_loop entry (430), dismiss spawn (770-805) | full spawn flow, instructions inject thread, dismiss thread | `pty_read_loop` body, `inject_to_agent`, `write_to_agent` typed variant — would push past 2h |
| `ci_watch.rs` spawn site | Lines 395-432 | the spawn site + its dummy-handle fallback | poll provider plumbing, GitHub auth, rate-limit backoff (covered by Sprint 18 PR-AP) |
| `health.rs` | Tier-2 walkthrough | `HealthState` enum, constants doc (lines 12-37), public API surface | crash counter math, decay logic — would need test runs |
| `watchdog.rs` | Tier-2 | `run_watchdog_pass` logic, dry-run env parse, tests | classification patterns (`state::classify_pty_output` lives elsewhere) |
| `poll_reminder.rs` | Tier-3 grep | spawn / loop / external mutation patterns | full read |
| `app/mod.rs` `kill_agent` | Cross-area read | F3 only | rest of app mode |
| `api/handlers/instance.rs` | Cross-area read | F2 delete + handle_kill + handle_spawn entry | all other instance API handlers |
| `backend.rs` `BackendPreset` | Cross-area read for matrix | preset fields, spawn_flags signature | per-backend args plumbing |

---

## 4. Refactor (vs Findings)

Findings = correctness gap (this codebase has a defect). Refactor = structural change without correctness gap.

- **R1. Extract `daemon::lifecycle` module** for `spawn_and_register_agent`, the respawn closure, and the (proposed) `replace_instance`. Currently these flows are scattered across `mod.rs:1025`, `mod.rs:629-704`, and `api/handlers/instance.rs:131-`. Centralizing makes the partial-failure trace (above) maintainable. Benefit: future "make spawn transactional" work has one place to edit. Cost: ~250 LOC move + integration test updates.
- **R2. Generalize `kill_process_tree` into a `BackendKill` trait** so the matrix gap (signal semantics ❓) becomes a typed obligation. Each backend implements `fn after_kill(&self, pid)` for backend-specific cleanup (kiro-cli might want to wait for the bun pipe to close). Benefit: fixes F3 + makes F2 partial-failure auditable. Cost: trait + 4 impls + plumbing through `kill_process_tree`.
- **R3. Add `daemon_tick` to a tick registry** so `instance_monitor`, supervisor, ci_watch, and the implicit "main loop" tick all show up in a single `list_ticks()` debug surface. Benefit: makes F11 mechanically obvious; future ticks (e.g. Sprint 21 health-decay tick) are forced through a registry. Cost: 1 small module + migration of 4 spawn sites.

These are not bugs — they are leverage moves enabled by the Findings.

---

## 5. Cross-area dependencies

(Each cross-area item also gets a label from the matched track.)

- **Track A (Channel)**: `supervisor::tick` calls `crate::channel::active_channel()` (lines 126, 134) and `ch.notify(...)` to push stall/recovery notices. If Track A's audit finds `active_channel()` non-thread-safe, supervisor inherits the issue. Cross-label: B + A.
- **Track A (Channel)**: `delete_instance` calls `fleet_broadcast::broadcast` (line 120) which Track A owns. The partial-failure trace step 8 above is a cross-area concern — Track A may need to add idempotency for InstanceDeleted on its receive side.
- **Track D (MCP handlers)**: All `handle_delete` / `handle_spawn` / `handle_kill` paths are MCP-facing. Track D should audit for the same partial-failure issues from the MCP-tool perspective (does the MCP tool retry semantics survive a partial delete?). Cross-label: B + D.
- **Track D (MCP handlers)**: `set_waiting_on` MCP tool writes the metadata that `supervisor::tick` reads (F7). If Track D's audit finds the MCP tool also writes the two fields independently, the F7 race is end-to-end, not just supervisor-local.
- **Track C (TUI)**: `serve_agent_tui` thread (mod.rs:1080) is the bridge to the TUI render layer. F5 (no rollback on TUI server thread spawn fail) means TUI's "agent attached" detection should treat missing TUI socket as a recoverable state, not assume the agent doesn't exist.
- **Out-of-scope but flagged for cross-pollination**: `instance_monitor::spawn_monitor_tick` (mod.rs:306) is dispatch-noted as "your PR-AZ" but lives in `src/instance_monitor.rs` — not in Track B file scope. Findings about its JoinHandle handling were the original motivation for this audit (per PR-AZ r2 reviewer-2 note); they are systemic across all tick spawns called from `daemon/mod.rs`.

---

## 6. Sprint 21 actionable tasks

(These are concrete tasks the audit produced. Each has a dispatch-ready scope.)

| ID | Title | Deliverable | Risk tier |
|---|---|---|---|
| S21-B1 | `spawn_agent` rollback on partial failure (F1) | Wrap `pty_system.openpty + spawn_command + take_writer + try_clone_reader + reg.insert + spawn(pty_read_loop)` in a transactional struct that kills the child + removes registry entry on early Err. | High (correctness) |
| S21-B2 | `delete_instance` synchronous wait on child exit (F2) | After `kill_process_tree`, poll `child.try_wait` with bounded timeout (5s) before mutating registry. Document timeout fallback (force-remove anyway, log warn). | Critical (correctness) |
| S21-B3 | `kill_agent` (app mode) parity with API delete (F3) | Switch app-mode kill to `kill_process_tree` + add cleanup parity (event_log, configs, ipc port). | Critical |
| S21-B4 | Document `pty_read_loop` shutdown contract (F4) | Add doc-comment at spawn site naming the "shutdown closes PTY → read returns EOF → loop exits" assumption. Optionally add `set_read_timeout` for explicit observability. | Medium |
| S21-B5 | TUI server spawn rollback (F5) | At both spawn sites, on Err return, also remove registry entry + kill child. Or: detach TUI socket creation from spawn (pre-create, fail spawn if not). | High |
| S21-B6 | Atomic `waiting_on` clear (F7) | Extend `agent_ops::save_metadata` to accept multi-field patch, OR add `clear_waiting_on_fields()` that writes both keys in one disk operation. | Medium |
| S21-B7 | Health-tracker restore in respawn before observable insert (F8) | Move `saved_health` restore to occur **inside** the registry-insert critical section (or do not insert until restored). Prevents the "fresh tracker" window. | Medium |
| S21-B8 | Tick channel try_send (F9) | Replace `tx.send(())` with `try_send` + `tracing::debug!("tick dropped — main loop busy")`. | Low |
| S21-B9 | Name the dismiss thread (F10) | Trivial `Builder::new().name(format!("{n}_dismiss"))` wrap at agent.rs:772. | Trivial |
| S21-B10 | Per-spawn-site rationale comment (F11) | Audit + add `// fire-and-forget: <reason>` comment at each spawn site without rationale. Use supervisor.rs module-doc as template. | Trivial (doc-only) |
| S21-B11 | Per-backend signal capability matrix typed (R2 / cross-A) | Add `verified_signal_semantics: bool` (or richer enum) to `BackendPreset`. Reviewers refuse cross-backend claims without per-backend test. Closes the same gap PR #159 left open. | Medium (long-tail) |
| S21-B12 | Daemon `replace_instance` API (cross-D) | Single-call atomic delete+spawn behind one registry lock. Removes the F1+F2 compounded window. | Medium |

Suggested Sprint 21 cluster: **S21-B1 + S21-B2 + S21-B5** as one PR (lifecycle transaction); **S21-B3** as one trivial PR; **S21-B6 + S21-B7** as one PR; **S21-B11 + S21-B12** as a backlog spike (needs scope decision before dispatch).

---

## 7. Peer pass

**TODO** — appended as a follow-up after Track A (`docs/codebase-review-2026-04-27/CHANNEL.md`) lands. 1-paragraph blindspot critique reading dev-impl-1's CHANNEL.md from a daemon-lifecycle perspective. Pipeline: I push this DAEMON.md first, wait for Track A push, then send peer-pass critique via inbox to dev-lead (or amend this file if dispatch prefers in-tree).
