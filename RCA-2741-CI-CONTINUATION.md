# RCA #2741 — Missing CI terminal continuation after rebuild/restart + rebase force-push

- **Task:** t-20260712052005716144-40783-1 (branch `spike/ci-terminal-restart-reconciliation-v2`). **Analysis only — no code.**
- **Freshness:** worktree HEAD `e395fe25` (= origin/main; includes the merged #2741 task-board facet).
- **Interacts with:** PR #2743 (S1 exact-head protected-main watch, **OPEN**) + decision d-20260712033954660984-4. Any ci_watch implementation from this RCA depends on #2743 S1 (see §7).
- **Method:** 3 parallel source traces (poller / startup-sweep / delivery+auto-arm), load-bearing sites re-verified firsthand; live `~/.agend-terminal/ci-watches` sampled; #2741 PR-comment incident reconstructed.

---

## 1. Recommendation / verdict (TL;DR)
The #2741 "missing continuation" is **NOT** a lost CI event in the poller and **NOT** force-push dropping the watch (both disproven, §4). It is a **structural deficiency**: the CI→continuation obligation is **ephemeral live-poller state bound to (branch, head, target) with (i) no delivery ACK, (ii) no boot-time reconciler, and (iii) no continuation channel for the orchestrator's post-VERIFIED merge-freshness step after a rebase.** Four confirmed loss modes (M1–M4) map onto codex's four hypotheses. Dominant for the witnessed instance: **M1 (design/routing gap) + M3 (no durable reconciler)**; M2 (no exactly-once) and M4 (bypass arm-downgrade) are confirmed sibling holes a durable fix must also close.

## 2. The witnessed incident (exact timeline, from PR #2741 comments)
`fix/task-result-persistence` reproduced the class:
- **T0** VERIFIED @ `31a01439` (watch armed on this head; reviewer = the exact-head reviewer, codex-125550).
- **T1** #2742 merges to main (`e40d9554`) → #2741 base BEHIND → canonical merge **REFUSED** (MERGE HOLD). Comment: *"Implementer has been asked to rebase … push with force-with-lease … and return a new exact head for freshness review."*
- **T2** implementer rebases onto `e40d9554`, **`git push --force-with-lease`** → new head `1df7f2b8`; CI run **29180365914** starts.
- **T3** (fleet busy; a daemon restart/rebuild is plausible — every live watch file's mtime + `last_polled_at` are clustered "just now", the signature of a mass re-poll on boot).
- **T4** run 29180365914 reaches terminal-green on `1df7f2b8`.
- **T5** **No automatic continuation reached the orchestrator.** codex **manually** ran `gh pr checks 2741` (the §7-forbidden poll) to post freshness UNVERIFIED→VERIFIED, then merged.

## 3. Confirmed mechanism (state machine, file:line)
Delivery path (feature-branch watch): `check_ci_watches_with_provider` (`poller.rs:468`) → `ci_check_repo` (`poller.rs:1442`) → `poll_ci_runs` queries **by branch** each tick (`provider.poll_runs(repo,branch)`, `poller.rs:1730`) → on current-head terminal-green, `fan_out_notifications` → `persist_watch_state` builds `make_ci_ready_for_action_msg` (`poller.rs:2146`) and **durably enqueues** it to each `next_after_ci` target via `crate::inbox::enqueue_with_idle_hint` (`poller.rs:2154`; inbox fsyncs JSONL *before* a best-effort PTY wake — `inbox/storage.rs:495-509`, wake discarded `inbox/notify.rs:341`). A head-anchored `ci_handoff_track::record` (`poller.rs:2163-2177`) drives a re-nudge watchdog (2 min) + escalate (10 min) + 24h backstop (`handoff_timeout_watchdog.rs:30-40`).

- **Send-before-stamp:** enqueue precedes the notify-stamp + `flush_watch_state` (`poller.rs:1596-1600`). ⇒ crash-between = harmless **duplicate**, not loss.
- **Head-move handling:** on head advance `resolve_head_advanced` **deletes** the old head's handoff track (`ci_handoff_track.rs:521-547`) and `last_notified_by_workflow.clear()` (`poller.rs:1524-1526`); `effective_last_run_id` resets when `prev_head!=cur` (`poller.rs:1397-1413`); `head_sha` self-heals (`poller.rs:1565,2220`). ⇒ watch survives force-push; a **fresh** obligation is created only when a **live poll** later sees the new head terminal.
- **Reload:** `WatchState` serde-defaults; nothing resets `last_polled_at`/`last_notified_*` on load; dedup seeded straight from disk (`poller.rs:1465-1476`). `startup_sweep` (`sweep.rs:533-569`) is **pure GC**. ⇒ no boot re-drive, no missed-continuation recovery.

### The four loss modes ↔ codex's four hypotheses
- **M1 — routing/design gap (hyp. b “expected”, + c).** `[ci-ready-for-action]` fires on **CI-green** to `next_after_ci` (the *reviewer-handoff* nudge, pre-verdict — it is NOT verdict-gated). The **orchestrator's post-VERIFIED merge-freshness recheck after a rebase is not modeled as any continuation**: the armed watch's `next_after_ci` targets the reviewer (already fired at T0), and MERGE-HOLD→rebase is a manually-orchestrated loop. So no channel carries "rebased head is green → re-verify/merge" to the orchestrator → manual `gh pr checks`. **This matches T5 exactly.**
- **M2 — no exactly-once (hyp. a).** The enqueue `Result` is fire-and-forget (`persist_or_log!` logs only, `macros.rs:30-41`); the notify-stamp (`new_notified_sha`, `poller.rs:1864-1867,1996-2002,2218-2246`) and `ci_handoff_track::record` are written **unconditionally** regardless of enqueue success. A dropped enqueue (disk RO `storage.rs:380-382`) is silently marked delivered, never retried. No ACK field exists on `InboxMessage` or `CiHandoffTrack`.
- **M3 — no durable reconciler (hyp. d).** `startup_sweep` never re-polls or re-evaluates terminal/continuation state; `replay_missed_at_startup` (`daemon/mod.rs:1614-1665`) replays one-shot **schedules** only. Nothing inspects "terminal-seen (`last_terminal_seen_at`) but action-chain undelivered → re-emit". Plain-restart == rebuild (same boot path).
- **M4 — bypass arm-downgrade (hyp. c, sibling).** A non-dispatch push (`AGEND_GIT_BYPASS`/raw/`--force-with-lease`) skips the dispatch-time arm; the server-side fallback `auto_arm_unwatched_open_prs` (`pr_state/auto_arm.rs:30-91`) arms **subscriber-only** (`next_after_ci` unset, `:67-70`) and **skips any pre-existing watch** (`:40-47`). So a bypass push to a *fresh* branch silently has no action-chain. (Not #2741's exact path — a watch pre-existed and persisted — but the same class.)

**Disproven:** force-push dropping the watch (self-heals by branch); stamp-before-send loss (send-before-stamp ⇒ duplicate); offline-target loss (inbox JSONL is durable/fsynced). `delete_instance` clears `next_after_ci` to avoid ghost routing (`instance_state/lifecycle.rs:232-240`) — a permanent-teardown edge, not restart.

## 4. Exact lost-vs-expected timeline (states)
| Step | Expected (durable design) | Actual (today) |
|---|---|---|
| T2 force-push H0→H1 | obligation re-armed for H1 targeting whoever must act (reviewer re-verify + orchestrator merge-gate) | watch self-heals to H1; `next_after_ci`=reviewer only; **no orchestrator channel** (M1) |
| T3 restart | boot reconciler re-derives "open PR, H1, terminal? → deliver-if-unacked" | `startup_sweep`=GC only; dedup reloaded verbatim (M3) |
| T4 H1 terminal-green | exactly-once `[ci-ready]`/merge-signal delivered + ACKed | if reviewer re-nudge fired it went to the reviewer, not the orchestrator; unconditional stamp hides any drop (M1/M2) |
| T5 | orchestrator auto-nudged to verify+merge | orchestrator polls `gh pr checks` by hand (§7 violation) |

**Honesty note:** the *dominant* mode for the specific #2741 instance (M1 routing vs an M2/M3 drop) cannot be 100 % pinned from source + PR comments alone — the live watch file was GC'd on merge and daemon logs weren't inspected. M1 is best-supported by the MERGE-HOLD comment (a manual "return a new exact head" loop). M2/M3/M4 are structurally confirmed regardless and are what make the class *non-reconcilable* today.

## 5. RED restart/force-push harness (deterministic — no sleep, no real CI/network)
Unit-level, driving the real poll + a fake `CiProvider`, over a temp `home/ci-watches`:
1. **`force_push_then_terminal_redelivers_exactly_once`** — arm watch on H0 with `next_after_ci=[orch]`; stamp notified for H0; write to disk. Move head→H1 (rewrite `head_sha`), persist. **Reload** state from disk into a fresh registry (simulated restart) + run the (new) boot reconciler. Drive one poll: fake provider returns H1 terminal-green. ASSERT exactly one `[ci-ready]` enqueued to `orch` for H1; run the poll again → **zero** additional (idempotent).
2. **`enqueue_failure_is_not_stamped_delivered`** — inject a fake inbox that fails the enqueue. ASSERT the watch is NOT stamped delivered for that (sha,target) and the next poll **retries** (at-least-once + dedup = exactly-once). (RED today: M2 stamps unconditionally.)
3. **`boot_reconciler_redelivers_terminal_seen_but_unacked`** — persist a watch with `last_terminal_seen_at` set for H1 but no delivery-ACK for `orch`; reload+reconcile; ASSERT one delivery. (RED today: M3 no reconciler.)
4. **`orchestrator_merge_freshness_channel_exists`** — model the rebase re-verify: ASSERT a continuation reaches the orchestrator/merge-gate owner (not only the reviewer) when the rebased head goes green. (RED today: M1.)
All assert on the durable inbox JSONL / a delivery-ledger, never on timing.

## 6. Minimal fix manifest (for adversarial approval — no code yet)
Smallest change that makes continuation **durable + exactly-once + reconcilable**, no sleep/polling:
- **F-A (exactly-once ACK ledger).** Add a per-(watch, head_sha, target) **delivery record** written **only after `enqueue` returns Ok** (gate the stamp on the `Result`; stop the unconditional `new_notified_sha`/`record`). Re-emit iff not-acked for the current head. Idempotency key = (watch_identity, head_sha, target). Converts at-most-once→exactly-once with the existing SHA/workflow dedup.
- **F-B (durable boot reconciler).** Extend `startup_sweep` (or a sibling boot pass) to: for each live, non-tombstoned watch on an OPEN PR, resolve terminal state **at the exact current head** and re-emit any action-chain delivery **missing an ACK** (F-A ledger). One-shot on boot; not a poll loop.
- **F-C (orchestrator merge-freshness channel).** Model the post-VERIFIED / post-rebase merge-gate owner as a continuation target (e.g. a `next_after_ci`-style "merge-ready" chain distinct from the reviewer handoff), so a rebased head going green nudges the merge owner — closing M1. (Scope/ownership is an operator/lead decision — teed up in §8.)
- **F-D (bypass arm parity, optional/smaller).** When `auto_arm_unwatched_open_prs` finds an existing watch, do NOT leave a bypass-created watch action-chain-less: carry forward a previously-set `next_after_ci` rather than downgrading to subscriber-only. (Closes M4 without touching the shim; the deeper push-hook re-enable stays gated on #1751.)

**Exact-once semantics:** delivery-ledger keyed on (watch, head_sha, target); write-after-Ok; reconciler + poller both consult it ⇒ each (head,target) continuation delivered exactly once across force-push (new head = new key) and restart (ledger persisted). **No sleep/manual-poll** anywhere — boot reconciler is event(boot)-driven; re-nudge reuses the existing `handoff_timeout_watchdog`, but the track must survive head-move for the *current* head (F-A re-anchors instead of deleting-without-replacement).

## 7. Interaction with PR #2743 S1 (dependency)
#2743 adds `WatchState.target_head_sha` + `CiProvider::poll_runs_for_sha` (GitHub `?head_sha=` resolves a run **regardless of branch advance**). The **boot reconciler (F-B) and the ledger key (F-A) MUST resolve terminal state by exact head** to avoid aliasing a stale/newer run after a rebase — i.e. they build on `poll_runs_for_sha`. Hence "any later ci_watch implementation must depend on S1/#2743." For exact-head (protected-main) watches, `target_head_sha` is the ledger key directly; for feature-branch watches, the observed `head_sha`. This RCA's fix should land AFTER #2743 merges and reuse its primitive, not fork a parallel by-sha poll.

## 8. Open questions for codex (before impl dispatch)
1. **M1 ownership:** should the merge-freshness continuation target the **reviewer** (re-verify the rebased head) or the **orchestrator/merge-gate owner**, or both? This is a workflow-contract decision, not derivable from code.
2. **Scope:** is the fix all of F-A+F-B (durability/exactly-once) **plus** F-C (M1 channel), or is M1 handled by convention (dev reports new head + lead re-dispatches) and the code fix limited to F-A/F-B/F-D? F-C is the largest and most contract-sensitive.
3. Confirm F-D (bypass arm parity) is in scope or deferred to the #1751 push-hook work.

## 9. Evidence
- Incident: PR #2741 comments (MERGE-HOLD + rebase `1df7f2b8` + manual `gh pr checks`), run 29180365914.
- poller: `poller.rs:468,1442,1730,2146,2154-2177,1864-1867,1996-2002,2218-2246,1500-1526,1397-1413,1465-1476,1596-1600`; `ci_handoff_track.rs:521-547,61-105`; `handoff_timeout_watchdog.rs:30-40`.
- reconciler/persist: `sweep.rs:533-569`; `registry.rs:4-5,259-265,330-369`; `daemon/mod.rs:1282,1614-1665`.
- delivery/arm: `inbox/storage.rs:495-509`, `inbox/notify.rs:341`, `pr_state/auto_arm.rs:4-7,30-91,67-70`; `instance_state/lifecycle.rs:232-240`.
- schema: `watch_state.rs` (no delivery/ACK field). Live sample: `fix/ci-watch-exact-head-main` head≠last_notified_head (force-push signature). #2743 body (S1 `target_head_sha`/`poll_runs_for_sha`).
