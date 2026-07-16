# Unified design: notification-noise 3-in-1 (spike → VET)

> **Historical design snapshot:** This point-in-time spike is retained as design
> provenance. Its gaps and proposed remedies are not a current runtime contract;
> verify them against current source before acting.
Tasks 35896-11 (poll/delivering) + 67777-1 (ci-ready dismiss) + 24134-4 (tombstone). base=main.
All claims file:line-confirmed (RCA read + Explore map). Design only — no code until vet.

## Root cause (one sentence)
The daemon has **three mutually-blind "this obligation is handled" mechanisms** — inbox
read-state (poll_reminder), the ci_handoff_track sidecar (renudge watchdog), and two
non-interoperating discharge ledgers — so no single agent action reliably silences a reminder.

## Re-fire map (source → fires on → stops on → gap)
| Source (cadence) | fires on | stops on (today) | GAP |
|---|---|---|---|
| **poll_reminder** (30t) `poll_reminder.rs:35` | inbox unread-count *changed* (`unread_count_after_discharge` — excludes `delivering`; only ci-watch is discharge-aware) `storage.rs:1454` | drain / ack / settle_read_by_id; ci-FAIL discharge | (a) `kind=report` reply settles reporter's dispatch row only if `ack_inbox=true` (default **false**) → reclaim cycles delivering→unread → renudge; (b) `LAST_NOTIFIED` in-mem → restart re-nudge |
| **handoff_timeout_watchdog** (12t) `handoff_timeout_watchdog.rs:105` | ci_handoff_track sidecar age≥2m (+ escalate 30m) | 6 resolvers: self-report / claim / pr-terminal / merge-blocked(#2603) / unwatch / head-advanced / 24h — **read/ack NOT among them by #1888 design** | **delegation (kind=task dispatch) is NOT a resolver** (messaging.rs:505-572 never touches track); no lightweight discharge (unwatch is blunt+tombstones the watch); sidecar invisible in `ci status`; `last_renudged` in-mem → restart burst |
| **reclaim_stale_delivering** (60t) `storage.rs:1865` | delivering >10m & kind unknown | fire-and-forget settle; discharge | `ci-ready-for-action` NOT in `known_fire_and_forget_kind` (storage.rs:1640) → reverts to unread → **2nd uncoordinated nudge stream** for the same event |

**Two discharge ledgers, non-interoperating:** `discharge_ledger` (#2537, key `(head,job)`, read ONLY by
`is_discharged_ci_fail` for kind=ci-watch → `send triaged=` on a ci-ready is a silent **dead-write**);
`channel_reply_discharge` (#2622, key `(agent, msg/group)`, disk-durable, MCP verb `inbox action=discharge`,
generic `find_message`, unconditionally `settle_read_by_id`) — but only feeds `reply_ledger`+inbox row, **never
ci_handoff_track**. #2622 is the best-shaped unifying primitive; the missing wire is the seam.

## Minimal unified fix (6 changes; lead-suggested 2-PR split; obligation-loss = DUAL)

### PR-A — dismiss-signal / obligation semantics (DUAL)
1. **Delegation resolver (acceptance-core).** In `track_dispatch` kind=="task"|"query" (messaging.rs:505-572,
   the same chokepoint as `dispatch_idle::mark_resolved`), if the DISPATCHER holds a ci_handoff_track for the
   same correlation (repo@branch or task_id) → `resolve_by_correlation(..., "delegated")`. "I dispatched the
   review = my discharge." → **lead's 4.5h scenario stops the instant they dispatch review** (no extra gesture).
2. **Route `inbox action=discharge` (#2622) into ci_handoff_track.** When discharging a `ci-ready-for-action`
   message, also `ci_handoff_track::resolve_by_...(discharge)` for its correlation → the ONE agent-facing verb
   now silences BOTH poll_reminder (settles read_at) AND the watchdog. Explicit gesture (not read) → preserves
   #1888 stuck-reviewer escalation. No new MCP surface.
3. **Sidecar visibility.** Add pending ci_handoff_track (correlation/age/renudge-count) to `ci action=status`
   → an agent can SEE why it's renudged and what to discharge (fixes "invisible").

### PR-B — state-transition repair
4. **`ci-ready-for-action` → known_fire_and_forget_kind** (storage.rs:1640) → reclaim settles it to read_at
   instead of reverting to unread → kills the 2nd poll-reminder stream (watchdog stays the single ci-ready renudge).
5. **`kind=report` w/ correlation auto-discharges the reporter's dispatch obligation** (default the ack_inbox
   behavior for report-with-correlation) → poll_reminder stops nudging an already-reported dispatch (35896-11 m-208).
   *Dependency (reviewer5, #2670):* `ack_by_correlation` keys the settle by `(sender, task_id==correlation_id)`
   only — it does NOT key on the report's target/original dispatcher. Correctness therefore relies on task /
   correlation ids being **globally unique** (the fleet's task-id design guarantee); a reused id across unrelated
   dispatches would let one report settle another's row. Sender-scoping already prevents cross-agent bleed (#2647).
6. **Persist the renudge/escalation throttle** (anchor `last_renudged`/`last_escalated`/`LAST_NOTIFIED` to a
   disk `last_renudged_at`/count on the track, not an in-mem map reset at boot) → no restart burst. **This is
   24134-4's cross-restart concern, solved minimally via the already-durable track + throttle — NOT a new
   content-hash tombstone subsystem** (Explore: full tombstone is bigger; observed harm is the burst).

## How each task + acceptance is closed
- 67777-1 (ci-ready dispatcher renudge): #1 (delegation) + #2 (discharge verb) + #3 (visibility).
- 35896-11 (poll/delivering): #4 (fire-and-forget) + #5 (report auto-discharge) + #6 (restart burst).
- 24134-4 (tombstone): #6 + the durable discharge ledgers — cross-restart replay closed without a new subsystem.
- **Acceptance core** (usage-limit → back online → ack + dispatch review): #1 makes the dispatch itself resolve;
  #6 ensures a post-limit restart doesn't re-burst; #2 as the explicit fallback.

## Open questions for VET
Q1. Discharge key: correlation_id (repo@branch/task_id) vs message_id vs content-hash? I lean correlation_id
    (matches ci_handoff_track + dispatch_idle keying; message_id is per-copy).
Q2. #5 scope: auto-discharge on ANY kind=report-with-correlation, or only terminal=true? (progress reports
    shouldn't necessarily clear the obligation). I lean: settle the reporter's own delivering row on any
    report-with-matching-correlation (they engaged), keep escalation on the watchdog side untouched.
Q3. PR split ok as A(1,2,3 DUAL)/B(4,5,6)? Or fold #6 into its own PR (it's the riskiest — touches throttle timing)?
Q4. Should #1 delegation resolver require the dispatched task carry the same branch (branch= on send), or match
    on correlation alone? (a lead could dispatch an unrelated task in the same tick.)
