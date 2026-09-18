//! #3666: busy-parked task dispatches + idle redrive.
//!
//! A `delegate_task` (or unified-`send` `kind=task`) dispatch refused by the
//! busy gate used to vanish: the gate returned `Err({"busy": true, …})` with no
//! durable trace, the dispatch was never tracked, and the `inbox_only` fallback
//! never wakes an idle target — so the work stranded until a human noticed.
//!
//! The gate itself is untouched (still rejects; never interrupts a busy agent;
//! `force=true` still bypasses). What changes is what happens on rejection:
//!
//! 1. **park** — [`park_busy_dispatch`] (called by `handle_delegate_task` on a
//!    `busy:true` rejection) persists the full dispatch intent under
//!    `<home>/parked-dispatches/` and records a `dispatch_tracking` pending
//!    entry, so the stuck-dispatch sweep sees the refused work. The rejection
//!    keeps its `busy:true` shape and gains additive `parked:true` / `park_id`
//!    fields — existing shape pins stay green.
//! 2. **redrive** — [`scan_and_redrive`] (driven every ~60s by the
//!    `BusyParkRedrive` per-tick handler) re-runs the normal dispatch pipeline
//!    for each parked intent whose target is now idle: pre-checks → branch
//!    authority → auto-create → lease / CI-watch → inbox enqueue WITH the idle
//!    PTY hint (the wake-up `inbox_only` lacks). Still-busy intents re-park
//!    with an attempt counter; past [`MAX_PARKED_REDRIVE_ATTEMPTS`] (mirroring
//!    71c85cc8's transport-level parked redrive) they fail closed with a
//!    dispatcher notification instead of retrying forever.
//!
//! The dispatch-idle watchdog sidecar is armed at DELIVERY time, not park time:
//! the wait-for-reply clock starts when the target actually receives the work,
//! not while it is legitimately busy on something else.
//!
//! Out of scope (deliberate): `review_assignment` marker dispatches are never
//! parked (their exact-head workspace authority cannot be rebuilt from a tick),
//! and the `#1286` branch-dedup rejection keeps its pure-reject semantics —
//! only the generic busy gate parks.

use crate::identity::Sender;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Durable parked-dispatch store version. Bump only with a migration.
const SCHEMA_VERSION: u32 = 1;
const PARKED_DIR: &str = "parked-dispatches";

/// Busy-parked intents are re-driven when the target goes idle. A redrive that
/// collides busy again is re-parked with `attempts + 1` and failed closed past
/// this cap — never retried forever. Mirrors 71c85cc8's
/// `MAX_PARKED_REDRIVE_ATTEMPTS` for the transport-level parked queue.
const MAX_PARKED_REDRIVE_ATTEMPTS: u32 = 3;

/// A task dispatch refused by the busy gate, waiting for the target's idle
/// transition. `args` is the full delegate-task args snapshot MINUS the force
/// keys (a redrive is never forced — stripping them at park time makes that
/// structural, not a runtime check the redrive could forget).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ParkedDispatch {
    #[serde(default)]
    pub(crate) schema_version: u32,
    #[serde(default)]
    pub(crate) park_id: String,
    /// The `send.from` identity (who will be credited on redrive).
    #[serde(default)]
    pub(crate) dispatcher: String,
    /// The busy target the redrive waits on.
    #[serde(default)]
    pub(crate) target: String,
    /// `args["task"]` at park time (the unified-send `message` is already
    /// lifted into it by the time the delegate pipeline runs).
    #[serde(default)]
    pub(crate) task_text: String,
    /// Composed `[delegate_task]` body at park time (success_criteria / context
    /// / branch blocks included, NO force block — see struct doc). The redrive
    /// appends the ` (task id: …)` suffix once the effective task id is known.
    #[serde(default)]
    pub(crate) msg_text: String,
    /// Full delegate args snapshot (force keys stripped) for the redrive
    /// pipeline: branch, thread_id, expect_reply_within_secs, review_class,
    /// second_reviewer(+reason), plan_ack(+reason), next_after_ci, …
    #[serde(default)]
    pub(crate) args: Value,
    /// `args["task_id"]` at park time. `None` ⇒ the board task is auto-created
    /// at redrive time (same shape as the live auto-create path).
    #[serde(default)]
    pub(crate) task_id: Option<String>,
    /// Busy-gate blocker at park time (`current_task.id`) — diagnostic only.
    #[serde(default)]
    pub(crate) blocker_task_id: Option<String>,
    #[serde(default)]
    pub(crate) parked_at: String,
    /// Park collisions so far (the initial busy refusal is 0; each failed
    /// redrive adds 1). Past [`MAX_PARKED_REDRIVE_ATTEMPTS`] the intent fails
    /// closed with a dispatcher notification.
    #[serde(default)]
    pub(crate) attempts: u32,
    /// Last redrive failure, surfaced in the cap-exhausted notification so the
    /// dispatcher gets an actionable reason, not just "still busy".
    #[serde(default)]
    pub(crate) last_error: Option<String>,
}

