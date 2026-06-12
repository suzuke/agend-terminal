# Refactor Plan — #2050 Architecture Pass

> Phased plan derived from [architecture.md](architecture.md) §6 (tensions)
> and the six subsystem surveys (`main` @ `65d9ad82`, 2026-06-12). Each item
> is sized to be one reviewable PR an implementing agent can land
> independently. Waves are ordered by risk-adjusted value; within a wave,
> items are independent unless noted.

## Principles

1. **Finish the extraction before new abstraction.** The dominant debt
   shape is "gradual extraction left two parallel mechanisms for one
   concept" (inline trackers vs PerTickHandlers; raw `Command` vs
   `git_bypass`; size-driven vs concept-driven handler splits). Completing
   these is low-risk, high-yield, and unblocks everything riskier.
2. **High-care items have hard prerequisites.** The two highest-risk
   targets — the supervisor `tick()` god-fn and the `agend-git` authority
   boundary — do not start until their listed gates are green.
3. **Each PR adds (or updates) a completeness invariant** where the smell
   was "a hand-picked subset silently drifts" (the #1002/#1719 class):
   assert the full set, not the curated list.
4. **No behavior change inside a structure PR.** Refactor PRs preserve
   semantics bit-for-bit; behavior fixes ride separate PRs. KISS: no
   speculative flexibility, no config for things nobody configures.
5. **Findings are hypotheses until verified.** The 2026-06-10 readiness
   audit had ~70% false-positive risk findings; survey smells marked
   [hypothesis] get a verify-first read before any code moves.

## Wave 1 — finish the extraction (low risk, high yield)

### W1.1 Unify periodic work under `PerTickHandler`
Wrap the 12 inline supervisor `run_loop` trackers (anti_stall,
idle_watchdog, decision_timeout, helper_staleness, mcp_registry,
waiting_on_stale, conflict_notify, canonical_drift, auto_release,
dispatch_idle, dispatch_idle_nudge, retention_supervisor;
supervisor.rs:266) as `PerTickHandler`s with declared cadence; delete the
inline calls; add a **completeness invariant test** asserting the
registered handler set is the full superset (the app-mode-wiring-drift
class). Preserve relative order — handler order is load-bearing
(daemon/mod.rs:577-579).
**Effort** ~1 day · **Risk** low (each tracker already self-contained) ·
**Source** survey 01-R2 / 06-A. Best value-to-risk in the plan; do first.
**Status** ✅ Done — PR #2065. All 12 trackers wrapped as `PerTickHandler`s
and appended to `build_default_handlers` (now 32 handlers) in their original
relative order; supervisor inline calls deleted; completeness invariant
`all_twelve_supervisor_trackers_registered_in_order` pins the full set.

### W1.2 `git_cmd` helper — absorb daemon-side raw-git boilerplate
`git_cmd(dir, args) -> Result<String>` (always-bypass, trimmed stdout,
structured error) + `git_ok(dir, args) -> bool`. Migrate the ~51
daemon-side raw `Command::new("git")` sites (worktree_pool 27,
worktree_cleanup 15, branch_sweep 8, binding…). **Excludes** the
`agend-git` shim (deliberately raw — it is the gated side) and tests
wanting raw control. Makes "forgot `AGEND_GIT_BYPASS`" structurally
impossible daemon-side and kills a flaky-test class.
**Effort** 0.5–1 day · **Risk** low, mechanical, reviewable per call-site ·
**Source** survey 05-R1.
**Status** ✅ First slice done — PR #2068. `git_cmd`/`git_ok` landed; 4 modules
migrated + sealed (`branch_sweep`, `worktree_cleanup`, `worktree_pool`,
`binding`) by the `tests/daemon_git_helper_invariant.rs` per-slice
`MODULE_SCOPE` scanner (FAILs CI on an unmarked raw `Command::new("git")` in a
sealed module). **Scope correction**: the daemon has **~150** raw git sites
across **~25** modules (not the ~51 estimate above); `MODULE_SCOPE` grows
monotonically as each later slice adds its module, so the seal never claims
unearned coverage. Remaining-module migration is the backlog: task `t-…766-17`.

### W1.3 Quick wins (one small PR each)
- Unify the duplicated tool-timeout maps (`request_dedup.rs:465` ↔
  `mcp_proxy.rs:20`) into one table — drift-prone today. (survey 02-#3)
- Extract `spawn_one` (api/mod.rs:665, cohesive 80-LOC agent-spawn fn)
  out of the api server file into `agent_ops`. (survey 02-#4)
- Fix `skills.rs` doc drift (says 5 backends, code has 4 — Gemini
  retired). (survey 06-E)

**Status** ✅ Done — three PRs: #2066 (skills doc 5→4 backends), #2067
(`spawn_one` → `agent_ops`, + a `// fire-and-forget:` rationale upgrade off
`api/mod.rs`'s legacy spawn-audit exemption), #2069 (tool-timeout). NOTE: the
tool-timeout item was **not** a duplicate-map merge — there is one per-tool map
(`mcp_proxy::tool_timeout`); `request_dedup::method_wait_timeout` already
delegates to it. The premise-check turned it into a **behavior-fix**: stale
post-consolidation names (`deploy_template`/`watch_ci`/`checkout_repo`) had
stopped matching, silently degrading `deployment`/`ci`/`repo` to the 30s
default (a latent false-timeout on long ops). #2069 restores the intended 60s,
adds a registry-coverage invariant (`tool_timeout_keys_are_registered_tools`,
the #2055 add/remove-tool closure), and de-dups the 5/30/60s band constants.

## Wave 2 — cap relief & mechanical decomposition

Gated on W1 only where noted; otherwise independent.

### W2.1 Split `instance.rs` by concern
748/750 LOC. → `instance_queries` / `instance_state` (absorbing the
existing size-driven `instance_spawn.rs` + `instance_lifecycle.rs` into a
coherent module) / `instance_metadata` (metadata + pane + health).
Zero API change; the three concerns share no state.
**Effort** M · **Risk** low · **Source** survey 02-#1.

### W2.2 Break up `handle_delegate_task` + extract `comms_gates`
comms.rs at cap; `handle_delegate_task` (317 LOC) inlines busy-gate,
second-reviewer, test-name validation, auto-task-creation; report path
inlines sha_gate + evidence_gate. Recompose as
resolve→validate→create→lease→send→track with gates as one fallible,
unit-testable pre-check module (fold in the existing sha_gate /
evidence_gate / anti_stall sibling files).
**Effort** M · **Risk** low-medium (failure ordering must be preserved —
route failure must still suppress provenance/dispatch side-effects) ·
**Source** survey 02-#2, 03-A.

### W2.3 `feed_with_fg` gate-pipeline extraction
Decompose the 549-LOC classifier method into named `apply_*_gate` steps in
an explicit call sequence (ordering stays encoded in one place). This is
the legibility prerequisite for every other state-detection change,
including #1523 phase-2.
**Effort** M · **Risk** low (structure-only) · **Source** survey 04-#2.

### W2.4 `CadenceGate` + `PerAgentLatch<T>` utilities
Collapse ~15 hand-rolled cadence counters and ~10 per-agent latch maps;
`PerAgentLatch` builds in boot-grace + prune so a new watchdog **cannot**
forget the boot-grace gate (today it is hand-wired per handler).
Do after W1.1 so the latch sites are all in handler form first.
**Effort** 0.5 day · **Risk** low, churns many files — keep it its own PR ·
**Source** survey 01-R3.
**Status** ✅ Done (scope-trimmed) — PR #2080. `CadenceGate` shipped: unifies
the ~23 hand-rolled cadence counters (11 handlers via `new`, 12 supervisor
trackers via `new_interval`) AND structurally bundles the notification-watchdog
boot-grace (`new_with_boot_grace`) — survey S6's real goal, so a new watchdog
gets boot-grace from the constructor, impossible to forget. **`PerAgentLatch`
was CANCELLED after survey**: its boot-grace closure is already delivered by
`CadenceGate::new_with_boot_grace`, and its "built-in prune" was a
dead-API-or-behaviour-change dilemma (the 3 `Mutex<HashMap>` latch sites never
prune today; adding prune changes same-name-redeploy state — a behaviour-fix,
not a refactor). Remaining value was below the churn cost (#1561
default-don't-build). The real in-mem leak (latches not pruning deleted agents)
is a separate behaviour-fix backlog item (the #1923 G5 cleanup-on-delete class);
`handoff_timeout`'s tuple-keyed latches stay as-is.

### W2.5 Backend preset factory collapse
`backend.rs preset()` 300-LOC 6-arm factory with repeated fields → table /
default-merge. Adding a profile field currently means 7 edits.
**Effort** M · **Risk** low · **Source** survey 04-#3.

### W2.6 Name the resize contract
Keep both resize chokepoints (#2048), extract a shared
`PaneContentRect`/`ResizeDecision` helper, and document the invariant:
layout pre-computes, render is authoritative for the final inner rect.
**Effort** S-M · **Risk** medium — do NOT remove render-time resize
without PTY tests · **Source** survey 03-C.

## Wave 3 — high-care strategic (hard prerequisites)

### W3.1 Supervisor `tick()` decomposition
The 469-LOC god-fn → phase pipeline with the #1530
collect-under-lock / emit-after-drop boundary made **structural**
(a `LockedPhase`→`EmitPhase` split) instead of the implicit
`let action = {}` block.
**Prerequisites (all green before starting):**
- [DAEMON-LOCK-ORDERING.md](DAEMON-LOCK-ORDERING.md) reviewed/extended to
  cover the refactored shape;
- the #1644 source-pin passing, plus a new emit-after-drop structural pin
  written for the phase form;
- the supervisor.rs:1857 residual core-under-registry read audited
  (#1530/F2).
**Effort** 1–1.5 days · **Risk** HIGH — hottest path, the #1492/#1530
invariants are load-bearing · **Source** survey 01-R1.

### W3.2 `agend-git::classify` table-driven + #2027 uniform deny
Replace the 214-LOC match with a `(&[subcmd], Gate)` table
(Passthrough / ChdirPass / Deny / SilentExempt / CleanupPushPass) so every
subcommand gets an explicit policy — closing the loud-deny vs silent-noop
inconsistency (#2027) **by construction**. Bundle the #2027 fix; same file,
same test discipline.
**Prerequisites:** full agend-git conflict suite green (run with
`PATH=/usr/bin` — the suite false-fails under the fleet shim).
**Effort** 1–1.5 days · **Risk** MEDIUM-HIGH — this is the authority
boundary · **Source** survey 05-R2.

### W3.3 #1523 phase-2 — authoritative state for the deciders
Migrate the 5 raw-heuristic per-tick deciders (hang_detection.rs:72,
watchdog.rs:56, recovery_dispatcher.rs:288, supervisor.rs:1313 + :1861) to
the hook-aware authoritative state. **Safe design (required):** snapshot.rs
pre-fills a per-tick authoritative cache before the decider sequence
(scheduler order snapshot → hang → recovery → watchdog → supervisor);
deciders read the cache — never call into hook_shadow from under the core
lock (registry⨯core inversion risk).
Keep raw: conflict_notify (hooks can't see GitConflict), hook_event:36
(it IS the comparison baseline), instance.rs:24 (restarting gate must be
live). Design call to make explicitly: query.rs:19 (LIST = last-tick
snapshot vs live).
**Phase-2 must inherit, not "fix", fail-open ToolUse** — no clock
backstop (architecture.md §4).
**Prerequisites:** W2.3 landed; wire `behavioral.rs` divergence telemetry
(built, unused) to gather heuristic⨯hook agreement data BEFORE flipping
any default.
**Effort** H · **Risk** medium (lock ordering) · **Source** survey 04-#1/§4.

## Wave 4 — opportunistic / design-call-first

Do when the owning file is already open, or after an explicit design
decision. Not scheduled.

- **Messaging pipeline modules** (validate/route/post_delivery split of
  messaging.rs, keeping `handle_send` as the single ordered choreography) —
  pairs with W2.2. (03-A)
- **DeferredNotificationDelivery service** — one named API over the
  queue/policy/TUI/headless four-layer split. (03-B)
- **Channel notify ownership** — decide whether `Channel::notify` is the
  canonical seam, then retire the concrete `notify_telegram*` entrypoints.
  HIGH risk, staged migration, needs the decision first. (03-D)
- **`process_error_recovery` extraction** (370 LOC SRL/ApiError machine) —
  riskiest semantic surface in daemon-core (#1946/#1985 live here);
  defer until W3.1 proves the phase shape. (01-R4)
- **Deployment lifecycle submodules**; **CLI command modules**;
  **schedules model/store/crud split**; **release-gate script**;
  **`SchemaVersioned` unification** (verify the trait's fail-closed
  default against fleet's warn-not-refuse #1989 before merging the two);
  **tasks/handler.rs split**; **dispatch_hook options-struct**;
  **row-level task-board cache**. (02/05/06 long tail)

## Sequencing summary

```
W1.1 trackers→handlers ─┐
W1.2 git_cmd            ├─ independent, start immediately
W1.3 quick wins         ─┘
W2.1/W2.2 cap relief    ── after W1 merges settle (same files)
W2.3 gate pipeline      ── independent; prerequisite for W3.3
W2.4 latch/cadence      ── after W1.1
W2.5/W2.6               ── independent
W3.1 tick() decompose   ── gates: lock doc + #1644 pin + :1857 audit
W3.2 classify table     ── gate: conflict suite green (PATH=/usr/bin)
W3.3 #1523 phase-2      ── gates: W2.3 + divergence telemetry wired
W4                      ── opportunistic
```

## Acceptance per PR

- Reviewer VERIFIED + required CI green (3-platform Check, LOC gate,
  audit) before merge — no shortcuts.
- Refactor PRs: `cargo test` byte-identical behavior expectations; any
  moved invariant gets its pin test moved/extended in the same PR.
- LOC-EST declared honestly in the PR body; oversized cohesive moves use
  the `loc-overrun-accepted` label after a reviewer cohesion-accept.
- Worktree-side test runs use `AGEND_GIT_BYPASS=1`.
