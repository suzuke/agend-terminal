[繁體中文](architecture.zh-TW.md)

# AgEnD Terminal — Architecture Map

> Current-state structural map, synthesized from the #2050 architecture pass
> (six subsystem surveys, all claims verified against `main` @ `65d9ad8`,
> 2026-06-12). Companion doc: [REFACTOR-PLAN.md](REFACTOR-PLAN.md) — the
> phased plan derived from this map.
>
> Newcomers: read [ARCHITECTURE-QUICK-START.md](ARCHITECTURE-QUICK-START.md)
> first. The original rewrite-era design doc is archived at
> [archived/architecture-design-doc-2026-05.md](archived/architecture-design-doc-2026-05.md).
> Lock discipline lives in [DAEMON-LOCK-ORDERING.md](DAEMON-LOCK-ORDERING.md)
> and is summarized (not duplicated) here.

## 1. What it is

A long-running daemon that supervises multiple AI coding agents
(claude / codex / kiro / agy / opencode) as PTY child processes and gives
them: inter-agent MCP communication, fleet-as-code configuration, per-agent
git-worktree isolation, health monitoring with auto-respawn, and TUI +
Telegram remote control. The core value is **multi-agent orchestration**;
everything else serves it.

~198K LOC Rust across 286 files. Three binaries: `agend-terminal` (daemon +
TUI + CLI), `agend-mcp-bridge` (per-agent stdio↔TCP MCP relay),
`agend-git` (PATH-shim `git` policy gate).

## 2. Subsystem map

Six subsystems, by size. Each has a dedicated survey trail in the #2050
pass; the file:line evidence below is the load-bearing subset.

### 2.1 Daemon core / lifecycle (~52K LOC)

The heart: observe each agent's state machine, react, recover.

