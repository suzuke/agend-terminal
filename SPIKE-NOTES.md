# Spike: notification-noise 3-in-1 bundle (unified design)
Task t-20260703072223163581-35896-11 (+ 67777-1 ci-ready dismiss, 24134-4 tombstone). dev2. base=main.
Deliverable: ONE diagram of re-fire sources + dismiss/discharge signals + gaps → minimal unified fix → lead VET (don't write code yet). Acceptance core: lead's 4.5h renudge scenario (recipient in usage-limit → back online, ack + dispatch review) must stop.

## Three noise classes (from the 3 tasks)
- **A) stuck-delivering / poll-reminder** (35896-11): real InboxMessage handled + kind=report replied, but `inbox message status` stays `delivering` FOREVER → poll_reminder judges "unread" → nudge. Sample m-...-208 (#2587 dispatch), handled 15:07, still delivering.
- **B) ci-ready renudge, dispatcher gap** (67777-1): handoff_timeout_watchdog re-nudges every 2min until one of 6 resolvers fires; inbox read/ack DELIBERATELY excluded (phase-1 lesson: read-resolve went blind for stuck reviewer). Lead/dispatcher legit action = dispatch review (kind=task) → no resolver → renudged. discharge ledger's is_discharged_ci_fail covers ci-FAIL only, NOT ci-ready (dev finding).
- **C) handoff sidecar** (35896-11 07-06 sample, 4.5h on lead): ci_handoff_track pending_handoffs sidecar is INVISIBLE (`ci action=status` empty), UN-dischargeable, consumption ≠ recipient-ack. + handoff_timeout_watchdog 30min timeout nudges.
- cross-cutting: **tombstone** (24134-4) — content-hash dedup surviving daemon restart; existing ack/settle is state-transition → replays on fresh-restart/reclaim-TTL.

## Already MERGED (don't redo) — #2603 Phase 2 (4d13b2f3)
"stop ci-ready renudge orphaning + fix inbox=N mislabeling (#26795)". = RCA options A+B:
(A) resolve_if_merge_blocked inline (REJECTED/Draft PR resolves renudge w/o live poll);
(B) header inbox=N → pending_handoffs=N + "(use ci action=status)".
Remaining = RCA option C (dispatcher dismiss) + sidecar visibility/discharge + class A delivering-stuck.

## RCA (workspace/gapfix-dev/PHANTOM-RENUDGE-RCA.md) — key established facts
- Renudge emitter: handoff_timeout_watchdog.rs::scan_and_emit_with (RENUDGE_AFTER/INTERVAL_MINS=2), per_tick/handoff_timeout.rs. Injects via inbox::notify::renudge_actionable_unread → DIRECT PTY inject (inject_with_submit), NOT inbox-backed (so `inbox drain` empty; the "phantom").
- Track: ci_handoff_track::record (ci_watch/poller.rs when CI passes → next_after_ci).
- 6 resolvers (STOP the renudge; read/ack NOT among them BY DESIGN):
  resolve_for_target_correlation (target's report), resolve_claimed (claim branch),
  resolve_by_correlation "pr_terminal" (merged/closed), "pr_merge_blocked" (REJECTED/Draft),
  resolve_head_advanced, sweep_expired (24h backstop).
- Deep failure: all state-resolvers depend on an ACTIVE ci-watch poll loop; watch torn down → orphan.
- RCA §4: A vs B/C = DIFFERENT code paths, SHARED philosophy (both "resolve on explicit signal", both
  over-correct into renudge-past-handled). A tombstone/dismiss idempotency layer COULD unify — as a
  SHARED PRIMITIVE. **Constraint: must NOT go blind for a genuinely-stuck reviewer (explicit dismiss, not read).**

## Discharge ledger (daemon::discharge_ledger; tests p6_discharge_consume.rs #2537 P6)
record_discharge(home, head, job, agent, reason) + is_discharged_ci_fail(home, msg) — keyed (head_sha, job),
ci-watch FAIL only. Consumed at reclaim_renudge_worthy + unread_count_after_discharge. #2622 = inbox action=discharge (channel-reply obligation). => candidate UNIFYING primitive if generalized to (correlation_id/message_id) key across all 3 sources.

## Unification hypothesis (to confirm with Explore map, then propose to lead)
Generalize the discharge/tombstone into ONE "obligation discharged" ledger keyed by correlation_id (or
message_id/content-hash), restart-surviving + GC'd, consulted by ALL re-fire sources
(poll_reminder, handoff_timeout_watchdog, ci-fail reclaim) before firing; uniform EXPLICIT discharge
gesture (extend inbox action=discharge). PLUS repair class-A delivering→read transition. Preserve
stuck-reviewer escalation (dismiss = explicit + time-boxed "hold till next action", not silent read).
Open: which key (correlation vs message-id); does "kind=task dispatch" auto-discharge the dispatcher's handoff.
