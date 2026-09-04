use serde_json::Value;
use std::path::Path;

use super::Task;

/// Check if an instance name is known (in fleet.yaml).
/// Returns true if fleet.yaml doesn't exist (no fleet = no restriction).
pub(super) fn instance_exists(home: &Path, name: &str) -> bool {
    let fleet_path = crate::fleet::fleet_yaml_path(home);
    if !fleet_path.exists() {
        return true; // no fleet config = no restriction
    }
    crate::fleet::FleetConfig::load(&fleet_path)
        .map(|c| c.instances.contains_key(name))
        .unwrap_or(true) // parse error = permissive
}

/// Check if caller is allowed to mutate a task (assignee or orchestrator).
/// Unassigned tasks can be mutated by anyone.
///
/// Sprint 23 P0: promoted from `fn` to `pub fn` to mirror
/// `decisions::can_mutate_decision` (PR #220, Sprint 21 Phase 2 D1). Public
/// visibility lets external auditors / tests verify the predicate without
/// going through `mutate_versioned`. Race-free invocation requires calling
/// from inside `mutate_versioned`'s locked closure (existing internal
/// callers at the `done` / `update` arms already do this).
///
/// **TOCTOU caveat** (Sprint 23 P0 r2 M2 doc strengthening): external
/// callers using read-only checks for diagnostics or tests are fine; callers
/// wanting to **act on the result** MUST do so inside `mutate_versioned`'s
/// locked closure to avoid time-of-check-to-time-of-use race on the
/// `assignee` field. A separate process / thread can change `assignee`
/// between an out-of-lock predicate call and a follow-up mutation, voiding
/// the gate.
///
/// **PR3 cutover note** — kept as a `pub` for any external auditor /
/// test still importing it. New in-tree handle arms use
/// [`can_mutate_record`] which operates on the replay-derived
/// `TaskRecord`. Marked `#[allow(dead_code)]` because the in-tree
/// usages migrated.
#[allow(dead_code)]
pub fn can_mutate_task(home: &Path, caller: &str, task: &Task) -> bool {
    match &task.assignee {
        None => true,
        Some(assignee) => {
            if assignee == caller {
                return true;
            }
            // Check if caller is orchestrator of assignee's team
            if crate::teams::is_orchestrator_of(home, caller, assignee) {
                return true;
            }
            // Check if assignee is a team name and caller is its orchestrator
            if let Ok(Some(orch)) = crate::teams::resolve_team_orchestrator(home, assignee) {
                if orch == caller {
                    return true;
                }
            }
            false
        }
    }
}

/// PR3 — predicate variant of [`can_mutate_task`] that operates on the
/// replay-derived record's `created_by` + `owner` fields. Behaviour
/// matches the legacy [`can_mutate_task`] surface (caller is owner OR
/// orchestrator-of-owner OR caller-is-orchestrator-and-owner-is-team).
///
/// **PR4 F2 absorbed (TOCTOU caveat, mirrors PR #235 r2 M2 doc on the
/// legacy `can_mutate_task`)**: the predicate reads from a `replay()`
/// snapshot taken **before** the read-out — there is no inherent lock on
/// the event log between this check and a follow-up `task_events::append`
/// emission. A separate process / thread can append a `Claimed` /
/// `OwnerAssigned` / `Released` event between an out-of-lock predicate
/// call and the caller's emission, voiding the gate. Production usage in
/// `handle`'s mutation arms accepts this small TOCTOU window: the
/// canonical authority is the event log itself, and conflicting emissions
/// resolve at replay time with the later seq winning. Auditors / tests
/// using this for diagnostic checks are fine.
/// System identities allowed to bypass normal ACL checks.
/// These are internal daemon modules that emit events on behalf of the system.
const SYSTEM_IDENTITIES: &[&str] = &[
    "system:auto_close",
    "system:auto_orphan",
    "system:branch_sweep",
    "system:overdue_sweep",
    "system:reclaim_usage_limit",
    "system:task_sweep",
];

/// Check if a caller is a recognized system identity.
pub fn is_system_identity(caller: &str) -> bool {
    SYSTEM_IDENTITIES.contains(&caller)
}

/// t-…-74 (decision d-…-22, Root ruling m-1087): the ONLY identity with
/// governance authority over a task's plan-ack metadata (`plan` +
/// `plan_ack_required`) is EXACTLY the task's creator (`created_by`) — or a
/// recognized system identity. This is deliberately narrower than
/// [`can_mutate_record`]: it is NOT the assignee/owner, NOT the team
/// orchestrator, and NOT any transitive dispatch-authority. Keeping it a
/// pure `created_by`/`system` check is the whole point — it must never expand
/// authority transitively (the gate this protects can otherwise be self-weakened
/// by the very agent it constrains). Operational authority (done / update /
/// non-governance metadata) stays with [`can_mutate_record`], unchanged.
pub(super) fn is_plan_governance_author(
    caller: &str,
    record: &crate::task_events::TaskRecord,
) -> bool {
    is_system_identity(caller) || record.created_by.0.as_str() == caller
}

