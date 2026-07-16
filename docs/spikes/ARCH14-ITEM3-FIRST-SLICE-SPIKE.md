# Architecture-14 Item 3: First Convergence Slice — Source Spike

**Baseline**: `0b127f2a` on main (includes PR #2810 merge)  
**Author**: archfix-opus-3  
**Date**: 2026-07-16  
**Status**: Analysis only — no code, no PR

## 1. Current Durable Authority Map

Six durable stores currently own action lifecycle. Each has independent
persistence, identity, state machine, and settlement semantics.

### 1a. Reviewer Assignment Authority

| Property | Current state | Source |
|---|---|---|
| **Persistence** | File-per-record under `home/reviewer-assignments/<repo>--<branch>--<hash>/` | `assignment_authority.rs:240-277` |
| **Identity** | `assignment_id` (UUID) + `pr_number` (mandatory generation binding) | `assignment_authority.rs:68,71` |
| **States** | Pending → Persisted (monotone) | `assignment_authority.rs:100-104` |
| **CAS** | `assignment_id` UUID on every mutation | `assignment_authority.rs:68` |
| **Supersession** | `tombstone_terminal_matches()` CAS-invalidates records whose `pr_number` appears in retained TerminalMarkers (no compaction — B20/I19) | `assignment_authority.rs:1211-1239` |
| **Restart reconciliation** | `assignment_reconcile.rs:107-148` — A10a tombstones, A2 re-fires Pending rows (nonce-dedup), A3/A4 nudge/repair with fresh nonce rotation | `per_tick/assignment_reconcile.rs:144-200` |
| **Settlement** | `durable_enqueue()` appends actionable inbox row, `nonce_present_actionable()` detects crash recovery | `assignment_authority.rs:858-895, 621-640` |

**Assessment**: Most mature. All six target properties present. Row-first
persistence, CAS via assignment_id, explicit supersession via pr_number
generation, restart reconciliation with idempotent nonce-dedup.

### 1b. CI Handoff Episode Track

| Property | Current state | Source |
|---|---|---|
| **Persistence** | File-per-key sidecar under `home/ci-handoff-tracks/`, atomic write (`.tmp` → `rename`) | `ci_handoff_track.rs:35-42` |
| **Identity** | `correlation` (repo@branch) + `ci_handoff_episode` (opaque UUID) | `ci_handoff_track.rs:70-75` |
| **States** | Recorded → Resolved (deletion); monotone delete | `ci_handoff_track.rs:26-49` |
| **CAS** | Per-key `.lock` sidecar; deleter re-reads `sent_at` and confirms unchanged before removing (delete-if-unchanged) | `ci_handoff_track.rs:38-42` |
| **Supersession** | Re-record same key: atomic overwrite replaces old episode | `ci_handoff_track.rs:35` |
| **Restart reconciliation** | `reconcile_processed()` with 30s `TRACK_RECONCILE_GRACE` + 24h `TRACK_MAX_AGE` backstop; protected settlement fail-closed | `ci_handoff_track.rs:61,672-718` |
| **Settlement** | Four resolution signals: report with matching correlation, terminal PR state, target binding, 24h backstop sweep | `ci_handoff_track.rs:14-23` |

**Assessment**: Strong. Per-key CAS, atomic writes, explicit episode identity,
reconciliation with grace window. Protected class settlement is fail-closed.
Feature class settlement is fail-open (no episode identity match).

### 1c. Dispatch Tracking (centralized JSON)

| Property | Current state | Source |
|---|---|---|
| **Persistence** | Centralized `dispatch_tracking.json`, atomic via `mutate_versioned` | `dispatch_tracking.rs:42-44` |
| **Identity** | `task_id` (string correlation) | `dispatch_tracking.rs:15` |
| **States** | "pending" / "warned" / "asked" / "completed" / "orphaned" / "no_report_expected" — NOT monotone (can cycle pending→warned→asked) | `dispatch_tracking.rs:22` |
| **CAS** | **NONE** — `mark_completed` does unconditional `retain` filter by task_id | `dispatch_tracking.rs:60-72` |
| **Supersession** | **NONE** — `reassign_to` re-points entries but does not invalidate the old dispatch epoch | `dispatch_tracking.rs:81-116` |
| **Restart reconciliation** | `sweep_stuck` re-evaluates on tick, no generation check | `daemon/mod.rs:1570-1618` |
| **Settlement** | `mark_completed(home, correlation_id, _to)` removes ALL entries with matching task_id; **the `_to` (reporter) parameter is unused** | `dispatch_tracking.rs:57` |

**Assessment**: Weakest store. No CAS, no generation, no monotone states,
no supersession. The `_to` parameter in `mark_completed` is declared but
ignored — settlement is unscoped.

### 1d. Dispatch Idle Watchdog (file-per-entry sidecars)

| Property | Current state | Source |
|---|---|---|
| **Persistence** | File-per-entry under `home/pending-dispatches/`, atomic write with per-file flock | `dispatch_idle/mod.rs:24,337-342` |
| **Identity** | `dispatch_id` (monotonic `next_dispatch_id()`) + `correlation_id` (task_id) | `dispatch_idle/mod.rs:56-57` |
| **States** | Pending → Exceeded → (Reported/Deleted); `reported_at` latch is monotone, status is not | `dispatch_idle/mod.rs:51-110` |
| **CAS** | Per-file flock on state transitions via `with_json_state` | `dispatch_idle/mod.rs:329-342` |
| **Supersession** | Partial — `record_dispatch` dedups by (dispatcher, target, correlation) and refreshes in place | `dispatch_idle/mod.rs:287-347` |
| **Restart reconciliation** | `scan_and_emit` re-reads all sidecars on every tick; throttled by `TICKS_PER_SCAN` | `dispatch_idle/mod.rs:1004,49` |
| **Settlement** | `mark_resolved(home, correlation_id)` deletes ALL sidecars with matching correlation_id — **not scoped by reporter** | `dispatch_idle/mod.rs:679-720` |

**Assessment**: Partial CAS and supersession for record creation. Settlement
is unscoped — any report with matching correlation_id removes all sidecars,
including those belonging to a different assignee's dispatch.

### 1e. Inbox Message Settlement

| Property | Current state | Source |
|---|---|---|
| **Persistence** | Per-agent JSONL files with atomic tmp→rename+fsync_parent | `inbox/storage.rs:81-138` |
| **Identity** | `message_id` (unique per message) | `inbox/storage.rs` |
| **States** | unread → delivering → processed; reclaim reverts delivering → unread (non-monotone) | `inbox/storage.rs:1039-1125, 2136-2235` |
| **CAS** | **NONE** — ack transitions based on state predicate, not compare-and-swap | `inbox/storage.rs:1039` |
| **Supersession** | **NONE** — each message has independent lifecycle | — |
| **Restart reconciliation** | `settle_delivering_for_session_reset` on fresh session; `reclaim_stale_delivering` at 600s TTL | `inbox/storage.rs:1220, 2136` |
| **Settlement** | `ack()` settles delivering → processed (sender-scoped via `name` parameter); `ack_by_correlation()` settles by correlation_id (also sender-scoped) | `inbox/storage.rs:1039, 1243` |

**Assessment**: Three-state machine is well-defined. Settlement IS sender-scoped
(better than dispatch tracking). Main weakness: reclaim reverts delivering →
unread (non-monotone), and no supersession mechanism.

### 1f. Task Board Settlement

| Property | Current state | Source |
|---|---|---|
| **Persistence** | Event-journal `tasks.jsonl` per project board, atomic via `persist_or_log!` | `tasks/handler.rs` |
| **Identity** | `task_id` (t-xxx) | — |
| **States** | open → claimed → in_progress → in_review → done/cancelled; `can_transition_to` prevents backward transitions (monotone) | `tasks/handler.rs` |
| **CAS** | Per-id lock via `with_revalidated_board()`; no generation/incarnation guard | `tasks/auto_close.rs:86-97` |
| **Supersession** | **NONE** — re-dispatch doesn't invalidate the old task-id; same task reused | — |
| **Restart reconciliation** | Task journal survives restart (durable); no explicit reconciliation pass | — |
| **Settlement** | `auto_close_on_report` requires reporter == current assignee AND terminal=true; assignee check is a partial generation guard | `tasks/auto_close.rs:7-119` |

**Assessment**: Monotone states and per-id lock are strong. Assignee check on
auto-close provides a partial guard against stale-reporter settlement. No
explicit generation identity.

---

## 2. The Precise Lost/Duplicate Action Gap

### 2a. Gap: Dispatch settlement is correlation-scoped, not generation-scoped

When a task is reassigned, `dispatch_tracking::reassign_to` re-points the
existing entry to the new assignee. `dispatch_idle::record_dispatch` creates a
new sidecar for the new assignee (dedups by dispatcher+target+correlation, so a
different target = new sidecar). But both settlement functions —
`mark_completed` and `mark_resolved` — settle by correlation_id alone, removing
ALL entries/sidecars for that correlation regardless of who owns them.

### 2b. Concrete failure scenario

```
T=0  Orchestrator dispatches task T1 to Agent-A
     → dispatch_tracking: entry (task_id=T1, to=Agent-A)
     → dispatch_idle: sidecar (target=Agent-A, correlation=T1, dispatch_id=d-001)

T=1  Agent-A is stuck. Orchestrator reassigns T1 to Agent-B.
     → dispatch_tracking: reassign_to(T1, Agent-B) re-points entry (to=Agent-B)
     → dispatch_idle: record_dispatch creates new sidecar (target=Agent-B, dispatch_id=d-002)
     → Agent-A's old sidecar (d-001) still exists (dedup only on same target)

T=2  Agent-A's stale work completes. Agent-A calls send(kind=report, correlation_id=T1).

     MCP handler side (comms.rs:274):
       dispatch_tracking::mark_completed(home, "T1", "Agent-A")
       → removes ALL entries with task_id=T1 — Agent-B's entry is DELETED

     Internal handler side (messaging.rs:583):
       dispatch_idle::mark_resolved(home, "T1")
       → deletes ALL sidecars with correlation_id=T1 — Agent-B's sidecar (d-002) is DELETED

     Task auto-close side (messaging.rs:596):
       auto_close_on_report(home, "report", "T1", "Agent-A", ..., terminal)
       → Reporter Agent-A ≠ current assignee Agent-B → task stays open ✓

T=3  Agent-B's dispatch is now INVISIBLE to both watchdogs.
     If Agent-B gets stuck, NO dispatch_stuck warning fires.
     If Agent-B never reports, NO dispatch_idle_threshold_exceeded fires.

     RESULT: Agent-B's dispatch is a LOST ACTION — watchdog coverage silently
     discarded by a stale reporter.
```

### 2c. Why this is material

The dispatch watchdog is the orchestrator's last line of defense against silently
stuck agents. Losing it means a reassigned task can hang indefinitely with no
alarm. This is a production scenario (reassignment happens when agents are stuck
or at context limits). The gap is silent — no error, no warning, no log entry
when the stale report removes the current assignee's tracking.

### 2d. Source citations for the gap

| What | Where |
|---|---|
| `mark_completed` ignores `_to` parameter | `dispatch_tracking.rs:57` |
| `mark_completed` removes by task_id alone | `dispatch_tracking.rs:68` (`retain(\|e\| e.task_id != cid)`) |
| `mark_resolved` removes by correlation alone | `dispatch_idle/mod.rs:696-698` (filter on `correlation_id == Some(correlation_id)`) |
| `reassign_to` doesn't invalidate old epoch | `dispatch_tracking.rs:81-116` |
| Sender passes unused `_to` to mark_completed | `comms.rs:274` |
| Reporter identity available but not passed to mark_resolved | `messaging.rs:577-583` |

---

## 3. Smallest Material Slice: Reporter-Scoped Dispatch Settlement

### 3a. What changes

Scope dispatch settlement to the reporting agent. Only remove dispatch
tracking entries and sidecars belonging to the agent who actually reported.

Three surgical edits:

1. **`dispatch_tracking.rs:mark_completed`** — rename `_to` to `reporter`,
   add `&& e.to == reporter` to the retain filter. An entry whose `to` was
   re-pointed to Agent-B won't match Agent-A's report.

2. **`dispatch_idle/mod.rs:mark_resolved`** — add a `reporter: &str` parameter,
   filter sidecars on `d.target == reporter` in addition to correlation_id match.
   A sidecar targeting Agent-B won't be deleted by Agent-A's report.

3. **`api/handlers/messaging.rs:577-583`** — pass `from` (the reporter identity)
   to `mark_resolved(home, corr, from)`.

### 3b. Why this is the smallest slice

| Item 3 property | What this slice establishes |
|---|---|
| Row-first persistence | Already present (both stores are durable). No change needed. |
| Stable correlation identity | The combination of (task_id, assignee) becomes the settlement identity. No new field — uses existing data. |
| Monotone states | Mark_completed/mark_resolved become one-way (only the reporter's own entry settles). A re-dispatched entry's lifecycle is independent. |
| CAS claim/settlement | Settlement is now conditional (reporter == entry.to). The existing per-file flock in dispatch_idle already serializes concurrent modifications. |
| Explicit supersession | A `reassign_to` call changes the `to` field, making the old reporter unable to settle the new entry. The old sidecar (target=old-agent) stays until the old agent reports (harmless — its nudge targets the OLD agent, which is no longer the assignee). |
| Restart reconciliation | Existing sweep/scan passes already re-read from disk. The reporter-scoped filter applies on every evaluation, including post-restart. |

### 3c. Why not a bigger slice

- **New generation field**: Adding a `dispatch_generation` UUID would be cleaner
  long-term but requires propagating the generation through the inbox message
  metadata, the reporter's report envelope, and every settlement call site.
  The reporter-scope fix uses existing data with no schema change.

- **Converging the two stores**: Merging `dispatch_tracking.json` and
  `pending-dispatches/` into one store is the eventual item 3 goal but requires
  unifying their settlement paths, sweep timing, and all callers. That's a
  multi-file refactor, not a first slice.

- **Shared action-row primitive**: Introducing a generic `ActionRow` struct
  shared across all six stores is item 3's endgame, not its first step.

---

## 4. Deterministic Real-Entry RED Test Matrix

All tests must use production entry points (MCP `send` handler → internal
message delivery), not call `mark_completed`/`mark_resolved` directly.

### RED1: `stale_reporter_removes_reassigned_dispatch_tracking`

```
Setup:  Orchestrator sends(kind=task, task_id=T1, target=Agent-A)
        → dispatch_tracking entry created (to=Agent-A)
        Task reassignment: reassign_to(T1, Agent-B)
        → entry re-pointed (to=Agent-B)

Action: Agent-A sends(kind=report, correlation_id=T1, target=Orchestrator)

Assert: dispatch_tracking entry for T1 with to=Agent-B STILL EXISTS
Currently: FAIL — mark_completed removes ALL entries with task_id=T1
```

### RED2: `stale_reporter_removes_reassigned_dispatch_idle_sidecar`

```
Setup:  Orchestrator sends(kind=task, task_id=T1, target=Agent-A, expect_reply_within_secs=600)
        → dispatch_idle sidecar created (target=Agent-A, dispatch_id=d-001)
        Orchestrator sends(kind=task, task_id=T1, target=Agent-B, expect_reply_within_secs=600)
        → dispatch_idle sidecar created (target=Agent-B, dispatch_id=d-002)

Action: Agent-A sends(kind=report, correlation_id=T1, target=Orchestrator)
        → messaging.rs calls mark_resolved(home, "T1")

Assert: sidecar d-002 (target=Agent-B) STILL EXISTS
Currently: FAIL — mark_resolved deletes ALL sidecars with correlation_id=T1
```

### RED3: `restart_after_stale_settlement_does_not_resurrect_watchdog`

```
Setup:  Create the RED2 scenario (both sidecars deleted by stale report)
        Simulate daemon restart (drop all in-memory state)
        scan_and_emit() tick fires

Assert: No sidecar exists for Agent-B after restart → watchdog CANNOT fire
        for Agent-B's dispatch
Proves: The gap survives restart — there is no reconciliation path that
        restores the lost sidecar
```

### RED4: `reassigned_dispatch_watchdog_fires_for_current_assignee`

```
Setup:  RED2 scenario PLUS Agent-B goes idle for threshold_secs

Action: scan_and_emit() tick fires

Assert: dispatch_idle_threshold_exceeded delivered to Orchestrator for Agent-B
Currently: FAIL — sidecar was deleted, no nudge fires
```

### Replay/restart proof requirements for GREEN

After fixing, these must additionally hold:

- **Crash between dispatch and sidecar**: If daemon crashes after
  `dispatch_tracking::track_dispatch` but before `record_dispatch`, the
  dispatch_tracking entry exists but no sidecar. `sweep_stuck` fires for the
  entry; no duplicate when sidecar is later created.

- **Crash between reassign and re-dispatch**: If daemon crashes after
  `reassign_to` but before the new `record_dispatch`, the old sidecar still
  targets Agent-A. Agent-A's report removes only their sidecar. No sidecar
  exists for Agent-B until re-dispatch completes.

- **Concurrent report and reassign**: If `mark_completed` and `reassign_to`
  race, the per-store mutate lock serializes them. If reassign wins, report
  finds no matching entry (to mismatch). If report wins, reassign re-points
  a just-removed entry (no-op, entry already gone). Both are safe.

---

## 5. Migration and Backwards Compatibility

### 5a. Migration: none required

No schema change. The fix uses existing fields (`to` in DispatchEntry,
`target` in PendingDispatch) and an existing parameter (`_to` in
`mark_completed`). No new durable state, no serialization change, no field
addition. Old entries are compatible because they already carry `to`/`target`.

### 5b. Rollback

An exact-merge revert restores the old behavior (unscoped settlement). This is
safe because:

- No durable state was changed (no new fields to orphan)
- The old behavior (remove all matching entries) is a superset of the new
  behavior (remove only reporter-matched entries)
- The revert's worst case is the pre-existing gap (stale report removes wrong
  tracking), which is the current production behavior

No forward-repair procedure needed. No generation or epoch state to downgrade.

### 5c. Forward compatibility

The reporter-scoped settlement establishes the identity pattern
`(correlation_id, assignee)` as the settlement key. Future slices can:

- Replace the string-match assignee check with a typed dispatch_generation UUID
  (stored in both DispatchEntry and PendingDispatch, propagated through inbox
  message metadata)
- Extend the same pattern to inbox ack (ack_by_correlation already sender-scoped)
- Eventually converge the two dispatch stores into one, using the scoped
  settlement as the foundation

---

## 6. Explicit Non-Goals

1. **Converging dispatch_tracking.json and pending-dispatches/ into one store.**
   This is the right long-term direction but requires unifying all callers,
   sweep timing, and settlement paths. It is a second slice, not the first.

2. **Adding a typed ActionRow or DurableAction primitive.** The endgame shared
   primitive across all six stores requires designing the common state machine,
   identity model, and CAS protocol. This spike identifies the properties
   (§1 map) but does not propose the shared type.

3. **Fixing inbox reclaim non-monotonicity.** The delivering → unread revert
   is a separate concern (item 8 territory). This slice does not touch inbox
   state machine.

4. **Adding generation/incarnation to task auto-close.** The assignee check
   already provides a partial guard. A full generation guard requires
   propagating dispatch_generation through the task board, which is a larger
   change and a separate slice.

5. **Converging CI handoff track with dispatch_idle.** Both are file-per-entry
   sidecar patterns with similar lifecycle, but their settlement semantics
   differ (CI handoff has episode identity and protected class fail-closed;
   dispatch_idle does not). Convergence requires resolving these semantic
   differences first.

6. **Broad framework rewrite.** This slice is three production edits in two
   files. The scope is bounded by the single gap (§2) and the single fix (§3).
   No new abstractions, no new stores, no new message fields.

7. **Feature-class CI handoff settlement.** The current feature-class handoff
   lacks episode identity (§1b, `ci_handoff_episode` is optional). Strengthening
   it is an item 8 concern, not item 3's first slice.
