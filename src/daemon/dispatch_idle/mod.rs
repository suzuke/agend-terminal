//! L1: cross-team-safe dispatch-idle watchdog.
//!
//! Tracks `send(kind=task)` calls that carry an explicit
//! `expect_reply_within_secs` opt-in. When the expected reply hasn't
//! arrived inside the threshold window, fires a
//! `dispatch_idle_threshold_exceeded` event to the dispatcher's inbox.
//!
//! Cross-team contract: this file MUST stay team-name-free.
//! `no_team_name_strings_in_l1` is the load-bearing invariant test —
//! any L1 grep hit for "fixup" / "reviewer" / "lead" fails CI.
//! Teams opt in by setting `expect_reply_within_secs` on their own
//! dispatches; the generic per-team automation lives in `team_nudge`
//! (was `fixup_nudge`, which was hard-coded to the fixup team).
//!
//! Pattern lineage: closest mirror is `decision_timeout` (threshold +
//! resolve + sidecar lifecycle); hook-site precedent is #870 in
//! `auto_release` (handle_send post-enqueue).

pub(crate) mod team_nudge;

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const PENDING_DIR: &str = "pending-dispatches";
const SCHEMA_VERSION: u32 = 1;

/// #1018: correlation_id sentinels that the watchdog treats as
/// placeholder values (no real task-board cross-reference possible).
/// Sidecars carrying any of these are cleared on the watchdog tick
/// instead of firing `dispatch_idle_threshold_exceeded` — they
/// generate noise because the same value is reused across multiple
/// parallel dispatches so `mark_resolved`'s first-match clears only
/// one slot while sibling sidecars linger.
///
/// Empty string is implicit via `correlation_id.is_none()`.
const PLACEHOLDER_CORRELATION_IDS: &[&str] = &["t-pending", "t-tbd"];

/// #1018: status values that count as "task still live" — sidecar
/// targeting a real task whose status matches one of these stays
/// armed. Anything else (`done`, `cancelled`, `verified`, etc.) means
/// the task is closed and the sidecar should be cleared.
const LIVE_TASK_STATUSES: &[&str] = &["open", "claimed", "in_progress", "blocked"];

/// Scan throttle in supervisor ticks. 6 ≈ 60s at the 10s tick rate —
/// faster than the 30-tick siblings because the threshold the watchdog
/// is gating (single-digit minutes for orchestrator-class dispatches)
/// demands ≤60s fire-time accuracy. Mirrors auto_release's
/// responsiveness-critical cadence rationale.
pub(crate) const TICKS_PER_SCAN: u64 = 6;

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct PendingDispatch {
    #[serde(default)]
    pub(crate) schema_version: u32,
    #[serde(default)]
    pub(crate) dispatch_id: String,
    /// The `send.from` identity (who is waiting for a reply).
    #[serde(default)]
    pub(crate) dispatcher: String,
    /// The `send.target` identity (who is expected to reply).
    #[serde(default)]
    pub(crate) target: String,
    /// `task_id` / thread correlation id from the original dispatch.
    #[serde(default)]
    pub(crate) correlation_id: Option<String>,
    /// Original dispatch kind: `"task"` (#1268: query excluded).
    #[serde(default)]
    pub(crate) expected_kind: String,
    #[serde(default)]
    pub(crate) threshold_secs: i64,
    #[serde(default)]
    pub(crate) issued_at: String,
    /// Sidecar lifecycle state. See [`DispatchStatus`].
    #[serde(default)]
    pub(crate) status: DispatchStatus,
    /// L2-owned dedup field: timestamp of the last nudge emitted for
    /// this dispatch. `None` = no nudge sent yet. L1 never reads this
    /// field; it lives in the L1 schema to keep the sidecar a single
    /// source of truth (avoids parallel L2 sidecar bookkeeping).
    #[serde(default)]
    pub(crate) nudge_sent_at: Option<String>,
    /// #1658: consecutive `scan_and_emit` ticks the target has appeared NOT
    /// working (snapshot state ∉ thinking/tool_use) while past threshold.
    /// Debounces the #1516 instantaneous-state gate: a brief idle gap during
    /// active heads-down work (or a momentarily-stale snapshot) that lands right
    /// on the threshold boundary must persist for [`DEBOUNCE_SCANS`] consecutive
    /// scans before firing, so it doesn't false-fire. Reset to 0 the moment the
    /// target is observed working again.
    #[serde(default)]
    pub(crate) not_working_streak: u32,
    /// #2008-p2: how many times the deadline has been auto-EXTENDED because the
    /// target showed activity past threshold (the `target_is_working`/`waiting_on`
    /// suppress path calls `refresh_issued_at`). Bounds the otherwise-unlimited
    /// extension: at [`REFRESH_CAP`] the watchdog escalates ONCE
    /// (`long_running_escalated`) instead of refreshing forever — a stuck-in-loop
    /// agent whose pane keeps churning must not stay invisible.
    #[serde(default)]
    pub(crate) refresh_count: u32,
    /// #2008-p2: latch — the one-time "long-running, confirm expected" escalation
    /// fired (escalate-don't-repeat). Cleared with the sidecar on resolution.
    #[serde(default)]
    pub(crate) long_running_escalated: bool,
    /// #2031: timestamp L1 flipped `status` to `Exceeded` (i.e. when the DISPATCHER
    /// was notified via `..._exceeded`). L2 reads it to TIER the escalation: the
    /// agent `..._nudge` — the costlier interrupt — is the SECOND rung, deferred
    /// until [`team_nudge::ESCALATE_TO_AGENT_AFTER_SECS`] past this stamp so the
    /// dispatcher gets a window to act first. `None` (legacy / pre-#2031 sidecar)
    /// fails OPEN to the old immediate-nudge behavior — a missing stamp must never
    /// SUPPRESS a real nudge.
    #[serde(default)]
    pub(crate) exceeded_at: Option<String>,
    /// #t-127: fire-once latch — set the moment a correlated REPORT arrives
    /// (`mark_resolved`), ATOMIC with the sidecar delete under the per-file lock.
    /// Normally the delete removes the sidecar outright; but if the delete FAILS
    /// (disk/permission) the persisted latch makes `scan_and_emit` SKIP the stale
    /// sidecar instead of firing a spurious "stuck" nudge for a dispatch the
    /// reviewer already answered. Mirrors the `long_running_escalated` latch.
    /// Reset to `None` on re-dispatch (a fresh episode).
    #[serde(default)]
    pub(crate) reported_at: Option<String>,
    /// #t-116/#78445-2: durable fire-once latch — the one-time escalation fired
    /// while the target is QUOTA-WEDGED (backend usage-limit / quota-reached
    /// hard-block, snapshot `agent_state == "usage_limit"`). Such an agent is
    /// EXPECTED to stay silent until the quota resets (often hours/days), so
    /// re-nudging it every threshold is pure noise (r5: agy quota wedged 6 days,
    /// pinged every 30 min). Escalate ONCE, then suppress. #78445-2: this is a
    /// ONE-SHOT per dispatch — it is NOT cleared on a non-wedged tick, so a
    /// snapshot flicker can't re-fire it (the observed same-heads-up-twice noise);
    /// only a re-dispatch (a genuinely new episode) resets it. Mirrors the
    /// `long_running_escalated` fire-once latch.
    #[serde(default)]
    pub(crate) quota_escalated: bool,
}

/// #2008-p2: max activity-based deadline extensions before the watchdog escalates
/// ONCE with a "long-running — confirm expected" notice instead of refreshing
/// forever. 3 × the dispatch threshold (≈90min at the #2031 1800s default) — long
/// enough that a genuine long task isn't pestered, short enough that a
/// stuck-in-loop agent surfaces for an operator confirm within a bounded window.
const REFRESH_CAP: u32 = 3;

/// #1658: how many consecutive not-working `scan_and_emit` ticks (past
/// threshold) the target must show before the dispatch-idle signal fires. A
/// single brief idle gap during active work is the common false-fire; requiring
/// a short streak filters it. Cost to a genuinely-stuck agent is at most
/// `(DEBOUNCE_SCANS - 1) * scan-cadence` of extra delay — negligible vs the
/// dispatch threshold. NOTE: this debounces the EXISTING #1516 gate; it does not
/// add a missing gate. The structurally-correct fix (gate on output-recency, not
/// instantaneous state — `AgentSnapshot` has no activity timestamp today) is a
/// documented follow-up if the residual is still annoying after this + #1657.
///
/// #2031 direction-3 (count "any completed turn since dispatch" as progress) was
/// SPIKED and DEFERRED to this same follow-up: the only turn-end signal is the
/// Stop hook event, which today lives only in the `AGEND_HOOK_STATE_POC`-gated,
/// claude-only hook_shadow layer (#1523/#2014) — not cheaply in the per-tick
/// snapshot. Folding it in is the "太繞" path; #2031 ships directions 1+2 (1800s
/// default + tiered escalation) instead, which already require ≥40min of genuine
/// silence before the costly agent interrupt. Revisit once hook state is promoted
/// past the POC flag and a turn-end timestamp lands in `AgentSnapshot` (#2008).
const DEBOUNCE_SCANS: u32 = 3;