pub(crate) fn parked_dir(home: &Path) -> PathBuf {
    home.join(PARKED_DIR)
}

fn parked_path(home: &Path, park_id: &str) -> PathBuf {
    parked_dir(home).join(format!("{park_id}.json"))
}

fn next_park_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let ts = chrono::Utc::now().format("%Y%m%d%H%M%S%6f");
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("park-{ts}-{seq}")
}

/// Read all parked intents. Forward-compat: skips unknown schema versions.
pub(crate) fn list_parked(home: &Path) -> Vec<ParkedDispatch> {
    let dir = parked_dir(home);
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
        let Ok(park) = serde_json::from_str::<ParkedDispatch>(&content) else {
            continue;
        };
        if park.schema_version != SCHEMA_VERSION {
            continue;
        }
        out.push(park);
    }
    out.sort_by(|a, b| a.parked_at.cmp(&b.parked_at));
    out
}

fn write_parked(home: &Path, park: &ParkedDispatch) -> bool {
    match serde_json::to_string_pretty(park) {
        Ok(body) => {
            crate::store::atomic_write(&parked_path(home, &park.park_id), body.as_bytes()).is_ok()
        }
        Err(_) => false,
    }
}

fn remove_parked(home: &Path, park_id: &str) {
    let _ = std::fs::remove_file(parked_path(home, park_id));
}

/// The busy-gate predicate, shared with the redrive idle check: the target is
/// busy while it owns any claimed/in-progress board task (same lifecycle the
/// gate in `dispatch.rs` inspects). Fail-closed: an unreadable task view
/// counts as busy — the redrive must never deliver blind, mirroring the gate's
/// `#3141` fail-closed rejection.
pub(crate) fn target_is_busy(home: &Path, target: &str) -> bool {
    let all_tasks = match crate::tasks::list_all_strict(home) {
        Ok(tasks) => tasks,
        Err(_) => return true,
    };
    all_tasks.iter().any(|t| {
        t.assignee.as_deref() == Some(target)
            && matches!(
                t.status,
                crate::task_events::TaskStatus::Claimed
                    | crate::task_events::TaskStatus::InProgress
            )
    })
}

/// Park predicate, kept as a pure function so the marker/force exclusions are
/// unit-testable without driving a full dispatch: only a generic busy-gate
/// rejection parks, never a marker dispatch (its exact-head workspace
/// authority cannot be rebuilt from a tick) and never anything else.
pub(crate) fn should_park(args: &Value, rejection: &Value) -> bool {
    rejection
        .get("busy")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        && !args["review_assignment"].as_bool().unwrap_or(false)
}