| Module | LOC | Role |
|---|---|---|
| `daemon/supervisor.rs` | 5408 | Per-agent state OBSERVATION + reaction emission + error recovery (SRL/ApiError). The 12 inline slow-tracker scans moved to `PerTickHandler`s in W1.1 (#2065) |
| `daemon/mod.rs` | 2764 | Entry/init/shutdown; builds the per-tick handler set (`build_default_handlers` → 32 handlers) |
| `daemon/per_tick/` | — | `PerTickHandler` trait + 32 handler impls, panic-guarded dispatch, boot-grace gate |
| `daemon/crash_respawn.rs` | 568 | Crash→respawn decision: health budget, escalation persist, respawn worker |
| `health.rs` | 2385 | Per-agent hang/crash budgets, blocked-reason, escalation persist+rehydrate |
| `state/mod.rs` | 2176 | Per-agent `StateTracker` — screen-heuristic state machine (see 2.4) |

Periodic work is now a single pipeline (W1.1 / #2065, see §6.1): the daemon
loop dispatches all 32 `PerTickHandler`s with per-handler `catch_unwind` +
timing (per_tick/mod.rs:182-214). The 12 trackers that used to run inline in
the supervisor `run_loop` were wrapped as handlers and appended in their
original relative order; the supervisor `run_loop` now hosts only `tick()` /
`process_error_recovery()` / a boot-time sidecar GC (still its own 10s thread).

### 2.2 MCP layer + inter-agent comms (~22K LOC)

Macro-architecturally sound: a single immutable 36-entry registry
(`mcp/registry.rs:20`) pairs JSON-schema definitions with handler
fn-pointers, dispatched through one validated chokepoint
(`mcp/handlers/dispatch.rs:77-117`). The strain is micro: two handler files
sit at the 750-LOC `file_size_invariant` cap, and extraction so far has been
size-driven rather than concept-driven (see §6.4).

The transport spine (an agent-to-agent `send`):

```
agent LLM → MCP tools/call {request_id: UUID}     bin/agend-mcp-bridge.rs:327
  → content-dedup (500ms double-fire guard)        agend-mcp-bridge.rs:146
  → cookie handshake → loopback TCP api socket     ipc.rs / api/mod.rs:229
  → operator_gate.check_operation_allowed          api/operator_gate.rs:121
  → request_dedup.dispatch (idempotent retry)      api/request_dedup.rs:165
  → messaging::handle_send (5 phases)              api/handlers/messaging.rs:610
      validate → team/quota gates → build message
      → route_and_deliver → inbox::enqueue (flock + atomic rename)
      → post side-effects (provenance/verdict/dispatch-tracking)
  → target's next poll: inbox::drain (48KB byte-cap, keeps the
    response dedup-cacheable so a lost-transport retry serves the
    cached batch)                                   inbox/storage.rs:286-407
  → daemon wake: compose_aware_inject → PTY        inbox/notify.rs:269-361
```

Idempotency spine: the bridge generates `request_id` once and reuses it
verbatim on retry; `DedupCache` (Fresh/InProgress/Cached/Oversized) bounds:
TTL 10min, 64KB/entry, 64MB total, 8 waiters/id.

The task board is event-sourced (`task_events.rs`, ~20 event variants,
full-replay reads cached by file-len/mtime/generation) with fail-closed
forward-compat.

### 2.3 Channels / API / TUI / render (~24K LOC)

Two overlapping delivery architectures (see §6.5): agent-to-agent sends
flow through `messaging.rs`; operator/channel notifications flow through
channel adapters + UX sinks + a persistent deferred-notification queue
(`notification_queue.rs`) drained exactly-once by either the TUI loop or
the daemon `notification_flush` handler (per-agent OS file lock + unique
claim file — rename-only arbitration double-delivered under CI before).

TUI: `Layout`/`Tab`/`PaneNode` own topology; `render/core_render.rs` owns
drawing; `VTerm` (alacritty wrapper) owns terminal emulation. After #2048
there are two intentional resize chokepoints — layout pre-computes sizes
(`layout/mod.rs:294`), render is authoritative for the final inner content
rect (`render/core_render.rs:437`). Render is deliberately not pure: it
drains pane output and may resize VTerm/PTY before drawing.

### 2.4 State detection / backend profiles (~13K LOC)

The known-most-fragile subsystem — the orchestration's eyes.

Screen-heuristic classifier: PTY bytes → vterm grid → per-backend ordered
regex first-match (`state/patterns.rs:244`) → an order-critical gate
gauntlet (anchor/position/working-marker/recovery/phantom-probe/
UsageLimit-release/heartbeat gates) → single transition funnel
`record_set` (state/mod.rs:2038). Pattern priority is enforced by Vec
position only — first-match-wins, no compile-time precedence invariant.

A second, authoritative path exists for hook-capable backends (claude
today): backend hooks POST events → `daemon/hook_shadow.rs`;
`authoritative_state()` promotes a Fresh hook state over the heuristic,
flag-gated, **snapshot-scoped only** (#1523 phase-1, the
`per_tick/snapshot.rs:51` chokepoint). Five per-tick deciders still read
raw heuristic state (see §6.2 — the planned phase-2 convergence).

Per-backend config is cleanly co-located in `BackendProfile`
(patterns/behavioral/markers per backend); `backend.rs` owns spawn,
model-args, resume and a 6-arm preset factory.

### 2.5 Worktree / git / fleet (~9K LOC)

Per-agent git isolation. The dispatch→bind→worktree chain:

```
dispatch_auto_bind_lease_* (mcp/handlers/dispatch_hook/)
  → BindGuard (per-agent in-flight gate)
  → per-branch flock lease (binding.rs:92) — serializes same-branch races
  → scan_existing_branch_binding — reject if another agent holds it
  → ensure_branch_exists (4-tier repo resolve; #2010 remote resolution;
    #869 stale-ref refresh)
  → worktree_pool::lease → create + .agend-managed marker + bind_full
  → auto-arm ci-watch (+ next_after_ci chain target)
```

`binding.json` has a single writer (`binding::bind_full`) and
HMAC-verified readers (the git shim trusts nothing unsigned). All
daemon-internal git goes through `git_helpers::git_bypass` (timeout +
process-group kill so a timeout-kill can never take down the daemon's own
process group) — but 209 call sites across the codebase still build raw
`Command::new("git")` (see §6.3).

The `agend-git` PATH shim is the authority boundary: `classify()`
(bin/agend-git.rs:679) gates each subcommand as
passthrough/chdir-pass/deny/exempt based on the agent's binding. Branch GC
(`branch_sweep` + `worktree_cleanup`) shares one squash-merge detector and
runs `git worktree prune` inside the delete transaction (#2011).

### 2.6 Ops / entry (bins, service, deployments, schedules, skills)

`main.rs` (1338 LOC) is the CLI switchboard: app/daemon/attach/admin/
service/doctor/skills/quickstart/verify. Service lifecycle is delegated to
the OS supervisor (launchd / systemd user / Task Scheduler) — the daemon
does not supervise itself. `deployments.rs` (2697) owns the template
deploy/teardown lifecycle with deliberately narrow store flocks (self-IPC
always outside the lock). `schedules.rs` (1480) splits storage from the
runtime executor (`daemon/cron_tick.rs`). Release pipeline: annotated tag →
gate (version/changelog/MSRV/semver) → 5-target build + AppImage → GH
Release → crates.io publish (see RELEASING.md).

## 3. Concurrency model

Full hierarchy and rationale: [DAEMON-LOCK-ORDERING.md](DAEMON-LOCK-ORDERING.md).
The load-bearing disciplines, each runtime- or CI-enforced:

| Discipline | Rule | Enforcement |
|---|---|---|
| Lock order | registry (L0) → per-agent core (L1) → side channels (L2) → heartbeat snapshot (L3); release in reverse | `sync_audit::assert_lock_tier` runtime asserts |
| #1492 | No self-IPC while holding registry/core lock | `assert_no_registry_lock_for_self_ipc` (api/mod.rs:784) — fail-fast Err, not deadlock; 90s socket-read backstop |
| #1530 | Collect reaction intents UNDER `core.lock()`, emit AFTER drop | the `let action = { … }` block boundary (supervisor.rs:1282-1293); CI source-pin `tick_emitters_run_after_core_lock_drops` (#1644) |
| Inbox | flock + atomic rename per enqueue/drain/sweep; per-agent `.jsonl.lock` | by construction (inbox/storage.rs:99-139) |
| Deferred notifications | per-agent OS file lock + unique process claim file | by construction (notification_queue.rs:401-424) |
| Deployment store | flock only around load-modify-save; self-IPC API calls outside | by construction (deployments.rs:434-445) |
| git subprocesses | process-group isolation (Unix `process_group(0)` / Windows `CREATE_NEW_PROCESS_GROUP`) so timeout-kill resolves the child pgid | git_helpers.rs |
| UX sinks | mutex protects the sink vec only; clone Arcs, emit after release; emit is fire-and-forget | sink_registry.rs:67-74 |
| Per-tick handlers | each `run()` wrapped in `catch_unwind` — one panic never skips siblings | per_tick/mod.rs:182-214 |

One audited residual: a `handle.core.lock()` inside a registry-lock scope
near supervisor.rs:1857 — flagged in #1530/F2; read it before any
supervisor refactor.

## 4. Load-bearing invariants (beyond locks)

| Invariant | Where | What breaks if violated |
|---|---|---|
| State pattern order: errors before Thinking/Idle, first-match wins | `BackendProfile.patterns` Vec order | misclassification → false idle/hang reactions |
| Gate-gauntlet order in `feed_with_fg` (position gate before working-marker override, etc.) | state/mod.rs:1208-1757 | FP suppression regimes stop composing |
| Hook promotion only via `authoritative_state()` (couples freshness to `has_state_hooks()`) — never `resolved_state_for` directly | hook_shadow.rs:113-123 | stale-hook trust on non-hook backends |
| Hook ToolUse is event-pair-closed, NOT clock-bounded (deliberate — protects long tools, the #1985 class) | hook_shadow.rs:79-98 | a clock backstop would re-break what #1523 exists to fix |
| `request_id` generated once at the bridge, reused on retry | agend-mcp-bridge.rs:329-340 | duplicate side effects on transport retry |
| Inbox drain response ≤ 48KB so it stays dedup-cacheable (< 64KB) | inbox/storage.rs:270 | lost-transport retry drops the remainder |
| Fleet allowlist parsing is fail-closed: one malformed entry fails the list → downstream auth denies all | fleet load / `is_authorized_recipient` | silent partial authorization |
| binding.json single-writer + HMAC sidecar | binding.rs | shim trusts forged bindings |
| Task event log: append under lock with re-replay (`append_checked`), fail-closed on unknown future events | task_events.rs:1034,1538 | TOCTOU board corruption / silent event drops |
| Boot grace (180s) suppresses notification watchdogs after restart | per_tick/mod.rs:118 | restart burst of false alerts (hand-wired per handler — see REFACTOR-PLAN W2) |
| Per-tick handler order matches pre-extraction call order | daemon/mod.rs:577-579 | subtle reaction reordering |
| `spawn` sites carry fire-and-forget rationale or store JoinHandle | protocol §10.4, Phase-5b invariant test | orphan tasks on shutdown |

## 5. Cross-cutting patterns

- **Per-agent latch maps**: `Mutex<HashMap<String, T>>` with bespoke prune
  appears ~10× (supervisor notify/retry tracks, context_handoff/alert
  states, inbox_stuck, handoff_timeout, …).
- **Cadence counters**: `AtomicU64` + `is_multiple_of(N)` hand-rolled ~15×
  across per-tick handlers.
- **Schema-version guards**: three near-identical implementations
  (`FLEET_SCHEMA_VERSION`, `BINDING_SCHEMA_VERSION`, the store-level
  `SchemaVersioned` trait). Fleet is warn-not-refuse (a hand-edited public
  interface, see COMPATIBILITY.md); stores are fail-closed.
- **Instrumentation that became load-bearing**: the #1808 SRL phantom-probe
  fields now drive the cross-cycle SRL→Idle fix (state/mod.rs:1547) —
  telemetry and classification are no longer separable there.

## 6. Known architectural tensions

These are the deliberate or accreted dual-paths. Each is a REFACTOR-PLAN
entry; none is free to "just clean up" without the listed care.

1. **Two periodic-work mechanisms** (§2.1): ✅ RESOLVED by W1.1 (#2065). The 12
   inline supervisor `maybe_scan` trackers are now `PerTickHandler`s in the one
   `build_default_handlers` pipeline (32 handlers); a completeness invariant
   pins the full set so the two-mechanism split can't silently reappear.
2. **Heuristic vs hook state, dual readers** (§2.4): the snapshot path is
   promoted (hook-aware), but hang/watchdog/recovery/supervisor escalation
   still read raw heuristic — in a single tick, dispatch_idle can suppress
   a nudge (snapshot=ToolUse) while hang_detection escalates
   (raw=heuristic-Idle). Bounded today by the #1999 escalation throttle.
   Convergence = #1523 phase-2 (W3; lock-ordering design required — a
   naive shared cache inverts registry⨯core order).
3. **Raw `Command::new("git")` vs `git_bypass`**: ~150 daemon raw git sites
   across ~25 modules; any site that forgets `AGEND_GIT_BYPASS=1` silently hits
   the shim (a whole flaky-test class). W1.2 (#2068) landed `git_cmd`/`git_ok`
   and SEALED its first 4 modules (`branch_sweep`, `worktree_cleanup`,
   `worktree_pool`, `binding`) behind `tests/daemon_git_helper_invariant.rs`, a
   per-slice `MODULE_SCOPE` scanner that grows monotonically as each later
   slice migrates its module. The remaining modules are the backlog
   (`t-…766-17`) — the seal makes regression structurally impossible only for
   already-migrated modules, never claiming unearned coverage.
4. **Size-driven extraction at the MCP cap**: `instance.rs`/`comms.rs` at
   the 750-LOC cap with "extracted for file_size_invariant" cross-file
   seams; `handle_delegate_task` is a 317-LOC god-fn with gates inlined.
   Concept-driven re-split is W2.
5. **Channel trait vs legacy notify functions**: `Channel::notify`
   delegates to older concrete `notify_telegram*` entrypoints; the trait
   is not yet the only production path. Ownership decision before any
   adapter work (W4 design call).
6. **Two resize chokepoints after #2048**: intentional (layout pre-pass +
   render last-mile authority) but undocumented as an invariant until now;
   keep both, name the contract (W2).

## 7. Testing & enforcement landscape

CI: 3-platform Check + LOC-overrun gate + cargo audit (required);
Coverage + daemon-boot flake-gate (non-required). Architecture-level
invariants are pinned by dedicated tests: `file_size_invariant` (750-LOC
handler cap), `tick_emitters_run_after_core_lock_drops` (#1644),
heartbeat-pair atomicity audit, spawn-rationale invariant (Phase 5b),
`daemon_git_helper_invariant` (#2068 — per-slice `MODULE_SCOPE` seal: no
unmarked raw `Command::new("git")` in a migrated module).
Worktree-side test runs need `AGEND_GIT_BYPASS=1` (the shim intercepts
raw-git subprocesses in managed worktrees). Review discipline §3.9: tests
enter through real entry points with representative fixtures — synthetic
unit-inject fixtures have repeatedly hidden production wiring gaps.

## 8. Provenance

Synthesized from the six #2050 survey documents (daemon-core, mcp-comms,
channels-tui, state-detection, worktree-git-fleet, ops-entry) authored by
fixup-dev, fixup-dev-2 and fixup-reviewer against `main` @ `65d9ad82`,
plus the 2026-06-10 production-readiness audit. Line numbers drift; the
named anchors (function names, invariant test names, issue numbers) are
the stable references. Update this map when a REFACTOR-PLAN wave lands,
not on every PR.