/// #1636: lifecycle of a dispatch-idle sidecar, replacing the stringly-typed
/// 4-state `status` field so the compiler enforces exhaustiveness at the guard
/// sites. Serializes to the SAME lowercase wire strings the `String` field used
/// (`pending` / `resolved` / `exceeded` / `cancelled`) via `rename_all`, so
/// on-disk sidecars and IPC payloads are byte-identical — pinned by
/// `dispatch_status_serde_roundtrip`. Strict like the pr_state enums next door:
/// an unknown status string fails to deserialize, so the sidecar is skipped by
/// `list_pending`'s existing fail-open loader (the module only ever writes the
/// four known states).
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum DispatchStatus {
    /// Dispatch in flight, watchdog armed — the only state that surfaces in
    /// operator views and matches the resolve/escalate scans.
    #[default]
    Pending,
    /// A matching report arrived → watchdog disarmed.
    Resolved,
    /// `threshold_secs` elapsed with no report → idle nudge fired.
    Exceeded,
    /// Underlying task reached a terminal state (done/cancelled) → sidecar void.
    Cancelled,
}

pub(crate) fn pending_dir(home: &Path) -> PathBuf {
    home.join(PENDING_DIR)
}

pub(crate) fn pending_path(home: &Path, dispatch_id: &str) -> PathBuf {
    pending_dir(home).join(format!("{dispatch_id}.json"))
}

fn dispatch_lock_path(home: &Path, dispatch_id: &str) -> PathBuf {
    pending_dir(home).join(format!("{dispatch_id}.lock"))
}

/// [M2] Delete a sidecar UNDER its `{dispatch_id}.lock`, so the delete is
/// mutually exclusive with the team-nudge / L1 locked read-modify-write
/// (`with_json_state` / `scan_and_emit`, which take the same lock). Without the
/// lock, an unlocked `remove_file` can land inside an RMW's read→write window
/// and the RMW then re-creates (resurrects) the just-deleted sidecar, leaking a
/// resolved dispatch forever. Returns `true` iff the sidecar file was removed.
///
/// Also removes the CORRECT lock file (`{dispatch_id}.lock`); the pre-fix delete
/// sites removed `{dispatch_id}.json.lock` (wrong name) and orphaned the real
/// one. Single correct implementation — ALL sidecar-delete call sites route
/// through it (verified by grep of `dispatch_idle/` for `remove_file` on a
/// `pending_path`):
/// - `mark_resolved` (report-arrival clear)
/// - `cleanup_pending_for_task_id` (#1018 task-close clear)
/// - `cleanup_pending_for_instance` (#1018 instance-delete clear)
/// - `scan_and_emit` (#1018-A tick-time stale-sidecar clear)
fn delete_sidecar_locked(home: &Path, dispatch_id: &str) -> bool {
    let path = pending_path(home, dispatch_id);
    let lock_path = dispatch_lock_path(home, dispatch_id);
    let guard = crate::store::acquire_file_lock(&lock_path).ok();
    let removed = std::fs::remove_file(&path).is_ok();
    // Release the OS lock BEFORE removing the lock file itself.
    drop(guard);
    let _ = std::fs::remove_file(&lock_path);
    removed
}

/// #t-127: persist the `reported_at` fire-once latch THEN delete the sidecar,
/// both UNDER the single `{dispatch_id}.lock` acquisition (atomic with the
/// delete). A report arriving disarms the watchdog; the normal path is the
/// delete, but if the `remove_file` FAILS the latch is already on disk, so
/// `scan_and_emit` sees `reported_at.is_some()` and skips firing a spurious
/// nudge instead of resurrecting the noise. Same lock as the L1/L2 RMW (no
/// resurrection race; correct `{id}.lock` removal). Returns `true` iff removed.
fn mark_reported_and_delete_locked(home: &Path, dispatch_id: &str) -> bool {
    let path = pending_path(home, dispatch_id);
    let lock_path = dispatch_lock_path(home, dispatch_id);
    let guard = crate::store::acquire_file_lock(&lock_path).ok();
    // Persist the latch BEFORE the delete so a failed remove still leaves it set.
    if let Ok(content) = std::fs::read_to_string(&path) {
        if let Ok(mut pd) = serde_json::from_str::<PendingDispatch>(&content) {
            if pd.reported_at.is_none() {
                pd.reported_at = Some(chrono::Utc::now().to_rfc3339());
                if let Ok(body) = serde_json::to_string_pretty(&pd) {
                    let _ = crate::store::atomic_write(&path, body.as_bytes());
                }
            }
        }
    }
    let removed = std::fs::remove_file(&path).is_ok();
    drop(guard);
    let _ = std::fs::remove_file(&lock_path);
    removed
}

/// Generate a deterministic-format dispatch id (`disp-<unix_micros>-<seq>`).
fn next_dispatch_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let ts = chrono::Utc::now().format("%Y%m%d%H%M%S%6f");
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("disp-{ts}-{seq}")
}

/// Record an outbound dispatch. Returns the new dispatch_id, or `None`
/// if the inputs are invalid / the disk write failed.
///
/// Called from `handle_send` post-enqueue: any failure here is silently
/// best-effort (the message has already been delivered; watchdog
/// coverage is a defence-in-depth layer that must never block the
/// dispatch primitive).
pub(crate) fn record_dispatch(
    home: &Path,
    dispatcher: &str,
    target: &str,
    correlation_id: Option<&str>,
    expected_kind: &str,
    threshold_secs: i64,
) -> Option<String> {
    if dispatcher.is_empty() || target.is_empty() || threshold_secs <= 0 {
        return None;
    }
    if !matches!(expected_kind, "task") {
        return None;
    }
    let dir = pending_dir(home);
    if std::fs::create_dir_all(&dir).is_err() {
        return None;
    }
    // t-dispatchidle-clear-on-report (2): dedup by (dispatcher, target,
    // correlation_id). Re-dispatching the SAME task (same task_id) used to create a
    // fresh duplicate sidecar each call (`next_dispatch_id()`), so 142 correlation
    // ids had duplicate sidecars live and a single report cleared only one. If a
    // NON-terminal sidecar with this exact key already exists, REFRESH it in place
    // (reset the clock + nudge state, keep its `dispatch_id`) instead of creating a
    // new one — one sidecar per (dispatcher, target, correlation) dispatch-intent.
    // Only when a `correlation_id` is present: without one we can't tell two
    // distinct dispatches apart, so fall through to a fresh sidecar.
    if let Some(corr) = correlation_id {
        let is_same_intent = |d: &PendingDispatch| {
            matches!(d.status, DispatchStatus::Pending | DispatchStatus::Exceeded)
                && d.dispatcher == dispatcher
                && d.target == target
                && d.correlation_id.as_deref() == Some(corr)
        };
        if let Some(mut existing) = list_pending(home).into_iter().find(is_same_intent) {
            existing.issued_at = chrono::Utc::now().to_rfc3339();
            existing.status = DispatchStatus::Pending;
            existing.nudge_sent_at = None;
            existing.not_working_streak = 0;
            existing.threshold_secs = threshold_secs;
            // #2008-p2 (codex review): a re-dispatch of the SAME correlation is a
            // NEW episode — reset the extension cap + escalation latch too. Without
            // this, a correlation already long-running-escalated is reborn at
            // refresh_count >= CAP with the latch set, so scan_and_emit neither
            // extends nor re-escalates and the fresh dispatch-intent's protection
            // is silently eaten by the stale latch.
            existing.refresh_count = 0;
            existing.long_running_escalated = false;
            // #2031: a revived (Exceeded→Pending) sidecar's escalation stamp is now
            // stale — clear it so L2's second-window timer restarts from the next
            // real Exceeded transition (L1 re-stamps `exceeded_at` then).
            existing.exceeded_at = None;
            // #t-127: a re-dispatch is a fresh episode — clear the report latch so
            // the revived sidecar's watchdog is armed again (a stale latch would
            // permanently suppress the new dispatch's nudge).
            existing.reported_at = None;
            // #t-116: likewise reset the quota-wedge escalation latch for the fresh
            // episode (a stale latch would suppress a genuine new quota escalation).
            existing.quota_escalated = false;
            // CR-2026-06-14: persist the refreshed episode under the per-file flock
            // via with_json_state — NOT a bare unlocked write of the list_pending
            // snapshot. scan_and_emit concurrently re-reads and flips the SAME
            // sidecar to Exceeded under that lock; an unlocked write here raced it
            // (lost update — a just-written Exceeded clobbered, or this Pending
            // reset clobbered). The mutable state is fully reset above, so
            // overwriting the locked-current state with `existing` IS the intended
            // re-dispatch semantics — now serialized against the scan.
            let dispatch_id = existing.dispatch_id.clone();
            let refreshed = crate::store::with_json_state::<PendingDispatch, _, _>(
                &pending_path(home, &dispatch_id),
                move |cur| {
                    *cur = existing;
                },
            );
            if matches!(refreshed, Ok(Some(()))) {
                return Some(dispatch_id);
            }
            // Refresh write failed / sidecar vanished under the lock → fall through
            // to a fresh sidecar (best effort).
        }
    }
    let dispatch_id = next_dispatch_id();
    let payload = PendingDispatch {
        schema_version: SCHEMA_VERSION,
        dispatch_id: dispatch_id.clone(),
        dispatcher: dispatcher.to_string(),
        target: target.to_string(),
        correlation_id: correlation_id.map(String::from),
        expected_kind: expected_kind.to_string(),
        threshold_secs,
        issued_at: chrono::Utc::now().to_rfc3339(),
        status: DispatchStatus::Pending,
        nudge_sent_at: None,
        not_working_streak: 0,
        refresh_count: 0,
        long_running_escalated: false,
        exceeded_at: None,
        reported_at: None,
        quota_escalated: false,
    };
    let body = match serde_json::to_string_pretty(&payload) {
        Ok(s) => s,
        Err(_) => return None,
    };
    if crate::store::atomic_write(&pending_path(home, &dispatch_id), body.as_bytes()).is_err() {
        return None;
    }
    // #1866 (b) clear-on-handoff: a NEW dispatch for the same (dispatcher, target)
    // means any EARLIER still-armed dispatch from that dispatcher to this target is
    // a stale hand-off — the agent has been re-tasked, so its old idle timer must
    // not keep nudging for work that's been superseded (#1861 class: dev pushed the
    // PR + moved on, the old dispatch still fired). Retire those sidecars now.
    // Boundary (avoid clobbering a genuinely-parallel dispatch): SAME dispatcher +
    // strictly OLDER `issued_at` + a DIFFERENT `correlation_id`. Gated on the new
    // dispatch carrying a `correlation_id` — without one we can't tell dispatches
    // apart (same guard as the in-place dedup above).
    if let (Some(new_corr), Ok(new_dt)) = (
        correlation_id,
        chrono::DateTime::parse_from_rfc3339(&payload.issued_at),
    ) {
        for stale in list_pending(home).into_iter().filter(|d| {
            matches!(d.status, DispatchStatus::Pending | DispatchStatus::Exceeded)
                && d.dispatcher == dispatcher
                && d.target == target
                && d.dispatch_id != dispatch_id
                && d.correlation_id.as_deref() != Some(new_corr)
                && chrono::DateTime::parse_from_rfc3339(&d.issued_at)
                    .map(|t| t < new_dt)
                    .unwrap_or(false)
        }) {
            if delete_sidecar_locked(home, &stale.dispatch_id) {
                tracing::debug!(
                    target: "dispatch_idle",
                    dispatch_id = %stale.dispatch_id,
                    target = %target,
                    superseded_by = %dispatch_id,
                    old_correlation_id = ?stale.correlation_id,
                    "#1866 retired stale dispatch sidecar — target re-dispatched"
                );
            }
        }
    }
    Some(dispatch_id)
}

