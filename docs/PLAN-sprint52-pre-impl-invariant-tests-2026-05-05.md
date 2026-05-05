# Sprint 52 Pre-IMPL Invariant Test Plan

**Date**: 2026-05-05
**Author**: lead
**Status**: PLAN (mandatory gate per operator §13 #10 before dev IMPL dispatch)
**Source-of-truth**: `origin/main` HEAD `569715a` (Sprint 52 PLAN PR #431 just merged)
**Source PLAN**: `docs/PLAN-sprint52-router-layer-channel-discipline-2026-05-05.md`

---

## §0 Why this exists

Operator §13 GO answers (m-0):
- **#5 feature flag default-ON ship day-1** (no opt-in cushion)
- **#10 pre-IMPL invariant test gate mandatory** (last cushion against deadlock)

Combined: when PR-B merges, every fleet instance immediately runs the new
router layer. The same posture sank Sprint 49 (PR #424 → reverted PR #425)
on a deadlock that surfaced post-merge. This plan defines the invariant
tests that must exist **inside PR-A and PR-B** so a Sprint 49-class regression
fails CI before merge, not in production.

The plan is itself a docs PR so operator can reject specific invariants
before dev IMPL begins. Non-PR alternative considered + rejected: writing
the invariants directly into a PR-A IMPL diff would conflate dispatch with
gate review, and operator m-0 explicitly named "pre-IMPL" as the boundary.

## §1 Scope

Five invariant categories. Each invariant is a concrete cargo test that PR-A
or PR-B must add (or extend), with the assertion specified at compile time
where possible.

| # | Invariant | Type | Lands in | Mandatory? |
|---|-----------|------|----------|------------|
| 1 | Lock ordering — registry → channel → router → heartbeat_pair | Runtime assertion + integration test | PR-A | Yes |
| 2 | No supervisor → daemon-API self-IPC from router thread | Source-grep regression test (`tests/`) | PR-A | Yes |
| 3 | reply_to lifecycle correctness (set/clear/per-turn) | Unit + property-based test | PR-A | Yes |
| 4 | Mirror dispatch dedup (event-id + 1s window) | Unit + property-based | PR-B | Yes |
| 5 | Stress: 5-10 agent flood + lock contention + restart recovery | Integration test (gated, opt-in) | PR-B | Yes — merge gate |

§13 §3 closure metric (`0 deadlock + mirror accuracy ≥85% + TUI zero
false-positive`) is verified in pilot, not by these tests. The tests
guarantee structural correctness; pilot guarantees runtime behaviour.

## §2 Invariant 1 — Lock ordering (PR-A)

### Hierarchy under contract

Per `docs/DAEMON-LOCK-ORDERING.md` + dev STRUCTURAL m-40 §8:

```
L1 registry (HashMap<InstanceId, AgentHandle>)
  └─ L2 agent_core (Mutex<AgentCore>, contains subscribers, state, vterm)
       └─ L3 heartbeat_pair (Mutex<HeartbeatState>, leaf)
```

Router thread MUST never acquire L1 or L2. It reads from a crossbeam channel
(no lock) and writes only to L3 (heartbeat_pair) and to channel adapters
(`channel::send_from_agent`, no daemon lock involved).

### Test 1a — runtime assertion (always-on, debug + release)

Add a thread-local `LockTier` cell. Each lock guard increments the cell on
acquire, decrements on drop, and asserts the new tier number is strictly
greater than the previous (i.e. you cannot acquire L2 while holding L3).
The router thread sets a `RouterThreadMarker` thread-local at spawn that
forbids tier ≤ 2 acquisition entirely.

The assertion is in the `lock_tier_assert!` macro in a new
`src/sync_audit.rs`. Behaviour:

- `cargo test`: panics on violation, fails the test.
- Release builds with `AGEND_LOCK_AUDIT=1`: logs error + bumps a metric, no
  panic. Production runs see the log line; CI canary catches it on first
  hit.
- Default release: macro compiles to `()` — zero overhead.

### Test 1b — integration test `router_thread_never_holds_L1_L2`

Spin up a daemon harness with one agent. Inject 100 PTY events through the
subscriber channel. Use `tracing` subscriber to capture every lock acquire
log line tagged with thread id. Assert no router-thread log line shows L1
(`registry`) or L2 (`agent_core`) acquisition.

This test is the empirical complement to the static design claim in
PLAN §3.3. If lock_tier_assert ever passes locally but the integration
test catches a violation, it means the assertion was bypassed (e.g. via
unsafe interior mutability) — flagged as bug.

## §3 Invariant 2 — No supervisor self-IPC (PR-A)

### Source-grep regression test `tests/sprint52_no_self_ipc.rs`

Sprint 49 deadlock root: supervisor tick called `api::call(...)` on the
same daemon, which acquired a lock supervisor already held. Sprint 52
router runs on a separate thread but a future contributor could
accidentally re-introduce the pattern.

Walk the source tree and assert:

```
For each file in src/daemon/router.rs and src/daemon/supervisor.rs:
  Forbid:
    - api::call(
    - Client::new(...).post(http://localhost ...)
    - api::handlers::* invocation paths
  Allow:
    - channel::send_from_agent (direct adapter, no daemon API)
    - heartbeat_pair::* (leaf state)
    - inject_to_agent (direct PTY write)
```

Implementation: read each file as text, scan with line-by-line regex,
emit per-violation panic. Source-grep is brittle but cheap — a future
refactor that renames `api::call` will need to update the test, which is
the explicit point of an invariant test.

The same test was added in Sprint 49 hotfix PR #432 (`re_inject_path_does_not_self_ipc`)
on a per-callsite basis. Sprint 52 generalises it module-wide.

## §4 Invariant 3 — reply_to lifecycle (PR-A)

### State machine

```
None ──(inbox dequeue)──► Some(channel, input_id, set_at_ms)
                              │
                              ├──(TUI keyboard write)─────► None
                              ├──(Ready/Idle transition)──► None
                              ├──(silence_timeout 3s)─────► None (PR-B)
                              └──(daemon restart)─────────► None (ephemeral)
```

### Unit tests

- `reply_to_set_on_inbox_dequeue` — telegram input → state contains
  `(channel="telegram", input_id=N, set_at_ms=T)`.
- `reply_to_cleared_on_tui_input` — TUI keyboard byte → state == None.
- `reply_to_cleared_on_ready_transition` — agent reaches `AgentState::Ready`
  → state == None.
- `reply_to_input_id_monotonic` — sequential dequeues produce strictly
  increasing `input_id`.
- `reply_to_ephemeral_across_restart` — write state, simulate restart,
  reload — state == None.

### Property-based test `reply_to_state_machine_converges`

`proptest`-style: generate random sequence of {Dequeue(channel), TuiKey,
Ready, Restart}. Apply to state machine. Final assertion: state ==
expected based on last clearing event. Run 1000 sequences (proptest
default). Catches edge cases like out-of-order events or stuck transitions.

## §5 Invariant 4 — Mirror dedup correctness (PR-B)

### State

Each `(input_id, mirror_event_id)` pair is dispatched at most once. Two
fallbacks ensure correctness:

1. Primary: `last_mirror_event_id` per agent — skip if already dispatched.
2. Fallback 1s: `mirror_dispatched_for_turn` flag — set true on dispatch,
   cleared on next Ready/Idle.

### Unit tests

- `mirror_dedup_blocks_double_emit` — same `mirror_event_id` dispatched
  twice → second dispatch skipped.
- `mirror_dedup_clears_on_next_turn` — dispatch in turn N, transition
  Ready, dispatch in turn N+1 → both succeed.
- `mirror_skip_set_on_reply_tool_call` — `handle_reply` sets
  `mirror_skip_until_next_turn` → router skips current turn dispatch.
- `mirror_silence_timeout_3s` — no PTY output for 3s after input → router
  treats as end-of-turn even without state edge.

### Property-based test `mirror_dedup_no_double_within_turn`

Random sequence of {PTYByte, StateTransition, ReplyToolCall, Restart}.
After each event, count mirrors dispatched within the current turn
window — must be ≤ 1.

## §6 Invariant 5 — Stress test merge gate (PR-B)

### Configuration

Operator §13 #8: 5-10 agent stress. Run as `cargo test --release
--test sprint52_stress -- --ignored` (gated to keep default CI fast).

### Stress categories

#### 5.1 Event flood

Spin 5 agents. For each: 1000 synthetic PTY bytes/sec for 60s. Assert:
- No deadlock (`thread_census` shows progress within 30s watchdog window).
- Mirror dispatch latency p95 < 2s.
- Subscriber channel never saturates beyond bounded capacity.

#### 5.2 Queue overflow

Bounded subscriber channel saturated by faster producer than consumer.
Assert drop policy fires + metric incremented + no panic. (Bounded
channel intentional per PRIOR-ART m-39 — drop+metric, not unbounded.)

#### 5.3 Lock contention

10 agents concurrent dequeue + mirror dispatch + state transitions.
Watchdog 30s. Assert no deadlock + per-agent mirror correctness preserved.

#### 5.4 Restart recovery

Run for 30s. Trigger daemon restart simulation (drop + reload state).
Assert ephemeral router state cleared correctly + new traffic routes
without stale entries.

### 1-2h soak (manual or CI cron)

Property-based generator runs continuously for 1h with random
{input/state/restart} sequences. Drift counter on dedup correctness.
Acceptable: <0.1% drift per reviewer COST-BENEFIT m-42 §6.

## §7 Implementation order

### PR-A delivers invariants 1, 2, 3

Lock ordering (1a + 1b), source-grep no-self-IPC (2), reply_to lifecycle (3).

Tests run on every CI build. Invariant 1a (`lock_tier_assert!`) is the
runtime contract that other code observes — must be in place before any
new lock-acquiring code lands.

### PR-B delivers invariants 4 and 5

Mirror dedup (4) and stress test gate (5). Invariant 5 is the merge gate:
PR-B does not merge without all 4 stress categories green + 1h soak.

24h soak deferred to Sprint 53 per reviewer COST-BENEFIT m-42 §6.

## §8 §13 candidate questions for operator

1. **Lock-tier macro release behaviour**: log + metric (no panic) vs panic on violation? Default = log + metric per §2 above.
2. **Source-grep test brittleness**: accept that future refactors must update the regex list, or build a more semantic AST-based check?
3. **Property test runtime budget**: 1000 sequences (proptest default) on every CI build, or gated to nightly?
4. **Stress test agent count**: 5-10 per operator §13 #8 — pick 5 (faster CI) or 10 (more contention)?
5. **Stress test runtime in CI**: ignore by default (`--ignored` gate) and run only as merge-gate manual step, or run as full CI on PR-B branch?
6. **Drift counter threshold**: <0.1% per PLAN §6 — accept, or different bar?
7. **Pre-IMPL plan landing**: this doc as standalone PR (current proposal) vs amend onto Sprint 52 PLAN doc PR #431 (already merged)?
8. **PR-A merge prerequisite**: invariants 1+2+3 all green; does test isolation regression count?

## §9 Estimates

- This plan PR review + iterate: ~30-60 min
- PR-A invariant tests: ~50 LOC (estimate 1-1.5h on top of PR-A's ~80 LOC base)
- PR-B invariant tests: ~150 LOC (stress harness ~100, property-based ~50, on top of PR-B's ~100 LOC base)
- 1h soak fixture setup: ~20 LOC
- Total invariant LOC: ~220, distributed across PR-A + PR-B

## §10 Reuse from prior synthesis

- Sprint 49 PR #432 `re_inject_path_does_not_self_ipc` — invariant 2 generalisation
- `feedback_test_parallel_race_check.md` — applies to PR-A + PR-B test design
- `docs/DAEMON-LOCK-ORDERING.md` — invariant 1 hierarchy source-of-truth
- Sprint 52 PLAN §3.3 lock-ordering claim — invariant 1 verifies empirically
- Sprint 52 PLAN §3.5 dedup design — invariant 4 verifies empirically

---

**End of pre-IMPL invariant test plan — awaiting operator §13 answers + GO before PR-A dispatch**
