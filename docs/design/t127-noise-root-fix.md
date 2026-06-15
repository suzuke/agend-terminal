# t-127 Design Spike — review-verdict terminal auto-close + dispatch-stuck sidecar close (noise root-fix)

**Status:** SPIKE (analysis only — no production code yet). For lead dialectic (dual reviewer).
**Freshness:** origin/main 946facb6. **Author:** fixup-dev-2.

## 1. Problem (empirical)

1. **Ghost review tasks pile up** (#2222/#2224/#2226 dual-review tasks: `updated_at == created_at`, never closed).
2. **Stuck-watchdog false-pings** — r4 nudged "stuck 30min" **5× in one round** on already-VERIFIED review dispatches.

## 2. Root cause — the task's framing is HALF the story

The task attributes (1) to "`terminal` defaults false for verdicts" and (2) to "sidecar not closed on terminal report". Reading the code (946facb6), the **deeper, shared root cause** is a **correlation-key mismatch**:

A reviewer's terminal verdict is sent as `kind=report` with **`correlation_id = "<owner>/<repo>@<branch>"`** (NOT the task id). This is REQUIRED — the pr-state/ci-handoff pipeline keys on `repo@branch` + `reviewed_head` to record the verdict and aggregate the dual-merge gate:
- `messaging.rs:591-599` comment: *"a report carrying the handoff's `repo@branch` correlation (reviewer verdicts do) RESOLVES the ci-handoff track"*.
- `daemon/pr_state/mod.rs:record_verdict` keys on `reviewed_head` → `[review-verdict]` message keyed `repo@branch`.

But BOTH the dispatch sidecar AND the review task are keyed on **`t-xxx`**:
- The review-dispatch sidecar (`dispatch_idle::record_dispatch`) stores `correlation_id = outbound_corr = msg.correlation_id.or(task_id)` = the review **task id** `t-94` (`messaging.rs:516,548`).
- The review task itself is `t-94`.

So in the report handler (`messaging.rs:584-609`), with `corr = repo@branch`:
- `mark_resolved(repo@branch)` → scans sidecars for `correlation_id == repo@branch` → **the `t-94` sidecar never matches → not cleared → watchdog fires** (root of #2).
- `auto_close_on_report` is gated behind `if corr.starts_with("t-")` → `repo@branch` skips it entirely → **the `t-94` task never closes** (root of #1). The `terminal` default is a *second* lock behind this — even if the verdict carried `t-94`, `terminal.unwrap_or(false)` would still block it.

**Conclusion:** fixing "`terminal=false`" alone is insufficient — the verdict report never reaches the `t-xxx`-keyed machinery at all. The fix must **bridge `repo@branch` → the review task `t-xxx`** at verdict time, then reuse the existing, correct machinery.

## 3. Existing machinery to reuse (do NOT rebuild)

| Primitive | Location | What it already does correctly |
|---|---|---|
| `auto_close_on_report` | `tasks/auto_close.rs:7-75` | Gates: `terminal` + `kind==report` + status whitelist (incl. `InReview` #1942) + **`assignee==reporter`** (self-close guard #2010). Closes the task. |
| `detect_verdict(summary)` | `mcp/handlers/comms_gates` (used `comms.rs:442`) | Parses leading `VERIFIED/REJECTED/UNVERIFIED` (§3.12). **Daemon already classifies a report as a verdict** — no reliance on reviewer flags. |
| `mark_resolved` / `cleanup_pending_for_task_id` | `dispatch_idle/mod.rs:604,341` | Deletes sidecar(s) under per-sidecar lock. **Closing the task (`cleanup_pending_for_task_id`, #1018) already clears the sidecar** — so fix A transitively achieves fix B for VERIFIED. |
| branch→task reverse lookup | `daemon/auto_release.rs:231` (`filter(|t| t.branch.as_deref()==Some(branch))`); link written by `link_branch_to_task` #1942 (`tasks/mod.rs:259`, stores `record.branch`) | Resolves a branch to its linked task(s). The review dispatch carries `branch=` (reviewers get a bound worktree on the PR branch), so `t-94.branch == PR branch` is set at dispatch time. |
| pr-state aggregation | `daemon/pr_state/scanner.rs:162-193` | `[pr-ready-for-merge]` is emitted by the SCANNER from verdict aggregation, **orthogonal to task lifecycle** → closing a review task does NOT affect merge-readiness. ✅ Safe. |

## 4. Proposed design (KISS, single choke point)

**Choke point:** the `kind=="report"` branch of `track_dispatch` (`messaging.rs:584-610`) — the one place both the MCP and API-fallback send routes converge, and where `mark_resolved` + `auto_close` already live.

**New bridge (additive — does not touch the existing `corr.starts_with("t-")` path):**

When the report is a **verdict** (`detect_verdict(msg.text).is_some()` AND `reviewed_head` present) and `corr` is a `repo@branch` (not `t-`):
1. Extract `branch` from `corr` (split once on `@`, take the tail).
2. Resolve **review task(s)**: replay tasks, `filter(branch == <branch> && assignee == reporter && status ∈ {Open,Claimed,InProgress,Blocked,InReview})`. (Reporter-scoped → dual reviewers never cross-close.)
3. For each resolved `t-xxx`:
   - **Always** (any verdict — the reviewer responded → not stuck): `mark_resolved(home, t-xxx)` → clears its dispatch sidecar. **(fix B)**
   - **If `VERIFIED`**: `auto_close_on_report(home, "report", t-xxx, reporter, text, terminal=true)` — `terminal=true` synthesized internally (root fix: independent of the reviewer setting any flag). **(fix A)**

This reuses every existing gate (`assignee==reporter`, status whitelist) and the existing sidecar-clear, adding only the `repo@branch → t-xxx` bridge.

## 5. Decision points (for dialectic / operator)

- **DP1 — REJECTED/UNVERIFIED → close the review task?**
  - The reviewer's *job* ("review PR #X") is arguably done on ANY verdict. But a REJECTED PR gets re-reviewed after rework — if the re-review **reuses** the same task, closing it loses tracking; if a **fresh** review task is dispatched, closing is correct.
  - **Recommendation (conservative, minimal-blast):** **only `VERIFIED` auto-closes the task.** This directly kills the empirical ghost-accumulation (the ghosts were all VERIFIED). REJECTED/UNVERIFIED leave the task open for lead/reviewer to manage re-review. The **sidecar still clears on any verdict** (the reviewer responded → silence the stuck-nudge regardless of verdict).
- **DP2 — Bridge source: branch↔task link (recommended) vs sidecar-stores-branch vs reviewer-carries-task_id.**
  - Recommend the **branch↔task reverse lookup** (existing #1942 link + `auto_release.rs:231` pattern) — single source of truth, daemon-side, behavior-independent. (Sidecar-stores-branch is a viable alt if a review dispatch is ever found NOT to carry `branch=`; reviewer-carries-task_id is rejected — relies on agent behavior, not a root fix.)
  - **Verify-before-impl:** confirm review dispatches reliably set `branch=` (so `t-94.branch` is populated). If not, fall back to DP2-alt (store `branch` on the sidecar from `params["branch"]` at `record_dispatch`).
- **DP3 — fire-once latch hardening (noise-reduction defense-in-depth).** Even with the bridge, a sidecar `delete` failure (disk/lock) lets `scan_and_emit` re-fire. **Optional:** add a `reported_at: Option<String>` latch to `PendingDispatch` (mirrors the existing `long_running_escalated` latch, mod.rs:102), set in `mark_resolved` before delete; `scan_and_emit` skips firing if set. Recommend **include** — it's the noise-reduction "fire-once > repeat" principle and cheap.

## 6. Related (separable) — t-116 quota-wedge fire-once

`health.rs` already has `BlockedReason::QuotaExceeded` and `check_quota_gate` blocks *dispatch* to a quota-blocked agent (`messaging.rs:635`), but `scan_and_emit` still pings. **Fix:** in `scan_and_emit`, when the target's health reason is `QuotaExceeded`/`AwaitingOperator`, escalate-once + latch (reuse the `long_running_escalated` idiom) instead of re-pinging every 30 min. **Recommend a SEPARATE small PR** (different subsystem from the verdict bridge; keeps each PR surgical + independently reviewable).

## 7. noise-reduction lens (operator priority)

- **When NOT to send:** sidecar cleared the instant the reviewer responds (any verdict) → no post-response stuck-ping.
- **Dedup/latch:** `reported_at` latch (DP3) + existing `long_running_escalated`/`not_working_streak` debounce; quota-wedge fire-once (§6).
- **Auto-collect not manual:** VERIFIED auto-closes the review task → no lead `task done` drift → no ghost accumulation.

## 8. Phasing (post-dialectic)

- **PR1:** verdict→task bridge in `track_dispatch` (fix A VERIFIED-close + fix B any-verdict sidecar-clear) + DP3 latch. Behavioral repro: dual review dispatch → reviewer VERIFIED with `repo@branch` corr → assert task closed + sidecar gone + (control) REJECTED leaves task open but sidecar cleared. **dual-review (concurrency: sidecar RMW under lock #2028).**
- **PR2 (separable):** §6 quota-wedge fire-once latch in `scan_and_emit`. **dual-review.**
- Backlog cleanup of existing #2217–#2225 ghosts: **lead** via `task sweep` dry-run→confirm (out of code scope).

## 9. Blast / risk

- Bridge is additive; existing `t-` correlation path untouched → non-verdict reports + implementer task reports byte-identical.
- `assignee==reporter` gate + reporter-scoped branch filter → cannot cross-close another reviewer's task or an implementer task.
- pr-state merge-gate orthogonal → no merge-readiness regression.
- Main risk: a review dispatch lacking `branch=` (→ DP2-alt) — verify first.
