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

/// The redrive pipeline: pre-checks → branch authority → auto-create →
/// lease / CI-watch → idle-hint enqueue → post-delivery bookkeeping. Mirrors
/// `handle_delegate_task`'s phase order (validate → create → lease → send →
/// track) minus review-assignment markers (never parked) and minus the runtime
/// transport (tick context has no registry — the inbox row + idle hint IS the
/// delivery, exactly as `execute_send`'s registry path leaves it for the
/// delivery worker).
fn deliver_parked(home: &Path, park: &ParkedDispatch) -> Result<(), String> {
    let dispatcher = Sender::new(&park.dispatcher)
        .ok_or_else(|| format!("dispatcher '{}' is not a valid sender", park.dispatcher))?;
    let target = park.target.as_str();
    let mut args = park.args.clone();
    // Re-run the pre-send gates against CURRENT state: a redrive that still
    // conflicts (busy again, branch dedup, test-name gate) is a redrive
    // failure, not a delivery — the caller re-parks it.
    let checks =
        super::dispatch::run_dispatch_pre_checks(home, &dispatcher, &args, target, &park.task_text)
            .map_err(|rejection| {
                rejection["error"]
                    .as_str()
                    .or(rejection["suggestion"].as_str())
                    .unwrap_or("dispatch pre-checks still reject")
                    .to_string()
            })?;
    if checks.review_assignment {
        return Err("review-assignment markers are never parked".to_string());
    }
    // Branch authority (same typed preflight as the live path; branchless
    // dispatches skip it identically). Owned early: `args` is mutated below
    // (task_id fill) and must not stay borrowed across it.
    let branch: Option<String> = args["branch"]
        .as_str()
        .filter(|b| !b.is_empty())
        .map(String::from);
    let branch_ref = branch.as_deref();
    let mut review_class_token: Option<&str> = None;
    let mut resolved_class: Option<crate::daemon::pr_state::ReviewClass> = None;
    if branch.is_some() {
        let task_id = args["task_id"].as_str().unwrap_or("");
        let class = crate::tasks::governance::resolve_dispatch_authority(
            home,
            task_id,
            &args,
            checks.second_reviewer,
        )
        .map_err(|refusal| {
            refusal["error"]
                .as_str()
                .unwrap_or("branch authority unresolved")
                .to_string()
        })?;
        review_class_token = Some(class.as_token());
        resolved_class = Some(class);
    }
    // Board task: reuse the parked id, or auto-create (same field shape as the
    // live `maybe_auto_create_task` — keep in sync with it).
    let mut effective_task_id = park.task_id.clone();
    if effective_task_id.is_none() {
        let created = auto_create_redrive_task(home, &args, &dispatcher, target, &checks)?;
        if branch.is_some() && created.review_class.is_none() {
            return Err("auto-created branch task has no durable review_class".to_string());
        }
        if resolved_class.is_none() {
            resolved_class = created.review_class;
            review_class_token = None;
        }
        effective_task_id = Some(created.task_id);
    }
    let task_id_str = effective_task_id.as_deref().unwrap_or("");
    if !task_id_str.is_empty() {
        args["task_id"] = json!(task_id_str);
    }
    // Lease / CI-watch for branch dispatches (bind:false arms the watch
    // without binding — same fork as the live path).
    if let (Some(branch), Some(class)) = (branch_ref, resolved_class) {
        let next_after_ci =
            crate::daemon::ci_watch::watch_state::normalize_next_after_ci(&args["next_after_ci"]);
        if args["bind"].as_bool() == Some(false) {
            let mut watch_args = args.clone();
            watch_args["task_id"] = json!(task_id_str);
            watch_args["review_class"] = json!(class.as_token());
            let watch_result = super::super::ci::handle_watch_ci(home, &watch_args, target);
            if watch_result["error"].is_string() {
                return Err(watch_result["error"]
                    .as_str()
                    .unwrap_or("ci watch arm failed")
                    .to_string());
            }
            tracing::info!(
                %target,
                %branch,
                "busy-park redrive armed typed CI watch without binding (bind:false)"
            );
        } else {
            let expected_head = args["expected_head"].as_str();
            super::super::dispatch_hook::dispatch_auto_bind_lease_with_source_and_chain(
                home,
                target,
                task_id_str,
                branch,
                args["repository"].as_str(),
                None,
                expected_head,
                &next_after_ci,
                review_class_token,
                true,
            )
            .map_err(|e| format!("dispatch rejected: {e}"))?;
        }
    }
    // Delivery: the composed body + task-id suffix (same suffix the live path
    // appends post-create), enqueued WITH the idle hint so an idle target is
    // woken — the wake-up path `inbox_only` lacks.
    let mut text = park.msg_text.clone();
    if !task_id_str.is_empty() {
        text.push_str(&format!(" (task id: {task_id_str})"));
    }
    let mut msg = crate::inbox::InboxMessage {
        from: format!("from:{}", dispatcher.as_str()),
        from_id: crate::agent::resolve_instance(home, dispatcher.as_str())
            .ok()
            .map(|(id, _)| id.full()),
        text,
        kind: Some("task".to_string()),
        timestamp: chrono::Utc::now().to_rfc3339(),
        delivery_mode: Some("transport_queued_unverified".to_string()),
        task_id: effective_task_id.clone(),
        correlation_id: effective_task_id.clone(),
        thread_id: args["thread_id"].as_str().map(String::from),
        parent_id: args["parent_id"].as_str().map(String::from),
        terminal: args["terminal"].as_bool(),
        eta_minutes: args["eta_minutes"].as_u64().map(|v| v as u32),
        reporting_cadence: args["reporting_cadence"].as_str().map(String::from),
        worktree_binding_required: args["worktree_binding_required"].as_bool(),
        delivery_nonce: args["delivery_nonce"].as_str().map(String::from),
        ..Default::default()
    };
    crate::inbox::stamp_message_id(&mut msg);
    crate::inbox::enqueue_with_idle_hint(home, target, msg)
        .map_err(|e| format!("redrive enqueue failed: {e}"))?;
    // Post-delivery bookkeeping (mirrors `track_dispatch`'s task-kind branch
    // minus the dispatch_tracking insert — the park-time entry already covers
    // it, and a second insert would stack a duplicate row).
    if let Some(branch) = branch_ref {
        if let Some(tid) = effective_task_id.as_deref() {
            let _ = crate::tasks::link_branch_to_task(home, tid, branch);
        }
    }
    let _ = crate::daemon::ci_handoff_track::resolve_delegated(
        home,
        dispatcher.as_str(),
        effective_task_id.as_deref(),
        branch_ref,
    );
    // Arm the wait-for-reply watchdog FROM DELIVERY (see module doc). Explicit
    // `expect_reply_within_secs` wins; `no_report_expected` opts out — same
    // resolution as the live `track_dispatch`.
    if args["no_report_expected"].as_bool() != Some(true) {
        let threshold = crate::daemon::dispatch_idle::team_nudge::resolve_threshold_for_dispatch(
            home,
            dispatcher.as_str(),
            args["expect_reply_within_secs"].as_i64(),
        );
        if let Some(threshold) = threshold {
            if crate::daemon::dispatch_idle::record_dispatch(
                home,
                dispatcher.as_str(),
                target,
                effective_task_id.as_deref(),
                "task",
                threshold,
            )
            .is_none()
            {
                tracing::warn!(
                    target,
                    park_id = %park.park_id,
                    "busy-park redrive delivered but the dispatch_idle sidecar was not written"
                );
            }
        }
    }
    tracing::info!(
        %target,
        dispatcher = dispatcher.as_str(),
        park_id = %park.park_id,
        task_id = ?effective_task_id,
        "busy-parked dispatch redelivered on idle transition"
    );
    Ok(())
}