pub(super) fn can_mutate_record(
    home: &Path,
    caller: &str,
    record: &crate::task_events::TaskRecord,
) -> bool {
    // B1: system identities pass ACL via explicit allow-list
    if is_system_identity(caller) {
        return true;
    }
    match record.owner.as_ref() {
        None => true,
        Some(owner) => {
            let owner_str = owner.0.as_str();
            if owner_str == caller {
                return true;
            }
            if crate::teams::is_orchestrator_of(home, caller, owner_str) {
                return true;
            }
            if let Ok(Some(orch)) = crate::teams::resolve_team_orchestrator(home, owner_str) {
                if orch == caller {
                    return true;
                }
            }
            false
        }
    }
}

/// #2127 Phase 1 / #2117 P3 — per-board mutation authority. May `caller` mutate
/// tasks on the board identified by `board_project`?
///
/// Semantics (aligned with [`can_mutate_record`]): a recognized system identity
/// bypasses; otherwise the caller's resolved project
/// (agent→team→`source_repo`→project, via
/// [`super::board_router::resolve_current_project`]) must equal `board_project`,
/// else **deny (fail-closed)**. A single-project fleet resolves every caller and
/// task to `DEFAULT_PROJECT`, so this never adds a denial until multi-board
/// (#2117) lands — byte-identical today.
///
/// Shared primitive: #2127's reclaim/reroute authorization (caller = the blocked
/// task owner, `board_project` = the task's board) and #2117 P3a's explicit
/// `project=<id>` mutation path both route through this — one ACL, no second
/// slug/resolution implementation. Re-exported `pub(crate)` via
/// `tasks::can_mutate_on_board` for callers outside the `tasks` module (the
/// reclaim per-tick handler).
pub(crate) fn can_mutate_on_board(home: &Path, caller: &str, board_project: &str) -> bool {
    if is_system_identity(caller) {
        return true;
    }
    // #2117 P3a (reviewer-4 #2133): use the FAIL-CLOSED resolver. The plain
    // `resolve_current_project` returns DEFAULT_PROJECT on a hard fleet.yaml read
    // failure, conflating it with a legitimate no-team caller — an ACL on that
    // would fail-OPEN to the default board. `_checked` returns `Err` on a hard
    // read failure (→ deny) while still returning `Ok(DEFAULT)` for a legitimate
    // no-team caller (→ single-project byte-identical).
    match super::board_router::resolve_current_project_checked(home, caller) {
        Ok(project) => project == board_project,
        Err(_) => false,
    }
}

/// #3511 — can the caller's OWN project be resolved at all?
///
/// The owner exception in `handler::cross_board_denied` must never fire while
/// the fleet is unreadable. [`can_mutate_on_board`] collapses two different
/// states into `false`: "resolved, and it is a different board" (a real
/// cross-board mismatch, which the exception is allowed to override) and
/// "could not resolve — hard fleet.yaml read failure" (#2117 P3a / #2133's
/// fail-CLOSED state, where nothing about the caller's authority is known).
/// Letting an exception through the second one would smuggle a mutation past a
/// deliberate hardening, so it is gated on a clean resolve. Exactly mirrors
/// `can_mutate_on_board`'s resolver so the two can never disagree about what
/// "unresolvable" means.
pub(super) fn caller_project_resolvable(home: &Path, caller: &str) -> bool {
    super::board_router::resolve_current_project_checked(home, caller).is_ok()
}

