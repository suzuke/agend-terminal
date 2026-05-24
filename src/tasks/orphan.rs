use serde_json::Value;
use std::path::Path;

/// #808: clear ownership on tasks owned by a deleted instance so the
/// ACL gate (`can_mutate_record`) doesn't lock survivors out. Called
/// from `full_delete_instance` after fleet-yaml membership cleanup.
///
/// Replays the event log, enumerates tasks where `owner == owner_name`
/// AND status is still "live" (Open/Claimed/InProgress/Blocked), and
/// emits one `OwnerAssigned { owner: None }` per affected task via
/// `append_batch` so the entire orphan transition lands under one
/// fsync. Done/Cancelled tasks are skipped — their terminal state
/// already disables ACL writes, so re-orphaning them would only churn
/// the event log.
///
/// Concurrency: the caller (`full_delete_instance`) issues
/// `api::method::DELETE` BEFORE invoking this helper, so the doomed
/// instance is already dead and cannot claim new tasks mid-flight.
/// The TOCTOU window between `replay()` and `append_batch()` is
/// acceptable — a sweeper or operator race that lands later still
/// wins at replay (later seq overrides).
///
/// Returns the count of orphaned tasks on success (0 when nothing
/// matched), or an `Err` carrying the underlying replay / append
/// failure detail for the caller to surface into its audit chain.
pub fn orphan_tasks_for_owner(home: &Path, owner_name: &str) -> Result<usize, String> {
    use crate::task_events::{InstanceName, TaskEvent, TaskStatus};

    let state = crate::task_events::replay(home).map_err(|e| e.to_string())?;
    let affected: Vec<crate::task_events::TaskId> = state
        .tasks
        .values()
        .filter(|r| r.owner.as_ref().map(|o| o.0 == owner_name).unwrap_or(false))
        .filter(|r| {
            matches!(
                r.status,
                TaskStatus::Open
                    | TaskStatus::Claimed
                    | TaskStatus::InProgress
                    | TaskStatus::Blocked
            )
        })
        .map(|r| r.id.clone())
        .collect();
    if affected.is_empty() {
        return Ok(0);
    }
    let count = affected.len();
    let emitter = InstanceName::from("system:auto_orphan");
    let events: Vec<TaskEvent> = affected
        .into_iter()
        .map(|id| TaskEvent::OwnerAssigned {
            task_id: id,
            by: emitter.clone(),
            owner: None,
            routed_to: None,
        })
        .collect();
    crate::task_events::append_batch(home, &emitter, events)
        .map(|_| count)
        .map_err(|e| e.to_string())
}

/// #829: classify a single owner string against the live runtime
/// registry + the fleet.yaml `instances:` set. Two ghost classes are
/// distinguished:
///
/// - **Strict**: owner is in NEITHER the live registry NOR fleet.yaml.
///   The owning instance is fully gone (never came back, was deleted
///   without cascading the orphan, or pre-existed before #828
///   shipped). Safe to auto-orphan at boot — no operator decision
///   needed because the owner is verifiably absent.
/// - **Soft**: owner IS in fleet.yaml but not in the live registry.
///   Could be a misconfigured agent, a transient binding lag during
///   boot, or an agent that's about to spawn but hasn't yet. NOT safe
///   to auto-orphan — dry-run + tracing::warn so the operator can
///   surface the case via `task action=sweep` if they decide.
#[derive(Debug, PartialEq, Eq)]
pub enum OwnerClassification {
    Live,
    Strict,
    Soft,
}

pub fn classify_owner(
    owner: &str,
    live: &std::collections::HashSet<String>,
    fleet_instances: &std::collections::HashSet<String>,
) -> OwnerClassification {
    if live.contains(owner) {
        OwnerClassification::Live
    } else if fleet_instances.contains(owner) {
        OwnerClassification::Soft
    } else {
        OwnerClassification::Strict
    }
}