/// Read all pending dispatch sidecars from disk. Forward-compat: skips
/// any sidecar whose `schema_version` is unknown.
pub(crate) fn list_pending(home: &Path) -> Vec<PendingDispatch> {
    let dir = pending_dir(home);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(d) = serde_json::from_str::<PendingDispatch>(&content) else {
            continue;
        };
        if d.schema_version != SCHEMA_VERSION {
            continue;
        }
        out.push(d);
    }
    out.sort_by(|a, b| a.issued_at.cmp(&b.issued_at));
    out
}

fn write_dispatch(home: &Path, d: &PendingDispatch) -> bool {
    let body = match serde_json::to_string_pretty(d) {
        Ok(s) => s,
        Err(_) => return false,
    };
    crate::store::atomic_write(&pending_path(home, &d.dispatch_id), body.as_bytes()).is_ok()
}

/// #1018 (A): is the correlation_id an explicit placeholder sentinel
/// (`t-pending`, `t-tbd`, or empty/whitespace-only string) that the
/// watchdog should clear without firing a notification?
///
/// **Note on `None`**: `correlation_id == None` is NOT treated as a
/// placeholder — it means the dispatch never had an upstream
/// correlation in the first place, and #947 (load-bearing operator
/// contract) requires the nudge to still fire with `dispatch_id` as
/// the fallback. Only sidecars that explicitly carry a placeholder
/// STRING are eligible for silent cleanup.
fn is_placeholder_correlation(corr: Option<&str>) -> bool {
    let Some(c) = corr else {
        return false;
    };
    let c = c.trim();
    if c.is_empty() {
        return true;
    }
    PLACEHOLDER_CORRELATION_IDS.contains(&c)
}

/// #1018 (A): is `agent` a known target in the current fleet registry?
/// Falls back to "yes" (treats unknown fleet.yaml state as live) on
/// any read/parse error so a transient I/O glitch doesn't flush real
/// pending dispatches.
fn target_in_fleet(home: &Path, agent: &str) -> bool {
    let Ok(fleet) = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)) else {
        return true; // fail-open
    };
    fleet.resolve_instance(agent).is_some()
}

/// #1018 (A): is `task_id` still live on the task board? Returns
/// `Some(true)` for live (one of `LIVE_TASK_STATUSES`); `Some(false)`
/// for a definitively closed OR definitively-absent task (the caller
/// skips the nudge / treats it dead); `None` when the route is UNPROVABLE
/// (treat as live — fail-open, keep nudging).
fn task_still_live(home: &Path, task_id: &str) -> Option<bool> {
    if task_id.is_empty() {
        return None;
    }
    // #2760 (codex ruling m-…-1154): apply task-liveness ONLY when the correlation
    // PARSES as a canonical task id (typed `TaskId::parse_canonical`, not a raw
    // string convention). A non-task / query correlation (a synthetic dispatch id,
    // a message correlation) was NEVER a board task, so a strict route always
    // returns NotFound — the old NotFound→Some(false) orphan-dead policy would
    // WRONGLY suppress its idle nag (the app-e2e / query-dispatch regression). A
    // non-task correlation fails OPEN → keep nudging. The `?` bails to `None`
    // (fail-open) when the correlation is not a canonical task id.
    crate::task_events::TaskId::parse_canonical(task_id)?;
    // #1608b/#1614: event-sourced lookup, NOT a `tasks/{id}.json` probe — that
    // file is never written, so the old read always failed → this check was dead.
    // #2760 R1 (explicit dispatch-idle policy) — for a CANONICAL task id only:
    //   Found                → real liveness (terminal status → Some(false) dead);
    //   NotFound             → Some(false): a DEFINITIVELY-absent task is orphan-dead
    //                          (the deliberate policy the frozen plan permits);
    //   Unreadable/Ambiguous → None: the route is UNPROVABLE → fail-open → treat as
    //                          live (keep nudging), never orphan an unprovable task.
    match crate::tasks::load_routed(home, task_id) {
        Ok(routed) => {
            let status = routed.task.status.to_string();
            Some(LIVE_TASK_STATUSES.contains(&status.as_str()))
        }
        Err(crate::tasks::TaskRouteError::NotFound) => Some(false),
        Err(_) => None,
    }
}

/// #1018 (B) eager cleanup: when a task transitions to a terminal
/// state (done / cancelled), scan pending sidecars and delete any
/// whose `correlation_id` matches the closed task_id. Prevents the
/// watchdog firing later on a dispatch whose work has already been
/// reported via the task board instead of via `kind=report`. Returns
/// the count of sidecars deleted (for callers that want to log).
pub(crate) fn cleanup_pending_for_task_id(home: &Path, task_id: &str) -> usize {
    if task_id.is_empty() || is_placeholder_correlation(Some(task_id)) {
        return 0;
    }
    let mut count = 0usize;
    for d in list_pending(home) {
        // A closed task → purge EVERY sidecar for it, not just Pending ones. A
        // late report on an already-Exceeded dispatch (or a task closing after the
        // idle nudge fired) must still clear the sidecar (codex probe #1).
        if d.correlation_id.as_deref() != Some(task_id) {
            continue;
        }
        // [M2] delete under the sidecar lock (no resurrection race vs a concurrent
        // team-nudge / L1 RMW; removes the correct `{id}.lock`).
        if delete_sidecar_locked(home, &d.dispatch_id) {
            count += 1;
            tracing::debug!(
                target: "dispatch_idle",
                dispatch_id = %d.dispatch_id,
                task_id = %task_id,
                "#1018 cleared stale sidecar — task_id closed"
            );
        }
    }
    if count > 0 {
        tracing::info!(
            target: "dispatch_idle",
            task_id = %task_id,
            count,
            "#1018 cleared pending sidecars on task closure"
        );
    }
    count
}

