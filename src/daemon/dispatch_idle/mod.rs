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
        if let Some(mut existing) = list_pending(home).into_iter().find(|d| {
            matches!(d.status, DispatchStatus::Pending | DispatchStatus::Exceeded)
                && d.dispatcher == dispatcher
                && d.target == target
                && d.correlation_id.as_deref() == Some(corr)
        }) {
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
            if let Ok(body) = serde_json::to_string_pretty(&existing) {
                if crate::store::atomic_write(
                    &pending_path(home, &existing.dispatch_id),
                    body.as_bytes(),
                )
                .is_ok()
                {
                    return Some(existing.dispatch_id);
                }
            }
            // Refresh write failed → fall through to a fresh sidecar (best effort).
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
/// for a definitively closed task (`done`, `cancelled`, etc.);
/// `None` when the task can't be found at all (treat as live —
/// fail-open).
fn task_still_live(home: &Path, task_id: &str) -> Option<bool> {
    if task_id.is_empty() {
        return None;
    }
    // #1608b/#1614: event-sourced lookup, NOT a `tasks/{id}.json` probe — that
    // file is never written, so the old read always failed → this check was dead
    // (always `None`) and the #1018 "skip the nudge for a closed task" branch was
    // unreachable. `load_by_id` returns `None` on absent OR a transient replay
    // error → fail-open (treat as live), the existing safe semantics.
    let task = crate::tasks::load_by_id(home, task_id)?;
    let status = task.status.to_string();
    Some(LIVE_TASK_STATUSES.contains(&status.as_str()))
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
pub(crate) fn mark_resolved(home: &Path, correlation_id: &str) -> Option<String> {
    if correlation_id.is_empty() {
        return None;
    }
    // A report arriving = the dispatch is done, whether it was still `Pending` or
    // had already timed out to `Exceeded` (idle nudge fired). Match BOTH so a LATE
    // report on an Exceeded dispatch still deletes the sidecar.
    //
    // t-dispatchidle-clear-on-report: delete ALL sidecars with this
    // `correlation_id`, not just the first. A re-dispatched task (the SAME task_id
    // sent twice — e.g. an initial dispatch + a design re-confirm, both
    // `task_id=t-…`) creates DUPLICATE sidecars via `record_dispatch`'s fresh
    // `next_dispatch_id()`. The pre-fix `.find()` deleted only one duplicate; the
    // survivor went `Exceeded` and nudged the delegate AFTER it had already
    // reported (the clear-on-report noise). (`record_dispatch`'s new dedup stops
    // NEW duplicates; this delete-all keeps the resolve correct regardless.)
    let mut first_deleted: Option<String> = None;
    for d in list_pending(home).into_iter().filter(|d| {
        matches!(d.status, DispatchStatus::Pending | DispatchStatus::Exceeded)
            && d.correlation_id.as_deref() == Some(correlation_id)
    }) {
        // [M2] DELETE the sidecar (rather than flip to `Resolved` and leave the
        // file to accumulate — the pre-fix primary `pending-dispatches/` leak)
        // UNDER its lock, so a concurrent team-nudge / L1 RMW can't resurrect it.
        if delete_sidecar_locked(home, &d.dispatch_id) {
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
pub(crate) fn refresh_issued_at(home: &Path, correlation_id: &str) -> Option<String> {
    if correlation_id.is_empty() {
        return None;
    }
    let matched = list_pending(home).into_iter().find(|d| {
        d.status == DispatchStatus::Pending && d.correlation_id.as_deref() == Some(correlation_id)
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
            // thinking/tool_use is NOT dispatch-idle's job: the hang_detector owns
            // it (`health::productive_silence_exceeds` → Hung at silent>600s).
            // active-recovery exempt = ONLY `server_rate_limit` (bounded retry +
            // #1744 exhaustion backstop); `api_error` stays NON-exempt (no
            // exhaustion signal owns it → dispatch-idle is its only watchdog,
            // codex #1775 HIGH). See the fn doc for the full rationale.
            matches!(
                a.agent_state.as_str(),
                "thinking" | "tool_use" | "server_rate_limit"
            ) || a.silent_secs < silence_threshold_secs
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
        let issued = match chrono::DateTime::parse_from_rfc3339(&d.issued_at) {
            Ok(t) => t.with_timezone(&chrono::Utc),
            Err(_) => continue,
        };
        let elapsed_secs = now.signed_duration_since(issued).num_seconds();
        if elapsed_secs <= d.threshold_secs {
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
                    let _ = refresh_issued_at(home, corr);
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
) -> String {
    let corr = correlation_id.unwrap_or("");
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
        if long_running {
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
    emit_dispatch_idle_event(home, d, elapsed_secs, false);
}

/// #2008-p2: the "long-running WITH ACTIVITY — confirm expected" escalation fired
/// once when the auto-extension cap is hit while the target is still active. Same
/// delivery shape (`long_running = true`); the subscriber renders the confirm
/// wording. Does NOT flip the sidecar to Exceeded — the target is working, not
/// stuck, so the stuck-alarm path still owns a later genuine idle.
fn emit_long_running_event(home: &Path, d: &PendingDispatch, elapsed_secs: i64) {
    emit_dispatch_idle_event(home, d, elapsed_secs, true);
}

fn emit_dispatch_idle_event(
    home: &Path,
    d: &PendingDispatch,
    elapsed_secs: i64,
    long_running: bool,
) {
    // Observability log runs regardless of the gate (it is not the notification).
    crate::event_log::log(
        home,
        if long_running {
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
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// #1636: the `DispatchStatus` enum MUST serialize to / deserialize from the
    /// exact lowercase wire strings the prior stringly-typed field used, so
    /// existing on-disk sidecars + IPC payloads stay byte-compatible.
    #[test]
    fn dispatch_status_serde_roundtrip() {
        for (variant, wire) in [
            (DispatchStatus::Pending, "\"pending\""),
            (DispatchStatus::Resolved, "\"resolved\""),
            (DispatchStatus::Exceeded, "\"exceeded\""),
            (DispatchStatus::Cancelled, "\"cancelled\""),
        ] {
            // enum → string matches the legacy wire form
            assert_eq!(serde_json::to_string(&variant).unwrap(), wire);
            // string → enum (legacy on-disk values still load)
            assert_eq!(
                serde_json::from_str::<DispatchStatus>(wire).unwrap(),
                variant
            );
        }
        // `#[serde(default)]` on the field → a sidecar JSON with no `status`
        // key loads as Pending, matching the old `default_status` fn.
        let no_status = r#"{"dispatch_id":"d1","dispatcher":"a","target":"b"}"#;
        let d: PendingDispatch = serde_json::from_str(no_status).unwrap();
        assert_eq!(d.status, DispatchStatus::Pending);
        // A full sidecar round-trips with the status as a lowercase string.
        let s = serde_json::to_string(&d).unwrap();
        assert!(
            s.contains("\"status\":\"pending\""),
            "status must serialize as the lowercase wire string, got: {s}"
        );
        // An unknown status string fails to deserialize (strict, like the
        // pr_state enums) — list_pending's fail-open loader then skips it.
        assert!(serde_json::from_str::<DispatchStatus>("\"bogus\"").is_err());
    }

    fn tmp_home(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-dispatch-idle-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    /// Write a backdated pending sidecar directly (bypasses
    /// `record_dispatch`) so timeout scenarios don't require sleeping.
    fn write_pending_at(
        home: &Path,
        dispatcher: &str,
        target: &str,
        correlation_id: Option<&str>,
        expected_kind: &str,
        threshold_secs: i64,
        issued_at: chrono::DateTime<chrono::Utc>,
    ) -> String {
        let dir = pending_dir(home);
        std::fs::create_dir_all(&dir).unwrap();
        let id = next_dispatch_id();
        let payload = PendingDispatch {
            schema_version: SCHEMA_VERSION,
            dispatch_id: id.clone(),
            dispatcher: dispatcher.to_string(),
            target: target.to_string(),
            correlation_id: correlation_id.map(String::from),
            expected_kind: expected_kind.to_string(),
            threshold_secs,
            issued_at: issued_at.to_rfc3339(),
            status: DispatchStatus::Pending,
            nudge_sent_at: None,
            not_working_streak: 0,
            refresh_count: 0,
            long_running_escalated: false,
            exceeded_at: None,
        };
        std::fs::write(
            pending_path(home, &id),
            serde_json::to_string_pretty(&payload).unwrap(),
        )
        .unwrap();
        id
    }

    /// t-dispatchidle-clear-on-report (1): a report clears EVERY sidecar with the
    /// matching correlation_id, not just the first — so a duplicate left by a
    /// re-dispatch can't survive to nudge after the report.
    #[test]
    fn mark_resolved_deletes_all_duplicate_sidecars_clearonreport() {
        let home = tmp_home("resolve-all-dups");
        let now = chrono::Utc::now();
        // Two sidecars, SAME correlation_id (the re-dispatch duplicate case).
        let dup_a = write_pending_at(&home, "lead", "dev", Some("t-dup"), "task", 600, now);
        let dup_b = write_pending_at(&home, "lead", "dev", Some("t-dup"), "task", 600, now);
        // An unrelated sidecar that must survive.
        let other = write_pending_at(&home, "lead", "dev", Some("t-other"), "task", 600, now);

        let resolved = mark_resolved(&home, "t-dup");
        assert!(resolved.is_some(), "must report a deletion");

        let pending = list_pending(&home);
        assert!(
            !pending
                .iter()
                .any(|p| p.dispatch_id == dup_a || p.dispatch_id == dup_b),
            "BOTH duplicate sidecars for t-dup must be deleted"
        );
        assert!(
            pending.iter().any(|p| p.dispatch_id == other),
            "the unrelated correlation's sidecar must survive"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// t-dispatchidle-clear-on-report (2): re-dispatching the SAME task
    /// (dispatcher, target, correlation_id) refreshes the existing sidecar in
    /// place instead of creating a duplicate.
    #[test]
    fn record_dispatch_dedups_redispatch_by_key_clearonreport() {
        let home = tmp_home("record-dedup");
        let first = record_dispatch(&home, "lead", "dev", Some("t-redispatch"), "task", 600);
        let second = record_dispatch(&home, "lead", "dev", Some("t-redispatch"), "task", 600);
        assert!(first.is_some() && second.is_some());
        assert_eq!(
            first, second,
            "re-dispatch must REFRESH the same sidecar (same dispatch_id), not create a new one"
        );
        let pending = list_pending(&home);
        let dups = pending
            .iter()
            .filter(|p| p.correlation_id.as_deref() == Some("t-redispatch"))
            .count();
        assert_eq!(
            dups, 1,
            "exactly ONE sidecar for the re-dispatched correlation"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// §3.9 #1916: a task REASSIGN (OwnerAssigned A→B) retargets the dispatch-idle
    /// sidecar to B AND resets its idle clock — so the watchdog nudges B (the new
    /// owner), not A, and B is not immediately nudged for a task it just received
    /// (the #1866 principle: nudge reflects the current owner's idle time).
    #[test]
    fn reassign_retargets_sidecar_to_new_owner_and_resets_clock_1916() {
        let home = tmp_home("1916-retarget");
        // A's sidecar is already near-threshold (590s of a 600s window).
        let aged = chrono::Utc::now() - chrono::Duration::seconds(590);
        write_pending_at(
            &home,
            "lead",
            "agent-a",
            Some("t-reassign"),
            "task",
            600,
            aged,
        );

        let moved = reassign_pending_for_task(&home, "t-reassign", Some("agent-b"));
        assert_eq!(moved, 1, "exactly one sidecar retargeted");

        let pending = list_pending(&home);
        let s = pending
            .iter()
            .find(|p| p.correlation_id.as_deref() == Some("t-reassign"))
            .expect("#1916: sidecar must SURVIVE a reassign (retargeted, not deleted)");
        assert_eq!(
            s.target, "agent-b",
            "#1916: sidecar must target the reassigned owner B, not the former owner A"
        );
        assert_eq!(
            s.status,
            DispatchStatus::Pending,
            "#1916: revived to Pending so B gets a fresh window"
        );
        assert_eq!(s.not_working_streak, 0, "#1916: debounce streak reset");
        let issued = chrono::DateTime::parse_from_rfc3339(&s.issued_at)
            .expect("issued_at rfc3339")
            .with_timezone(&chrono::Utc);
        assert!(
            chrono::Utc::now().signed_duration_since(issued).num_seconds() < 60,
            "#1916: idle clock RESET on reassign — B must not inherit A's near-threshold age (#1866)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// §3.9 #1916: an ORPHAN (OwnerAssigned with owner=None — #1903 disband/delete
    /// orphan) CLEARS the sidecar — there is no owner to nudge. It must NOT leave a
    /// sidecar with target=None (which would nudge nobody / a placeholder forever).
    #[test]
    fn reassign_none_clears_orphaned_sidecar_1916() {
        let home = tmp_home("1916-orphan");
        record_dispatch(&home, "lead", "agent-a", Some("t-orphan"), "task", 600)
            .expect("dispatch recorded");

        let cleared = reassign_pending_for_task(&home, "t-orphan", None);
        assert_eq!(cleared, 1, "orphan (owner=None) clears the sidecar");
        assert!(
            list_pending(&home)
                .iter()
                .all(|p| p.correlation_id.as_deref() != Some("t-orphan")),
            "#1916: orphaned task's sidecar must be removed — nobody to nudge (never target=None)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// §3.9 #1866 (b) clear-on-handoff: re-dispatching the SAME (dispatcher,
    /// target) to a NEW task (different correlation_id) retires the older still-
    /// armed sidecar (the #1861 stale-handoff false-nudge), but must NOT clobber
    /// a different dispatcher's parallel dispatch or a newer sidecar.
    #[test]
    fn record_dispatch_retires_stale_handoff_sidecar_1866() {
        let home = tmp_home("retire-handoff");
        let now = chrono::Utc::now();
        let older = now - chrono::Duration::seconds(700);
        // OLD dispatch (task A) lead→dev — still Pending (the stale handoff).
        let old_a = write_pending_at(&home, "lead", "dev", Some("t-A"), "task", 600, older);
        // A DIFFERENT dispatcher's parallel dispatch to dev — must survive.
        let parallel = write_pending_at(&home, "lead2", "dev", Some("t-A2"), "task", 600, older);
        // A NEWER lead→dev sidecar (issued after the re-dispatch) — must survive
        // (the "strictly older" boundary).
        let newer = write_pending_at(
            &home,
            "lead",
            "dev",
            Some("t-future"),
            "task",
            600,
            now + chrono::Duration::seconds(60),
        );

        // Re-dispatch dev to a NEW task B via the real entry point.
        let new_b = record_dispatch(&home, "lead", "dev", Some("t-B"), "task", 600)
            .expect("new dispatch recorded");

        let ids: Vec<String> = list_pending(&home)
            .into_iter()
            .map(|d| d.dispatch_id)
            .collect();
        assert!(
            !ids.contains(&old_a),
            "#1866 (b): the stale same-(dispatcher,target) older sidecar (task A) must be retired"
        );
        assert!(ids.contains(&new_b), "the new dispatch sidecar must exist");
        assert!(
            ids.contains(&parallel),
            "#1866 (b): a DIFFERENT dispatcher's dispatch must NOT be retired"
        );
        assert!(
            ids.contains(&newer),
            "#1866 (b): a NEWER sidecar must NOT be retired (older-only boundary)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// §3.9 #1866 (a) state-aware: an overdue dispatch whose target has RECENT
    /// in-mem activity (heartbeat_at_ms advanced) is SUPPRESSED — past the
    /// wall-clock threshold it stays Pending instead of firing.
    #[test]
    fn scan_suppresses_on_recent_heartbeat_1866() {
        let home = tmp_home("stateaware-hb");
        // Unique target → isolated process-global heartbeat_pair entry.
        let target = "dev-1866-hb";
        let id = write_pending_at(
            &home,
            "lead",
            target,
            Some("t-hb"),
            "task",
            600,
            chrono::Utc::now() - chrono::Duration::seconds(700),
        );
        // Target made MCP activity just now (heads-down inter-agent work).
        crate::daemon::heartbeat_pair::update_with(target, |p| {
            p.heartbeat_at_ms = crate::daemon::heartbeat_pair::now_ms();
        });

        scan_and_emit(&home);

        let d = list_pending(&home)
            .into_iter()
            .find(|d| d.dispatch_id == id)
            .expect("sidecar must still exist (suppressed, not swept)");
        assert_eq!(
            d.status,
            DispatchStatus::Pending,
            "#1866 (a): recent heartbeat must suppress the nudge despite the wall-clock threshold"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// §3.9 #1866 (a) the OTHER half: a fully-idle target (no recent heartbeat /
    /// input, no pane activity) past threshold STILL fires — the new signals only
    /// ADD suppression for provably-recent activity, never hide a real stuck.
    #[test]
    fn scan_still_fires_when_target_fully_idle_1866() {
        let home = tmp_home("stateaware-idle");
        let target = "dev-1866-idle"; // unique → stale (0) heartbeat_pair
                                      // Live fleet + task so the sidecar isn't swept as stale before it fires.
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            // #1923 G2: seed the dispatcher (`lead`) too — the new
            // dispatcher-in-fleet stale check requires it (prod always has it).
            format!("instances:\n  lead:\n    backend: claude\n  {target}:\n    backend: claude\n"),
        )
        .unwrap();
        let task_id = "t-idle-99";
        let tasks_dir = home.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join(format!("{task_id}.json")),
            serde_json::to_string_pretty(&serde_json::json!({
                "id": task_id, "status": "in_progress", "title": "w", "assignee": target
            }))
            .unwrap(),
        )
        .unwrap();
        let id = write_pending_at(
            &home,
            "lead",
            target,
            Some(task_id),
            "task",
            600,
            chrono::Utc::now() - chrono::Duration::seconds(700),
        );
        // NO heartbeat / input set → all activity signals stale → truly idle.

        scan_and_emit(&home);

        let d = list_pending(&home)
            .into_iter()
            .find(|d| d.dispatch_id == id)
            .expect("sidecar present");
        assert_eq!(
            d.status,
            DispatchStatus::Exceeded,
            "#1866 (a): a fully-idle target (all activity signals stale) must STILL fire"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #2031: L1 must STAMP `exceeded_at` when it flips a sidecar to `Exceeded`.
    /// This is the signal L2's second-window tiering reads — if L1 stopped
    /// stamping, L2 would fail-open to an immediate nudge and silently regress the
    /// tiering, so pin it explicitly.
    #[test]
    fn scan_and_emit_stamps_exceeded_at_2031() {
        let home = tmp_home("2031-l1-stamp");
        let target = "dev-2031-stamp";
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            format!("instances:\n  lead:\n    backend: claude\n  {target}:\n    backend: claude\n"),
        )
        .unwrap();
        let task_id = "t-stamp-2031";
        let tasks_dir = home.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join(format!("{task_id}.json")),
            serde_json::to_string_pretty(&serde_json::json!({
                "id": task_id, "status": "in_progress", "title": "w", "assignee": target
            }))
            .unwrap(),
        )
        .unwrap();
        let id = write_pending_at(
            &home,
            "lead",
            target,
            Some(task_id),
            "task",
            600,
            chrono::Utc::now() - chrono::Duration::seconds(700),
        );
        // No snapshot → debounce fails open → fires on this single scan.
        scan_and_emit(&home);

        let d = list_pending(&home)
            .into_iter()
            .find(|d| d.dispatch_id == id)
            .expect("sidecar present");
        assert_eq!(d.status, DispatchStatus::Exceeded, "precondition: fired");
        assert!(
            d.exceeded_at.is_some(),
            "#2031: L1 must stamp exceeded_at on the Exceeded transition (L2 tiering depends on it)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// §3.9 #2008-p2: below the auto-extension cap, an ACTIVE target's deadline is
    /// extended (refresh_count++) with NO alarm of any kind — the existing
    /// activity-suppress, now counted toward the cap.
    #[test]
    fn below_cap_extends_active_target_without_alarm() {
        let home = tmp_home("p2-below-cap");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
        let id = write_pending_at(&home, "lead", "dev", Some("t-below"), "task", 600, issued);
        write_target_snapshot(&home, "dev", "tool_use"); // target_is_working

        scan_and_emit(&home);

        let d = list_pending(&home)
            .into_iter()
            .find(|d| d.dispatch_id == id)
            .expect("sidecar");
        assert_eq!(d.refresh_count, 1, "one activity-based extension counted");
        assert!(!d.long_running_escalated);
        assert_eq!(
            d.status,
            DispatchStatus::Pending,
            "still pending, not fired"
        );
        let elog = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
        assert!(
            !elog.contains("dispatch_idle_long_running")
                && !elog.contains("dispatch_idle_threshold_exceeded"),
            "no alarm of any kind while extending an active target: {elog}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// §3.9 #2008-p2: at the cap, a still-ACTIVE target gets ONE "long-running —
    /// confirm expected" escalation (latched) — NOT the stuck/Exceeded alarm, and
    /// NOT repeated on the next scan (escalate-don't-repeat).
    #[test]
    fn cap_reached_escalates_long_running_once_then_latches() {
        let home = tmp_home("p2-cap-escalate");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
        let id = write_pending_at(&home, "lead", "dev", Some("t-cap"), "task", 600, issued);
        for _ in 0..REFRESH_CAP {
            bump_refresh_count(&home, &id); // already AT the extension cap
        }
        write_target_snapshot(&home, "dev", "tool_use"); // still working

        scan_and_emit(&home);

        let d = list_pending(&home)
            .into_iter()
            .find(|d| d.dispatch_id == id)
            .expect("sidecar");
        assert!(
            d.long_running_escalated,
            "cap → the escalate-once latch is set"
        );
        assert_eq!(
            d.status,
            DispatchStatus::Pending,
            "long-running is NOT the stuck/Exceeded path — the target is working"
        );
        let elog = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
        assert_eq!(
            elog.matches("dispatch_idle_long_running").count(),
            1,
            "exactly one long-running escalation: {elog}"
        );
        assert!(
            !elog.contains("dispatch_idle_threshold_exceeded"),
            "no stuck alarm for a working target: {elog}"
        );

        // A second scan must NOT re-escalate (latched).
        scan_and_emit(&home);
        let elog2 = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
        assert_eq!(
            elog2.matches("dispatch_idle_long_running").count(),
            1,
            "escalate-don't-repeat: still exactly one after a second scan: {elog2}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// §3.9 #2008-p2 (codex review): a re-dispatch of the SAME correlation is a NEW
    /// episode — the in-place refresh must reset BOTH the extension cap counter and
    /// the escalation latch, or the reborn dispatch inherits a stale
    /// "already long-running, don't protect" state and is silently unguarded.
    #[test]
    fn redispatch_same_correlation_resets_cap_and_latch() {
        let home = tmp_home("p2-redispatch-reset");
        let id =
            record_dispatch(&home, "lead", "dev", Some("t-redisp"), "task", 600).expect("first");
        // Drive it to the latched, capped state (as a long-running escalation does).
        for _ in 0..REFRESH_CAP {
            bump_refresh_count(&home, &id);
        }
        set_long_running_escalated(&home, &id);
        let before = list_pending(&home)
            .into_iter()
            .find(|d| d.dispatch_id == id)
            .expect("sidecar");
        assert!(
            before.refresh_count >= REFRESH_CAP && before.long_running_escalated,
            "precondition: capped + latched"
        );

        // Re-dispatch the SAME correlation → in-place refresh.
        let id2 = record_dispatch(&home, "lead", "dev", Some("t-redisp"), "task", 600)
            .expect("redispatch");
        assert_eq!(id2, id, "same correlation refreshes in place (one sidecar)");

        let after = list_pending(&home)
            .into_iter()
            .find(|d| d.dispatch_id == id)
            .expect("sidecar");
        assert_eq!(
            after.refresh_count, 0,
            "re-dispatch resets the extension cap counter"
        );
        assert!(
            !after.long_running_escalated,
            "re-dispatch clears the escalation latch (fresh episode is protected again)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// §3.9 #1961 (instrument-only): when a dispatch fires, the `#1961-fire-signals`
    /// diagnostic is emitted at the fire point (so production can see which
    /// work-aware suppress signal slipped) WHILE the fire behavior is byte-identical
    /// (the dispatch still flips to Exceeded). Drops the instrument → the log
    /// assertion fails; changes a gate → the Exceeded assertion fails.
    #[test]
    #[tracing_test::traced_test]
    fn fire_signals_instrumented_zero_behavior_1961() {
        let home = tmp_home("1961-instrument");
        let target = "dev-1961-idle"; // unique → stale heartbeat_pair → idle
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            format!("instances:\n  lead:\n    backend: claude\n  {target}:\n    backend: claude\n"),
        )
        .unwrap();
        let task_id = "t-idle-1961";
        let tasks_dir = home.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join(format!("{task_id}.json")),
            serde_json::to_string_pretty(&serde_json::json!({
                "id": task_id, "status": "in_progress", "title": "w", "assignee": target
            }))
            .unwrap(),
        )
        .unwrap();
        let id = write_pending_at(
            &home,
            "lead",
            target,
            Some(task_id),
            "task",
            600,
            chrono::Utc::now() - chrono::Duration::seconds(700),
        );

        scan_and_emit(&home);

        // Behavior unchanged: the dispatch still fires (flips to Exceeded).
        let d = list_pending(&home)
            .into_iter()
            .find(|d| d.dispatch_id == id)
            .expect("sidecar present");
        assert_eq!(
            d.status,
            DispatchStatus::Exceeded,
            "#1961: the instrument must NOT change the fire behavior"
        );
        // Instrument live: the fire-signals diagnostic is emitted at the fire point.
        assert!(
            logs_contain("#1961-fire-signals"),
            "#1961: the fire-signals diagnostic must be logged when a dispatch fires"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// 1. Throttle contract — TICKS_PER_SCAN-1 calls return false, the
    /// next fires (returns true), and the counter resets.
    #[test]
    fn tracker_throttles_to_tick_per_scan() {
        let home = tmp_home("throttle");
        let mut tracker = DispatchIdleTracker::default();
        for i in 0..(TICKS_PER_SCAN - 1) {
            assert!(
                !tracker.maybe_scan(&home),
                "tick {i} (pre-throttle) must return false"
            );
        }
        assert!(
            tracker.maybe_scan(&home),
            "{}th tick must fire scan and return true",
            TICKS_PER_SCAN
        );
        assert!(
            !tracker.maybe_scan(&home),
            "post-fire tick must reset counter and return false"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// 2. `record_dispatch` writes a sidecar that `list_pending` can
    /// round-trip.
    #[test]
    fn record_and_list_pending_dispatch() {
        let home = tmp_home("record");
        let id = record_dispatch(&home, "lead", "reviewer", Some("t-abc"), "task", 600)
            .expect("record must return id");
        let pending = list_pending(&home);
        assert_eq!(pending.len(), 1);
        let p = &pending[0];
        assert_eq!(p.dispatch_id, id);
        assert_eq!(p.dispatcher, "lead");
        assert_eq!(p.target, "reviewer");
        assert_eq!(p.correlation_id.as_deref(), Some("t-abc"));
        assert_eq!(p.expected_kind, "task");
        assert_eq!(p.threshold_secs, 600);
        assert_eq!(p.status, DispatchStatus::Pending);
        std::fs::remove_dir_all(&home).ok();
    }

    /// 3. `scan_and_emit` flips exceeded entries and emits an inbox
    /// event to the dispatcher.
    #[test]
    fn fires_on_threshold_exceeded() {
        let home = tmp_home("fires");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
        let id = write_pending_at(&home, "alpha", "beta", Some("t-fires"), "task", 600, issued);
        scan_and_emit(&home);
        let inbox = crate::inbox::drain(&home, "alpha");
        assert!(
            inbox.iter().any(
                |m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded")
                    && m.correlation_id.as_deref() == Some("t-fires")
            ),
            "must emit dispatch_idle_threshold_exceeded event to dispatcher's inbox: {inbox:?}"
        );
        let pending = list_pending(&home);
        let p = pending.iter().find(|p| p.dispatch_id == id).unwrap();
        assert_eq!(
            p.status,
            DispatchStatus::Exceeded,
            "sidecar must flip pending→exceeded"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1658 helper: write a fleet snapshot setting `target`'s agent_state
    /// (reuses [`mk_agent_snapshot`]).
    fn write_target_snapshot(home: &std::path::Path, target: &str, state: &str) {
        crate::snapshot::save(home, &[mk_agent_snapshot(target, state)]);
    }

    /// #1658: with a snapshot showing the target NOT working, the signal
    /// debounces — it requires DEBOUNCE_SCANS consecutive not-working scans past
    /// threshold before firing (filters the #1516 instantaneous gate's
    /// false-fire on a brief idle gap during active heads-down work).
    #[test]
    fn debounce_idle_requires_consecutive_scans_1658() {
        let home = tmp_home("debounce-idle");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
        let id = write_pending_at(&home, "lead", "dev", Some("t-deb"), "task", 600, issued);
        write_target_snapshot(&home, "dev", "idle");

        // The first DEBOUNCE_SCANS-1 scans defer: no event, stays Pending, streak grows.
        for i in 1..DEBOUNCE_SCANS {
            scan_and_emit(&home);
            let p = list_pending(&home)
                .into_iter()
                .find(|p| p.dispatch_id == id)
                .unwrap();
            assert_eq!(p.status, DispatchStatus::Pending, "scan {i}: must defer");
            assert_eq!(p.not_working_streak, i, "scan {i}: streak must grow");
            assert!(
                crate::inbox::drain(&home, "lead").is_empty(),
                "scan {i}: must NOT emit yet"
            );
        }
        // The DEBOUNCE_SCANS-th consecutive not-working scan fires once.
        scan_and_emit(&home);
        let p = list_pending(&home)
            .into_iter()
            .find(|p| p.dispatch_id == id)
            .unwrap();
        assert_eq!(
            p.status,
            DispatchStatus::Exceeded,
            "the DEBOUNCE_SCANS-th idle scan must fire"
        );
        assert!(
            crate::inbox::drain(&home, "lead")
                .iter()
                .any(|m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded")),
            "the firing scan must emit the dispatcher event"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1658: observing the target working resets the debounce streak, so a
    /// momentary idle blip never accumulates to a false-fire.
    #[test]
    fn debounce_resets_streak_when_working_1658() {
        let home = tmp_home("debounce-reset");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
        let id = write_pending_at(&home, "lead", "dev", Some("t-rst"), "task", 600, issued);

        // One idle scan → streak 1, deferred.
        write_target_snapshot(&home, "dev", "idle");
        scan_and_emit(&home);
        let p = list_pending(&home)
            .into_iter()
            .find(|p| p.dispatch_id == id)
            .unwrap();
        assert_eq!(p.not_working_streak, 1);
        assert_eq!(p.status, DispatchStatus::Pending);

        // Target resumes working → streak resets to 0, still no fire.
        write_target_snapshot(&home, "dev", "tool_use");
        scan_and_emit(&home);
        let p = list_pending(&home)
            .into_iter()
            .find(|p| p.dispatch_id == id)
            .unwrap();
        assert_eq!(p.not_working_streak, 0, "working must reset the streak");
        assert_eq!(p.status, DispatchStatus::Pending);
        assert!(crate::inbox::drain(&home, "lead").is_empty());
        std::fs::remove_dir_all(&home).ok();
    }

    /// 4. Load-bearing contract: `mark_resolved` keys on
    /// `correlation_id`, NOT on `dispatcher`. Decision_timeout's
    /// sender-keyed semantic would resolve the wrong sidecar when a
    /// single dispatcher has multiple in-flight dispatches.
    #[test]
    fn mark_resolved_keys_on_correlation_id_not_sender() {
        let home = tmp_home("resolve-by-corr");
        let now = chrono::Utc::now();
        let id_a = write_pending_at(&home, "lead", "dev-1", Some("t-aaa"), "task", 600, now);
        let id_b = write_pending_at(&home, "lead", "dev-2", Some("t-bbb"), "task", 600, now);
        let resolved = mark_resolved(&home, "t-aaa");
        assert_eq!(
            resolved.as_deref(),
            Some(id_a.as_str()),
            "must resolve the correlation_id-matching sidecar, not sender-matching"
        );
        let pending = list_pending(&home);
        // A: the matched sidecar is DELETED on resolve (no longer flipped to
        // Resolved + left behind), so it must be absent from list_pending.
        assert!(
            !pending.iter().any(|p| p.dispatch_id == id_a),
            "matched sidecar must be deleted on resolve"
        );
        let p_b = pending.iter().find(|p| p.dispatch_id == id_b).unwrap();
        assert_eq!(
            p_b.status,
            DispatchStatus::Pending,
            "unmatched sidecar from same dispatcher must NOT be touched"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// A: `cleanup_pending_for_instance` deletes EVERY sidecar for the instance,
    /// including non-Pending (Exceeded/Resolved) ones — previously it skipped
    /// them, leaving resolved/exceeded sidecars to accumulate.
    #[test]
    fn cleanup_pending_deletes_non_pending_sidecars() {
        let home = tmp_home("cleanup-non-pending");
        let now = chrono::Utc::now();
        let id = write_pending_at(&home, "lead", "gone-agent", Some("t-x"), "task", 600, now);
        // Flip the sidecar to a terminal (non-Pending) status on disk.
        let path = pending_path(&home, &id);
        let mut pd: PendingDispatch =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        pd.status = DispatchStatus::Exceeded;
        std::fs::write(&path, serde_json::to_string_pretty(&pd).unwrap()).unwrap();

        let removed = cleanup_pending_for_instance(&home, "gone-agent");
        assert_eq!(removed, 1, "must delete the non-Pending (Exceeded) sidecar");
        assert!(!path.exists(), "Exceeded sidecar must be removed");
        std::fs::remove_dir_all(&home).ok();
    }

    /// codex probe #1 regression: a LATE report on a dispatch that already timed
    /// out (Pending → Exceeded, idle nudge fired) must STILL delete the sidecar.
    /// Pre-fix `mark_resolved` matched only `Pending`, so the Exceeded sidecar
    /// leaked until the slow retention / terminal-sweep path.
    #[test]
    fn mark_resolved_clears_exceeded_sidecar() {
        let home = tmp_home("resolve-exceeded");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
        let id = write_pending_at(&home, "lead", "dev", Some("t-late"), "task", 600, issued);
        // Flip to Exceeded on disk, as the idle scan would once the threshold passes.
        let path = pending_path(&home, &id);
        let mut pd: PendingDispatch =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        pd.status = DispatchStatus::Exceeded;
        std::fs::write(&path, serde_json::to_string_pretty(&pd).unwrap()).unwrap();

        let resolved = mark_resolved(&home, "t-late");
        assert_eq!(
            resolved.as_deref(),
            Some(id.as_str()),
            "late report must resolve the already-Exceeded sidecar"
        );
        assert!(
            !path.exists(),
            "late report must delete the Exceeded sidecar (not leak it)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// 5. After `mark_resolved`, the subsequent `scan_and_emit` does
    /// NOT fire an event (status was resolved before threshold check).
    #[test]
    fn mark_resolved_suppresses_fire() {
        let home = tmp_home("resolved-no-fire");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
        write_pending_at(
            &home,
            "alpha",
            "beta",
            Some("t-suppress"),
            "task",
            600,
            issued,
        );
        let resolved = mark_resolved(&home, "t-suppress");
        assert!(resolved.is_some(), "mark_resolved must locate sidecar");
        scan_and_emit(&home);
        let inbox = crate::inbox::drain(&home, "alpha");
        assert!(
            !inbox
                .iter()
                .any(|m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded")),
            "resolved dispatch must NOT fire timeout event: {inbox:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// 6. Load-bearing contract (parallel dev-1 + dev-2 configuration):
    /// two sidecars, only the exceeded one fires; the fresh one stays
    /// pending and does NOT pollute the exceeded one's event.
    #[test]
    fn parallel_dispatch_isolation() {
        let home = tmp_home("parallel-iso");
        let stale = chrono::Utc::now() - chrono::Duration::seconds(700);
        let fresh = chrono::Utc::now() - chrono::Duration::seconds(60);
        let id_stale =
            write_pending_at(&home, "lead", "dev-1", Some("t-stale"), "task", 600, stale);
        let id_fresh =
            write_pending_at(&home, "lead", "dev-2", Some("t-fresh"), "task", 600, fresh);
        scan_and_emit(&home);
        let inbox = crate::inbox::drain(&home, "lead");
        let exceeded_events: Vec<_> = inbox
            .iter()
            .filter(|m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded"))
            .collect();
        assert_eq!(
            exceeded_events.len(),
            1,
            "exactly one exceeded event for the stale dispatch"
        );
        assert_eq!(
            exceeded_events[0].correlation_id.as_deref(),
            Some("t-stale"),
            "the event must reference the stale correlation_id"
        );
        let pending = list_pending(&home);
        let p_stale = pending.iter().find(|p| p.dispatch_id == id_stale).unwrap();
        let p_fresh = pending.iter().find(|p| p.dispatch_id == id_fresh).unwrap();
        assert_eq!(p_stale.status, DispatchStatus::Exceeded);
        assert_eq!(
            p_fresh.status,
            DispatchStatus::Pending,
            "fresh dispatch must remain pending"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// 7. Invariant: L1 file MUST stay team-name-free. Cross-team-safe
    /// design contract. If this test ever fails, the L1 primitive has
    /// leaked team-specific knowledge and a sibling module is the
    /// right home for that code.
    ///
    /// Two structural allowances: comment lines (any `// …` prefix) and
    /// the boilerplate `pub(crate) mod team_nudge;` declaration that
    /// wires the L2 submodule into the dispatch_idle module tree.
    /// Test-module contents are also exempt — placeholder names like
    /// "lead" / "reviewer" / "dev-1" are legitimate test inputs.
    #[test]
    fn no_team_name_strings_in_l1() {
        let manifest = env!("CARGO_MANIFEST_DIR");
        let l1_path = std::path::PathBuf::from(manifest).join("src/daemon/dispatch_idle/mod.rs");
        let body = std::fs::read_to_string(&l1_path)
            .expect("L1 file must be readable from CARGO_MANIFEST_DIR");
        let mut offenders: Vec<(usize, &str, String)> = Vec::new();
        let mut in_test_mod = false;
        for (lineno, line) in body.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") {
                continue;
            }
            if trimmed.starts_with("mod tests") {
                in_test_mod = true;
            }
            if in_test_mod {
                continue;
            }
            // Allowlist: the L2 submodule declaration is structural,
            // not behavioral. Behaviour-side references stay forbidden.
            if trimmed == "pub(crate) mod team_nudge;" {
                continue;
            }
            for needle in ["fixup", "reviewer", "lead"] {
                if line.contains(needle) {
                    offenders.push((lineno + 1, needle, line.to_string()));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "L1 file must stay team-name-free; offenders: {offenders:?}"
        );
    }

    /// 8. Forward-compat: a future v2 sidecar must be left on disk and
    /// skipped by the v1 list_pending. Sprint 58 Wave 1 PR-2 contract.
    #[test]
    fn forward_compat_serde() {
        let home = tmp_home("forward-compat");
        let dir = pending_dir(&home);
        std::fs::create_dir_all(&dir).unwrap();
        let payload = serde_json::json!({
            "schema_version": SCHEMA_VERSION + 1,
            "dispatch_id": "disp-future",
            "dispatcher": "x",
            "target": "y",
            "expected_kind": "task",
            "threshold_secs": 600,
            "issued_at": "2026-05-09T00:00:00Z",
            "status": "pending",
        });
        std::fs::write(
            pending_path(&home, "disp-future"),
            serde_json::to_string_pretty(&payload).unwrap(),
        )
        .unwrap();
        let pending = list_pending(&home);
        assert!(
            pending.is_empty(),
            "future-version sidecar must be skipped by v1 reader"
        );
        // File preserved on disk so a v2 reader could pick it up later.
        assert!(pending_path(&home, "disp-future").exists());
        std::fs::remove_dir_all(&home).ok();
    }

    // ── PR2 L3 visibility tests for pending_for_instance ──

    /// Dispatcher view: pending sidecars where this agent is the
    /// outbound dispatcher surface in `dispatched_waiting_for`.
    #[test]
    fn pending_for_instance_surfaces_dispatcher_view() {
        let home = tmp_home("pfi-dispatcher");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(120);
        write_pending_at(
            &home,
            "fixup-lead",
            "fixup-reviewer",
            Some("t-l3-disp"),
            "task",
            600,
            issued,
        );
        let (as_dispatcher, as_target) = pending_for_instance(&home, "fixup-lead");
        assert_eq!(as_dispatcher.len(), 1);
        assert_eq!(as_dispatcher[0].target, "fixup-reviewer");
        assert_eq!(
            as_dispatcher[0].correlation_id.as_deref(),
            Some("t-l3-disp")
        );
        assert_eq!(as_dispatcher[0].threshold_secs, 600);
        assert!(
            (110..=130).contains(&as_dispatcher[0].elapsed_secs),
            "elapsed_secs within 10s window of expected 120: {}",
            as_dispatcher[0].elapsed_secs
        );
        assert!(
            as_target.is_empty(),
            "dispatcher agent must NOT appear in its own target view"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Target view: pending sidecars where this agent owes a reply
    /// surface in `pending_response_to`.
    #[test]
    fn pending_for_instance_surfaces_target_view() {
        let home = tmp_home("pfi-target");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(120);
        write_pending_at(
            &home,
            "fixup-lead",
            "fixup-reviewer",
            Some("t-l3-target"),
            "task",
            600,
            issued,
        );
        let (_, as_target) = pending_for_instance(&home, "fixup-reviewer");
        assert_eq!(as_target.len(), 1);
        assert_eq!(as_target[0].dispatcher, "fixup-lead");
        assert_eq!(as_target[0].correlation_id.as_deref(), Some("t-l3-target"));
        assert_eq!(as_target[0].threshold_secs, 600);
        std::fs::remove_dir_all(&home).ok();
    }

    /// Cross-team-safe: a non-fixup agent (and any agent not on a
    /// sidecar) sees empty arrays. Non-fixup teams that haven't opted
    /// in to the watchdog never record sidecars (see L2 default
    /// threshold logic), so L3 is a no-op for them.
    #[test]
    fn pending_for_instance_empty_for_unaffected_agent() {
        let home = tmp_home("pfi-unaffected");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(60);
        write_pending_at(
            &home,
            "fixup-lead",
            "fixup-reviewer",
            Some("t-fixup"),
            "task",
            600,
            issued,
        );
        let (as_dispatcher, as_target) = pending_for_instance(&home, "research-dev");
        assert!(
            as_dispatcher.is_empty() && as_target.is_empty(),
            "unaffected agent surfaces empty arrays"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1923 G2: a pending-dispatch sidecar whose DISPATCHER has left the fleet
    /// (deleted / redeployed) is stale — its idle nudge would route to a ghost
    /// dispatcher. `stale_sidecar_reason` must flag it `dispatcher_not_in_fleet`
    /// (mirroring the existing `target_not_in_fleet` check); a live dispatcher is
    /// not flagged.
    #[test]
    fn stale_sidecar_reason_flags_deleted_dispatcher_1923_g2() {
        let home = tmp_home("g2-dispatcher-stale");
        // fleet has the TARGET (`dev`) but NOT the dispatcher (it was deleted).
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  dev:\n    command: /bin/cat\n",
        )
        .expect("seed fleet.yaml");
        let mk = |dispatcher: &str| PendingDispatch {
            schema_version: SCHEMA_VERSION,
            dispatch_id: "d1".into(),
            dispatcher: dispatcher.into(),
            target: "dev".into(),
            correlation_id: Some("t-realtask".into()),
            expected_kind: "task".into(),
            threshold_secs: 600,
            issued_at: chrono::Utc::now().to_rfc3339(),
            status: DispatchStatus::Pending,
            nudge_sent_at: None,
            not_working_streak: 0,
            refresh_count: 0,
            long_running_escalated: false,
            exceeded_at: None,
        };
        assert_eq!(
            stale_sidecar_reason(&home, &mk("ghost-lead")),
            Some("dispatcher_not_in_fleet"),
            "#1923 G2: a sidecar whose dispatcher left the fleet is stale"
        );
        assert_ne!(
            stale_sidecar_reason(&home, &mk("dev")),
            Some("dispatcher_not_in_fleet"),
            "a live dispatcher must NOT be flagged stale"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Stale filter: resolved / exceeded / cancelled sidecars do NOT
    /// surface. Only `status == "pending"` reaches L3.
    #[test]
    fn pending_for_instance_filters_stale_sidecars() {
        let home = tmp_home("pfi-stale-filter");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(60);
        // One pending (must surface)
        write_pending_at(&home, "lead", "dev", Some("t-pending"), "task", 600, issued);
        // Three non-pending (must be filtered).
        for (corr, status) in [
            ("t-resolved", "resolved"),
            ("t-exceeded", "exceeded"),
            ("t-cancelled", "cancelled"),
        ] {
            let id = write_pending_at(&home, "lead", "dev", Some(corr), "task", 600, issued);
            // Flip status on disk.
            let path = pending_path(&home, &id);
            let body = std::fs::read_to_string(&path).unwrap();
            let mut v: serde_json::Value = serde_json::from_str(&body).unwrap();
            v["status"] = serde_json::Value::String(status.to_string());
            std::fs::write(&path, serde_json::to_string_pretty(&v).unwrap()).unwrap();
        }
        let (as_dispatcher, _) = pending_for_instance(&home, "lead");
        assert_eq!(
            as_dispatcher.len(),
            1,
            "only status=pending sidecars surface"
        );
        assert_eq!(
            as_dispatcher[0].correlation_id.as_deref(),
            Some("t-pending"),
            "non-pending entries must be filtered"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Wire shape: the serde-derived JSON for the L3 metadata uses
    /// stable snake_case field names. Pins the operator-visible
    /// schema so future renames are an intentional break, not a
    /// silent regression.
    #[test]
    fn pending_for_instance_serializes_with_stable_field_names() {
        let home = tmp_home("pfi-shape");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(60);
        write_pending_at(&home, "lead", "dev", Some("t-shape"), "task", 600, issued);
        let (as_dispatcher, as_target) = pending_for_instance(&home, "lead");
        let j = serde_json::to_value(&as_dispatcher[0]).unwrap();
        assert!(j.get("correlation_id").is_some());
        assert!(j.get("target").is_some());
        assert!(j.get("threshold_secs").is_some());
        assert!(j.get("elapsed_secs").is_some());
        assert!(
            as_target.is_empty(),
            "lead is dispatcher only — target view stays empty"
        );
        let (_, as_target_dev) = pending_for_instance(&home, "dev");
        let j2 = serde_json::to_value(&as_target_dev[0]).unwrap();
        assert!(j2.get("correlation_id").is_some());
        assert!(j2.get("dispatcher").is_some());
        assert!(j2.get("threshold_secs").is_some());
        assert!(j2.get("elapsed_secs").is_some());
        std::fs::remove_dir_all(&home).ok();
    }

    // ── #947 fallback contract: dispatch_idle nudge's correlation_id ──
    //
    // Pre-#947 behavior: `emit_exceeded_event` cloned `d.correlation_id`
    // verbatim. When the original `send` omitted correlation_id, the
    // outbound nudge inherited `None` — operators couldn't backtrack
    // from the nudge to the source sidecar.
    //
    // Post-#947: when upstream correlation_id is None, fall back to
    // `d.dispatch_id` (format `disp-{ts}-{seq}`, self-documenting via
    // the `disp-` prefix). The schema field is reused; no new field.
    //
    // The blend (upstream-chain vs producer-record) is acceptable because
    // the prefix conventions (`disp-`, `t-`, `m-`) make value class
    // identifiable at grep time. If a future producer breaks the prefix
    // convention, file a follow-up to add `source_record_id: Option<String>`
    // for clean separation (option A from /tmp/dialectic-947-dev-primary.md).

    /// #947 test 1 — when upstream correlation_id is present, it is
    /// preserved (NOT replaced with dispatch_id). The fallback applies
    /// only when upstream is None.
    #[test]
    fn dispatch_idle_emit_with_upstream_correlation_preserves_it() {
        let home = tmp_home("947-upstream-preserved");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
        write_pending_at(
            &home,
            "alpha",
            "beta",
            Some("upstream-corr-abc"),
            "task",
            600,
            issued,
        );
        scan_and_emit(&home);
        let inbox = crate::inbox::drain(&home, "alpha");
        let nudge = inbox
            .iter()
            .find(|m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded"))
            .expect("exceeded nudge must enqueue");
        assert_eq!(
            nudge.correlation_id.as_deref(),
            Some("upstream-corr-abc"),
            "upstream correlation_id must be preserved verbatim"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #947 test 2 — when upstream correlation_id is None, fall back to
    /// `d.dispatch_id`. The nudge becomes traceable to its source sidecar.
    #[test]
    fn dispatch_idle_emit_without_upstream_falls_back_to_dispatch_id() {
        let home = tmp_home("947-fallback-dispid");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
        let dispatch_id = write_pending_at(&home, "alpha", "beta", None, "task", 600, issued);
        scan_and_emit(&home);
        let inbox = crate::inbox::drain(&home, "alpha");
        let nudge = inbox
            .iter()
            .find(|m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded"))
            .expect("exceeded nudge must enqueue");
        assert_eq!(
            nudge.correlation_id.as_deref(),
            Some(dispatch_id.as_str()),
            "missing upstream correlation_id must fall back to dispatch_id"
        );
        // Format check: dispatch_id starts with `disp-` (self-documenting prefix).
        assert!(
            dispatch_id.starts_with("disp-"),
            "dispatch_id format must use `disp-` prefix: {dispatch_id}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #947 test 5 (e2e) — after the fix, dispatch_idle nudges ALWAYS
    /// carry a non-empty correlation_id, regardless of upstream presence.
    /// This is the load-bearing operator contract for reverse-lookup.
    #[test]
    fn dispatch_idle_nudge_correlation_id_always_non_empty() {
        let home = tmp_home("947-always-non-empty");
        let now = chrono::Utc::now();
        // Two pending dispatches: one with upstream, one without.
        write_pending_at(
            &home,
            "alpha",
            "beta",
            Some("with-chain"),
            "task",
            600,
            now - chrono::Duration::seconds(700),
        );
        write_pending_at(
            &home,
            "gamma",
            "delta",
            None,
            "task",
            600,
            now - chrono::Duration::seconds(800),
        );
        scan_and_emit(&home);
        let alpha_inbox = crate::inbox::drain(&home, "alpha");
        let gamma_inbox = crate::inbox::drain(&home, "gamma");
        for m in alpha_inbox
            .iter()
            .filter(|m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded"))
        {
            let c = m.correlation_id.as_deref().unwrap_or("");
            assert!(
                !c.is_empty(),
                "alpha nudge correlation_id must be non-empty: {m:?}"
            );
        }
        for m in gamma_inbox
            .iter()
            .filter(|m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded"))
        {
            let c = m.correlation_id.as_deref().unwrap_or("");
            assert!(
                !c.is_empty(),
                "gamma nudge correlation_id must be non-empty (fallback): {m:?}"
            );
        }
        std::fs::remove_dir_all(&home).ok();
    }

    // ── #1018: stale-sidecar cleanup ───────────────────────────────────

    /// #1018 (A) — placeholder correlation_id classifier covers the
    /// known sentinels (`t-pending`, `t-tbd`) and the explicit-empty
    /// string variant. `None` is NOT a placeholder per #947 contract:
    /// dispatches without upstream correlation must still fire the
    /// threshold event with `dispatch_id` as fallback. Other strings
    /// (even short / suspicious-looking ones) are also NOT placeholders.
    #[test]
    fn t1018_a_placeholder_correlation_predicate() {
        assert!(is_placeholder_correlation(Some("t-pending")));
        assert!(is_placeholder_correlation(Some("t-tbd")));
        assert!(is_placeholder_correlation(Some("")));
        assert!(is_placeholder_correlation(Some("   ")));
        assert!(
            !is_placeholder_correlation(None),
            "None != placeholder — #947 fallback contract preserved"
        );
        assert!(!is_placeholder_correlation(Some(
            "t-20260520163333000054-1"
        )));
        assert!(!is_placeholder_correlation(Some("t-pending-real")));
        assert!(!is_placeholder_correlation(Some("real-id")));
    }

    /// #1018 (A) — sidecar with placeholder correlation_id is cleared
    /// at scan tick without firing the threshold event.
    #[test]
    fn t1018_a_placeholder_correlation_swept_silently() {
        let home = tmp_home("1018-placeholder");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
        let id = write_pending_at(
            &home,
            "fixup-lead",
            "fixup-dev-2",
            Some("t-pending"),
            "task",
            600,
            issued,
        );
        scan_and_emit(&home);
        let inbox = crate::inbox::drain(&home, "fixup-lead");
        assert!(
            !inbox
                .iter()
                .any(|m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded")),
            "placeholder correlation_id MUST NOT fire threshold event: {inbox:?}"
        );
        let pending = list_pending(&home);
        assert!(
            pending.iter().all(|p| p.dispatch_id != id),
            "stale placeholder sidecar MUST be removed from disk"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1018 (A) — sidecar targeting a non-fleet agent is cleared
    /// silently (fleet.yaml exists but instance not listed).
    #[test]
    fn t1018_a_missing_target_in_fleet_swept_silently() {
        let home = tmp_home("1018-missing-target");
        // Empty fleet.yaml → resolve_instance returns None for any name.
        std::fs::write(crate::fleet::fleet_yaml_path(&home), "instances: {}\n").unwrap();
        let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
        let id = write_pending_at(
            &home,
            "fixup-lead",
            "ghost-agent",
            Some("t-real-task-123"),
            "task",
            600,
            issued,
        );
        scan_and_emit(&home);
        let inbox = crate::inbox::drain(&home, "fixup-lead");
        assert!(
            !inbox
                .iter()
                .any(|m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded")),
            "missing target MUST NOT fire threshold event: {inbox:?}"
        );
        let pending = list_pending(&home);
        assert!(
            pending.iter().all(|p| p.dispatch_id != id),
            "stale missing-target sidecar MUST be removed from disk"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1018 (A) — sidecar correlation_id maps to a real task_id but
    /// that task is already `done` on the board. Cleared silently.
    #[test]
    fn t1018_a_closed_task_id_swept_silently() {
        let home = tmp_home("1018-closed-task");
        // Provide a fleet.yaml that includes the target so the
        // missing-target branch doesn't trip first.
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  fixup-dev-2:\n    backend: claude\n",
        )
        .unwrap();
        let task_id = "t-closed-12345";
        // #1608b: seed a REAL closed task on the event-sourced board (the path
        // `task_still_live` now reads), not a `tasks/<id>.json` file the board
        // never writes.
        {
            use crate::task_events::{append, DoneSource, InstanceName, TaskEvent, TaskId};
            let emitter = InstanceName::from("test:operator");
            let tid = TaskId(task_id.into());
            append(
                &home,
                &emitter,
                TaskEvent::Created {
                    task_id: tid.clone(),
                    title: "test".into(),
                    description: String::new(),
                    priority: "normal".into(),
                    owner: Some(InstanceName::from("fixup-dev-2")),
                    due_at: None,
                    depends_on: Vec::new(),
                    routed_to: None,
                    branch: None,
                    bind: None,
                    eta_secs: None,
                    tags: vec![],
                    parent_id: None,
                },
            )
            .unwrap();
            append(
                &home,
                &emitter,
                TaskEvent::Done {
                    task_id: tid,
                    by: InstanceName::from("fixup-dev-2"),
                    source: DoneSource::OperatorManual {
                        authored_at: chrono::Utc::now().to_rfc3339(),
                        result: None,
                    },
                },
            )
            .unwrap();
        }
        let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
        let id = write_pending_at(
            &home,
            "fixup-lead",
            "fixup-dev-2",
            Some(task_id),
            "task",
            600,
            issued,
        );
        scan_and_emit(&home);
        let inbox = crate::inbox::drain(&home, "fixup-lead");
        assert!(
            !inbox
                .iter()
                .any(|m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded")),
            "closed task_id MUST NOT fire threshold event: {inbox:?}"
        );
        let pending = list_pending(&home);
        assert!(
            pending.iter().all(|p| p.dispatch_id != id),
            "stale closed-task sidecar MUST be removed from disk"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1018 (A) anti-regression — real task_id + present target +
    /// task status `in_progress` MUST still fire the threshold event
    /// when overdue. Guards against over-rotation into clearing live
    /// sidecars.
    #[test]
    fn t1018_a_live_dispatch_still_fires() {
        let home = tmp_home("1018-live");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            // #1923 G2: seed the DISPATCHER too (not just the target) — the
            // dispatcher-in-fleet stale check now requires it, and in prod the
            // dispatcher is always a live fleet agent.
            "instances:\n  fixup-lead:\n    backend: claude\n  fixup-dev-2:\n    backend: claude\n",
        )
        .unwrap();
        let task_id = "t-live-99";
        // #1608b: seed a REAL LIVE task on the event-sourced board (the path
        // `task_still_live` reads via load_by_id → replay), NOT a
        // `tasks/{id}.json` file — that file is never written, so the old write
        // was ignored and the test only passed via the missing-task fail-open,
        // not because the task was recognized as live. A freshly-Created task is
        // `open`, which is in LIVE_TASK_STATUSES, so `task_still_live` returns
        // Some(true) and the overdue dispatch must still fire.
        {
            use crate::task_events::{append, InstanceName, TaskEvent, TaskId};
            let emitter = InstanceName::from("test:operator");
            append(
                &home,
                &emitter,
                TaskEvent::Created {
                    task_id: TaskId(task_id.into()),
                    title: "live work".into(),
                    description: String::new(),
                    priority: "normal".into(),
                    owner: Some(InstanceName::from("fixup-dev-2")),
                    due_at: None,
                    depends_on: Vec::new(),
                    routed_to: None,
                    branch: None,
                    bind: None,
                    eta_secs: None,
                    tags: vec![],
                    parent_id: None,
                },
            )
            .unwrap();
        }
        let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
        write_pending_at(
            &home,
            "fixup-lead",
            "fixup-dev-2",
            Some(task_id),
            "task",
            600,
            issued,
        );
        scan_and_emit(&home);
        let inbox = crate::inbox::drain(&home, "fixup-lead");
        assert!(
            inbox.iter().any(
                |m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded")
                    && m.correlation_id.as_deref() == Some(task_id)
            ),
            "live overdue dispatch MUST still fire — got: {inbox:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1018 (B) — `cleanup_pending_for_task_id` deletes sidecars
    /// matching the closed task_id; leaves others untouched.
    #[test]
    fn t1018_b_cleanup_pending_for_task_id() {
        let home = tmp_home("1018-task-done-cleanup");
        let now = chrono::Utc::now();
        let id_match_1 = write_pending_at(
            &home,
            "fixup-lead",
            "dev-1",
            Some("t-target"),
            "task",
            600,
            now,
        );
        let id_match_2 = write_pending_at(
            &home,
            "fixup-lead",
            "dev-2",
            Some("t-target"),
            "task",
            600,
            now,
        );
        let id_other = write_pending_at(
            &home,
            "fixup-lead",
            "dev-1",
            Some("t-different"),
            "task",
            600,
            now,
        );

        let cleared = cleanup_pending_for_task_id(&home, "t-target");
        assert_eq!(cleared, 2, "must delete both sidecars for closed task");

        let pending = list_pending(&home);
        assert!(pending.iter().all(|p| p.dispatch_id != id_match_1));
        assert!(pending.iter().all(|p| p.dispatch_id != id_match_2));
        assert!(
            pending.iter().any(|p| p.dispatch_id == id_other),
            "unrelated task_id sidecar must NOT be cleared"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1018 (B) — `cleanup_pending_for_task_id` refuses to act on
    /// placeholder task_ids so a stray `task_id=t-pending` close
    /// can't wipe unrelated sidecars.
    #[test]
    fn t1018_b_cleanup_refuses_placeholder_task_id() {
        let home = tmp_home("1018-cleanup-placeholder");
        let now = chrono::Utc::now();
        let id = write_pending_at(
            &home,
            "fixup-lead",
            "dev-1",
            Some("t-pending"),
            "task",
            600,
            now,
        );
        let cleared = cleanup_pending_for_task_id(&home, "t-pending");
        assert_eq!(cleared, 0, "placeholder task_id MUST NOT trigger cleanup");
        // Sidecar still exists on disk.
        let pending = list_pending(&home);
        assert!(pending.iter().any(|p| p.dispatch_id == id));
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1018 (C) — `cleanup_pending_for_instance` deletes sidecars
    /// targeting the deleted instance; leaves dispatcher-side and
    /// other-target sidecars untouched.
    #[test]
    fn t1018_c_cleanup_pending_for_instance() {
        let home = tmp_home("1018-instance-delete-cleanup");
        let now = chrono::Utc::now();
        let id_target = write_pending_at(
            &home,
            "fixup-lead",
            "fixup-reviewer",
            Some("t-aaa"),
            "task",
            600,
            now,
        );
        let id_other_target = write_pending_at(
            &home,
            "fixup-lead",
            "fixup-dev-2",
            Some("t-bbb"),
            "task",
            600,
            now,
        );
        // Sidecar where the deleted instance is the DISPATCHER, not
        // the target. Must NOT be cleared by this cleanup (different
        // failure mode — dispatcher-side bookkeeping is operator's
        // responsibility via task board).
        let id_dispatcher_role = write_pending_at(
            &home,
            "fixup-reviewer",
            "fixup-dev-2",
            Some("t-ccc"),
            "task",
            600,
            now,
        );

        let cleared = cleanup_pending_for_instance(&home, "fixup-reviewer");
        assert_eq!(cleared, 1, "must delete only target-matching sidecar");
        let pending = list_pending(&home);
        assert!(pending.iter().all(|p| p.dispatch_id != id_target));
        assert!(
            pending.iter().any(|p| p.dispatch_id == id_other_target),
            "different-target sidecar untouched"
        );
        assert!(
            pending.iter().any(|p| p.dispatch_id == id_dispatcher_role),
            "dispatcher-role sidecar untouched"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── #1047: refresh_issued_at (kind=update/query timer reset) ──

    /// #1047 T1: dispatchee sends kind=update within threshold → timer
    /// resets → subsequent scan_and_emit does NOT fire at the original
    /// threshold boundary.
    #[test]
    fn t1047_refresh_issued_at_prevents_false_positive() {
        let home = tmp_home("1047-refresh");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(550);
        write_pending_at(&home, "lead", "dev", Some("t-1047-a"), "task", 600, issued);
        // Dispatchee sends update → timer resets.
        let refreshed = refresh_issued_at(&home, "t-1047-a");
        assert!(refreshed.is_some(), "refresh must locate sidecar");
        // Now 600s hasn't elapsed from the refreshed issued_at.
        scan_and_emit(&home);
        let inbox = crate::inbox::drain(&home, "lead");
        assert!(
            !inbox
                .iter()
                .any(|m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded")),
            "#1047: refreshed sidecar must NOT fire: {inbox:?}"
        );
        let pending = list_pending(&home);
        assert!(
            pending.iter().any(|p| p.status == DispatchStatus::Pending),
            "sidecar must remain pending after refresh"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1047 T2: dispatchee silent past threshold → fire (regression preserved).
    #[test]
    fn t1047_silent_dispatchee_still_fires() {
        let home = tmp_home("1047-silent");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
        write_pending_at(&home, "lead", "dev", Some("t-1047-b"), "task", 600, issued);
        // No refresh — dispatchee is silent.
        scan_and_emit(&home);
        let inbox = crate::inbox::drain(&home, "lead");
        assert!(
            inbox
                .iter()
                .any(|m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded")),
            "#1047 regression: silent dispatchee must still fire: {inbox:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1047 T3: kind=report still fully closes sidecar (status=resolved).
    #[test]
    fn t1047_report_still_resolves() {
        let home = tmp_home("1047-report");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(550);
        write_pending_at(&home, "lead", "dev", Some("t-1047-c"), "task", 600, issued);
        let resolved = mark_resolved(&home, "t-1047-c");
        assert!(resolved.is_some(), "report must resolve sidecar");
        let pending = list_pending(&home);
        assert!(
            !pending
                .iter()
                .any(|p| p.correlation_id.as_deref() == Some("t-1047-c")),
            "kind=report must resolve (delete) the sidecar"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── #1516: agent_state gate (don't fire while target is working) ──

    /// #1694②: map the legacy state label to a productive-silence value so the
    /// pre-existing #1516/#1658 state-based tests keep their intent under the
    /// silence-clock gate — `thinking`/`tool_use` = recently productive
    /// (`silent_secs` 0 → working), anything else = productive-silent past any
    /// window. New silence-specific tests use [`mk_agent_snapshot_silence`].
    fn mk_agent_snapshot(name: &str, agent_state: &str) -> crate::snapshot::AgentSnapshot {
        let silent_secs = match agent_state {
            "thinking" | "tool_use" => 0,
            _ => i64::MAX,
        };
        mk_agent_snapshot_silence(name, agent_state, silent_secs)
    }

    fn mk_agent_snapshot_silence(
        name: &str,
        agent_state: &str,
        silent_secs: i64,
    ) -> crate::snapshot::AgentSnapshot {
        // #1961 phase-2: pane-change signal FAIL-CLOSED (no recent change) in
        // the legacy fixtures so every pre-existing gate test exercises the
        // original three signals unchanged; pane-delta tests use
        // [`mk_agent_snapshot_pane`].
        mk_agent_snapshot_pane(name, agent_state, silent_secs, i64::MAX)
    }

    fn mk_agent_snapshot_pane(
        name: &str,
        agent_state: &str,
        silent_secs: i64,
        output_silent_secs: i64,
    ) -> crate::snapshot::AgentSnapshot {
        crate::snapshot::AgentSnapshot {
            name: name.to_string(),
            backend_command: "opencode".to_string(),
            args: vec![],
            working_dir: None,
            submit_key: "\r".to_string(),
            health_state: "healthy".to_string(),
            agent_state: agent_state.to_string(),
            silent_secs,
            output_silent_secs,
        }
    }

    /// #1961 phase-2 — THE production false-fire shape: the state-detector
    /// mis-reads a code-writing agent as "idle", productive markers missed
    /// (silent_secs=MAX), no MCP heartbeat — all three legacy gates slip. The
    /// pane CONTENT is changing (token streaming → screen-hash delta), so the
    /// classification-free 4th signal must suppress.
    #[test]
    fn pane_change_suppresses_when_all_state_signals_slip_1961() {
        const T: i64 = 600;
        let snap = crate::snapshot::FleetSnapshot {
            timestamp: "t".to_string(),
            agents: vec![mk_agent_snapshot_pane(
                "misread",
                "idle",
                i64::MAX, // detector says idle, markers missed
                10,       // …but the pane changed 10s ago (streaming)
            )],
        };
        assert!(
            target_is_working(Some(&snap), "misread", T),
            "#1961: a recently-changing pane must suppress even when every \
             classification-based signal reads idle/silent"
        );
    }

    /// #1961 phase-2 fail-toward-fire: a genuinely idle agent — pane completely
    /// static past the window, all other signals idle — must STILL fire (the
    /// new signal only ADDS suppression, never blocks a real stuck).
    #[test]
    fn truly_static_pane_still_fires_1961() {
        const T: i64 = 600;
        let snap = crate::snapshot::FleetSnapshot {
            timestamp: "t".to_string(),
            agents: vec![mk_agent_snapshot_pane(
                "stuck",
                "idle",
                i64::MAX, // not productive
                i64::MAX, // pane has not changed at all
            )],
        };
        assert!(
            !target_is_working(Some(&snap), "stuck", T),
            "#1961: a fully-static pane keeps firing — the pane signal must not \
             hide a real stuck"
        );
        // Old-format snapshot (field missing → serde default MAX) behaves the
        // same: fail-open to firing.
        let legacy = crate::snapshot::FleetSnapshot {
            timestamp: "t".to_string(),
            agents: vec![mk_agent_snapshot_silence("legacy", "idle", i64::MAX)],
        };
        assert!(
            !target_is_working(Some(&legacy), "legacy", T),
            "fail-closed fixture (= old-format default) must not suppress"
        );
    }

    /// #1694②: the gate reads the productive-SILENCE clock, not the
    /// instantaneous thinking/tool_use state. Recently-productive
    /// (`silent_secs < threshold`) → working (suppress); productive-silent past
    /// the window → not working (fire); active-recovery states are exempt
    /// regardless of silence.
    #[test]
    fn target_is_working_reads_silence_clock_1694() {
        const T: i64 = 600;
        let snap = crate::snapshot::FleetSnapshot {
            timestamp: "t".to_string(),
            agents: vec![
                // recently productive while NOT thinking/tool_use → still working
                mk_agent_snapshot_silence("fresh_idle", "idle", 10),
                // #toolu-gap: a long LOCAL tool_use (9-min Bash) emits no pane
                // marker / MCP heartbeat → silent_secs high, but agent_state is
                // tool_use → WORKING. A hung one is the hang_detector's job
                // (productive_silence_exceeds → Hung at silent>600s).
                mk_agent_snapshot_silence("long_tool_use", "tool_use", 700),
                // same for thinking: instantaneous-working → WORKING here; a hung
                // thinking is the hang_detector's, not dispatch-idle's.
                mk_agent_snapshot_silence("long_thinking", "thinking", 700),
                // active-recovery exempt: ONLY server_rate_limit (bounded retry +
                // #1744 exhaustion backstop) → silent but exempt
                mk_agent_snapshot_silence("rate_limited", "server_rate_limit", 700),
                // api_error is NOT exempt (no exhaustion backstop) → silent = stuck
                mk_agent_snapshot_silence("api_err", "api_error", 700),
            ],
        };
        assert!(
            target_is_working(Some(&snap), "fresh_idle", T),
            "recently productive (silent<threshold) → working even when not thinking"
        );
        assert!(
            target_is_working(Some(&snap), "long_tool_use", T),
            "#toolu-gap: long local tool_use (silent past window, no pane/heartbeat) \
             → WORKING (instantaneous state); hang_detector owns a genuinely hung one"
        );
        assert!(
            target_is_working(Some(&snap), "long_thinking", T),
            "thinking past window → WORKING (instantaneous state); hang_detector owns hung"
        );
        assert!(
            target_is_working(Some(&snap), "rate_limited", T),
            "ServerRateLimit → active-recovery exempt (suppress nudge)"
        );
        assert!(
            !target_is_working(Some(&snap), "api_err", T),
            "ApiError is NOT exempt (no exhaustion backstop) → silent past window fires"
        );
        assert!(
            !target_is_working(Some(&snap), "ghost", T),
            "absent → not working (fire)"
        );
        assert!(
            !target_is_working(None, "fresh_idle", T),
            "no snapshot → not working (fail-open fire)"
        );
    }

    /// Core #1516 regression: an overdue dispatch whose target is demonstrably
    /// WORKING (Thinking/ToolUse) must NOT fire — the timer resets instead.
    /// Pre-fix this false-fired (5× the night it landed).
    #[test]
    fn working_target_does_not_fire_1516() {
        let home = tmp_home("gate-working");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
        let id = write_pending_at(&home, "lead", "worker", Some("t-w"), "task", 600, issued);
        crate::snapshot::save(&home, &[mk_agent_snapshot("worker", "thinking")]);

        scan_and_emit(&home);

        assert!(
            crate::inbox::drain(&home, "lead").is_empty(),
            "a working (thinking) target must NOT trigger an idle nudge"
        );
        let p = list_pending(&home)
            .into_iter()
            .find(|p| p.dispatch_id == id)
            .unwrap();
        assert_eq!(
            p.status,
            DispatchStatus::Pending,
            "sidecar stays pending (clock refreshed)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Real-stuck still caught: an overdue dispatch whose target is Idle (not
    /// working) with no report still fires (Q4 — the gate only suppresses
    /// demonstrable progress). #1658: now after the DEBOUNCE_SCANS debounce
    /// window (was 1 scan) — the signal is delayed, not lost.
    #[test]
    fn idle_target_still_fires_1516() {
        let home = tmp_home("gate-idle");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
        let id = write_pending_at(&home, "lead", "worker", Some("t-i"), "task", 600, issued);
        crate::snapshot::save(&home, &[mk_agent_snapshot("worker", "idle")]);

        // #1658: a snapshot present + not-working debounces — fires on the
        // DEBOUNCE_SCANS-th consecutive idle scan, not the first.
        for _ in 0..DEBOUNCE_SCANS {
            scan_and_emit(&home);
        }

        assert!(
            crate::inbox::drain(&home, "lead")
                .iter()
                .any(|m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded")),
            "idle + overdue + no report must still fire"
        );
        let p = list_pending(&home)
            .into_iter()
            .find(|p| p.dispatch_id == id)
            .unwrap();
        assert_eq!(p.status, DispatchStatus::Exceeded);
        std::fs::remove_dir_all(&home).ok();
    }

    /// Graceful degradation: no snapshot at all → fall back to firing (the
    /// pre-#1516 behavior; the gate never makes things worse).
    #[test]
    fn no_snapshot_falls_back_to_firing_1516() {
        let home = tmp_home("gate-nosnap");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
        write_pending_at(&home, "lead", "worker", Some("t-n"), "task", 600, issued);
        // No snapshot.json written.
        scan_and_emit(&home);
        assert!(
            crate::inbox::drain(&home, "lead")
                .iter()
                .any(|m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded")),
            "no snapshot → must fall back to firing (no worse than pre-#1516)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1694② de-noise regression: an overdue dispatch whose target is NOT in
    /// thinking/tool_use but is recently PRODUCTIVE (low `silent_secs`) must NOT
    /// fire. Pre-#1694 the #1516 state gate fired here (state ≠ thinking) — the
    /// exact "reminders became noise" complaint, e.g. a dev heads-down for 13 min
    /// whose snapshot state isn't thinking at the scan instant but who is plainly
    /// producing output.
    #[test]
    fn productive_but_not_thinking_suppressed_1694() {
        let home = tmp_home("silence-productive");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(800);
        let id = write_pending_at(&home, "lead", "dev", Some("t-p"), "task", 600, issued);
        // Idle state label, but productive output 60s ago (well under the 600s
        // window) → the silence clock says "working".
        crate::snapshot::save(&home, &[mk_agent_snapshot_silence("dev", "idle", 60)]);

        for _ in 0..DEBOUNCE_SCANS + 1 {
            scan_and_emit(&home);
        }

        assert!(
            crate::inbox::drain(&home, "lead").is_empty(),
            "recently-productive target must NOT trigger an idle nudge"
        );
        let p = list_pending(&home)
            .into_iter()
            .find(|p| p.dispatch_id == id)
            .unwrap();
        assert_eq!(
            p.status,
            DispatchStatus::Pending,
            "sidecar stays pending (silence clock refreshed it)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #toolu-gap: a long LOCAL tool_use (e.g. a 9-min `Bash` run) emits no pane
    /// marker / MCP heartbeat, so `silent_secs` climbs past the window — but the
    /// agent is plainly WORKING (`agent_state=tool_use`). It must NOT fire (the
    /// live noise dev-2 hit: `✻ Proofing… 9m`, not stuck, even shipped a PR). A
    /// genuinely hung tool_use is the hang_detector's job, not dispatch-idle's.
    #[test]
    fn long_tool_use_silent_does_not_fire_toolu_gap() {
        let home = tmp_home("silence-tooluse");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(800);
        let id = write_pending_at(&home, "lead", "dev", Some("t-tu"), "task", 600, issued);
        // tool_use AND productive-silent past the window (no pane/heartbeat output).
        crate::snapshot::save(&home, &[mk_agent_snapshot_silence("dev", "tool_use", 700)]);

        for _ in 0..DEBOUNCE_SCANS + 1 {
            scan_and_emit(&home);
        }

        assert!(
            crate::inbox::drain(&home, "lead").is_empty(),
            "long tool_use (silent past window) must NOT fire — it is working, not stuck"
        );
        let p = list_pending(&home)
            .into_iter()
            .find(|p| p.dispatch_id == id)
            .unwrap();
        assert_eq!(
            p.status,
            DispatchStatus::Pending,
            "sidecar stays pending (instantaneous tool_use state suppresses the nudge)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1694② finding #4: an overdue dispatch whose target is in an
    /// active-recovery state (ServerRateLimit) must NOT fire even when
    /// productive-silent — the auto-retry machinery owns the recovery, so
    /// nudging is pure noise (and would re-create the very proxy-drop noise this
    /// change removes).
    #[test]
    fn active_recovery_exempt_does_not_fire_1694() {
        let home = tmp_home("silence-recovery");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(800);
        let id = write_pending_at(&home, "lead", "dev", Some("t-r"), "task", 600, issued);
        // Productive-silent (700s) AND in an auto-recovery state → exempt.
        crate::snapshot::save(
            &home,
            &[mk_agent_snapshot_silence("dev", "server_rate_limit", 700)],
        );

        for _ in 0..DEBOUNCE_SCANS + 1 {
            scan_and_emit(&home);
        }

        assert!(
            crate::inbox::drain(&home, "lead").is_empty(),
            "active-recovery (ServerRateLimit) target must NOT trigger an idle nudge"
        );
        let p = list_pending(&home)
            .into_iter()
            .find(|p| p.dispatch_id == id)
            .unwrap();
        assert_eq!(p.status, DispatchStatus::Pending);
        std::fs::remove_dir_all(&home).ok();
    }

    /// codex #1775 HIGH: `api_error` is NOT an exempt active-recovery state (it
    /// has no retry-exhaustion backstop), so a wedged api_error agent that is
    /// productive-silent past the window must still fire — dispatch-idle is its
    /// only watchdog (hang_detector misses it: no BlockedReason → IdleLong, not
    /// Hung). Contrast with [`active_recovery_exempt_does_not_fire_1694`]
    /// (server_rate_limit, which IS exempt).
    #[test]
    fn stuck_api_error_silent_still_fires_1775() {
        let home = tmp_home("silence-apierror");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(800);
        let id = write_pending_at(&home, "lead", "dev", Some("t-ae"), "task", 600, issued);
        // api_error AND productive-silent (700s > 600s window) → must fire.
        crate::snapshot::save(&home, &[mk_agent_snapshot_silence("dev", "api_error", 700)]);

        for _ in 0..DEBOUNCE_SCANS {
            scan_and_emit(&home);
        }

        assert!(
            crate::inbox::drain(&home, "lead")
                .iter()
                .any(|m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded")),
            "stuck api_error (silent past window) must fire — no exhaustion backstop owns it"
        );
        let p = list_pending(&home)
            .into_iter()
            .find(|p| p.dispatch_id == id)
            .unwrap();
        assert_eq!(p.status, DispatchStatus::Exceeded);
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1694② complement to [`active_recovery_exempt_does_not_fire_1694`]: a
    /// genuinely stuck target — productive-silent past the window AND in a
    /// non-recovery state — still fires after the debounce (the watchdog must
    /// not be neutered by the de-noise change).
    #[test]
    fn productive_silent_non_recovery_still_fires_1694() {
        let home = tmp_home("silence-stuck");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(800);
        let id = write_pending_at(&home, "lead", "dev", Some("t-s"), "task", 600, issued);
        // Productive-silent (700s > 600s window), ordinary state → not working → fire.
        crate::snapshot::save(&home, &[mk_agent_snapshot_silence("dev", "idle", 700)]);

        for _ in 0..DEBOUNCE_SCANS {
            scan_and_emit(&home);
        }

        assert!(
            crate::inbox::drain(&home, "lead")
                .iter()
                .any(|m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded")),
            "productive-silent past window + overdue + no report must still fire"
        );
        let p = list_pending(&home)
            .into_iter()
            .find(|p| p.dispatch_id == id)
            .unwrap();
        assert_eq!(p.status, DispatchStatus::Exceeded);
        std::fs::remove_dir_all(&home).ok();
    }

    /// #absorb-blocked (the N=3 false-positive replay): a target that is
    /// idle/silent (NOT "working") but has declared an ACTIVE `waiting_on`
    /// (intentional block/queue, e.g. waiting on a dependency PR) must NOT fire —
    /// the sidecar stays Pending, so neither the dispatcher `..._exceeded` event
    /// NOR the downstream L2 `..._nudge` to the target is sent.
    #[test]
    fn blocked_target_with_waiting_on_is_absorbed() {
        let home = tmp_home("absorb-blocked");
        let target = "absorb-blocked-tgt";
        let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
        let id = write_pending_at(&home, "lead", target, Some("t-ab"), "task", 600, issued);
        // Idle + productive-silent past the window (would normally fire) ...
        crate::snapshot::save(&home, &[mk_agent_snapshot_silence(target, "idle", 700)]);
        // ... BUT the target declared an active waiting_on (set_waiting_on).
        crate::daemon::heartbeat_pair::update_with(target, |p| {
            p.waiting_on_since_ms = Some(crate::daemon::heartbeat_pair::now_ms());
        });

        for _ in 0..DEBOUNCE_SCANS + 1 {
            scan_and_emit(&home);
        }

        assert!(
            crate::inbox::drain(&home, "lead").is_empty(),
            "#absorb-blocked: an active-waiting_on target must NOT fire the exceeded event"
        );
        let p = list_pending(&home)
            .into_iter()
            .find(|p| p.dispatch_id == id)
            .unwrap();
        assert_eq!(
            p.status,
            DispatchStatus::Pending,
            "#absorb-blocked: sidecar stays Pending (absorbed) → the L2 target nudge is also suppressed"
        );
        // Global hygiene: clear this name's waiting_on (heartbeat_pair is process-global).
        crate::daemon::heartbeat_pair::update_with(target, |p| {
            p.waiting_on_since_ms = None;
        });
        std::fs::remove_dir_all(&home).ok();
    }

    /// #absorb-blocked boundary: once the target CLEARS its waiting_on
    /// (`set_waiting_on("")` → `waiting_on_since_ms = None`), the absorb releases —
    /// a still-overdue, still-silent target fires normally. We must not permanently
    /// suppress a genuinely-stuck-after-unblock target.
    #[test]
    fn cleared_waiting_on_resumes_firing() {
        let home = tmp_home("absorb-cleared");
        let target = "absorb-cleared-tgt";
        let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
        let id = write_pending_at(&home, "lead", target, Some("t-ac"), "task", 600, issued);
        crate::snapshot::save(&home, &[mk_agent_snapshot_silence(target, "idle", 700)]);
        // Cleared (no active waiting_on) — the resume side of the boundary.
        crate::daemon::heartbeat_pair::update_with(target, |p| {
            p.waiting_on_since_ms = None;
        });

        for _ in 0..DEBOUNCE_SCANS {
            scan_and_emit(&home);
        }

        assert!(
            crate::inbox::drain(&home, "lead")
                .iter()
                .any(|m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded")),
            "#absorb-blocked: a cleared (no waiting_on) overdue+silent target must still fire"
        );
        let p = list_pending(&home)
            .into_iter()
            .find(|p| p.dispatch_id == id)
            .unwrap();
        assert_eq!(p.status, DispatchStatus::Exceeded);
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1629 invariant (#1617 lock-while-blocking class): `emit_exceeded_event`
    /// (self-IPC via notify_system → loopback api::call) must NEVER run while the
    /// #1340 dispatch flock is held. The RMW happens inside the `let to_emit = {
    /// ... }` flock block; the emit runs after the block (lock-free). Structural
    /// source-scan: brace-match the to_emit block and assert the emit call is NOT
    /// inside it and IS after. Needle is `concat`-built and the scan is
    /// prod-sliced so this test can't self-satisfy.
    #[test]
    fn emit_exceeded_not_called_under_flock() {
        let src = include_str!("mod.rs");
        let cfg_test = ["#[cfg(", "test)]"].concat();
        let prod = match src.find(&cfg_test) {
            Some(i) => &src[..i],
            None => src,
        };
        let block_anchor = ["let to", "_emit"].concat();
        let astart = prod
            .find(&block_anchor)
            .expect("to_emit flock block present");
        let open_rel = prod[astart..].find('{').expect("flock block opens");
        let block_start = astart + open_rel;
        let mut depth = 0usize;
        let mut block_end = block_start;
        for (i, c) in prod[block_start..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        block_end = block_start + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        assert!(block_end > block_start, "flock block must close");
        let emit_needle = ["emit_exceeded", "_event("].concat();
        let block_body = &prod[block_start..=block_end];
        assert!(
            !block_body.contains(&emit_needle),
            "emit_exceeded_event must NOT run inside the #1340 dispatch flock block (#1617 class)"
        );
        assert!(
            prod[block_end..].contains(&emit_needle),
            "emit_exceeded_event must run AFTER the dispatch flock is dropped"
        );
    }

    // ── #event-bus pattern #3: emit→subscriber vs legacy parity ──
    // No `env_lock` needed: the recipient is `dispatcher` (from the sidecar), not
    // an env-derived value, so there is no process-global env race here.

    /// The comparable inbox payload (ignoring volatile id/timestamp).
    fn drained_payloads(
        home: &Path,
        recipient: &str,
    ) -> Vec<(String, Option<String>, String, Option<String>)> {
        crate::inbox::drain(home, recipient)
            .into_iter()
            .map(|m| (m.from, m.kind, m.text, m.correlation_id))
            .collect()
    }

    /// PARITY (gate-ON): the bus `emit`→subscriber path delivers payloads
    /// byte-identical (from/kind/text/correlation) to the legacy direct enqueue.
    /// Exercises the REAL bus emit→fan-out→subscriber wiring.
    #[test]
    fn gate_on_emit_subscriber_matches_legacy_direct_enqueue() {
        let (dispatch_id, dispatcher, target, expected_kind, corr, elapsed, threshold) = (
            "di-parity",
            "lead",
            "dev",
            "task",
            Some("t-9"),
            900_i64,
            300_i64,
        );

        // Legacy direct delivery (the gate-OFF path).
        let home_legacy = tmp_home("parity-legacy");
        deliver_dispatch_idle(
            &home_legacy,
            dispatch_id,
            dispatcher,
            target,
            expected_kind,
            corr,
            elapsed,
            threshold,
            false,
        );

        // Bus emit→subscriber delivery (the gate-ON path) — real fan-out.
        let home_bus = tmp_home("parity-bus");
        let bus = crate::daemon::event_bus::EventBus::new();
        bus.subscribe(handle_event);
        bus.emit(
            &home_bus,
            crate::daemon::event_bus::EventKind::DispatchIdleExceeded {
                dispatcher: dispatcher.to_string(),
                target: target.to_string(),
                elapsed_secs: elapsed,
                dispatch_id: dispatch_id.to_string(),
                expected_kind: expected_kind.to_string(),
                threshold_secs: threshold,
                correlation_id: corr.map(String::from),
                long_running: false,
            },
        );

        let legacy = drained_payloads(&home_legacy, dispatcher);
        let viabus = drained_payloads(&home_bus, dispatcher);
        assert_eq!(
            legacy, viabus,
            "emit→subscriber payload must equal legacy direct enqueue"
        );
        assert!(
            !legacy.is_empty(),
            "parity test must actually deliver ≥1 message (else it proves nothing)"
        );
        std::fs::remove_dir_all(&home_legacy).ok();
        std::fs::remove_dir_all(&home_bus).ok();
    }

    /// #event-bus Step 2 (legacy-zero): `emit_exceeded_event` emits to the global
    /// bus; the registered subscriber delivers via `deliver_dispatch_idle` to the
    /// event's home (this test's home).
    #[test]
    fn emit_exceeded_event_delivers_via_bus() {
        let home = tmp_home("via-bus");
        let d = PendingDispatch {
            dispatch_id: "di-gateoff".into(),
            dispatcher: "lead".into(),
            target: "dev".into(),
            expected_kind: "task".into(),
            correlation_id: Some("t-1".into()),
            threshold_secs: 300,
            ..Default::default()
        };
        emit_exceeded_event(&home, &d, 900);
        assert!(
            !drained_payloads(&home, "lead").is_empty(),
            "#event-bus Option A: gate-off must deliver via the legacy path (no regression)"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