/// #829: scan results, split into auto-apply (strict) vs dry-run
/// (soft) buckets. Owners are kept as separate keys so the boot
/// orchestrator can batch one `orphan_tasks_for_owner` call per
/// strict owner — each call lands a single event-log fsync (mirrors
/// #828's per-member cascade pattern).
///
/// Ordering: `BTreeMap` for deterministic iteration order — the tests
/// pattern-match on the result so stable ordering matters more than
/// the constant-factor `HashMap` win.
///
/// Reused by #830 `task action=health` (dispatch sequencing): same
/// scan, same classification, just `apply=false` to feed the health
/// metrics surface. The scan fn is therefore `pub`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct OrphanScanResult {
    pub strict: std::collections::BTreeMap<String, Vec<crate::task_events::TaskId>>,
    pub soft: std::collections::BTreeMap<String, Vec<crate::task_events::TaskId>>,
}

/// #829 pure scan. Walks `state.tasks` and classifies each non-
/// terminal task's owner via [`classify_owner`]. Terminal-status tasks
/// (Done / Cancelled) are skipped — their ACL is already disabled at
/// the event-log layer, so re-orphaning would be noise.
///
/// `live` MUST come from `crate::api::call(LIST)` — the canonical
/// runtime registry. `fleet_instances` MUST come from
/// `fleet::FleetConfig::load(...).instances.keys()` — the
/// configuration-time set. Caller responsibility to populate both;
/// this fn is pure so it's testable without a daemon.
///
/// #829: orphan-owner sweeper. Sibling to
/// `crate::binding::reconcile_orphans` + `crate::worktree_pool::
/// reconcile_orphan_leases` in `src/bootstrap/mod.rs`. Best-effort,
/// tracing-audited, no return value.
///
/// Strict-case auto-apply (owner ∉ fleet.yaml ∧ ∉ live registry):
/// emits one `system:auto_orphan` batched event per ghost owner via
/// `orphan_tasks_for_owner`. Idempotent — re-running on an already-
/// orphaned task is a no-op at the event-log replay layer.
///
/// Soft-case dry-run (owner ∈ fleet.yaml ∧ ∉ live registry): emits a
/// `tracing::warn!` listing the candidates. NO mutation — preserves
/// operator judgment for the "agent hasn't booted yet" race that
/// `auto_start_fleet` will resolve seconds after bootstrap returns.
///
/// Fix A note (#829 follow-up): this wrapper is the post-boot /
/// periodic entrypoint that still resolves `live` via
/// `api::call(LIST)`. The boot caller now uses
/// [`reconcile_orphan_owners_with_live`] directly with an empty live
/// set — see that fn for the bootstrap-time rationale. Keeping the
/// `api::call` path here means a future periodic-sweep tick can reuse
/// the same orchestrator without re-implementing live-set fetching.
///
/// Skip-on-api-fail: if `api::call(LIST)` returns `None`, the wrapper
/// early-returns BEFORE touching any state. Better to leave residue
/// for the next periodic tick than to over-orphan against a stale
/// (empty) live picture in a post-boot context where agents are
/// genuinely meant to be running.
#[allow(dead_code)] // Fix A: reserved for periodic-sweep callers (no wired site today)
pub fn reconcile_orphan_owners(home: &Path) {
    let Some(live) = crate::runtime::list_live_agents(home) else {
        tracing::info!("#829: api::call(LIST) unavailable — skipping orphan-owner sweep");
        return;
    };
    reconcile_orphan_owners_with_live(home, &live);
}