/// Park a busy-refused dispatch and augment the rejection with the park
/// receipt. `msg_text` is the composed non-forced `[delegate_task]` body
/// (built by the caller, which owns composition); `task_text` is the raw
/// `args["task"]` for the redrive-time pre-checks.
///
/// Best-effort AFTER the durable write: if the park store write fails, the
/// original rejection is returned unparked (fail-closed — the dispatcher still
/// gets the busy signal and can re-dispatch or force).
pub(crate) fn park_busy_dispatch(
    home: &Path,
    dispatcher: &Sender,
    target: &str,
    task_text: &str,
    msg_text: &str,
    args: &Value,
    rejection: &Value,
) -> Value {
    let task_id = args["task_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from);
    // Dedup: a retry of the same refused dispatch while still parked must not
    // stack duplicate park rows + tracking entries. Keyed on the effective
    // task when present, else on (dispatcher, target, text).
    if let Some(existing) = list_parked(home).into_iter().find(|p| {
        p.target == target
            && p.dispatcher == dispatcher.as_str()
            && match (&p.task_id, &task_id) {
                (Some(a), Some(b)) => a == b,
                (None, None) => p.task_text == task_text,
                _ => false,
            }
    }) {
        return parked_rejection(rejection, &existing.park_id, true);
    }
    // A redrive is never forced: strip the force keys from the stored snapshot
    // so the redrive pipeline cannot re-arm them even if the args change shape.
    let mut stored_args = args.clone();
    if let Some(obj) = stored_args.as_object_mut() {
        obj.remove("force");
        obj.remove("force_reason");
    }
    let park = ParkedDispatch {
        schema_version: SCHEMA_VERSION,
        park_id: next_park_id(),
        dispatcher: dispatcher.as_str().to_string(),
        target: target.to_string(),
        task_text: task_text.to_string(),
        msg_text: msg_text.to_string(),
        args: stored_args,
        task_id: task_id.clone(),
        blocker_task_id: rejection["current_task"]["id"].as_str().map(String::from),
        parked_at: chrono::Utc::now().to_rfc3339(),
        attempts: 0,
        last_error: None,
    };
    if !write_parked(home, &park) {
        tracing::warn!(
            target,
            dispatcher = dispatcher.as_str(),
            "busy-park store write failed — returning the bare busy rejection"
        );
        return rejection.clone();
    }
    // Visibility: the refused dispatch enters the dispatch-tracking pending set
    // so the stuck-dispatch sweep surfaces it (the "at minimum, make it
    // visible" rung of #3666). The dispatch-idle watchdog sidecar is armed at
    // DELIVERY time instead — the wait-for-reply clock starts when the target
    // actually receives the work.
    let status = if args["no_report_expected"].as_bool() == Some(true) {
        "no_report_expected"
    } else {
        "pending"
    };
    crate::dispatch_tracking::track_dispatch(
        home,
        crate::dispatch_tracking::DispatchEntry {
            task_id: task_id.clone(),
            from: dispatcher.as_str().to_string(),
            to: target.to_string(),
            from_id: crate::agent::resolve_instance(home, dispatcher.as_str())
                .ok()
                .map(|(id, _)| id.full()),
            to_id: crate::agent::resolve_instance(home, target)
                .ok()
                .map(|(id, _)| id.full()),
            delegated_at: chrono::Utc::now().to_rfc3339(),
            status: status.to_string(),
        },
    );
    tracing::info!(
        target,
        dispatcher = dispatcher.as_str(),
        park_id = %park.park_id,
        task_id = ?task_id,
        "busy-gated dispatch parked — auto-redrive on the target's idle transition"
    );
    parked_rejection(rejection, &park.park_id, false)
}

/// The busy rejection shape is preserved byte-for-byte; the park receipt is
/// purely additive so existing `busy:true` pins keep passing.
fn parked_rejection(rejection: &Value, park_id: &str, duplicate: bool) -> Value {
    let mut out = rejection.clone();
    if let Some(obj) = out.as_object_mut() {
        obj.insert("parked".into(), json!(true));
        obj.insert("park_id".into(), json!(park_id));
        if duplicate {
            obj.insert("park_duplicate".into(), json!(true));
        }
        obj.insert(
            "redrive".into(),
            json!(
                "parked for auto-redrive when the target goes idle — \
                 no need to re-dispatch or force"
            ),
        );
    }
    out
}

/// A successful delivery of (`target`, `task_id`) through ANY path retires the
/// parked intent for it — the work is no longer stranded, so a later scan must
/// not deliver it a second time (e.g. the dispatcher re-dispatched manually
/// while parked, then the target idles). Called from the send-time
/// `track_dispatch` choke point (covers normal + force re-dispatches); the
/// redrive's own delivery skips that choke point by construction, so it can
/// never self-invalidate. No-op when no park matches.
pub(crate) fn invalidate_parked_for_delivery(home: &Path, target: &str, task_id: &str) {
    if task_id.is_empty() {
        return;
    }
    for park in list_parked(home) {
        if park.target == target && park.task_id.as_deref() == Some(task_id) {
            remove_parked(home, &park.park_id);
            tracing::info!(
                target,
                park_id = %park.park_id,
                task_id,
                "busy-park intent retired — task delivered through another path"
            );
        }
    }
}

/// Per-tick scan outcome (also the regression-test assertion surface).
#[derive(Debug, Default)]
pub(crate) struct ScanStats {
    pub scanned: usize,
    pub delivered: usize,
    pub reparked: usize,
    pub dropped: usize,
}

/// Redrive every parked intent whose target is now idle. One parked intent is
/// removed from the store BEFORE its delivery attempt text is built so a
/// delivery is never composed twice; a failed attempt re-parks with
/// `attempts + 1` (fail-closed past the cap, mirroring 71c85cc8).
pub(crate) fn scan_and_redrive(home: &Path) -> ScanStats {
    let mut stats = ScanStats::default();
    for park in list_parked(home) {
        stats.scanned += 1;
        match redrive_one(home, &park) {
            RedriveOutcome::Delivered => stats.delivered += 1,
            RedriveOutcome::Reparked => stats.reparked += 1,
            RedriveOutcome::Dropped => stats.dropped += 1,
        }
    }
    stats
}

enum RedriveOutcome {
    Delivered,
    Reparked,
    Dropped,
}

/// Board liveness for a parked task id: live (still worth delivering),
/// terminal (work is over — drop silently), or gone/moved (reassigned away
/// from the park target, or the route is unprovable).
enum TaskLiveness {
    Live,
    Terminal,
    Reassigned,
    RouteUnprovable,
}

fn parked_task_liveness(home: &Path, park: &ParkedDispatch) -> TaskLiveness {
    let Some(task_id) = park.task_id.as_deref() else {
        return TaskLiveness::Live;
    };
    match crate::tasks::load_routed(home, task_id) {
        Ok(routed) => {
            if routed.task.assignee.as_deref() != Some(park.target.as_str()) {
                return TaskLiveness::Reassigned;
            }
            if matches!(
                routed.task.status,
                crate::task_events::TaskStatus::Done
                    | crate::task_events::TaskStatus::Cancelled
                    | crate::task_events::TaskStatus::Superseded
                    | crate::task_events::TaskStatus::Verified
            ) {
                TaskLiveness::Terminal
            } else {
                TaskLiveness::Live
            }
        }
        Err(crate::tasks::TaskRouteError::NotFound) => TaskLiveness::Terminal,
        Err(_) => TaskLiveness::RouteUnprovable,
    }
}

fn redrive_one(home: &Path, park: &ParkedDispatch) -> RedriveOutcome {
    // Staleness first: never deliver work whose board task is over, moved, or
    // gone. Terminal/NotFound drops are silent (the terminal cleanup already
    // cleared the tracking rows + sidecars); a reassignment pages the
    // dispatcher because the new owner never received this dispatch.
    match parked_task_liveness(home, park) {
        TaskLiveness::Terminal => {
            if let Some(task_id) = park.task_id.as_deref() {
                crate::dispatch_tracking::remove_all_for_task(home, task_id);
            }
            remove_parked(home, &park.park_id);
            return RedriveOutcome::Dropped;
        }
        TaskLiveness::Reassigned => {
            remove_parked(home, &park.park_id);
            notify_dispatcher(
                home,
                park,
                &format!(
                    "parked dispatch for task {} dropped — the task was reassigned away \
                     from {}; re-dispatch if it is still needed",
                    park.task_id.as_deref().unwrap_or("(untracked)"),
                    park.target
                ),
            );
            return RedriveOutcome::Dropped;
        }
        TaskLiveness::RouteUnprovable => {
            // The route cannot be proven right now (ambiguous/unreadable board) —
            // fail OPEN by keeping the intent, burning one attempt like any other
            // deferred redrive so a permanently-broken route still surfaces.
            return repark_with_error(home, park, "task route unprovable; retrying");
        }
        TaskLiveness::Live => {}
    }
    // Idle check (same predicate as the busy gate — the TOCTOU between this
    // read and the enqueue below is benign: the message row lands in the inbox
    // either way, and a genuinely-busy target triages it on its next drain).
    if target_is_busy(home, &park.target) {
        return repark_with_error(home, park, "target still busy");
    }
    match deliver_parked(home, park) {
        Ok(()) => {
            remove_parked(home, &park.park_id);
            notify_dispatcher(
                home,
                park,
                &format!(
                    "parked dispatch for task {} auto-delivered to {} on its idle transition",
                    park.task_id.as_deref().unwrap_or("(untracked)"),
                    park.target
                ),
            );
            RedriveOutcome::Delivered
        }
        Err(error) => repark_with_error(home, park, &error),
    }
}

/// Failed redrive: attempts + 1, kept while under the cap, dropped + dispatcher
/// notified past it. The last error travels in the record so the cap-exhausted
/// notification names the real cause.
fn repark_with_error(home: &Path, park: &ParkedDispatch, error: &str) -> RedriveOutcome {
    let attempts = park.attempts.saturating_add(1);
    if attempts > MAX_PARKED_REDRIVE_ATTEMPTS {
        remove_parked(home, &park.park_id);
        notify_dispatcher(
            home,
            park,
            &format!(
                "parked dispatch for task {} dropped after {attempts} redrive attempts \
                 (last error: {error}) — please re-dispatch or force with a reason",
                park.task_id.as_deref().unwrap_or("(untracked)"),
            ),
        );
        tracing::warn!(
            target = %park.target,
            park_id = %park.park_id,
            attempts,
            error,
            "busy-park redrive attempts exhausted — intent dropped, dispatcher notified"
        );
        return RedriveOutcome::Dropped;
    }
    let mut next = park.clone();
    next.attempts = attempts;
    next.last_error = Some(error.to_string());
    if write_parked(home, &next) {
        RedriveOutcome::Reparked
    } else {
        // The re-park write failed: keep the OLD row (still on disk — this
        // function never removed it) so the intent is not lost; count it as
        // reparked without the attempt bump.
        tracing::warn!(
            target = %park.target,
            park_id = %park.park_id,
            "busy-park re-park write failed — prior row preserved"
        );
        RedriveOutcome::Reparked
    }
}

/// Passive FYI to the dispatcher (plain inbox enqueue — never an idle hint, so
/// a busy dispatcher is never interrupted by its own parked intent resolving).
fn notify_dispatcher(home: &Path, park: &ParkedDispatch, body: &str) {
    let text = format!("[busy-park] {body} (park_id={})", park.park_id);
    let mut msg = crate::inbox::InboxMessage::new_system("system:busy-park", "update", text)
        .with_delivery_mode("inbox_fallback");
    if let Some(task_id) = park.task_id.as_deref() {
        msg = msg.with_correlation_id(task_id);
    }
    if let Err(error) = crate::inbox::enqueue(home, &park.dispatcher, msg) {
        tracing::warn!(
            dispatcher = %park.dispatcher,
            park_id = %park.park_id,
            %error,
            "busy-park dispatcher notification enqueue failed"
        );
    }
}

// Redrive pipeline (pre-checks → authority → auto-create → lease/watch →
// enqueue), split per the 750-LOC handler ceiling.
mod redrive;
use redrive::deliver_parked;

#[cfg(test)]
mod tests;
