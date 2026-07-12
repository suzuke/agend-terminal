# RCA #2741 — R2 CONVERGED manifest (durable CI continuation)

- **Task:** t-20260712053545326332-40783-5 (adversarial R2). **Analysis only — no code.** Supersedes the R1 fix section; R1 trace evidence stands (`RCA-2741-CI-CONTINUATION.md`).
- **Freshness:** HEAD `e395fe25`. Impl gated on PR #2743 S1 (`poll_runs_for_sha`/`target_head_sha`).

## 1. #2741 determination — UNSUBSCRIBED / protocol gap, NOT a lost event (codex R2 hypothesis CONFIRMED)
`next_after_ci` is **explicit-only**, never defaulted: the dispatch hook propagates only what the dispatcher passes (`dispatch_hook/auto_watch.rs:26,55-56` logs `explicit = !next_after_ci.is_empty()`; the arm carries it solely from `send`). The archfix fleet reviews **on-demand** — codex dispatched #2737 / the #2741 spike / this R2 with **no** `next_after_ci`, and reviews after the dev's §6 milestone update. Live `~/.agend-terminal/ci-watches` corroborates: ~20/22 watches have `next_after_ci=None`.

⇒ #2741's watch had **no reviewer `next_after_ci` subscription**. The witnessed loss is therefore *structural absence of an armed continuation*, not a dropped/lost CI event:
- Direct evidence in the PR comments: codex posted **"UNVERIFIED freshness update … Final VERIFIED is withheld only until new-head CI run 29180365914 is terminal-green"** — an explicit *outstanding review obligation pending CI* — then had to **manually** `gh pr checks` when it went green. Nothing was armed to wake that obligation.
- The merge side behaved **correctly**: `pr_state` emits `[pr-ready-for-merge]` only on **CI-green ∧ required-VERIFIED at the *same* head** (`pr_state/mod.rs:1187-1214`), and a **stale-SHA verdict must not flip an advanced head** (`mod.rs:2022`). The VERIFIED@`31a01439` was correctly stale after rebase to `1df7f2b8`; no false merge-ready fired.

**So the only true gap is: nothing wakes the reviewer's outstanding (UNVERIFIED-pending-CI) obligation when the *current* head reaches CI-terminal-green.** M2/M3 (no exactly-once, no reconciler) are latent hardening needs, not the #2741 trigger. M4 is out of scope (see §6).

## 2. Delivery semantics — durable AT-LEAST-ONCE + persistent dedup (NOT "exactly-once")
R1's "exactly-once" is **withdrawn**: the inbox JSONL append (`inbox/storage.rs:495-509`, its own `fsync`) and any continuation ledger write are **separate fsyncs — not atomic**, so exactly-once is unprovable across a crash. Provable model:
- **Durable at-least-once**: enqueue is fsync-durable; the *arm/obligation* record is fsync-durable; a boot reconciler re-drives any obligation whose completion isn't durably recorded. A crash can cause a **redelivery**, never a loss.
- **Persistent dedup / idempotent consumption**: a persisted `(pr_identity, head_sha, target, kind)` continuation-key suppresses a duplicate wake; and the consumer action (reviewer re-checks CI at head) is **naturally idempotent** — a duplicate `[ci-ready]` costs one extra `gh pr checks`, never a wrong action. Effectively-once via at-least-once + idempotent consume.

### Crash matrix (each row deterministic; RED tests in §5)
| Crash point | Persisted state after crash | Boot reconciler outcome | Net |
|---|---|---|---|
| before enqueue | obligation armed, no delivery-key | re-resolve head@terminal → enqueue | delivered (at-least-once) |
| after enqueue, before delivery-key write | msg in inbox, no key | re-enqueue → dedup at consume OR key-on-redeliver | 1 effective delivery |
| after delivery-key write | msg in inbox + key | key present → skip | no duplicate |
| head advanced during downtime (rebase) | obligation@old head; live head=new | old-head obligation **invalidated** (never revived); new head re-armed iff obligation still outstanding | correct head only |

## 3. Routing authority (precise)
- **CI-terminal-green @ current head → wake the OUTSTANDING-REVIEW-OBLIGATION owner(s) only** — i.e. a reviewer with an UNVERIFIED-pending-CI (or no verdict yet) **at the current head**. Not the author, not a general channel.
- **Merge owner → unchanged**: `pr_state` already emits `[pr-ready-for-merge]` to the team orchestrator / merge authority (`pr_state/mod.rs:3115`, NOT the implementer `:3147`) once CI-green ∧ required-VERIFIED hold at the same head. F-C must **not** duplicate this.
- Verdict landing already notifies the author via `[review-verdict]` when not-yet-merge-ready (`mod.rs:1170-1183`). Untouched.