/// #829 Fix A: boot-path entrypoint that takes `live` explicitly.
///
/// Pre-Fix A, `reconcile_orphan_owners` ran from `bootstrap::prepare`
/// BEFORE `api::serve` bound the Unix socket (the socket is opened
/// later in `daemon::run_with_prepared`), so `api::call(LIST)` always
/// returned `None` and the sweep early-exited every boot. Operator
/// surfaced the symptom on 2026-05-18 (45 accumulated ghost owners).
///
/// At bootstrap time `live = ∅` is provably correct: no agents have
/// spawned yet (auto-start runs later in `run_with_prepared`). The
/// bootstrap caller now passes `HashSet::new()` directly, severing
/// the broken `api::call` chain. The classifier still does the right
/// thing in this context:
///
/// - Owner ∈ fleet.yaml → Soft (warn, don't kill — correctly defers
///   the "agent not yet spawned" case).
/// - Owner ∉ fleet.yaml → Strict (auto-apply — catches the ghost
///   buildup on next daemon boot).
///
/// Body factored out of `reconcile_orphan_owners`; this fn is the
/// shared core used by both the boot path (empty live) and the
/// periodic path (api-derived live). Strict/soft semantics unchanged.
pub fn reconcile_orphan_owners_with_live(home: &Path, live: &std::collections::HashSet<String>) {
    let fleet_instances: std::collections::HashSet<String> =
        crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
            .ok()
            .map(|c| c.instances.keys().cloned().collect())
            .unwrap_or_default();
    let state = match crate::task_events::replay(home) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "#829: task_events replay failed — skipping orphan-owner sweep"
            );
            return;
        }
    };

    let result = scan_orphan_candidates(&state, live, &fleet_instances);
    if result.strict.is_empty() && result.soft.is_empty() {
        tracing::debug!("#829: orphan-owner sweep clean — no ghost owners detected");
        return;
    }

    // Strict bucket → auto-apply via orphan_tasks_for_owner.
    for (owner, task_ids) in &result.strict {
        match orphan_tasks_for_owner(home, owner) {
            Ok(n) => tracing::info!(
                owner = %owner,
                tasks = task_ids.len(),
                orphaned = n,
                "#829: orphan-owner sweep applied (strict — owner fully gone)"
            ),
            Err(e) => tracing::warn!(
                owner = %owner,
                error = %e,
                "#829: orphan-owner sweep failed for strict candidate"
            ),
        }
    }

    // Soft bucket → dry-run + warn (no mutation).
    if !result.soft.is_empty() {
        let soft_summary: Vec<(String, usize)> = result
            .soft
            .iter()
            .map(|(owner, ids)| (owner.clone(), ids.len()))
            .collect();
        tracing::warn!(
            ?soft_summary,
            "#829: detected tasks owned by configured-but-not-live agents \
             (in fleet.yaml ∧ ∉ live registry); operator may run `task action=sweep` \
             to apply orphan cleanup"
        );
    }
}

pub fn scan_orphan_candidates(
    state: &crate::task_events::TaskBoardState,
    live: &std::collections::HashSet<String>,
    fleet_instances: &std::collections::HashSet<String>,
) -> OrphanScanResult {
    use crate::task_events::TaskStatus;
    let mut result = OrphanScanResult::default();
    for record in state.tasks.values() {
        if matches!(record.status, TaskStatus::Done | TaskStatus::Cancelled) {
            continue;
        }
        let Some(owner) = record.owner.as_ref() else {
            continue;
        };
        let bucket = match classify_owner(owner.0.as_str(), live, fleet_instances) {
            OwnerClassification::Strict => &mut result.strict,
            OwnerClassification::Soft => &mut result.soft,
            OwnerClassification::Live => continue,
        };
        bucket
            .entry(owner.0.clone())
            .or_default()
            .push(record.id.clone());
    }
    result
}

/// #830 hardcoded severity thresholds. Defaults are tuned for the
/// current operator's fleet shape (10s of agents, low-100s of
/// tasks); revisit if v1.5 brings demand for config-ability.
const OVER_30D_WARN_THRESHOLD: usize = 5;
const STALE_BLOCKED_WARN_THRESHOLD: usize = 10;