/// #1916: a task REASSIGN (`OwnerAssigned`) must move the dispatch-idle sidecar to
/// the new owner — otherwise the watchdog keeps nudging the FORMER owner about a
/// task they no longer hold. Call-site hook (mirrors `cleanup_pending_for_task_id`;
/// keeps `task_events` free of a dispatch_idle dependency).
///
/// - `Some(new_owner)`: RETARGET every sidecar for `task_id` IN PLACE (same
///   `dispatch_id` file) → `target = new_owner`, and RESET the idle clock
///   (`issued_at = now`, `not_working_streak = 0`, revive `Exceeded` → `Pending`,
///   clear `nudge_sent_at`). The new owner just took over; inheriting the prior
///   owner's near-threshold clock would nudge them immediately (#1866 principle:
///   the nudge must reflect the CURRENT owner's idle time, not an inherited age).
/// - `None` (orphan / #1903 disband-orphan): CLEAR the sidecars — there is no owner
///   to nudge. Delegates to [`cleanup_pending_for_task_id`] (never sets `target=None`).
///
/// In-place RMW preserves the `dispatch_id` file identity, so `record_dispatch`'s
/// `(dispatcher, target, correlation_id)` dedup key recomputes correctly — a later
/// re-dispatch to the new owner refreshes THIS sidecar rather than duplicating it,
/// and nothing is orphaned.
pub(crate) fn reassign_pending_for_task(
    home: &Path,
    task_id: &str,
    new_owner: Option<&str>,
) -> usize {
    if task_id.is_empty() || is_placeholder_correlation(Some(task_id)) {
        return 0;
    }
    let Some(new_owner) = new_owner else {
        // No owner to nudge → clear (NOT target=None).
        return cleanup_pending_for_task_id(home, task_id);
    };
    let mut count = 0usize;
    for d in list_pending(home) {
        if d.correlation_id.as_deref() != Some(task_id) {
            continue;
        }
        if d.target == new_owner {
            continue; // already targets the new owner — nothing to move
        }
        let path = pending_path(home, &d.dispatch_id);
        let updated = crate::store::with_json_state::<PendingDispatch, _, _>(&path, |cur| {
            cur.target = new_owner.to_string();
            cur.issued_at = chrono::Utc::now().to_rfc3339();
            cur.not_working_streak = 0;
            cur.nudge_sent_at = None;
            cur.status = DispatchStatus::Pending;
            // #2031: revived for a new owner — drop the prior owner's escalation
            // stamp so L2's second window restarts from the next Exceeded.
            cur.exceeded_at = None;
            true
        });
        if matches!(updated, Ok(Some(true))) {
            count += 1;
            tracing::info!(
                target: "dispatch_idle",
                dispatch_id = %d.dispatch_id,
                task_id = %task_id,
                new_owner = %new_owner,
                "#1916 retargeted pending sidecar to reassigned owner (idle clock reset)"
            );
        }
    }
    count
}

/// #1018 (C) eager cleanup: when an instance is deleted, scan pending
/// sidecars and delete any whose `target` matches the deleted instance
/// name. The deleted instance can never deliver a `kind=report`, so
/// every sidecar targeting it would fire watchdog noise indefinitely.
/// Returns the count of sidecars deleted (best-effort; failures are
/// silently skipped, matching the rest of `full_delete_instance`'s
/// cleanup contract).
pub(crate) fn cleanup_pending_for_instance(home: &Path, instance_name: &str) -> usize {
    if instance_name.is_empty() {
        return 0;
    }
    let mut count = 0usize;
    for d in list_pending(home) {
        // Clean EVERY sidecar for the deleted instance, not just Pending ones.
        // Resolved/Exceeded sidecars have no further use once the target is gone,
        // and skipping them (the pre-fix behaviour) left them to accumulate.
        if d.target != instance_name {
            continue;
        }
        // [M2] delete under the sidecar lock (no resurrection race; correct `{id}.lock`).
        if delete_sidecar_locked(home, &d.dispatch_id) {
            count += 1;
            tracing::debug!(
                target: "dispatch_idle",
                dispatch_id = %d.dispatch_id,
                instance = %instance_name,
                "#1018 cleared stale sidecar — target instance deleted"
            );
        }
    }
    if count > 0 {
        tracing::info!(
            target: "dispatch_idle",
            instance = %instance_name,
            count,
            "#1018 cleared pending sidecars on instance deletion"
        );
    }
    count
}

/// #1907 teardown audit: does any pending-dispatch sidecar still target
/// `instance_name`? Mirrors [`cleanup_pending_for_instance`]'s `d.target ==`
/// predicate exactly so the residual audit and the cleanup never disagree.
pub(crate) fn has_pending_for_instance(home: &Path, instance_name: &str) -> bool {
    if instance_name.is_empty() {
        return false;
    }
    list_pending(home).iter().any(|d| d.target == instance_name)
}

/// Resolve a pending dispatch by `correlation_id` (NOT by sender —
/// decision_timeout's sender-keyed semantic is wrong here because a
/// single dispatcher can have multiple pending dispatches outstanding,
/// each with a distinct correlation_id). Returns the resolved
/// dispatch_id, or `None` if no matching pending entry exists.
pub(crate) fn mark_resolved(home: &Path, correlation_id: &str, reporter: &str) -> Option<String> {
    if correlation_id.is_empty() || reporter.is_empty() {
        return None;
    }
    let mut first_deleted: Option<String> = None;
    for d in list_pending(home).into_iter().filter(|d| {
        matches!(d.status, DispatchStatus::Pending | DispatchStatus::Exceeded)
            && d.correlation_id.as_deref() == Some(correlation_id)
            && d.target == reporter
    }) {
        // [M2] DELETE the sidecar (rather than flip to `Resolved` and leave the
        // file to accumulate — the pre-fix primary `pending-dispatches/` leak)
        // UNDER its lock, so a concurrent team-nudge / L1 RMW can't resurrect it.
        // #t-127: latch `reported_at` atomically with the delete, so a failed
        // remove still disarms the watchdog (see `mark_reported_and_delete_locked`).
        if mark_reported_and_delete_locked(home, &d.dispatch_id) {
            first_deleted.get_or_insert(d.dispatch_id);
        } else {
            // #2004: a matching sidecar whose delete failed WILL fire a
            // spurious idle nudge once the target goes Idle — surface the
            // swallowed failure (non-fatal: the next resolve attempt or the
            // sweep can still clear it).
            tracing::warn!(
                dispatch_id = %d.dispatch_id,
                correlation = %correlation_id,
                "dispatch_idle resolve: sidecar delete failed — stale sidecar may fire a spurious idle nudge"
            );
        }
    }
    first_deleted
}

/// #1047: reset the timer on a pending sidecar when the dispatchee sends
/// a non-report message (kind=update/query) with matching correlation_id.
/// The sidecar stays live (future silence still fires), but the threshold
/// clock restarts from now. Returns the refreshed dispatch_id, or `None`
/// if no matching pending sidecar exists.
pub(crate) fn refresh_issued_at(
    home: &Path,
    correlation_id: &str,
    reporter: &str,
) -> Option<String> {
    if correlation_id.is_empty() || reporter.is_empty() {
        return None;
    }
    let matched = list_pending(home).into_iter().find(|d| {
        d.status == DispatchStatus::Pending
            && d.correlation_id.as_deref() == Some(correlation_id)
            && d.target == reporter
    });
    let d = matched?;
    let id = d.dispatch_id.clone();
    let path = pending_path(home, &id);
    crate::store::with_json_state::<PendingDispatch, _, _>(&path, |current| {
        if current.status != DispatchStatus::Pending {
            return None;
        }
        current.issued_at = chrono::Utc::now().to_rfc3339();
        Some(id.clone())
    })
    .ok()
    .flatten()
    .flatten()
}

/// #1658: set the debounce streak on a Pending sidecar (RMW under the store's
/// per-file lock). No-op if the sidecar is gone or no longer Pending (e.g. a
/// concurrent mark_resolved won the race) — we never resurrect a closed sidecar.
fn set_not_working_streak(home: &Path, dispatch_id: &str, val: u32) {
    let path = pending_path(home, dispatch_id);
    let _ = crate::store::with_json_state::<PendingDispatch, _, _>(&path, |cur| {
        if cur.status == DispatchStatus::Pending {
            cur.not_working_streak = val;
        }
        Some(())
    });
}