struct RedriveCreatedTask {
    task_id: String,
    review_class: Option<crate::daemon::pr_state::ReviewClass>,
}

/// Auto-create mirror for task-less parked intents. Field shape matches
/// `comms_delegate::maybe_auto_create_task` — keep the two in sync (title
/// truncation, assignee/branch/project/priority/plan-ack/review-class/
/// governing-decision).
fn auto_create_redrive_task(
    home: &Path,
    args: &Value,
    dispatcher: &Sender,
    target: &str,
    checks: &super::dispatch::DispatchPreChecks,
) -> Result<RedriveCreatedTask, String> {
    let auto_title = args["message"]
        .as_str()
        .or_else(|| args["task"].as_str())
        .unwrap_or("(untitled dispatch)")
        .chars()
        .take(80)
        .collect::<String>();
    let target_project = crate::tasks::resolve_target_project(home, target);
    let create_args = json!({
        "action": "create",
        "title": auto_title,
        "assignee": target,
        "branch": args["branch"].as_str(),
        "priority": "normal",
        "project": target_project,
        "plan_ack_required": checks.plan_ack_required,
        "plan_ack_reason": args["plan_ack_reason"].as_str(),
        "review_class": args["review_class"].as_str(),
        "governing_decision_id": args["governing_decision_id"].as_str(),
    });
    let task_result = crate::tasks::handle(home, dispatcher.as_str(), &create_args);
    if task_result["error"].is_string() {
        return Err(task_result["error"]
            .as_str()
            .unwrap_or("auto-create task failed")
            .to_string());
    }
    let Some(id) = task_result["id"].as_str() else {
        return Err("auto-created task did not return a durable id".to_string());
    };
    let created_review_class = task_result["task"]["metadata"]["review_class"]
        .as_str()
        .and_then(
            |raw| match crate::daemon::pr_state::ReviewClass::parse_fail_closed(Some(raw)) {
                crate::daemon::pr_state::ReviewClass::Single => {
                    Some(crate::daemon::pr_state::ReviewClass::Single)
                }
                crate::daemon::pr_state::ReviewClass::Dual => {
                    Some(crate::daemon::pr_state::ReviewClass::Dual)
                }
                crate::daemon::pr_state::ReviewClass::Unresolved => None,
            },
        );
    crate::daemon::task_progress::touch(
        home,
        id,
        crate::daemon::task_progress::ProgressSource::Broadcast,
    );
    Ok(RedriveCreatedTask {
        task_id: id.to_string(),
        review_class: created_review_class,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp_home(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("agend-busy-park-{}-{tag}-{id}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    /// Create a task owned by `assignee` (Open). Returns the task id.
    fn seed_open_task(home: &Path, caller: &str, title: &str, assignee: &str) -> String {
        let created = crate::tasks::handle(
            home,
            caller,
            &json!({
                "action": "create",
                "title": title,
                "assignee": assignee,
            }),
        );
        created["id"].as_str().expect("seeded task id").to_string()
    }

    /// Create + claim: the target owns claimed work, i.e. the busy gate fires.
    fn seed_claimed_task(home: &Path, caller: &str, title: &str, assignee: &str) -> String {
        let tid = seed_open_task(home, caller, title, assignee);
        let claim = crate::tasks::handle(home, assignee, &json!({"action": "claim", "id": &tid}));
        assert_eq!(claim["status"], "claimed", "claim must succeed: {claim}");
        tid
    }

    fn done_task(home: &Path, caller: &str, task_id: &str) {
        let done = crate::tasks::handle(home, caller, &json!({"action": "done", "id": task_id}));
        assert!(done["error"].is_null(), "done must succeed: {done}");
    }

    fn delegate_args(target: &str, task: &str, task_id: Option<&str>) -> Value {
        let mut args = json!({
            "instance": target,
            "task": task,
            "expect_reply_within_secs": 600,
        });
        if let Some(tid) = task_id {
            args["task_id"] = json!(tid);
        }
        args
    }

    fn delegate(home: &Path, sender: &Option<Sender>, args: &Value) -> Value {
        super::super::super::comms::handle_delegate_task(home, args, sender, None)
    }

    fn inbox_texts(home: &Path, name: &str) -> Vec<(Option<String>, String, Option<String>)> {
        crate::inbox::drain(home, name)
            .into_iter()
            .map(|m| (m.kind.clone(), m.text.clone(), m.task_id.clone()))
            .collect()
    }

    #[test]
    fn should_park_only_generic_busy_non_marker() {
        let busy = json!({"busy": true, "current_task": {"id": "t-a"}});
        // Busy + ordinary ⇒ park.
        assert!(should_park(&json!({"instance": "dev"}), &busy));
        // Busy + review-assignment marker ⇒ never park (exact-head workspace
        // authority cannot be rebuilt from a tick).
        assert!(!should_park(&json!({"review_assignment": true}), &busy));
        // Non-busy rejections (dedup / validation) ⇒ never park.
        assert!(!should_park(
            &json!({"instance": "dev"}),
            &json!({"error": "dispatch rejected: dev already has active task t on branch b"})
        ));
        assert!(!should_park(
            &json!({"instance": "dev"}),
            &json!({"error": "force=true requires a non-empty 'force_reason'"})
        ));
        assert!(!should_park(
            &json!({"instance": "dev"}),
            &json!({"ok": true})
        ));
    }

    #[test]
    fn target_is_busy_matches_gate_predicate() {
        let home = tmp_home("idle-check");
        assert!(!target_is_busy(&home, "dev"), "fresh home is idle");
        // Open work does NOT make the target busy (gate only sees
        // claimed/in-progress) — the redrive must fire for it.
        let open = seed_open_task(&home, "lead", "open work", "dev");
        assert!(!target_is_busy(&home, "dev"), "open task is not busy");
        let claim = crate::tasks::handle(&home, "dev", &json!({"action": "claim", "id": &open}));
        assert_eq!(claim["status"], "claimed");
        assert!(target_is_busy(&home, "dev"), "claimed task is busy");
        done_task(&home, "dev", &open);
        assert!(!target_is_busy(&home, "dev"), "done task is idle again");
        // Unreadable task view fails CLOSED (never redrive blind).
        std::fs::write(home.join("boards"), "not a directory").unwrap();
        assert!(
            target_is_busy(&home, "dev"),
            "unreadable view counts as busy"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #3666 core: a busy-refused dispatch parks with an additive receipt and
    /// becomes visible to the stuck-dispatch sweep (no more silent loss).
    #[test]
    fn busy_reject_parks_with_additive_receipt() {
        let home = tmp_home("park-receipt");
        let blocker = seed_claimed_task(&home, "lead", "in-flight work", "dev");
        let parked_task = seed_open_task(&home, "lead", "stranded work", "dev");
        let sender = Some(Sender::new("lead").unwrap());

        let out = delegate(
            &home,
            &sender,
            &delegate_args("dev", "do stranded work", Some(&parked_task)),
        );

        // The busy shape is preserved byte-for-byte…
        assert_eq!(out["busy"], true, "{out}");
        assert!(out["current_task"]["id"].is_string(), "{out}");
        assert!(out["options"].is_array(), "{out}");
        assert!(out["suggestion"].is_string(), "{out}");
        assert_eq!(
            out["current_task"]["id"].as_str(),
            Some(blocker.as_str()),
            "blocker identity preserved: {out}"
        );
        // …and the park receipt is purely additive.
        assert_eq!(out["parked"], true, "{out}");
        let park_id = out["park_id"].as_str().expect("park_id: {out}");
        assert!(!park_id.is_empty());

        let parks = list_parked(&home);
        assert_eq!(parks.len(), 1, "exactly one park row: {parks:?}");
        assert_eq!(parks[0].park_id, park_id);
        assert_eq!(parks[0].dispatcher, "lead");
        assert_eq!(parks[0].target, "dev");
        assert_eq!(parks[0].task_id.as_deref(), Some(parked_task.as_str()));
        assert_eq!(parks[0].blocker_task_id.as_deref(), Some(blocker.as_str()));
        assert_eq!(parks[0].attempts, 0);
        // A redrive is never forced — the stored snapshot cannot re-arm it.
        assert!(parks[0].args.get("force").is_none(), "{:?}", parks[0].args);
        assert!(parks[0].args.get("force_reason").is_none());

        // Visibility rung: the refused dispatch enters dispatch_tracking, so
        // `sweep_stuck` (and the reclaim reroute) sees it.
        let taken = crate::dispatch_tracking::take_pending_dispatchers_to(&home, "dev");
        assert!(
            taken
                .iter()
                .any(|e| e.task_id.as_deref() == Some(parked_task.as_str())
                    && e.from == "lead"
                    && e.status == "pending"),
            "refused dispatch must be tracked: {taken:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Retrying the same refused dispatch while parked dedups to the existing
    /// intent (no stacked rows, no stacked tracking entries).
    #[test]
    fn duplicate_park_returns_existing_receipt() {
        let home = tmp_home("park-dedup");
        seed_claimed_task(&home, "lead", "in-flight work", "dev");
        let parked_task = seed_open_task(&home, "lead", "stranded work", "dev");
        let sender = Some(Sender::new("lead").unwrap());
        let args = delegate_args("dev", "do stranded work", Some(&parked_task));

        let first = delegate(&home, &sender, &args);
        let second = delegate(&home, &sender, &args);
        assert_eq!(first["parked"], true);
        assert_eq!(second["parked"], true);
        assert_eq!(
            first["park_id"], second["park_id"],
            "dedup: {first} vs {second}"
        );
        assert_eq!(second["park_duplicate"], true);
        assert_eq!(list_parked(&home).len(), 1);
        std::fs::remove_dir_all(&home).ok();
    }

    /// #3666 regression: refused-dispatch → target idle → automatic delivery
    /// with the idle-hint wake, watchdog armed from delivery, dispatcher told.
    #[test]
    fn refused_dispatch_redrives_on_idle_transition() {
        let home = tmp_home("redrive-e2e");
        let blocker = seed_claimed_task(&home, "lead", "in-flight work", "dev");
        let parked_task = seed_open_task(&home, "lead", "stranded work", "dev");
        let sender = Some(Sender::new("lead").unwrap());

        let out = delegate(
            &home,
            &sender,
            &delegate_args("dev", "do stranded work", Some(&parked_task)),
        );
        assert_eq!(out["parked"], true, "{out}");

        // While the target is still busy the scan re-parks (attempt-capped,
        // never delivered, never dropped).
        let stats = scan_and_redrive(&home);
        assert_eq!(stats.scanned, 1);
        assert_eq!(stats.reparked, 1);
        assert_eq!(stats.delivered, 0);
        assert_eq!(list_parked(&home).len(), 1);
        assert_eq!(list_parked(&home)[0].attempts, 1);
        assert!(
            inbox_texts(&home, "dev").is_empty(),
            "no delivery while busy"
        );

        // The blocker completes → the target goes idle → the next scan
        // delivers automatically.
        done_task(&home, "dev", &blocker);
        assert!(!target_is_busy(&home, "dev"));
        let stats = scan_and_redrive(&home);
        assert_eq!((stats.scanned, stats.delivered, stats.dropped), (1, 1, 0));

        // The target's inbox holds the task row (kind=task, idle-hint wake —
        // not the wake-less inbox_only fallback).
        let rows = inbox_texts(&home, "dev");
        let delivered = rows.iter().find(|(kind, _, tid)| {
            kind.as_deref() == Some("task") && tid.as_deref() == Some(parked_task.as_str())
        });
        assert!(
            delivered.is_some(),
            "target must receive the parked task on idle: {rows:?}"
        );
        let (_, text, _) = delivered.unwrap();
        assert!(text.contains("do stranded work"), "{text}");
        assert!(
            text.contains(&format!("(task id: {parked_task})")),
            "{text}"
        );

        // The park row is gone (exactly-once per intent).
        assert!(list_parked(&home).is_empty());

        // The wait-for-reply watchdog is armed FROM DELIVERY…
        let sidecars = crate::daemon::dispatch_idle::list_pending(&home);
        assert!(
            sidecars.iter().any(
                |d| d.correlation_id.as_deref() == Some(parked_task.as_str())
                    && d.target == "dev"
                    && d.dispatcher == "lead"
            ),
            "dispatch_idle sidecar must arm on delivery: {sidecars:?}"
        );
        // …and the dispatcher gets a passive FYI (plain inbox row, no wake).
        let lead_rows = inbox_texts(&home, "lead");
        assert!(
            lead_rows
                .iter()
                .any(|(_, text, _)| text.contains("[busy-park]")
                    && text.contains("auto-delivered")
                    && text.contains("dev")),
            "dispatcher must be told about the redrive: {lead_rows:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// A parked intent whose board task closes before the redrive drops
    /// silently (terminal cleanup already cleared tracking + sidecars).
    #[test]
    fn stale_parked_task_drops_without_delivery() {
        let home = tmp_home("stale-drop");
        seed_claimed_task(&home, "lead", "in-flight work", "dev");
        let parked_task = seed_open_task(&home, "lead", "stranded work", "dev");
        let sender = Some(Sender::new("lead").unwrap());

        let out = delegate(
            &home,
            &sender,
            &delegate_args("dev", "do stranded work", Some(&parked_task)),
        );
        assert_eq!(out["parked"], true, "{out}");

        // The parked task itself closes (claimed + done by its owner).
        let claim = crate::tasks::handle(
            &home,
            "dev",
            &json!({"action": "claim", "id": &parked_task}),
        );
        assert_eq!(claim["status"], "claimed");
        done_task(&home, "dev", &parked_task);

        let stats = scan_and_redrive(&home);
        assert_eq!((stats.scanned, stats.dropped), (1, 1));
        assert!(list_parked(&home).is_empty());
        // Drained rows, if any, must not carry the stale task delivery.
        let rows = inbox_texts(&home, "dev");
        assert!(
            rows.iter().all(|(kind, _, tid)| {
                !(kind.as_deref() == Some("task") && tid.as_deref() == Some(parked_task.as_str()))
            }),
            "stale task must never deliver: {rows:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Still-busy intents fail closed past the cap: dropped + dispatcher paged
    /// (mirrors 71c85cc8's attempt cap), never retried forever.
    #[test]
    fn redrive_attempts_cap_fails_closed_with_notify() {
        let home = tmp_home("attempt-cap");
        seed_claimed_task(&home, "lead", "in-flight work", "dev");
        let parked_task = seed_open_task(&home, "lead", "stranded work", "dev");
        let sender = Some(Sender::new("lead").unwrap());

        let out = delegate(
            &home,
            &sender,
            &delegate_args("dev", "do stranded work", Some(&parked_task)),
        );
        assert_eq!(out["parked"], true, "{out}");

        for expected_attempts in 1..=MAX_PARKED_REDRIVE_ATTEMPTS {
            let stats = scan_and_redrive(&home);
            assert_eq!((stats.reparked, stats.dropped), (1, 0));
            assert_eq!(list_parked(&home)[0].attempts, expected_attempts);
        }
        // One more scan exhausts the cap: dropped, dispatcher notified, target
        // never received the work.
        let stats = scan_and_redrive(&home);
        assert_eq!((stats.dropped, stats.delivered), (1, 0));
        assert!(list_parked(&home).is_empty());
        assert!(inbox_texts(&home, "dev").is_empty(), "never delivered");
        let lead_rows = inbox_texts(&home, "lead");
        assert!(
            lead_rows
                .iter()
                .any(|(_, text, _)| text.contains("dropped after")
                    && text.contains("re-dispatch or force")),
            "cap exhaustion must page the dispatcher: {lead_rows:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// `force=true` still bypasses the gate with zero park involvement.
    #[test]
    fn force_dispatch_never_parks() {
        let home = tmp_home("force-no-park");
        seed_claimed_task(&home, "lead", "in-flight work", "dev");
        let parked_task = seed_open_task(&home, "lead", "urgent work", "dev");
        let sender = Some(Sender::new("lead").unwrap());

        let out = delegate(
            &home,
            &sender,
            &json!({
                "instance": "dev",
                "task": "urgent override",
                "task_id": parked_task,
                "force": true,
                "force_reason": "operator-approved interruption",
            }),
        );
        assert!(out.get("parked").is_none(), "force must not park: {out}");
        assert!(out.get("busy").is_none(), "force must bypass busy: {out}");
        assert!(list_parked(&home).is_empty());
        std::fs::remove_dir_all(&home).ok();
    }

    /// A review-assignment marker refused by the busy gate keeps its pure
    /// reject (never parked — exact-head authority is tick-unrebuildable).
    #[test]
    fn marker_busy_reject_never_parks() {
        let home = tmp_home("marker-no-park");
        seed_claimed_task(&home, "lead", "in-flight work", "dev");
        let sender = Some(Sender::new("lead").unwrap());

        let out = delegate(
            &home,
            &sender,
            &json!({
                "instance": "dev",
                "task": "review the PR",
                "task_id": "t-other",
                "review_assignment": true,
            }),
        );
        assert_eq!(out["busy"], true, "{out}");
        assert!(out.get("parked").is_none(), "marker must not park: {out}");
        assert!(list_parked(&home).is_empty());
        std::fs::remove_dir_all(&home).ok();
    }

    /// A task-less dispatch parks the intent and auto-creates the board task
    /// at redrive time (same field shape as the live auto-create).
    #[test]
    fn taskless_park_auto_creates_on_redrive() {
        let home = tmp_home("taskless");
        let blocker = seed_claimed_task(&home, "lead", "in-flight work", "dev");
        let sender = Some(Sender::new("lead").unwrap());

        let out = delegate(
            &home,
            &sender,
            &delegate_args("dev", "ad-hoc stranded work", None),
        );
        assert_eq!(out["parked"], true, "{out}");
        assert!(list_parked(&home)[0].task_id.is_none());

        done_task(&home, "dev", &blocker);
        let stats = scan_and_redrive(&home);
        assert_eq!((stats.scanned, stats.delivered), (1, 1));

        let rows = inbox_texts(&home, "dev");
        let delivered = rows.iter().find(|(kind, text, _)| {
            kind.as_deref() == Some("task") && text.contains("ad-hoc stranded work")
        });
        assert!(delivered.is_some(), "{rows:?}");
        let (_, _, tid) = delivered.unwrap();
        let tid = tid.clone().expect("auto-created task id on the row");
        // The auto-created board task exists and is owned by the target.
        let routed = crate::tasks::load_routed(&home, &tid).expect("board task exists");
        assert_eq!(routed.task.assignee.as_deref(), Some("dev"));
        std::fs::remove_dir_all(&home).ok();
    }

    /// A delivery through another path retires the park (no double delivery):
    /// driving the send-time `track_dispatch` choke point for the same
    /// (target, task) must invalidate the parked intent.
    #[test]
    fn manual_delivery_invalidates_park() {
        let home = tmp_home("invalidate");
        seed_claimed_task(&home, "lead", "in-flight work", "dev");
        let parked_task = seed_open_task(&home, "lead", "stranded work", "dev");
        let sender = Some(Sender::new("lead").unwrap());

        let out = delegate(
            &home,
            &sender,
            &delegate_args("dev", "do stranded work", Some(&parked_task)),
        );
        assert_eq!(out["parked"], true, "{out}");
        assert_eq!(list_parked(&home).len(), 1);

        // The same task goes out through another path (e.g. a force dispatch
        // that reached `execute_send`): the send-time track choke point fires…
        let req = crate::agent_ops::messaging::SendRequest {
            from: "lead".to_string(),
            target: "dev".to_string(),
            text: "do stranded work".to_string(),
            kind: Some("task".to_string()),
            thread_id: None,
            parent_id: None,
            correlation_id: None,
            reviewed_head: None,
            report_purpose: None,
            code_review: None,
            eta_minutes: None,
            reporting_cadence: None,
            worktree_binding_required: None,
            expect_reply_within_secs: None,
            terminal: None,
            no_report_expected: None,
            delivery_nonce: None,
            task_id: Some(parked_task.clone()),
            force_meta: None,
            provenance: None,
            branch: None,
            broadcast_context: None,
            priority: None,
        };
        let msg = crate::inbox::InboxMessage {
            from: "from:lead".to_string(),
            text: "do stranded work".to_string(),
            kind: Some("task".to_string()),
            timestamp: chrono::Utc::now().to_rfc3339(),
            task_id: Some(parked_task.clone()),
            correlation_id: Some(parked_task.clone()),
            ..Default::default()
        };
        crate::agent_ops::messaging::track_dispatch(&home, &req, "lead", "dev", &msg);
        // …and the parked intent is retired so the idle scan cannot deliver
        // the same work a second time.
        assert!(list_parked(&home).is_empty());
        std::fs::remove_dir_all(&home).ok();
    }
}