/// #830: structured-recommendations health response. Pure pub fn so
/// tests can drive it with synthesized inputs (no daemon spawn).
///
/// `state` — `crate::task_events::replay(home)` output
/// `live` — `crate::runtime::list_live_agents(home)` — `None` when
///   the daemon is unreachable (surfaced as a degraded-mode hint in
///   the response).
/// `fleet_instances` — keys from `crate::fleet::FleetConfig::load
///   (...).instances` — the configured set (vs `live` runtime set).
///
/// Reuses `scan_orphan_candidates` (#829) for the ghost_owners
/// section so the boot sweeper and the health surface share one
/// classification pass. Sorted output where feasible
/// (`BTreeMap`/`sort_unstable`) for stable test pinning.
pub fn build_health_response(
    state: &crate::task_events::TaskBoardState,
    live: Option<&std::collections::HashSet<String>>,
    fleet_instances: &std::collections::HashSet<String>,
) -> Value {
    use crate::task_events::TaskStatus;
    use chrono::DateTime;
    let now = chrono::Utc::now();

    // ── Status counts + non-terminal collector ──
    let mut by_status: std::collections::BTreeMap<&'static str, usize> =
        std::collections::BTreeMap::new();
    let mut non_terminal_ages_days: Vec<i64> = Vec::new();
    for record in state.tasks.values() {
        let key = match record.status {
            TaskStatus::Open => "open",
            TaskStatus::Claimed => "claimed",
            TaskStatus::InProgress => "in_progress",
            TaskStatus::Blocked => "blocked",
            TaskStatus::Done => "done",
            TaskStatus::Cancelled => "cancelled",
            TaskStatus::Verified => "verified",
        };
        *by_status.entry(key).or_insert(0) += 1;
        if !matches!(record.status, TaskStatus::Done | TaskStatus::Cancelled) {
            if let Ok(dt) = DateTime::parse_from_rfc3339(&record.created_at) {
                let age = now.signed_duration_since(dt.with_timezone(&chrono::Utc));
                non_terminal_ages_days.push(age.num_days());
            }
        }
    }
    let total_all: usize = by_status.values().copied().sum();
    let total_terminal = by_status.get("done").copied().unwrap_or(0)
        + by_status.get("cancelled").copied().unwrap_or(0);
    let total_non_terminal = total_all.saturating_sub(total_terminal);

    // ── Ghost owners (reuse #829 scan_orphan_candidates) ──
    let empty_live = std::collections::HashSet::new();
    let live_set = live.unwrap_or(&empty_live);
    let scan = scan_orphan_candidates(state, live_set, fleet_instances);
    let strict_count: usize = scan.strict.values().map(|v| v.len()).sum();
    let soft_count: usize = scan.soft.values().map(|v| v.len()).sum();
    let strict_owners: Vec<&String> = scan.strict.keys().collect();
    let soft_owners: Vec<&String> = scan.soft.keys().collect();

    // ── Stale claims (replicates sweep_overdue_claimed's predicate,
    //    read-only — no mutation) ──
    let mut stale_claim_ids: Vec<String> = Vec::new();
    for record in state.tasks.values() {
        if record.status != TaskStatus::Claimed {
            continue;
        }
        let Some(due) = &record.due_at else {
            continue;
        };
        let Ok(due_utc) = DateTime::parse_from_rfc3339(due) else {
            continue;
        };
        if now > due_utc.with_timezone(&chrono::Utc) {
            stale_claim_ids.push(record.id.0.clone());
        }
    }
    stale_claim_ids.sort_unstable();

    // ── Age aggregates ──
    non_terminal_ages_days.sort_unstable();
    let oldest_days = non_terminal_ages_days.last().copied().unwrap_or(0);
    let median_days = if non_terminal_ages_days.is_empty() {
        0
    } else {
        non_terminal_ages_days[non_terminal_ages_days.len() / 2]
    };
    let over_30d_count = non_terminal_ages_days.iter().filter(|d| **d > 30).count();
    let over_90d_count = non_terminal_ages_days.iter().filter(|d| **d > 90).count();

    // ── Recommendations ──
    let blocked_count = by_status.get("blocked").copied().unwrap_or(0);
    let mut recommendations: Vec<Value> = Vec::new();
    if let Some(rec) = rec_ghost_owners_strict(&scan, strict_count) {
        recommendations.push(rec);
    }
    if let Some(rec) = rec_ghost_owners_soft(&scan, soft_count) {
        recommendations.push(rec);
    }
    if let Some(rec) = rec_stale_claims(&stale_claim_ids) {
        recommendations.push(rec);
    }
    if let Some(rec) = rec_over_30d(over_30d_count) {
        recommendations.push(rec);
    }
    if let Some(rec) = rec_blocked_overflow(blocked_count) {
        recommendations.push(rec);
    }

    serde_json::json!({
        "as_of": now.to_rfc3339(),
        "live_agents_available": live.is_some(),
        "totals": {
            "all": total_all,
            "non_terminal": total_non_terminal,
            "terminal": total_terminal,
        },
        "by_status": by_status,
        "ghost_owners": {
            "strict_count": strict_count,
            "strict_owners": strict_owners,
            "soft_count": soft_count,
            "soft_owners": soft_owners,
        },
        "stale_claims": {
            "overdue_count": stale_claim_ids.len(),
            "overdue_ids": stale_claim_ids,
        },
        "age": {
            "oldest_non_terminal_days": oldest_days,
            "median_non_terminal_days": median_days,
            "over_30d_count": over_30d_count,
            "over_90d_count": over_90d_count,
        },
        "recommendations": recommendations,
    })
}