/// #2008-p2: increment the activity-extension counter on a Pending sidecar (RMW
/// under the store lock). No-op if the sidecar is gone / no longer Pending.
fn bump_refresh_count(home: &Path, dispatch_id: &str) {
    let path = pending_path(home, dispatch_id);
    let _ = crate::store::with_json_state::<PendingDispatch, _, _>(&path, |cur| {
        if cur.status == DispatchStatus::Pending {
            cur.refresh_count = cur.refresh_count.saturating_add(1);
        }
        Some(())
    });
}

/// #2008-p2: latch the one-time long-running escalation on a Pending sidecar
/// (escalate-don't-repeat). No-op if the sidecar is gone / no longer Pending.
fn set_long_running_escalated(home: &Path, dispatch_id: &str) {
    let path = pending_path(home, dispatch_id);
    let _ = crate::store::with_json_state::<PendingDispatch, _, _>(&path, |cur| {
        if cur.status == DispatchStatus::Pending {
            cur.long_running_escalated = true;
        }
        Some(())
    });
}

/// #t-116: latch the one-time quota-wedge escalation (mirrors
/// `set_long_running_escalated`). RMW under the per-file lock, so it serializes
/// against `scan_and_emit` / team-nudge on the SAME sidecar.
fn set_quota_escalated(home: &Path, dispatch_id: &str) {
    let path = pending_path(home, dispatch_id);
    let _ = crate::store::with_json_state::<PendingDispatch, _, _>(&path, |cur| {
        if cur.status == DispatchStatus::Pending {
            cur.quota_escalated = true;
        }
        Some(())
    });
}

/// #t-116: is `target` QUOTA-WEDGED — i.e. its snapshot `agent_state` is
/// `"usage_limit"` (the classifier's `AgentState::UsageLimit`, which maps to
/// `BlockedReason::QuotaExceeded`)? Such an agent is hard-blocked on a backend
/// usage-limit / quota-reached and is EXPECTED to stay silent until the quota
/// resets. Read from the SAME file-based snapshot the working-state gate uses (no
/// registry lock).
fn target_is_quota_wedged(snapshot: Option<&crate::snapshot::FleetSnapshot>, target: &str) -> bool {
    snapshot
        .and_then(|s| s.agents.iter().find(|a| a.name == target))
        .map(|a| a.agent_state == "usage_limit")
        .unwrap_or(false)
}

/// PR2 L3 visibility — per-instance dispatch metadata view.
/// Pending sidecars where this instance is the **dispatcher**: outbound
/// dispatches it's still waiting for replies on.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub(crate) struct DispatchedWaitingFor {
    pub correlation_id: Option<String>,
    pub target: String,
    pub threshold_secs: i64,
    pub elapsed_secs: i64,
}

/// PR2 L3 visibility — per-instance dispatch metadata view.
/// Pending sidecars where this instance is the **target**: inbound
/// dispatches it owes a reply on.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub(crate) struct PendingResponseTo {
    pub correlation_id: Option<String>,
    pub dispatcher: String,
    pub threshold_secs: i64,
    pub elapsed_secs: i64,
}

/// PR2 L3 helper: return the per-instance dispatch-idle metadata for
/// `agent`, split into outbound (as dispatcher) and inbound (as target)
/// views.
///
/// Only `status == "pending"` sidecars surface — `resolved`,
/// `exceeded`, and `cancelled` entries are filtered out so the
/// operator-facing view stays focused on live work.
pub(crate) fn pending_for_instance(
    home: &Path,
    agent: &str,
) -> (Vec<DispatchedWaitingFor>, Vec<PendingResponseTo>) {
    let now = chrono::Utc::now();
    let mut as_dispatcher = Vec::new();
    let mut as_target = Vec::new();
    if agent.is_empty() {
        return (as_dispatcher, as_target);
    }
    for d in list_pending(home) {
        if d.status != DispatchStatus::Pending {
            continue;
        }
        let elapsed_secs = chrono::DateTime::parse_from_rfc3339(&d.issued_at)
            .map(|t| {
                now.signed_duration_since(t.with_timezone(&chrono::Utc))
                    .num_seconds()
            })
            .unwrap_or(0);
        if d.dispatcher == agent {
            as_dispatcher.push(DispatchedWaitingFor {
                correlation_id: d.correlation_id.clone(),
                target: d.target.clone(),
                threshold_secs: d.threshold_secs,
                elapsed_secs,
            });
        }
        if d.target == agent {
            as_target.push(PendingResponseTo {
                correlation_id: d.correlation_id.clone(),
                dispatcher: d.dispatcher.clone(),
                threshold_secs: d.threshold_secs,
                elapsed_secs,
            });
        }
    }
    (as_dispatcher, as_target)
}

/// #1694② silence-clock + #1516 state gate: should the dispatch-idle nudge be
/// SUPPRESSED for `target`? Suppresses when ANY of three snapshot-read signals
/// says the agent is working (fire only when NONE hold):
///
/// 1. **instantaneous working state** — `agent_state ∈ {thinking, tool_use}`: a
///    long LOCAL tool_use (e.g. a 9-min `Bash` run) produces NO pane marker and
///    NO MCP heartbeat, so its `silent_secs` climbs past the window even though it
///    is plainly working — the silence clock alone (#1775) false-fired on it.
///    This instantaneous state covers that gap. A genuinely HUNG thinking/tool_use
///    is NOT dispatch-idle's job: `health::productive_silence_exceeds` flags it
///    Hung at silent>600s and the hang_detector owns that recovery path, so
///    treating these as "working" here cannot hide a real stuck.
/// 2. **active-recovery exempt** — `agent_state == server_rate_limit` ONLY: a
///    rate-limited agent is in a bounded retry-backoff wait (#1696) whose
///    exhaustion (12 retries → #1744 escalation) is its own stuck-backstop, so
///    suppressing the idle nudge avoids noise in a legit wait without hiding a
///    real stuck (#1694 dialectic finding #4). `api_error` is deliberately NOT
///    exempt — it is a once-per-episode nudge with no retry-loop owning it and
///    no exhaustion signal, so dispatch-idle is the ONLY watchdog that surfaces
///    a wedged api_error agent (hang_detector misses it: no BlockedReason →
///    IdleLong, not Hung). Exempting it would silence a real stuck forever
///    (codex #1775 HIGH).
/// 3. **productive-silence gate** — `silent_secs < silence_threshold_secs`: the
///    agent has produced *productive* output (marker/heartbeat-gated, so a
///    spinner / junk / cursor-blink does NOT count) within the window, i.e. it is
///    making progress, just slow. This catches the #1516 momentary-not-thinking
///    case — an agent producing output whose snapshot state isn't thinking at the
///    scan instant. (It does NOT subsume condition 1: a long local tool_use
///    produces no pane/heartbeat output, so its `silent_secs` is high — hence the
///    instantaneous-state OR above.)
///
/// Pure for testability. Unknown target / missing snapshot → `false` (don't
/// suppress → fire; degrades to the pre-#1516 fail-open, never worse). The
/// `silent_secs` field itself fails open (large default) on an old-format
/// snapshot, so a missing field also fires rather than silently suppressing.
fn target_is_working(
    snapshot: Option<&crate::snapshot::FleetSnapshot>,
    target: &str,
    silence_threshold_secs: i64,
) -> bool {
    let snapshot_working = snapshot
        .and_then(|s| s.agents.iter().find(|a| a.name == target))
        .map(|a| {
            // Working = instantaneous-working state OR active-recovery OR
            // productive-silence. `thinking`/`tool_use` re-added (#1775's
            // silence-clock dropped them) because a long LOCAL tool_use — e.g. a
            // 9-min `Bash` run — emits NO pane marker and NO MCP heartbeat, so
            // `silent_secs` climbs past the window and the silence clock alone
            // false-fired on a plainly-working agent. A genuinely HUNG
            // `active` is NOT dispatch-idle's job: the hang_detector owns
            // it (`health::productive_silence_exceeds` → Hung at silent>600s).
            // active-recovery exempt = ONLY `server_rate_limit` (bounded retry +
            // #1744 exhaustion backstop); `api_error` stays NON-exempt (no
            // exhaustion signal owns it → dispatch-idle is its only watchdog,
            // codex #1775 HIGH). See the fn doc for the full rationale.
            matches!(a.agent_state.as_str(), "active" | "server_rate_limit")
                || a.silent_secs < silence_threshold_secs
                // #1961 phase-2 (4th OR, fail-toward-suppress): the pane
                // CONTENT changed within the window (raw screen-hash delta,
                // `output_silent_secs`) — an activity signal that does NOT
                // depend on state classification. The production false-fire
                // had agent_state="idle" (detector mis-read a code-writing
                // claude 3 scans straight), silent_secs=i64::MAX (productive
                // markers missed), heartbeat 734s — all three gates slipped
                // because all three sit on the same fragile classification;
                // a streaming/working pane keeps changing regardless.
                // Spinner-vs-hung: a spinner animating = the backend is still
                // running = dispatch-idle ("you've gone silent, status?")
                // correctly stays quiet; a genuinely HUNG agent is the
                // hang_detector's job (productive-silence), not this
                // watchdog's. Old-format snapshots default the field to MAX →
                // no suppression (fail-open to firing, same as silent_secs).
                || a.output_silent_secs < silence_threshold_secs
        })
        .unwrap_or(false);
    if snapshot_working {
        return true;
    }
    // #1866 (a) state-aware: ALSO suppress on the per-agent in-mem activity
    // timestamps (`heartbeat_pair`, zero-lock, no file read). The snapshot gate
    // above misses an agent that is engaged via the DAEMON but quiet on the pane:
    // - `heartbeat_at_ms` advances on the target's MCP activity (send / inbox /
    //   task / report …) — heads-down inter-agent work the pane doesn't mark.
    // - `last_input_at_ms` advances when the target is freshly handed input —
    //   someone is actively interacting, so a "you've gone silent?" nudge is noise.
    // Recency window = the dispatch threshold. Stale fields (0 / never set /
    // restart-reset) → a huge delta → NO suppress, so this only ever ADDS
    // suppression for provably-recent activity and never hides a real stuck: a
    // wedged agent makes no MCP calls and gets no input → both stale → still fires.
    let hb = crate::daemon::heartbeat_pair::snapshot_for(target);
    let now = crate::daemon::heartbeat_pair::now_ms();
    let window_ms = (silence_threshold_secs.max(0) as u64).saturating_mul(1000);
    now.saturating_sub(hb.heartbeat_at_ms) < window_ms
        || now.saturating_sub(hb.last_input_at_ms) < window_ms
}