/// #2117 P3a (FM5 / board isolation): per-board mutation authority. A task
/// mutation resolves its board from the task_id, so a caller can name a task that
/// lives on ANOTHER project's board. Deny unless the caller acts in that board's
/// project — [`can_mutate_on_board`] (system identities bypass; a hard
/// fleet read failure fail-closes). Single-project → caller project == task board project (both DEFAULT)
/// → allow → byte-identical (no new denial). Returns `Some(error)` when denied.
// #2760: `board_project` is the caller's ALREADY-resolved authoritative board
// (from `super::load_routed`), so the mutation routes ONCE and this gate never
// re-resolves (nor silently defaults). `None` = allowed.
//
// #3511: `owner` is the row's assignee when the call site has already resolved
// the record AND the requested operation is settlement; it carries the ONE
// exception below. `None` means "this call grants no owner exception" — either
// the action has none at all, or (see [`update_settlement_owner`]) this
// particular request is not a pure cancellation.
/// #3511 — which `update` requests may use the board exception at all.
///
/// `update` is the GENERAL mutation verb: `priority`, `assignee`, `description`,
/// `tags` and `result` all ride the same call as `status`. Handing
/// [`cross_board_denied`] the owner unconditionally therefore turned "close your
/// own tail" into general write authority over a row on someone else's board —
/// caught in review by a behavioural probe that changed a foreign board's row to
/// `priority=urgent` and nothing else. `assignee` is the sharper version of the
/// same hole: it hands the work to a third party.
///
/// So the exception is OPERATION-aware, not just identity-aware. It is offered
/// only for a pure terminal cancellation, expressed as an ALLOW-list of the keys
/// such a request may carry. An allow-list is the point: a future mutable field
/// added to `update` is board-isolated by default and only becomes exception-
/// eligible when someone deliberately adds it here. Any extra key — even
/// alongside a genuine `status=cancelled` — puts the whole call back behind
/// board isolation, so nothing can ride along with a settlement.
///
/// `done` needs no equivalent: it is terminal by construction.
pub(super) fn update_settlement_owner<'a>(
    record: &'a crate::task_events::TaskRecord,
    args: &Value,
) -> Option<&'a str> {
    /// Routing / identity keys only — none of them mutate the row.
    const PURE_CANCELLATION_KEYS: &[&str] = &["action", "id", "task_id", "status", "project"];
    if args["status"].as_str() != Some("cancelled") {
        return None;
    }
    let only_settlement_keys = args.as_object().is_some_and(|obj| {
        obj.keys()
            .all(|k| PURE_CANCELLATION_KEYS.contains(&k.as_str()))
    });
    if !only_settlement_keys {
        return None;
    }
    record.owner.as_ref().map(|o| o.0.as_str())
}

pub(super) fn cross_board_denied(
    home: &Path,
    caller: &str,
    id: &str,
    board_project: &str,
    owner: Option<&str>,
) -> Option<Value> {
    if can_mutate_on_board(home, caller, board_project) {
        return None;
    }
    // #3511 — the one exception to board isolation: a row's OWN assignee may
    // settle it from another project's board. Assignment already crosses boards
    // but settlement did not, so a row created on board A and assigned to an
    // agent acting on board B was closable by nobody: the assignee cleared
    // `can_mutate_record` and was stopped here, while everyone on board A
    // cleared this gate and was stopped by `can_mutate_record`. Acting on a row
    // that NAMES you is not reaching into another board's work; it is collecting
    // your own tail.
    //
    // Deliberately EXACT-owner and nothing else — not the creator (granting a
    // non-owner mutation rights changes the OWNERSHIP model, a strictly larger
    // grant than this one), not the orchestrator-of-owner (transitive: it would
    // reach every board where a member happens to be assigned), and not an
    // unassigned row (no owner ⇒ no exception, so work can never be PULLED
    // across a boundary). The ownership ACL still runs immediately after this
    // gate at every call site that passes an owner.
    //
    // Gated on a clean project resolve: `can_mutate_on_board` returns `false`
    // both for a genuine cross-board mismatch AND for a hard fleet.yaml read
    // failure (#2133's fail-CLOSED state). The exception may override the
    // first — the row names the caller, which the task log proves without the
    // fleet — but never the second, where nothing about the caller's authority
    // is knowable. `fleet_read_failure_denies_mutation_fail_closed_2117_p3a`
    // is the pin: its caller IS the row's owner, so it fails without this.
    if owner == Some(caller) && caller_project_resolvable(home, caller) {
        return None;
    }
    Some(serde_json::json!({
        "error": format!(
            "cross-board mutation denied: task '{id}' lives on the '{board_project}' project \
             board but caller '{caller}' acts in a different project (board isolation, #2117 P3a)"
        )
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("agend-acl-board-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn can_mutate_on_board_system_identity_bypasses() {
        let home = tmp_home("sys");
        // A system identity is authorized on any board, even a non-default one.
        assert!(can_mutate_on_board(
            &home,
            "system:reclaim_usage_limit",
            "owner/repo"
        ));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn can_mutate_on_board_default_project_matches_and_mismatches() {
        let home = tmp_home("default");
        // No fleet/team → caller resolves to DEFAULT_PROJECT ("default").
        assert!(
            can_mutate_on_board(&home, "dev-a", "default"),
            "same (default) board → allowed (single-project byte-identical)"
        );
        assert!(
            !can_mutate_on_board(&home, "dev-a", "owner/other-repo"),
            "different board → fail-closed deny"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