/// Trigger: any `scan.strict` entries — owners verifiably gone
/// (∉ fleet.yaml ∧ ∉ live). Next-action hint mentions #829 auto-orphan
/// on next daemon boot (the same scan that produced these candidates
/// will fire automatically) so operator can either wait or run
/// `task action=sweep` to apply now.
fn rec_ghost_owners_strict(scan: &OrphanScanResult, count: usize) -> Option<Value> {
    if scan.strict.is_empty() {
        return None;
    }
    let candidate_ids: Vec<String> = scan
        .strict
        .values()
        .flat_map(|ids| ids.iter().map(|t| t.0.clone()))
        .collect();
    Some(serde_json::json!({
        "code": "ghost_owners_strict",
        "severity": "warn",
        "hint": format!(
            "{count} task(s) owned by fully-gone agents (∉ fleet.yaml ∧ ∉ live); \
             next daemon boot will auto-orphan via #829 — or run `task action=sweep` now"
        ),
        "candidate_ids": candidate_ids,
    }))
}

/// Trigger: any `scan.soft` entries — owners in fleet.yaml but not
/// in the live runtime registry. Could be transient (agent
/// restarting) or a real misconfig; operator decides.
fn rec_ghost_owners_soft(scan: &OrphanScanResult, count: usize) -> Option<Value> {
    if scan.soft.is_empty() {
        return None;
    }
    let candidate_ids: Vec<String> = scan
        .soft
        .values()
        .flat_map(|ids| ids.iter().map(|t| t.0.clone()))
        .collect();
    Some(serde_json::json!({
        "code": "ghost_owners_soft",
        "severity": "info",
        "hint": format!(
            "{count} task(s) owned by configured-but-not-live agents \
             (∈ fleet.yaml ∧ ∉ live); could be transient — check `binding_state` \
             or run `task action=sweep` if persistent"
        ),
        "candidate_ids": candidate_ids,
    }))
}

/// Trigger: any tasks past their `due_at`. Daemon's
/// `sweep_overdue_claimed` already auto-releases these on its tick,
/// so this is info-level (operator just sees the in-flight state).
fn rec_stale_claims(ids: &[String]) -> Option<Value> {
    if ids.is_empty() {
        return None;
    }
    Some(serde_json::json!({
        "code": "stale_claims",
        "severity": "info",
        "hint": format!(
            "{} claim(s) past due_at; daemon's overdue sweep will release on next tick",
            ids.len()
        ),
        "candidate_ids": ids.to_vec(),
    }))
}

/// Trigger: more than `OVER_30D_WARN_THRESHOLD` non-terminal tasks
/// older than 30 days. Suggests `task action=sweep` for board
/// hygiene.
fn rec_over_30d(count: usize) -> Option<Value> {
    if count <= OVER_30D_WARN_THRESHOLD {
        return None;
    }
    Some(serde_json::json!({
        "code": "over_30d",
        "severity": "warn",
        "hint": format!(
            "{count} non-terminal task(s) older than 30 days; \
             consider `task action=sweep` for stale-task review"
        ),
    }))
}

/// Trigger: more than `STALE_BLOCKED_WARN_THRESHOLD` tasks in the
/// blocked state. Indicates accumulating dependency backlog or
/// unattended `block_reason` causes.
fn rec_blocked_overflow(count: usize) -> Option<Value> {
    if count <= STALE_BLOCKED_WARN_THRESHOLD {
        return None;
    }
    Some(serde_json::json!({
        "code": "blocked_overflow",
        "severity": "warn",
        "hint": format!(
            "{count} task(s) currently in `blocked` state; \
             check `block_reason` per task and unblock or cancel"
        ),
    }))
}