/// #absorb-blocked: does `target` have an ACTIVE `waiting_on` — i.e. it called
/// `set_waiting_on(<condition>)` (declaring an intentional block/queue) and hasn't
/// cleared it? Read from the in-mem `heartbeat_pair` (`waiting_on_since_ms` is
/// `Some` while a condition is set, `None` after `set_waiting_on("")`), so this
/// adds NO registry lock and NO file read in the scan.
///
/// A blocked agent is intentionally waiting (e.g. on a dependency PR), so the
/// dispatch-idle "you've gone silent, status?" nudge is noise — absorb it. KISS:
/// any active `waiting_on` absorbs, with NO correlation to a specific dispatch (a
/// blocked agent is waiting regardless of which dispatch is overdue;
/// correlate-to-`correlation_id` is a deferred over-precision refinement).
///
/// FOLLOW-UP (in-mem-reset-on-restart class): `heartbeat_pair` is in-memory, so a
/// daemon restart zeroes `waiting_on_since_ms` → a still-blocked target is nudged
/// once post-restart until it re-sets `waiting_on`. The persistent source is the
/// instance metadata `waiting_on` field (written by `set_waiting_on`); a
/// boot-rehydrate (metadata → `heartbeat_pair`) would close this. Deferred: the
/// restart edge is transient + minor (the silence-clock `silent_secs` also resets
/// on restart), but it IS the known in-mem-reset class — do not forget.
fn target_has_active_waiting_on(target: &str) -> bool {
    crate::daemon::heartbeat_pair::snapshot_for(target)
        .waiting_on_since_ms
        .is_some()
}

