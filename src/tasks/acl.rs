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
    super::board_router::resolve_current_project(home, caller) == board_project
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