## 4. F-C — smallest explicit review-lifecycle arm/reconcile contract (narrowed)
**Arm:** when a reviewer's verdict is recorded as **UNVERIFIED-pending-CI at head H** (an explicit "I will finalize once H is terminal-green"), record a durable review-obligation `{pr, head=H, reviewer}`. (This reuses `pr_state`'s existing verdict+head tracking — the obligation is derived from an UNVERIFIED verdict whose head has no terminal CI yet — not a new subscription channel.)
**Fire:** on CI-terminal-green at H, wake `reviewer` with the existing `[ci-ready-for-action]` message (durable enqueue), keyed for dedup.
**Reconcile (boot):** see §5 state machine.
**Head-invalidation:** if the PR head advances past H before terminal (rebase/force-push), the H-obligation is **invalidated, never fired** — the reviewer must re-verify the new head, which re-arms on their next UNVERIFIED-pending-CI. (Mirrors the existing head-anchored `resolve_head_advanced`, `ci_handoff_track.rs:521-547`, and #2008 head-anchoring.)
This is the whole of F-C: no orchestrator channel, no default next_after_ci, no new wire kind — an obligation derived from an existing verdict state + a wake reusing `[ci-ready-for-action]`.

## 5. Boot reconciler + head-invalidation state machine (exact-head, TTL-bounded, restart-idempotent)
State per outstanding obligation `{pr, head_sha=H, reviewer, armed_at}`, persisted:
```
ARMED(H) ──CI terminal-green @H (poll_runs_for_sha(H), #2743)──▶ FIRE → DELIVERED(H) [dedup key written]
   │
   ├── head advances to H'≠H (live poll or boot) ─────────────▶ INVALIDATED(H)  (never fires; drop)
   ├── reviewer posts VERIFIED@H ────────────────────────────▶ RESOLVED (obligation discharged; pr_state → merge path)
   └── armed_at + TTL exceeded ───────────────────────────────▶ EXPIRED (GC; no fire)
```
- **Boot reconciler** (extends `startup_sweep`, or a sibling boot pass): for each ARMED obligation on an OPEN PR, resolve terminal state **at exact H via #2743 `poll_runs_for_sha`** (never by branch — avoids rebase run-aliasing); if terminal-green and no DELIVERED key → FIRE (at-least-once). If PR head ≠ H → INVALIDATE. Idempotent: DELIVERED key + dedup make re-run a no-op.
- **TTL-bounded**: reuse the watch TTL/inactivity reaps; an obligation older than TTL EXPIRES (no indefinite growth), consistent with `gc_stale_watches`.
- **Never revive old-head**: INVALIDATED/EXPIRED are terminal; only a *fresh* UNVERIFIED-pending-CI re-arms, at the new head.

## 6. Scope decisions
- **F-A → recast** as durable at-least-once + persistent dedup (§2). Concrete: gate the "notified/delivered" stamp on enqueue `Ok` (fix the unconditional stamp `poller.rs:1864-1867,2218-2246` + fire-and-forget `2154-2158`), and add the `(pr,head,target,kind)` dedup key. No atomicity claim.
- **F-B → the §5 boot reconciler** (exact-head/TTL/idempotent).
- **F-C → §4** (review-lifecycle obligation), the minimal contract.
- **F-D (bypass arm parity) → DEFERRED to #1751 (confirmed not needed here).** A *normal bound* `git push --force-with-lease` does NOT lose the continuation: the watch is keyed on **repo+branch** (sha-independent), so force-push leaves it intact and the poller self-heals `head_sha`; no push-time re-arm is required (and the push-hook arm is #1751's scope — `should_bypass` short-circuit, `pr_state/auto_arm.rs:4-7`). #2741's gap was *no obligation armed at all* (§1), not force-push dropping one. So F-D stays with #1751.

## 7. Implementation slices — all AFTER #2743 S1 merges (ordered, each RED-first)
1. **S-key** — persisted continuation dedup key `(pr_identity, head_sha, target, kind)` + arm/obligation record (schema; no behavior). RED: key round-trips; head-scoped.
2. **S-atleast-once** — gate the notify/handoff stamp on enqueue `Ok`; on failure leave unstamped for retry. RED: `enqueue_failure_is_not_stamped_delivered` retries next poll; success writes key once.
3. **S-obligation** — derive the review-obligation from an UNVERIFIED-pending-CI verdict@H in `pr_state`; arm `{pr,H,reviewer}`. RED: `unverified_pending_ci_arms_reviewer_obligation`; `verified_discharges`; `head_advance_invalidates`.
4. **S-reconciler** — boot pass over ARMED obligations using `poll_runs_for_sha(H)`; fire-if-terminal-and-unkeyed; invalidate-if-head-moved; TTL-expire. RED (crash matrix §2 as the test matrix): `force_push_then_terminal_fires_new_head_only`, `restart_redelivers_undelivered_obligation`, `restart_after_delivered_is_noop`, `old_head_obligation_never_revived`.

## 8. RED test surface (deterministic — no sleep, no real CI/network)
All drive real reducers/pollers with a fake `CiProvider` + temp `home`; assert on durable inbox JSONL + persisted obligation/key state. Enumerated in §7 per slice + the §2 crash-matrix rows. Reverse-mutation: removing the `Ok`-gate ⇒ `enqueue_failure_is_not_stamped_delivered` RED; removing head-invalidation ⇒ `old_head_obligation_never_revived` RED.

## 9. Migration / rollout
- Additive schema (obligation record + dedup key); absent on legacy watches → seeded on first post-upgrade poll (mirrors `last_notified_by_workflow` migration, `watch_state.rs:85-91`). No wire-kind change (`[ci-ready-for-action]`/`[pr-ready-for-merge]` reused). Backward-compatible: no obligation ⇒ current behavior (on-demand review) unchanged. Rollout behind the existing ci_watch path; no operator action.

## 10. Open items for codex
1. Confirm the review-obligation source = **UNVERIFIED-pending-CI verdict** (§4) vs an explicit reviewer-dispatch record. (I recommend deriving from the verdict — zero new dispatch surface.)
2. Confirm boot reconciler lives in `startup_sweep` sibling vs the poll loop's first tick. (Recommend a dedicated boot pass so it's exact-head, not branch-poll.)
3. Confirm F-D deferral to #1751 (§6) and F-A recast to at-least-once (§2).