/// Per-tick scan: flip eligible pending entries to `exceeded` and emit
/// the inbox event to the dispatcher. Exposed `pub(crate)` for tests.
pub(crate) fn scan_and_emit(home: &Path) {
    let now = chrono::Utc::now();
    // #1516: read the fleet snapshot ONCE (file-based: `<home>/snapshot.json`,
    // rewritten ~every tick) so the working-state gate below adds NO registry
    // lock and NO self-IPC-under-lock (#1492) risk. We deliberately read
    // snapshot.json, NOT state-transitions.jsonl (the latter misses some
    // transitions — see #1470-#4 — so it's an unreliable "current state").
    let snapshot = crate::snapshot::load(home);
    for d in list_pending(home) {
        if d.status != DispatchStatus::Pending {
            continue;
        }
        // #t-127: a correlated report already arrived (reviewer responded) but its
        // sidecar delete failed — the persisted `reported_at` latch survives that
        // failure. Don't fire a spurious "stuck" nudge; retry the delete and move on.
        if d.reported_at.is_some() {
            delete_sidecar_locked(home, &d.dispatch_id);
            continue;
        }
        let issued = match chrono::DateTime::parse_from_rfc3339(&d.issued_at) {
            Ok(t) => t.with_timezone(&chrono::Utc),
            Err(_) => continue,
        };
        let elapsed_secs = now.signed_duration_since(issued).num_seconds();
        if elapsed_secs <= d.threshold_secs {
            continue;
        }

        // #t-116/#78445-2: a backend quota / usage-limit hard-block (snapshot
        // `agent_state == "usage_limit"`) is EXPECTED to stay silent until the
        // quota resets (often hours/days) — re-nudging every threshold is pure
        // noise (r5: agy quota wedged 6 days, pinged every 30 min). Escalate ONCE
        // with a quota-specific "blocked on usage_limit, expected silent" event
        // (NOT the long-running "still showing activity" text — the target is NOT
        // active), then LATCH-suppress. Do NOT flip to Exceeded — the agent is
        // blocked, not stuck. #78445-2: `quota_escalated` is a DURABLE one-shot —
        // it is NOT cleared on a non-wedged tick, so a snapshot flicker can't
        // re-fire it (the observed same-heads-up-twice noise); a genuinely
        // recovered-but-idle dispatch instead falls through to the stuck path
        // below, and only a re-dispatch (new episode) resets the latch.
        if target_is_quota_wedged(snapshot.as_ref(), &d.target) {
            if !d.quota_escalated {
                tracing::info!(
                    target: "dispatch_idle",
                    dispatch_id = %d.dispatch_id,
                    target = %d.target,
                    "#t-116 target quota-wedged (usage_limit) — escalating once then suppressing"
                );
                emit_quota_wedged_event(home, &d, elapsed_secs);
                set_quota_escalated(home, &d.dispatch_id);
            }
            continue;
        }

        // #1516/#1694②: the dispatch-idle threshold is for "agent went silent
        // and never replied", but the idle timer only resets on a correlated
        // report — so a slow-but-progressing impl agent (heads-down coding /
        // generating, not sending updates) false-fired 5× the night #1516
        // landed. #1694② replaced the instantaneous Thinking/ToolUse check with
        // a productive-SILENCE clock (`silent_secs` from the snapshot) plus an
        // active-recovery exemption: suppress while the agent is producing
        // productive output within `threshold_secs` (making progress, just slow)
        // or is in an auto-retry state. A genuinely wedged agent stops producing
        // productive output → `silent_secs` climbs past the window → this gate
        // releases → the watchdog fires as designed. (The hang detector
        // independently catches infinite-gen / MCP-active-but-stuck.)
        // #absorb-blocked: ALSO suppress when the target has an active
        // `waiting_on` — it has declared an intentional block/queue (e.g. waiting
        // on a dependency PR), so a "you've gone silent, status?" nudge is pure
        // noise. (The N=3 false-positives this session: a blocked/queued target
        // reads as idle/silent — not "working" — so it flipped to Exceeded and got
        // nudged despite replying BUSY + blocked_on.) KISS: reuse the existing
        // `set_waiting_on` signal, no correlation to THIS dispatch — a blocked
        // agent is waiting regardless of which dispatch is overdue. Absorbing here
        // at the L1 source suppresses BOTH the dispatcher `..._exceeded` event AND
        // the L2 `..._nudge` to the target. Boundary: `set_waiting_on("")` clears
        // it → the gate releases → a genuinely-stuck-after-unblock target fires.
        if target_is_working(snapshot.as_ref(), &d.target, d.threshold_secs)
            || target_has_active_waiting_on(&d.target)
        {
            // #1658: the target is producing output (or intentionally blocked) —
            // reset the debounce streak so a later idle run starts fresh.
            if d.not_working_streak != 0 {
                set_not_working_streak(home, &d.dispatch_id, 0);
            }
            // #2008-p2: bound the auto-extension. Below the cap, refresh the
            // deadline (the existing activity-suppress). At the cap, escalate ONCE
            // with a DISTINCT "long-running — confirm expected" notice and STOP
            // refreshing — so a stuck-in-loop agent (pane churning → suppressed by
            // the gate) can't stay invisible forever, while a genuinely long task
            // is surfaced for one confirm, not nagged every ~2 min. Past the
            // cap+escalation it just suppresses (deadline frozen): if the target
            // later goes truly idle, the next scan falls through to the stuck-alarm
            // path below.
            if d.refresh_count < REFRESH_CAP {
                if let Some(corr) = d.correlation_id.as_deref() {
                    let _ = refresh_issued_at(home, corr, &d.target);
                }
                bump_refresh_count(home, &d.dispatch_id);
            } else if !d.long_running_escalated {
                emit_long_running_event(home, &d, elapsed_secs);
                set_long_running_escalated(home, &d.dispatch_id);
            }
            continue;
        }

        // #1018 (A): tick-time validation before firing. Stale sidecars
        // (placeholder correlation_id / deleted target instance / closed
        // task_id) are deleted silently — operator already received the
        // canonical signal via task board / instance lifecycle, no need
        // to surface a second-class "idle threshold" notification.
        if let Some(reason) = stale_sidecar_reason(home, &d) {
            // [M2] delete under the sidecar lock (no resurrection race vs a
            // concurrent team-nudge / L1 RMW; removes the correct `{id}.lock`).
            delete_sidecar_locked(home, &d.dispatch_id);
            tracing::debug!(
                target: "dispatch_idle",
                dispatch_id = %d.dispatch_id,
                target = %d.target,
                correlation_id = ?d.correlation_id,
                reason,
                "#1018 cleared stale sidecar at tick"
            );
            continue;
        }

        // #1658: debounce the #1516 instantaneous-state gate. A brief idle gap
        // during active heads-down work (or a momentarily-stale snapshot) that
        // lands on the threshold boundary would otherwise false-fire. Require
        // DEBOUNCE_SCANS consecutive not-working scans past threshold: persist
        // the growing streak and defer; the busy-branch above resets it the
        // moment the target produces output. A genuinely idle/stuck target keeps
        // accumulating → fires once the streak reaches the cap (≤ a couple
        // scan-cadences of extra delay vs the dispatch threshold).
        //
        // Only debounce when a snapshot EXISTS to judge work-state against —
        // debouncing snapshot noise is meaningless without one. No snapshot
        // (daemon boot, or tests) → fail-open to immediate fire, matching the
        // #1516 gate's own fail-open. In production the per-tick snapshot is
        // always present, so the debounce always applies there.
        if snapshot.is_some() {
            let streak = d.not_working_streak.saturating_add(1);
            if streak < DEBOUNCE_SCANS {
                set_not_working_streak(home, &d.dispatch_id, streak);
                continue;
            }
        }

        // #1340: flock + re-read to serialize against concurrent mark_resolved.
        // #1629: do the RMW (re-read → flip status → write) UNDER the flock, then
        // drop it and emit lock-free. emit_exceeded_event self-IPCs (notify_system
        // → enqueue_with_idle_hint → loopback api::call); it must never run while a
        // flock is held (#1617 lock-while-blocking class). The emit reads no mutated
        // field (status is not in the message), so flipping before emit is neutral.
        let to_emit: Option<PendingDispatch> = {
            let _lock =
                match crate::store::acquire_file_lock(&dispatch_lock_path(home, &d.dispatch_id)) {
                    Ok(l) => l,
                    Err(_) => continue,
                };
            let path = pending_path(home, &d.dispatch_id);
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let mut current: PendingDispatch = match serde_json::from_str(&content) {
                Ok(d) => d,
                Err(_) => continue,
            };
            if current.status != DispatchStatus::Pending {
                continue;
            }
            current.status = DispatchStatus::Exceeded;
            // #2031: stamp when the dispatcher was notified — L2 defers the agent
            // nudge to a second window past this (escalation tiering).
            current.exceeded_at = Some(now.to_rfc3339());
            if !write_dispatch(home, &current) {
                tracing::warn!(dispatch_id = %d.dispatch_id, "dispatch-idle exceeded status write failed");
            }
            Some(current)
        };
        if let Some(current) = to_emit {
            // #1961 phase-1 instrument (ZERO behavior change): the dispatch is
            // ABOUT TO FIRE — re-read the SAME three work-aware suppress signals
            // `target_is_working` consulted (snapshot agent_state + silent_secs,
            // and the heartbeat_pair deltas) and log them, so the next production
            // false-fire (heads-down agent nudged mid-work) shows WHICH of the
            // three signals slipped at fire time. Read-only: no gate, no
            // control-flow change — just one info line before the emit that was
            // already going to happen. The real fix waits on this evidence.
            let snap_agent = snapshot
                .as_ref()
                .and_then(|s| s.agents.iter().find(|a| a.name == d.target));
            let hb = crate::daemon::heartbeat_pair::snapshot_for(&d.target);
            let now_ms = crate::daemon::heartbeat_pair::now_ms();
            tracing::info!(
                tag = "#1961-fire-signals",
                dispatch_id = %d.dispatch_id,
                target = %d.target,
                correlation_id = ?d.correlation_id,
                elapsed_secs,
                threshold_secs = d.threshold_secs,
                not_working_streak = d.not_working_streak.saturating_add(1),
                agent_state = snap_agent
                    .map(|a| a.agent_state.as_str())
                    .unwrap_or("<no-snapshot>"),
                silent_secs = ?snap_agent.map(|a| a.silent_secs),
                output_silent_secs = ?snap_agent.map(|a| a.output_silent_secs),
                // #1961 phase-2 instrument fix: an unset pair field is 0, and
                // `now - 0` printed as an "age" reads like an absolute
                // epoch-ms timestamp (the phase-1 readout confusion). Print
                // None for never-set instead of a bogus huge age.
                heartbeat_age_ms = ?(hb.heartbeat_at_ms != 0)
                    .then(|| now_ms.saturating_sub(hb.heartbeat_at_ms)),
                last_input_age_ms = ?(hb.last_input_at_ms != 0)
                    .then(|| now_ms.saturating_sub(hb.last_input_at_ms)),
                "#1961: dispatch_idle firing — work-aware suppress signals at fire time (diagnose which slipped)"
            );
            emit_exceeded_event(home, &current, elapsed_secs);
        }
    }
}

/// #1018 (A): classify whether a sidecar is stale (eligible for
/// silent cleanup) at the watchdog tick. Returns `Some(reason)` for
/// stale; `None` for live (proceed with normal exceeded-event emit).
///
/// Three stale classes:
/// - placeholder correlation_id (lead-side hygiene bug — sidecars
///   sharing `t-pending` etc. across parallel dispatches)
/// - target instance no longer in fleet (deleted)
/// - correlation_id is a real task_id that's already done/cancelled
///
/// Fail-open semantics: any read/parse error in the lookup paths
/// treats the sidecar as live (preserves the existing behavior under
/// transient I/O glitches).
fn stale_sidecar_reason(home: &Path, d: &PendingDispatch) -> Option<&'static str> {
    if is_placeholder_correlation(d.correlation_id.as_deref()) {
        return Some("placeholder_correlation_id");
    }
    if !target_in_fleet(home, &d.target) {
        return Some("target_not_in_fleet");
    }
    // #1923 G2: the DISPATCHER left the fleet (deleted / redeployed) → the
    // pending-dispatch sidecar is an orphan whose idle nudge would route to a
    // ghost dispatcher (and `dispatcher_team` lookups silently no-op, so the
    // sidecar never self-cleans → infinite retry). `target_in_fleet` is a generic
    // "agent in fleet.yaml" check, reused here for the dispatcher.
    if !target_in_fleet(home, &d.dispatcher) {
        return Some("dispatcher_not_in_fleet");
    }
    if let Some(corr) = d.correlation_id.as_deref() {
        if let Some(false) = task_still_live(home, corr) {
            return Some("task_closed");
        }
    }
    None
}

