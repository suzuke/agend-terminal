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
//! dispatches; team-specific automation lives in sibling modules
//! (see `fixup_nudge`).
//!
//! Pattern lineage: closest mirror is `decision_timeout` (threshold +
//! resolve + sidecar lifecycle); hook-site precedent is #870 in
//! `auto_release` (handle_send post-enqueue).

pub(crate) mod fixup_nudge;

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
}

/// #1658: how many consecutive not-working `scan_and_emit` ticks (past
/// threshold) the target must show before the dispatch-idle signal fires. A
/// single brief idle gap during active work is the common false-fire; requiring
/// a short streak filters it. Cost to a genuinely-stuck agent is at most
/// `(DEBOUNCE_SCANS - 1) * scan-cadence` of extra delay — negligible vs the
/// 600s threshold. NOTE: this debounces the EXISTING #1516 gate; it does not
/// add a missing gate. The structurally-correct fix (gate on output-recency, not
/// instantaneous state — `AgentSnapshot` has no activity timestamp today) is a
/// documented follow-up if the residual is still annoying after this + #1657.
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
    };
    let body = match serde_json::to_string_pretty(&payload) {
        Ok(s) => s,
        Err(_) => return None,
    };
    if crate::store::atomic_write(&pending_path(home, &dispatch_id), body.as_bytes()).is_err() {
        return None;
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
        if d.status != DispatchStatus::Pending {
            continue;
        }
        if d.correlation_id.as_deref() != Some(task_id) {
            continue;
        }
        let path = pending_path(home, &d.dispatch_id);
        if std::fs::remove_file(&path).is_ok() {
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
        if d.status != DispatchStatus::Pending {
            continue;
        }
        if d.target != instance_name {
            continue;
        }
        let path = pending_path(home, &d.dispatch_id);
        if std::fs::remove_file(&path).is_ok() {
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

/// Resolve a pending dispatch by `correlation_id` (NOT by sender —
/// decision_timeout's sender-keyed semantic is wrong here because a
/// single dispatcher can have multiple pending dispatches outstanding,
/// each with a distinct correlation_id). Returns the resolved
/// dispatch_id, or `None` if no matching pending entry exists.
pub(crate) fn mark_resolved(home: &Path, correlation_id: &str) -> Option<String> {
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
        current.status = DispatchStatus::Resolved;
        Some(id.clone())
    })
    .ok()
    .flatten()
    .flatten()
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

/// #1516: is `target` demonstrably working right now? True iff the fleet
/// snapshot reports its `agent_state` as `thinking` or `tool_use` — the
/// PTY-output-fed "actively generating / running a tool" signal (the same one
/// `health.rs` treats as productive). Pure for testability. Unknown target /
/// missing snapshot → `false` (don't suppress → fire as before; degrades to
/// the pre-#1516 behavior, never worse).
fn target_is_working(snapshot: Option<&crate::snapshot::FleetSnapshot>, target: &str) -> bool {
    snapshot
        .and_then(|s| s.agents.iter().find(|a| a.name == target))
        .map(|a| matches!(a.agent_state.as_str(), "thinking" | "tool_use"))
        .unwrap_or(false)
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

        // #1516: the dispatch-idle threshold is for "agent went silent and
        // never replied", but the idle timer only resets on a correlated
        // report — so a slow-but-progressing impl agent (heads-down coding /
        // generating, not sending updates) false-fired 5× the night this
        // landed. If the target is demonstrably WORKING (Thinking/ToolUse per
        // the snapshot), reset its clock and don't fire — it's making progress,
        // not stuck. A genuinely wedged agent stops producing output, so its
        // latched Thinking/ToolUse expires (LATCHED_STATE_EXPIRY, supervisor.rs)
        // → state flips to Idle/Hang → this gate releases → the watchdog fires
        // as designed. (The hang detector independently catches infinite-gen.)
        if target_is_working(snapshot.as_ref(), &d.target) {
            // #1658: the target is producing output — reset the debounce streak
            // so a later idle run starts fresh.
            if d.not_working_streak != 0 {
                set_not_working_streak(home, &d.dispatch_id, 0);
            }
            if let Some(corr) = d.correlation_id.as_deref() {
                let _ = refresh_issued_at(home, corr);
            }
            continue;
        }

        // #1018 (A): tick-time validation before firing. Stale sidecars
        // (placeholder correlation_id / deleted target instance / closed
        // task_id) are deleted silently — operator already received the
        // canonical signal via task board / instance lifecycle, no need
        // to surface a second-class "idle threshold" notification.
        if let Some(reason) = stale_sidecar_reason(home, &d) {
            let path = pending_path(home, &d.dispatch_id);
            let _ = std::fs::remove_file(&path);
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
        // scan-cadences of extra delay vs the 600s threshold).
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
            if !write_dispatch(home, &current) {
                tracing::warn!(dispatch_id = %d.dispatch_id, "dispatch-idle exceeded status write failed");
            }
            Some(current)
        };
        if let Some(current) = to_emit {
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
fn dispatch_idle_text(
    dispatch_id: &str,
    dispatcher: &str,
    target: &str,
    expected_kind: &str,
    correlation_id: Option<&str>,
    elapsed_secs: i64,
    threshold_secs: i64,
) -> String {
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
        corr = correlation_id.unwrap_or(""),
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
) {
    let text = dispatch_idle_text(
        dispatch_id,
        dispatcher,
        target,
        expected_kind,
        correlation_id,
        elapsed_secs,
        threshold_secs,
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
        "dispatch_idle_threshold_exceeded",
        text,
        Some(&corr),
        correlation_id,
    ) {
        tracing::warn!(error = %e, dispatcher, dispatch_id, "dispatch_idle: enqueue failed");
    }
}

/// #event-bus pattern #3: bus subscriber — deliver on a `DispatchIdleExceeded`
/// event (the gate-ON path). Registered once at daemon startup via [`register_subscriber`].
fn handle_event(event: &crate::daemon::event_bus::Event) {
    if let crate::daemon::event_bus::EventKind::DispatchIdleExceeded {
        dispatcher,
        target,
        elapsed_secs,
        dispatch_id,
        expected_kind,
        threshold_secs,
        correlation_id,
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
        );
    }
}

/// #event-bus pattern #3: register the dispatch_idle delivery subscriber on the
/// global bus. Call ONCE at daemon startup. Dormant while the bus is gate-off.
pub fn register_subscriber() {
    crate::daemon::event_bus::global().subscribe(handle_event);
}

fn emit_exceeded_event(home: &Path, d: &PendingDispatch, elapsed_secs: i64) {
    // Observability log runs regardless of the gate (it is not the notification).
    crate::event_log::log(
        home,
        "dispatch_idle_threshold_exceeded",
        &d.dispatcher,
        &format!(
            "dispatch_id={} target={} corr={} elapsed_secs={} threshold_secs={}",
            d.dispatch_id,
            d.target,
            d.correlation_id.as_deref().unwrap_or(""),
            elapsed_secs,
            d.threshold_secs,
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
        },
    );
}

/// Per-loop scheduler state.
#[derive(Debug, Default)]
pub(crate) struct DispatchIdleTracker {
    tick_count: u64,
}

impl DispatchIdleTracker {
    /// Per-tick entry. Increments the counter; on the throttled
    /// boundary, fires `scan_and_emit` and returns `true`. Returns
    /// `false` for all pre-boundary ticks.
    pub(crate) fn maybe_scan(&mut self, home: &Path) -> bool {
        self.tick_count = self.tick_count.saturating_add(1);
        if self.tick_count < TICKS_PER_SCAN {
            return false;
        }
        self.tick_count = 0;
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
        };
        std::fs::write(
            pending_path(home, &id),
            serde_json::to_string_pretty(&payload).unwrap(),
        )
        .unwrap();
        id
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
        let p_a = pending.iter().find(|p| p.dispatch_id == id_a).unwrap();
        let p_b = pending.iter().find(|p| p.dispatch_id == id_b).unwrap();
        assert_eq!(
            p_a.status,
            DispatchStatus::Resolved,
            "matched sidecar must flip"
        );
        assert_eq!(
            p_b.status,
            DispatchStatus::Pending,
            "unmatched sidecar from same dispatcher must NOT flip"
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
    /// the boilerplate `pub(crate) mod fixup_nudge;` declaration that
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
            if trimmed == "pub(crate) mod fixup_nudge;" {
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
            "instances:\n  fixup-dev-2:\n    backend: claude\n",
        )
        .unwrap();
        let task_id = "t-live-99";
        let tasks_dir = home.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join(format!("{task_id}.json")),
            serde_json::to_string_pretty(&serde_json::json!({
                "id": task_id,
                "status": "in_progress",
                "title": "live work",
                "assignee": "fixup-dev-2"
            }))
            .unwrap(),
        )
        .unwrap();
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
        let d = pending
            .iter()
            .find(|p| p.correlation_id.as_deref() == Some("t-1047-c"))
            .unwrap();
        assert_eq!(
            d.status,
            DispatchStatus::Resolved,
            "kind=report must set status=resolved"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── #1516: agent_state gate (don't fire while target is working) ──

    fn mk_agent_snapshot(name: &str, agent_state: &str) -> crate::snapshot::AgentSnapshot {
        crate::snapshot::AgentSnapshot {
            name: name.to_string(),
            backend_command: "opencode".to_string(),
            args: vec![],
            working_dir: None,
            submit_key: "\r".to_string(),
            health_state: "healthy".to_string(),
            agent_state: agent_state.to_string(),
        }
    }

    #[test]
    fn target_is_working_reads_snapshot_state_1516() {
        let snap = crate::snapshot::FleetSnapshot {
            timestamp: "t".to_string(),
            agents: vec![
                mk_agent_snapshot("worker", "thinking"),
                mk_agent_snapshot("tooler", "tool_use"),
                mk_agent_snapshot("idler", "idle"),
            ],
        };
        assert!(
            target_is_working(Some(&snap), "worker"),
            "thinking → working"
        );
        assert!(
            target_is_working(Some(&snap), "tooler"),
            "tool_use → working"
        );
        assert!(
            !target_is_working(Some(&snap), "idler"),
            "idle → not working"
        );
        assert!(
            !target_is_working(Some(&snap), "ghost"),
            "absent → not working"
        );
        assert!(
            !target_is_working(None, "worker"),
            "no snapshot → not working"
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
