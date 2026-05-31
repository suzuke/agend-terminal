# #1525 Analysis Spike — dispatch_idle sidecar lingers after verdict/report

**Author:** fixup-dev-2 · **Status:** spike (read-only) · **Base:** main @ e20f1eb (incl. #1516)

## TL;DR (honest premise correction)

The issue premise — *"report delivery doesn't clear the sidecar"* — is **inaccurate on current main**. `kind=report` **already** clears via `mark_resolved` (messaging.rs:489, landed in #1372). The real root cause is a **record↔clear key asymmetry**:

- **Record** keys the sidecar on `correlation_id` **`.or(task_id)`** (messaging.rs:469).
- **Clear** (report) matches on `correlation_id` **only — no `.or(task_id)`** (messaging.rs:488).

`mark_resolved` requires an **exact** `correlation_id` match. So a dispatch whose sidecar was keyed via the **task_id fallback**, but whose verdict report carries the id in `task_id` (and leaves `correlation_id` empty), never matches → sidecar lingers → spurious nudge once the agent is Idle. **Fix = one line: make the clear path symmetric (`correlation_id.or(task_id)`).**

## ① Sidecar lifecycle (located)

| Phase | Fn | Trigger | Key |
|---|---|---|---|
| **Create** | `dispatch_idle::record_dispatch` (mod.rs:116) | `track_dispatch` on outbound `kind=task` (messaging.rs:467-487). Rejects non-`task` (`if !matches!(expected_kind,"task") return None`). | stores `correlation_id.or(task_id)` |
| **Reset** | `refresh_issued_at` (mod.rs:358) | inbound `kind=update`/`query` w/ matching `correlation_id` (messaging.rs:503) | restarts `issued_at` |
| **Clear (resolve)** | `mark_resolved` (mod.rs:331) — flips `status="resolved"` (scan skips non-pending) | inbound `kind=report` w/ matching **`correlation_id`** (messaging.rs:489) | **exact `correlation_id` only** |
| **Clear (delete)** | `cleanup_pending_for_task_id` (mod.rs:251) — `remove_file` | task `done`/`auto_close` (handler.rs:354/621, auto_close.rs:56) | by `task_id` |
| Sweep | retention/pending_dispatches.rs | age-based GC | — |

## ② Root cause confirmed (with the precise asymmetry)

`track_dispatch` (messaging.rs):

```rust
if matches!(kind_str, "task" | "query") {
    let outbound_corr = msg.correlation_id.as_deref().or(msg.task_id.as_deref());  // ← .or(task_id)
    … record_dispatch(home, from, target, outbound_corr, …)
} else if kind_str == "report" {
    if let Some(corr) = msg.correlation_id.as_deref() {                            // ← correlation_id ONLY
        let _ = mark_resolved(home, corr);
        if corr.starts_with("t-") { auto_close_on_report(…) }
    }
}
```

`mark_resolved`: `find(|d| d.status=="pending" && d.correlation_id.as_deref()==Some(correlation_id))` — **exact match, silent no-op (`None`) on miss**.

**Failure path:** dispatch `kind=task` with `task_id=T`, no `correlation_id` → sidecar keyed `T` (via fallback). Verdict `kind=report` with `task_id=T`, `correlation_id` empty → clear path sees `msg.correlation_id == None` → `mark_resolved` never called → sidecar stays `pending` → fires `dispatch_idle_threshold_exceeded` at the threshold even though the dispatch is functionally complete. (Also misses when the two ids are in different namespaces, but the task_id-fallback asymmetry is the load-bearing, fixable one.)

So: report **does** attempt to clear; it just can't because the clear key is narrower than the record key.

## ③ Fix

**Primary (KISS, 1 line) — restore record↔clear symmetry:**

```rust
} else if kind_str == "report" {
    if let Some(corr) = msg.correlation_id.as_deref().or(msg.task_id.as_deref()) {  // mirror record
        let _ = mark_resolved(home, corr);
        if corr.starts_with("t-") { auto_close_on_report(…) }
    }
}
```

Now any sidecar keyed via the task_id fallback is cleared by a report that carries the id in either field. `mark_resolved` already flips status (idempotent; second clear is a no-op). Zero change to `mark_resolved`/`record_dispatch`.

**Optional robustness (lower priority, more risk):** a sender-pair fallback clear — when a `target` sends a terminal report to its `dispatcher` and no id matches, clear that `(dispatcher → target)` pending dispatch (mirrors `decision_timeout::mark_resolved_for_sender`). Risk: clears the wrong dispatch when several are in flight to the same target, so gate on `terminal=true` and single-pending. Recommend deferring unless the symmetry fix proves insufficient.

## ④ Relation to #1516 (just merged)

#1516 gates the watchdog on the target's **working-state** (snapshot.json) — it suppresses the nudge **while the agent is working**. #1525 is the *opposite* timing: the agent has **delivered the verdict and gone Idle**, so #1516's gate no longer suppresses, and the stale (un-cleared) sidecar fires. The two are complementary, non-overlapping; #1516 cannot cover #1525.

## ⑤ RED + §3.15

**RED→GREEN:** record a sidecar via the task_id fallback (`record_dispatch(corr = task_id, no correlation_id)`); deliver a `kind=report` carrying `task_id=T` with `correlation_id=None`; assert the sidecar flips to `resolved` (pre-fix it stays `pending`). Plus a no-regression test: report with `correlation_id=T` still clears.

**§3.15:** `mark_resolved`/`cleanup_pending_for_task_id` are pure file ops (read sidecar dir, status-flip-write / `remove_file`). No self-IPC, no lock-across-IPC → **no #1492 class**. The fix only widens the key lookup (`.or(task_id)`); it does not change the IPC/lock profile. No §3.15 stress/lock analysis needed.

## KISS

The symmetry fix is one line and makes the clear key exactly mirror the record key — the most KISS possible. The bulk of this PR's value is actually the **RED test** that pins the asymmetry so it can't regress. Recommend: symmetry fix + the two tests; defer the sender-pair fallback.