/// #event-bus pattern #3: the threshold-exceeded notification text, built from
/// the exact fields carried by `EventKind::DispatchIdleExceeded`, so the legacy
/// direct enqueue and the bus subscriber produce a BYTE-IDENTICAL message
/// (overshoot is derived from elapsed - threshold).
#[allow(clippy::too_many_arguments)]
fn dispatch_idle_text(
    dispatch_id: &str,
    dispatcher: &str,
    target: &str,
    expected_kind: &str,
    correlation_id: Option<&str>,
    elapsed_secs: i64,
    threshold_secs: i64,
    long_running: bool,
    quota_wedged: bool,
) -> String {
    let corr = correlation_id.unwrap_or("");
    // #78445-2: the quota-wedge escalation — HONEST about the target being blocked
    // on its provider quota (usage_limit), NOT "still showing activity" (which the
    // long-running text below wrongly claimed for a usage_limit target).
    if quota_wedged {
        return format!(
            "[dispatch_idle_quota_wedged] dispatch {dispatch_id} from '{dispatcher}' → '{target}' \
             (kind={expected_kind}, correlation_id={corr}) has been unreplied for {elapsed_secs}s, and \
             '{target}' is currently QUOTA-WEDGED (backend usage_limit / provider quota hard-block).\n\n\
             This is NOT a stuck/silent alarm and NOT active work — '{target}' is blocked on its \
             provider quota and is EXPECTED to stay silent until the quota resets (often hours/days). \
             ONE heads-up (no repeats):\n\
             - Expected → no action; it resolves when the quota lifts and the report arrives.\n\
             - Can't wait → re-dispatch to a healthy same-role peer ('{target}' won't reply meanwhile).",
        );
    }
    // #2008-p2: the long-running escalation is DELIBERATELY worded to read nothing
    // like the stuck alarm — "active, just long, confirm" vs "went silent, may be
    // stuck/crashed" — so the dispatcher tells them apart at a glance.
    if long_running {
        return format!(
            "[dispatch_idle_long_running] dispatch {dispatch_id} from '{dispatcher}' → '{target}' \
             (kind={expected_kind}, correlation_id={corr}) has been ACTIVE but unreplied for {elapsed_secs}s \
             (past {cap}× the {threshold_secs}s threshold).\n\n\
             This is NOT a stuck/silent alarm — '{target}' is still showing activity (e.g. a long tool run / \
             heads-down work), just taking a while. The auto-extension cap was hit, so this is ONE heads-up \
             (no repeats):\n\
             - Long run EXPECTED → no action; it resolves when the report arrives.\n\
             - Looks wrong → pane-check. A genuinely-stuck target will later go silent and fire the normal \
             '…threshold_exceeded' alarm.",
            cap = REFRESH_CAP,
        );
    }
    let overshoot = elapsed_secs - threshold_secs;
    format!(
        "[dispatch_idle_threshold_exceeded] dispatch {dispatch_id} from '{dispatcher}' → '{target}' \
         (kind={expected_kind}, correlation_id={corr}) idle for {elapsed_secs}s \
         (threshold {threshold_secs}s, exceeded by {overshoot}s).\n\n\
         Action checklist:\n\
         1. Check target agent's pane — is it stuck or just slow?\n\
         2. If stuck → force release worktree + redispatch\n\
         3. If slow but progressing → extend patience\n\
         4. If crashed → restart agent, reassign task",
        dispatch_id = dispatch_id,
        dispatcher = dispatcher,
        target = target,
        expected_kind = expected_kind,
        corr = corr,
        elapsed_secs = elapsed_secs,
        threshold_secs = threshold_secs,
        overshoot = overshoot,
    )
}

/// #event-bus pattern #3: the actual delivery (notify_system to the dispatcher).
/// Shared by BOTH the legacy gate-off path and the bus subscriber, so the two are
/// identical by construction — the parity test proves the event carries enough.
// The arg list mirrors the notification's fields (the `DispatchIdleExceeded`
// payload); a one-use param struct just to satisfy the lint would not add clarity.
#[allow(clippy::too_many_arguments)]
fn deliver_dispatch_idle(
    home: &Path,
    dispatch_id: &str,
    dispatcher: &str,
    target: &str,
    expected_kind: &str,
    correlation_id: Option<&str>,
    elapsed_secs: i64,
    threshold_secs: i64,
    long_running: bool,
    quota_wedged: bool,
) {
    let text = dispatch_idle_text(
        dispatch_id,
        dispatcher,
        target,
        expected_kind,
        correlation_id,
        elapsed_secs,
        threshold_secs,
        long_running,
        quota_wedged,
    );
    // #947: fall back to dispatch_id when upstream correlation_id is None so the
    // nudge is always traceable to its source sidecar.
    let corr = correlation_id
        .map(String::from)
        .unwrap_or_else(|| dispatch_id.to_string());
    if let Err(e) = crate::inbox::notify_system(
        home,
        dispatcher,
        "system:dispatch_idle",
        // #2008-p2: a distinct subtype so the dispatcher's inbox tooling can tell a
        // "still active, just long" confirm from a "went silent" stuck alarm.
        if quota_wedged {
            "dispatch_idle_quota_wedged"
        } else if long_running {
            "dispatch_idle_long_running"
        } else {
            "dispatch_idle_threshold_exceeded"
        },
        text,
        Some(&corr),
        correlation_id,
    ) {
        tracing::warn!(error = %e, dispatcher, dispatch_id, "dispatch_idle: enqueue failed");
    }
}

/// #event-bus pattern #3: bus subscriber — deliver on a `DispatchIdleExceeded`
/// event (the gate-ON path). Registered once at daemon startup via [`register_subscriber`].
fn handle_event(event: &crate::daemon::event_bus::Event) -> bool {
    if let crate::daemon::event_bus::EventKind::DispatchIdleExceeded {
        dispatcher,
        target,
        elapsed_secs,
        dispatch_id,
        expected_kind,
        threshold_secs,
        correlation_id,
        long_running,
        quota_wedged,
    } = &event.kind
    {
        deliver_dispatch_idle(
            &event.home,
            dispatch_id,
            dispatcher,
            target,
            expected_kind,
            correlation_id.as_deref(),
            *elapsed_secs,
            *threshold_secs,
            *long_running,
            *quota_wedged,
        );
        true
    } else {
        false
    }
}

/// #event-bus pattern #3: register the dispatch_idle delivery subscriber on the
/// global bus. Call ONCE at daemon startup. Dormant while the bus is gate-off.
pub fn register_subscriber() {
    crate::daemon::event_bus::global().subscribe(handle_event);
}

fn emit_exceeded_event(home: &Path, d: &PendingDispatch, elapsed_secs: i64) {
    emit_dispatch_idle_event(home, d, elapsed_secs, false, false);
}

/// #2008-p2: the "long-running WITH ACTIVITY — confirm expected" escalation fired
/// once when the auto-extension cap is hit while the target is still active. Same
/// delivery shape (`long_running = true`); the subscriber renders the confirm
/// wording. Does NOT flip the sidecar to Exceeded — the target is working, not
/// stuck, so the stuck-alarm path still owns a later genuine idle.
fn emit_long_running_event(home: &Path, d: &PendingDispatch, elapsed_secs: i64) {
    emit_dispatch_idle_event(home, d, elapsed_secs, true, false);
}

/// #78445-2: the quota-wedge escalation — the target is blocked on its provider
/// quota (usage_limit), NOT active. A distinct honest message + the
/// `dispatch_idle_quota_wedged` tag, fired once per dispatch (durable one-shot).
fn emit_quota_wedged_event(home: &Path, d: &PendingDispatch, elapsed_secs: i64) {
    emit_dispatch_idle_event(home, d, elapsed_secs, false, true);
}

fn emit_dispatch_idle_event(
    home: &Path,
    d: &PendingDispatch,
    elapsed_secs: i64,
    long_running: bool,
    quota_wedged: bool,
) {
    // Observability log runs regardless of the gate (it is not the notification).
    crate::event_log::log(
        home,
        if quota_wedged {
            "dispatch_idle_quota_wedged"
        } else if long_running {
            "dispatch_idle_long_running"
        } else {
            "dispatch_idle_threshold_exceeded"
        },
        &d.dispatcher,
        &format!(
            "dispatch_id={} target={} corr={} elapsed_secs={} threshold_secs={} long_running={}",
            d.dispatch_id,
            d.target,
            d.correlation_id.as_deref().unwrap_or(""),
            elapsed_secs,
            d.threshold_secs,
            long_running,
        ),
    );
    // #event-bus Step 2 (legacy-zero): the bus is the sole delivery path. The
    // flock-drop-before-emit ordering (#1617) is preserved (scan caller unchanged).
    crate::daemon::event_bus::global().emit(
        home,
        crate::daemon::event_bus::EventKind::DispatchIdleExceeded {
            dispatcher: d.dispatcher.clone(),
            target: d.target.clone(),
            elapsed_secs,
            dispatch_id: d.dispatch_id.clone(),
            expected_kind: d.expected_kind.clone(),
            threshold_secs: d.threshold_secs,
            correlation_id: d.correlation_id.clone(),
            long_running,
            quota_wedged,
        },
    );
}

/// Per-loop scheduler state.
pub(crate) struct DispatchIdleTracker {
    /// Cadence gate — throttles scans to once per [`TICKS_PER_SCAN`]
    /// supervisor ticks (fire-on-Nth).
    gate: crate::daemon::cadence_gate::CadenceGate,
}

impl Default for DispatchIdleTracker {
    fn default() -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new_interval(TICKS_PER_SCAN),
        }
    }
}

impl DispatchIdleTracker {
    /// Per-tick entry. Increments the counter; on the throttled
    /// boundary, fires `scan_and_emit` and returns `true`. Returns
    /// `false` for all pre-boundary ticks.
    pub(crate) fn maybe_scan(&mut self, home: &Path) -> bool {
        if !self.gate.fire() {
            return false;
        }
        scan_and_emit(home);
        true
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::doc_lazy_continuation
)]
#[path = "tests.rs"]
mod tests;
