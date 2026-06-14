use super::acl::{can_mutate_task, is_system_identity};
use super::orphan::{
    build_health_response, classify_owner, release_inprogress_orphans_with_live,
    scan_inprogress_orphans, scan_orphan_candidates, OwnerClassification,
};
use super::sweep;
use super::*;

fn tmp_home(name: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-tasks-test-{}-{}-{}",
        std::process::id(),
        name,
        id
    ));
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// Sprint 23 P0 r2 F2 helper — minimal Task with assignee for
/// `can_mutate_task` predicate tests. Mirrors decisions.rs
/// `make_test_decision` fixture pattern.
fn make_test_task(assignee: Option<&str>) -> Task {
    Task {
        id: "t-test-fixture".into(),
        title: "fixture".into(),
        description: String::new(),
        status: crate::task_events::TaskStatus::Open,
        priority: crate::task_events::TaskPriority::Normal,
        assignee: assignee.map(String::from),
        routed_to: None,
        created_by: "test".into(),
        depends_on: Vec::new(),
        result: None,
        created_at: "2026-04-27T00:00:00Z".into(),
        updated_at: "2026-04-27T00:00:00Z".into(),
        due_at: None,
        branch: None,
        started_at: None,
        eta_secs: None,
        tags: vec![],
        parent_id: None,
        auto_release_on_verdict: None,
        metadata: std::collections::BTreeMap::new(),
    }
}

// Sprint 23 P0 r2 F2 (dev-reviewer-2 most-material): mirror
// `decisions::can_mutate_decision` test coverage onto
// `tasks::can_mutate_task`. Decisions added 5 dedicated unit tests
// (PR #220 Sprint 21 Phase 2 D1); tasks shipped the predicate
// pub-promotion in Sprint 23 P0 with zero direct unit coverage —
// closed here. Behavioural mirror of the Phase 2 D1 operator-pitfall
// gate.

// ── #829 boot orphan-owner sweep ──

fn make_record(
    id: &str,
    status: crate::task_events::TaskStatus,
    owner: Option<&str>,
) -> crate::task_events::TaskRecord {
    crate::task_events::TaskRecord {
        id: crate::task_events::TaskId(id.to_string()),
        title: format!("title-{id}"),
        description: String::new(),
        priority: "normal".into(),
        status,
        owner: owner.map(crate::task_events::InstanceName::from),
        linked_prs: Vec::new(),
        block_reason: None,
        history: Vec::new(),
        created_by: crate::task_events::InstanceName::from("test"),
        created_at: "2026-05-15T00:00:00Z".into(),
        updated_at: "2026-05-15T00:00:00Z".into(),
        due_at: None,
        depends_on: Vec::new(),
        routed_to: None,
        result: None,
        branch: None,
        bind: None,
        started_at: None,
        eta_secs: None,
        tags: vec![],
        parent_id: None,
        metadata: std::collections::BTreeMap::new(),
    }
}

fn make_state(records: Vec<crate::task_events::TaskRecord>) -> crate::task_events::TaskBoardState {
    let mut state = crate::task_events::TaskBoardState::default();
    for r in records {
        state.tasks.insert(r.id.clone(), r);
    }
    state
}

fn make_set(names: &[&str]) -> std::collections::HashSet<String> {
    names.iter().map(|s| (*s).to_string()).collect()
}

/// #829 C3 GREEN: pure-fn classifier test. Three sets cover the
/// full state space: in-live, in-fleet-only, in-neither.
#[test]
fn classify_owner_categorizes_three_states() {
    let live = make_set(&["charlie829"]);
    let fleet = make_set(&["bob829", "charlie829"]);
    assert_eq!(
        classify_owner("charlie829", &live, &fleet),
        OwnerClassification::Live,
        "charlie829 ∈ live must classify Live (live wins over fleet membership)"
    );
    assert_eq!(
        classify_owner("bob829", &live, &fleet),
        OwnerClassification::Soft,
        "bob829 ∈ fleet but ∉ live must classify Soft"
    );
    assert_eq!(
        classify_owner("alice829", &live, &fleet),
        OwnerClassification::Strict,
        "alice829 ∈ neither must classify Strict"
    );
}

/// #829 C3 GREEN: cold start case — empty event log → empty result.
/// Locks the no-op contract for fresh daemons. C1 RED test already
/// implicitly covers this through the assertion on alice829's
/// presence, but explicit coverage hardens the contract surface.
#[test]
fn scan_orphan_candidates_handles_empty_state() {
    let state = make_state(vec![]);
    let live = make_set(&["alpha"]);
    let fleet = make_set(&["alpha", "beta"]);
    let result = scan_orphan_candidates(&state, &live, &fleet);
    assert!(result.strict.is_empty(), "empty state → empty strict");
    assert!(result.soft.is_empty(), "empty state → empty soft");
}

/// #829 C3 GREEN: terminal-status tasks (Done / Cancelled) are
/// excluded even when their owner is a ghost. The ACL is already
/// disabled at the event-log layer for terminal records, so
/// re-orphaning is noise. Locks the skip behavior.
#[test]
fn scan_orphan_candidates_excludes_terminal_status() {
    use crate::task_events::TaskStatus;
    // ghost-829 owns four tasks across the four terminal-vs-live
    // statuses. Only Open + Claimed should land in `strict`.
    let state = make_state(vec![
        make_record("t-open", TaskStatus::Open, Some("ghost829")),
        make_record("t-claimed", TaskStatus::Claimed, Some("ghost829")),
        make_record("t-done", TaskStatus::Done, Some("ghost829")),
        make_record("t-cancel", TaskStatus::Cancelled, Some("ghost829")),
    ]);
    let live = make_set(&[]);
    let fleet = make_set(&[]);
    let result = scan_orphan_candidates(&state, &live, &fleet);
    let ghost_strict = result.strict.get("ghost829").cloned().unwrap_or_default();
    let ids: Vec<String> = ghost_strict.iter().map(|t| t.0.clone()).collect();
    assert_eq!(
        ids,
        vec!["t-claimed".to_string(), "t-open".to_string()],
        "Done + Cancelled must be excluded; non-terminal must surface (BTreeMap order is by TaskId)"
    );
}

// ── #830 task action=health build_health_response ──

fn make_record_with_age_days(
    id: &str,
    status: crate::task_events::TaskStatus,
    owner: Option<&str>,
    age_days: i64,
) -> crate::task_events::TaskRecord {
    let created_at = (chrono::Utc::now() - chrono::Duration::days(age_days)).to_rfc3339();
    let mut r = make_record(id, status, owner);
    r.created_at = created_at;
    r
}

/// #830: status counts roll up across all 4 active states +
/// Done/Cancelled terminals. Totals stay consistent
/// (`all = non_terminal + terminal`).
#[test]
fn build_health_response_reports_status_counts_correctly() {
    use crate::task_events::TaskStatus;
    let state = make_state(vec![
        make_record("t-1", TaskStatus::Open, None),
        make_record("t-2", TaskStatus::Open, None),
        make_record("t-3", TaskStatus::Claimed, Some("alpha")),
        make_record("t-4", TaskStatus::InProgress, Some("alpha")),
        make_record("t-5", TaskStatus::Blocked, None),
        make_record("t-6", TaskStatus::Done, Some("alpha")),
        make_record("t-7", TaskStatus::Cancelled, None),
    ]);
    let live = make_set(&["alpha"]);
    let fleet = make_set(&["alpha"]);

    let resp = build_health_response(&state, Some(&live), &fleet);

    assert_eq!(resp["totals"]["all"], 7);
    assert_eq!(resp["totals"]["non_terminal"], 5);
    assert_eq!(resp["totals"]["terminal"], 2);
    assert_eq!(resp["by_status"]["open"], 2);
    assert_eq!(resp["by_status"]["claimed"], 1);
    assert_eq!(resp["by_status"]["in_progress"], 1);
    assert_eq!(resp["by_status"]["blocked"], 1);
    assert_eq!(resp["by_status"]["done"], 1);
    assert_eq!(resp["by_status"]["cancelled"], 1);
}

/// #830: a clean board (no ghosts, no stale claims, low age,
/// blocked count under threshold) must produce an EMPTY
/// `recommendations` array. Positive signal for operators —
/// "everything's fine, nothing to do".
#[test]
fn build_health_response_clean_board_emits_empty_recommendations() {
    use crate::task_events::TaskStatus;
    let state = make_state(vec![
        make_record_with_age_days("t-1", TaskStatus::Open, None, 1),
        make_record_with_age_days("t-2", TaskStatus::InProgress, Some("alpha"), 1),
    ]);
    let live = make_set(&["alpha"]);
    let fleet = make_set(&["alpha"]);

    let resp = build_health_response(&state, Some(&live), &fleet);
    let recs = resp["recommendations"]
        .as_array()
        .expect("recommendations must be array");
    assert!(
        recs.is_empty(),
        "clean board → empty recommendations, got: {recs:?}"
    );
}

/// #830: ghost-owner candidates from `scan_orphan_candidates` (#829)
/// must surface in BOTH the `ghost_owners` section AND a
/// `ghost_owners_strict` / `ghost_owners_soft` recommendation entry
/// with structured `{code, severity, hint, candidate_ids}`.
#[test]
fn build_health_response_includes_ghost_owners_from_scan() {
    use crate::task_events::TaskStatus;
    let state = make_state(vec![
        // alice is ∉ live ∧ ∉ fleet → strict
        make_record("t-1", TaskStatus::Claimed, Some("alice830")),
        // bob is ∈ fleet ∧ ∉ live → soft
        make_record("t-2", TaskStatus::Open, Some("bob830")),
    ]);
    let live = make_set(&[]);
    let fleet = make_set(&["bob830"]);

    let resp = build_health_response(&state, Some(&live), &fleet);

    assert_eq!(resp["ghost_owners"]["strict_count"], 1);
    assert_eq!(resp["ghost_owners"]["soft_count"], 1);
    let recs = resp["recommendations"]
        .as_array()
        .expect("recommendations array");
    let codes: Vec<&str> = recs.iter().filter_map(|r| r["code"].as_str()).collect();
    assert!(
        codes.contains(&"ghost_owners_strict"),
        "ghost_owners_strict recommendation must fire, got codes: {codes:?}"
    );
    assert!(
        codes.contains(&"ghost_owners_soft"),
        "ghost_owners_soft recommendation must fire, got codes: {codes:?}"
    );
}

/// #830: claims past `due_at` surface as `stale_claims` entry +
/// `stale_claims` recommendation. Locks the read-only replication
/// of `sweep_overdue_claimed`'s predicate.
#[test]
fn build_health_response_includes_stale_claims_via_due_at() {
    use crate::task_events::TaskStatus;
    let past = (chrono::Utc::now() - chrono::Duration::days(2)).to_rfc3339();
    let mut overdue = make_record("t-overdue", TaskStatus::Claimed, Some("alpha"));
    overdue.due_at = Some(past);
    let state = make_state(vec![overdue]);
    let live = make_set(&["alpha"]);
    let fleet = make_set(&["alpha"]);

    let resp = build_health_response(&state, Some(&live), &fleet);

    assert_eq!(resp["stale_claims"]["overdue_count"], 1);
    let ids = resp["stale_claims"]["overdue_ids"]
        .as_array()
        .expect("overdue_ids must be array");
    assert_eq!(ids.len(), 1);
    assert_eq!(ids[0], "t-overdue");
    let recs = resp["recommendations"]
        .as_array()
        .expect("recommendations array");
    let codes: Vec<&str> = recs.iter().filter_map(|r| r["code"].as_str()).collect();
    assert!(
        codes.contains(&"stale_claims"),
        "stale_claims recommendation must fire, got codes: {codes:?}"
    );
}

/// #830: daemon-offline degrades gracefully. `live = None` flows
/// through (`live_agents_available: false` in the response) — the
/// ghost_owners scan effectively treats all owners as ghosts (live
/// set is empty), which is the worst case but at least operator
/// can see the snapshot.
#[test]
fn build_health_response_handles_daemon_offline_gracefully() {
    use crate::task_events::TaskStatus;
    let state = make_state(vec![make_record("t-1", TaskStatus::Open, None)]);
    let fleet = make_set(&[]);

    let resp = build_health_response(&state, None, &fleet);

    assert_eq!(resp["live_agents_available"], false);
    assert_eq!(resp["totals"]["all"], 1);
}

/// #829 C1 RED: classify three tasks across the strict / soft /
/// live spectrum, plus one Done-status task (excluded). The pure
/// scan must surface `alice` as strict (not in fleet.yaml, not live),
/// `bob` as soft (in fleet.yaml, not live), and drop `charlie`
/// (live) + `done-owner` (terminal status).
#[test]
fn scan_orphan_candidates_splits_strict_and_soft() {
    use crate::task_events::{TaskId, TaskStatus};
    let state = make_state(vec![
        make_record("t-1", TaskStatus::Claimed, Some("alice829")),
        make_record("t-2", TaskStatus::InProgress, Some("alice829")),
        make_record("t-3", TaskStatus::Open, Some("bob829")),
        make_record("t-4", TaskStatus::InProgress, Some("charlie829")),
        make_record("t-5", TaskStatus::Done, Some("alice829")),
        make_record("t-6", TaskStatus::Open, None),
    ]);
    let live = make_set(&["charlie829"]);
    let fleet = make_set(&["bob829", "charlie829"]);

    let result = scan_orphan_candidates(&state, &live, &fleet);

    // alice829 → strict (fully gone). 2 tasks (t-1 + t-2), Done t-5 excluded.
    let alice_tasks: Vec<TaskId> = result.strict.get("alice829").cloned().unwrap_or_default();
    assert_eq!(
        alice_tasks,
        vec![TaskId("t-1".into()), TaskId("t-2".into())],
        "alice829 (not in fleet.yaml, not live) must be classified strict with 2 non-terminal tasks"
    );

    // bob829 → soft (in fleet.yaml but not live). 1 task.
    let bob_tasks: Vec<TaskId> = result.soft.get("bob829").cloned().unwrap_or_default();
    assert_eq!(
        bob_tasks,
        vec![TaskId("t-3".into())],
        "bob829 (in fleet.yaml, not live) must be classified soft"
    );

    // charlie829 → live. Should appear in neither bucket.
    assert!(
        !result.strict.contains_key("charlie829"),
        "live owner must not appear in strict"
    );
    assert!(
        !result.soft.contains_key("charlie829"),
        "live owner must not appear in soft"
    );
}

/// #829 Fix A — C1 RED anchor.
///
/// Locks the boot-path contract: with an explicit empty live set
/// (the verifiable state at `bootstrap::prepare` time — `api::serve`
/// has not yet bound the socket, no agents have been spawned), a
/// ghost-owned task (owner ∉ fleet.yaml ∧ ∉ live) MUST land as
/// Strict and have its owner cleared via `orphan_tasks_for_owner`.
///
/// Pre-Fix A this symbol doesn't exist: compile-fail = RED. Post-Fix
/// A the wrapper factors out the body and the boot caller can pass
/// `HashSet::new()` directly, severing the broken `api::call` chain.
#[test]
fn reconcile_orphan_owners_with_live_empty_set_orphans_strict_ghost() {
    use crate::task_events::{InstanceName, TaskEvent, TaskId};
    let home = tmp_home("reconcile_with_live_empty_orphans_strict");
    // No fleet.yaml written → FleetConfig::load Err → empty fleet
    // instances → "dev-impl-1" classifies Strict.

    let emitter = InstanceName::from("test:legacy_migration");
    let tid = TaskId("t-ghost-1".into());
    crate::task_events::append_batch(
        &home,
        &emitter,
        vec![TaskEvent::Created {
            task_id: tid.clone(),
            title: "ghost-owned legacy task".into(),
            description: String::new(),
            priority: "normal".into(),
            owner: Some(InstanceName::from("dev-impl-1")),
            due_at: None,
            depends_on: Vec::new(),
            routed_to: None,
            branch: None,
            bind: None,
            eta_secs: None,
            tags: vec![],
            parent_id: None,
        }],
    )
    .expect("seed Created event");

    // Pre-condition: replay sees the ghost owner.
    let pre = crate::task_events::replay(&home).expect("pre replay");
    assert_eq!(
        pre.tasks
            .get(&tid)
            .and_then(|r| r.owner.as_ref().map(|o| o.0.clone())),
        Some("dev-impl-1".into()),
        "pre-condition: ghost owner persisted to event log"
    );

    // Subject under test: boot-path entrypoint that the bootstrap
    // call site uses. `live = ∅` is the verifiable state at boot
    // (pre-auto-start, pre-api::serve bind). Does NOT touch the
    // periodic `reconcile_orphan_owners(home)` path that fetches
    // live via `api::call`.
    reconcile_orphan_owners_with_live(&home, &std::collections::HashSet::new());

    let post = crate::task_events::replay(&home).expect("post replay");
    let owner_after = post
        .tasks
        .get(&tid)
        .and_then(|r| r.owner.as_ref().map(|o| o.0.clone()));
    assert!(
        owner_after.is_none(),
        "after boot sweep with empty live, Strict ghost's owner must be cleared (got {owner_after:?})"
    );

    std::fs::remove_dir_all(&home).ok();
}

// ── Boot orphan sweep (task t-20260526155509233515-8) ──
// STATUS-orphan sweep: at boot, `live = ∅` (auto_start runs after
// bootstrap), so every in_progress task is a prev-session orphan and is
// released to open. Unlike the owner sweep there is NO Soft-defer — a
// re-spawning fleet agent does not resume its prior in_progress.

use crate::task_events::TaskStatus;

/// RED→GREEN pure-fn matrix. With `live = ∅` (boot), only `InProgress`
/// tasks are orphans regardless of owner; non-in_progress statuses are
/// always skipped; a `None`-owner in_progress (malformed) still counts.
#[test]
fn scan_inprogress_orphans_boot_empty_live_matrix() {
    let state = make_state(vec![
        make_record("t-ip-owned", TaskStatus::InProgress, Some("dev-impl-1")),
        make_record("t-ip-fleet", TaskStatus::InProgress, Some("dev-impl-2")),
        make_record("t-ip-noowner", TaskStatus::InProgress, None),
        make_record("t-open", TaskStatus::Open, Some("dev-impl-1")),
        make_record("t-claimed", TaskStatus::Claimed, Some("dev-impl-1")),
        make_record("t-done", TaskStatus::Done, Some("dev-impl-1")),
        make_record("t-cancelled", TaskStatus::Cancelled, Some("dev-impl-1")),
    ]);
    let live = std::collections::HashSet::new(); // boot: no agent alive yet

    let mut got: Vec<String> = scan_inprogress_orphans(&state, &live)
        .into_iter()
        .map(|t| t.0)
        .collect();
    got.sort();
    assert_eq!(
        got,
        vec![
            "t-ip-fleet".to_string(),
            "t-ip-noowner".to_string(),
            "t-ip-owned".to_string()
        ],
        "boot (live=∅): all in_progress (incl. no-owner) are orphans; \
         open/claimed/done/cancelled skipped — NO Soft-defer for in_progress"
    );
}

/// The `live` param IS honoured for a future per-tick variant: an
/// in_progress task whose owner is in the live set is actively running →
/// NOT an orphan. A no-owner in_progress is still an orphan.
#[test]
fn scan_inprogress_orphans_live_owner_is_not_orphan() {
    let state = make_state(vec![
        make_record("t-ip-live", TaskStatus::InProgress, Some("dev-impl-1")),
        make_record("t-ip-dead", TaskStatus::InProgress, Some("dev-impl-9")),
        make_record("t-ip-noowner", TaskStatus::InProgress, None),
    ]);
    let live = make_set(&["dev-impl-1"]);

    let mut got: Vec<String> = scan_inprogress_orphans(&state, &live)
        .into_iter()
        .map(|t| t.0)
        .collect();
    got.sort();
    assert_eq!(
        got,
        vec!["t-ip-dead".to_string(), "t-ip-noowner".to_string()],
        "owner ∈ live ⇒ actively running ⇒ kept; dead owner + no-owner ⇒ orphan"
    );
}

/// Integration: seed an in_progress task in the event log, run the boot
/// entrypoint (empty live), and confirm replay shows it released back to
/// Open with the owner cleared (re-dispatchable). Idempotent re-run is a
/// no-op (the task is no longer in_progress).
#[test]
fn release_inprogress_orphans_releases_to_open_and_clears_owner() {
    use crate::task_events::{InstanceName, TaskEvent, TaskId};
    let home = tmp_home("release_inprogress_orphans");
    let emitter = InstanceName::from("test:seed");
    let tid = TaskId("t-stuck-1".into());
    let worker = InstanceName::from("dev-impl-1");
    crate::task_events::append_batch(
        &home,
        &emitter,
        vec![
            TaskEvent::Created {
                task_id: tid.clone(),
                title: "stuck in_progress across restart".into(),
                description: String::new(),
                priority: "normal".into(),
                owner: Some(worker.clone()),
                due_at: None,
                depends_on: Vec::new(),
                routed_to: None,
                branch: None,
                bind: None,
                eta_secs: None,
                tags: vec![],
                parent_id: None,
            },
            TaskEvent::Claimed {
                task_id: tid.clone(),
                by: worker.clone(),
            },
            TaskEvent::InProgress {
                task_id: tid.clone(),
                by: worker.clone(),
            },
        ],
    )
    .expect("seed in_progress task");

    // Pre-condition: replay sees it in_progress + owned.
    let pre = crate::task_events::replay(&home).expect("pre replay");
    let pre_rec = pre.tasks.get(&tid).expect("seeded task present");
    assert_eq!(pre_rec.status, TaskStatus::InProgress, "pre: in_progress");
    assert!(pre_rec.owner.is_some(), "pre: owned");

    // Subject: boot entrypoint with live=∅ (the bootstrap call site).
    let released = release_inprogress_orphans_with_live(&home, &std::collections::HashSet::new());
    assert_eq!(released, vec![tid.clone()], "the stuck task is released");

    let post = crate::task_events::replay(&home).expect("post replay");
    let post_rec = post.tasks.get(&tid).expect("task still present");
    assert_eq!(
        post_rec.status,
        TaskStatus::Open,
        "after boot sweep: in_progress orphan released back to open"
    );
    assert!(
        post_rec.owner.is_none(),
        "Released clears owner → re-dispatchable (got {:?})",
        post_rec.owner
    );

    // Idempotent: re-running finds no in_progress orphan → no-op.
    let again = release_inprogress_orphans_with_live(&home, &std::collections::HashSet::new());
    assert!(again.is_empty(), "re-run is a no-op once released to open");

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn can_mutate_task_assignee_match() {
    let home = tmp_home("can_mutate_assignee");
    let task = make_test_task(Some("dev-impl-2"));
    assert!(can_mutate_task(&home, "dev-impl-2", &task));
    std::fs::remove_dir_all(&home).ok();
}

/// Sprint 54 fleet-yaml unification: teams now live in fleet.yaml's
/// `teams:` block (was: separate `teams.json` runtime store).
/// Helper writes the dev team directly there, bypassing `teams::create`
/// validation paths so tests stay focused on `can_mutate_task` logic.
fn write_dev_team_to_fleet(home: &std::path::Path) {
    std::fs::write(
        crate::fleet::fleet_yaml_path(home),
        "teams:\n  dev:\n    members: [dev-lead, dev-impl-2]\n    \
         orchestrator: dev-lead\n    created_at: \"2026-04-27T00:00:00Z\"\n",
    )
    .expect("write fleet.yaml");
}

#[test]
fn can_mutate_task_orchestrator_of_assignee() {
    // dev-lead is the orchestrator of the "dev" team; "dev-impl-2"
    // belongs to that team. Cross-team orchestrator path → pass.
    let home = tmp_home("can_mutate_orchestrator");
    write_dev_team_to_fleet(&home);
    let task = make_test_task(Some("dev-impl-2"));
    assert!(
        can_mutate_task(&home, "dev-lead", &task),
        "orchestrator of assignee's team must be allowed to mutate"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn can_mutate_task_team_assignee_orchestrator() {
    // Task-specific path absent in decisions: assignee is a team
    // NAME (not an instance). The team's orchestrator must be
    // allowed to mutate even though their name doesn't match
    // `task.assignee`.
    let home = tmp_home("can_mutate_team_assignee");
    write_dev_team_to_fleet(&home);
    let task = make_test_task(Some("dev")); // assignee = team name
    assert!(
        can_mutate_task(&home, "dev-lead", &task),
        "team orchestrator must mutate task assigned to team name"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn can_mutate_task_unassigned_returns_true() {
    // Task-specific branch absent in decisions: unassigned tasks
    // (`assignee = None`) are open to mutation by anyone — the gate
    // returns `true` regardless of caller. Lock the contract so
    // future refactor doesn't accidentally tighten this.
    let home = tmp_home("can_mutate_unassigned");
    let task = make_test_task(None);
    assert!(can_mutate_task(&home, "anyone", &task));
    assert!(can_mutate_task(&home, "even-fleet-stranger", &task));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn can_mutate_task_string_compare_no_numeric_coerce() {
    // Operator-pitfall regression mirror (decisions test of same
    // name): caller string vs assignee. `Task.assignee` is
    // `Option<String>` (e.g. "dev-impl-1"); the gate compares
    // strings, never parses an int. Verify alphabetically-similar
    // but non-equal callers do NOT pass, and that numeric-suffixed
    // names compare verbatim.
    let home = tmp_home("can_mutate_string_compare");
    let task = make_test_task(Some("dev-impl-1"));
    // Exact string match — passes.
    assert!(can_mutate_task(&home, "dev-impl-1", &task));
    // Suffix mismatch — rejects (no int coerce to "1 == 1").
    assert!(!can_mutate_task(&home, "dev-impl-2", &task));
    // Bare numeric caller — rejects (would only "match" under int
    // coerce path, which we explicitly do not have).
    assert!(!can_mutate_task(&home, "1", &task));
    // Substring of assignee — rejects (no prefix-match path).
    assert!(!can_mutate_task(&home, "dev-impl", &task));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_create_list_claim_done() {
    let home = tmp_home("crud");
    let r = handle(
        &home,
        "agent1",
        &serde_json::json!({"action": "create", "title": "Fix bug", "priority": "high"}),
    );
    assert_eq!(r["status"], "created");
    let id = r["id"].as_str().expect("id").to_string();

    let listed = handle(&home, "agent1", &serde_json::json!({"action": "list"}));
    assert_eq!(listed["tasks"].as_array().expect("arr").len(), 1);
    assert_eq!(listed["tasks"][0]["status"], "open");

    let claim = handle(
        &home,
        "agent2",
        &serde_json::json!({"action": "claim", "id": id}),
    );
    assert_eq!(claim["status"], "claimed");
    assert_eq!(claim["assignee"], "agent2");

    let done = handle(
        &home,
        "agent2",
        &serde_json::json!({"action": "done", "id": id, "result": "fixed"}),
    );
    assert_eq!(done["status"], "done");

    let listed = handle(
        &home,
        "agent1",
        &serde_json::json!({"action": "list", "filter_status": "done"}),
    );
    assert_eq!(listed["tasks"][0]["result"], "fixed");

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_claim_nonexistent() {
    let home = tmp_home("claim_nonexistent");
    let r = handle(
        &home,
        "a",
        &serde_json::json!({"action": "claim", "id": "nope"}),
    );
    assert!(r["error"].as_str().is_some());
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn task_assign_to_team_routes_to_orchestrator() {
    let home = tmp_home("team_route");
    crate::teams::create(
        &home,
        &serde_json::json!({"name": "devs", "members": ["lead", "worker"], "orchestrator": "lead"}),
    );
    let r = handle(
        &home,
        "user",
        &serde_json::json!({"action": "create", "title": "fix bug", "assignee": "devs"}),
    );
    assert_eq!(r["status"], "created");
    let tasks = list_all(&home);
    let t = tasks.iter().find(|t| t.title == "fix bug").expect("task");
    assert_eq!(t.assignee.as_deref(), Some("devs"));
    assert_eq!(t.routed_to.as_deref(), Some("lead"));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn task_assign_to_degraded_team_rejects() {
    let home = tmp_home("degraded_reject");
    crate::teams::create(
        &home,
        &serde_json::json!({"name": "devs", "members": ["lead", "worker"], "orchestrator": "lead"}),
    );
    crate::teams::remove_member_from_all(&home, "lead");
    let r = handle(
        &home,
        "user",
        &serde_json::json!({"action": "create", "title": "fix bug", "assignee": "devs"}),
    );
    assert!(
        r["error"].as_str().expect("err").contains("degraded"),
        "got: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn task_assign_to_agent_unchanged() {
    let home = tmp_home("agent_direct");
    let r = handle(
        &home,
        "user",
        &serde_json::json!({"action": "create", "title": "fix bug", "assignee": "at-dev-2"}),
    );
    assert_eq!(r["status"], "created");
    let tasks = list_all(&home);
    let t = tasks.iter().find(|t| t.title == "fix bug").expect("task");
    assert_eq!(t.assignee.as_deref(), Some("at-dev-2"));
    assert!(
        t.routed_to.is_none(),
        "no routing for direct agent assignment"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn claim_clears_routed_to() {
    let home = tmp_home("claim_clears_rt");
    // Sprint 54 fleet-yaml unification: teams::create now writes
    // to fleet.yaml, so the claim path's instance_exists check fires
    // (previously fleet.yaml stayed absent → permissive). Pre-seed
    // instances so the claim by `worker` resolves to a fleet member.
    write_fleet_yaml(&home, &["lead", "worker"]);
    crate::teams::create(
        &home,
        &serde_json::json!({"name": "devs", "members": ["lead", "worker"], "orchestrator": "lead"}),
    );
    handle(
        &home,
        "user",
        &serde_json::json!({"action": "create", "title": "fix", "assignee": "devs"}),
    );
    let id = list_all(&home)[0].id.clone();
    assert_eq!(list_all(&home)[0].routed_to.as_deref(), Some("lead"));
    handle(
        &home,
        "worker",
        &serde_json::json!({"action": "claim", "id": id}),
    );
    let t = &list_all(&home)[0];
    assert_eq!(t.assignee.as_deref(), Some("worker"));
    assert!(t.routed_to.is_none(), "claim should clear routed_to");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn update_assignee_re_resolves_routed_to() {
    let home = tmp_home("update_re_resolve");
    crate::teams::create(
        &home,
        &serde_json::json!({"name": "alpha", "members": ["a1"], "orchestrator": "a1"}),
    );
    crate::teams::create(
        &home,
        &serde_json::json!({"name": "beta", "members": ["b1"], "orchestrator": "b1"}),
    );
    handle(
        &home,
        "user",
        &serde_json::json!({"action": "create", "title": "task", "assignee": "alpha"}),
    );
    let id = list_all(&home)[0].id.clone();
    assert_eq!(list_all(&home)[0].routed_to.as_deref(), Some("a1"));
    handle(
        &home,
        "a1",
        &serde_json::json!({"action": "update", "id": id, "assignee": "beta"}),
    );
    let t = &list_all(&home)[0];
    assert_eq!(t.assignee.as_deref(), Some("beta"));
    assert_eq!(t.routed_to.as_deref(), Some("b1"));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn task_board_create_via_handle() {
    let home = tmp_home("board_create");
    let r = handle(
        &home,
        "user",
        &serde_json::json!({"action": "create", "title": "new feature", "priority": "normal"}),
    );
    assert_eq!(r["status"], "created");
    let tasks = list_all(&home);
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].title, "new feature");
    assert_eq!(tasks[0].status, crate::task_events::TaskStatus::Open);
    assert_eq!(tasks[0].priority, crate::task_events::TaskPriority::Normal);
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn task_board_move_status() {
    let home = tmp_home("board_move");
    handle(
        &home,
        "user",
        &serde_json::json!({"action": "create", "title": "item", "priority": "low"}),
    );
    let tasks = list_all(&home);
    let id = &tasks[0].id;
    handle(
        &home,
        "user",
        &serde_json::json!({"action": "update", "id": id, "priority": "normal"}),
    );
    let t = &list_all(&home)[0];
    assert_eq!(t.priority, crate::task_events::TaskPriority::Normal);
    assert_eq!(t.status, crate::task_events::TaskStatus::Open);
    handle(
        &home,
        "user",
        &serde_json::json!({"action": "update", "id": id, "status": "claimed"}),
    );
    assert_eq!(
        list_all(&home)[0].status,
        crate::task_events::TaskStatus::Claimed
    );
    handle(
        &home,
        "user",
        &serde_json::json!({"action": "update", "id": id, "status": "done"}),
    );
    assert_eq!(
        list_all(&home)[0].status,
        crate::task_events::TaskStatus::Done
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn task_board_assign_agent() {
    let home = tmp_home("board_assign");
    handle(
        &home,
        "user",
        &serde_json::json!({"action": "create", "title": "fix bug"}),
    );
    let id = &list_all(&home)[0].id.clone();
    handle(
        &home,
        "user",
        &serde_json::json!({"action": "update", "id": id, "assignee": "at-dev-2"}),
    );
    assert_eq!(list_all(&home)[0].assignee.as_deref(), Some("at-dev-2"));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn task_board_cancel() {
    let home = tmp_home("board_cancel");
    handle(
        &home,
        "user",
        &serde_json::json!({"action": "create", "title": "remove me"}),
    );
    let id = &list_all(&home)[0].id.clone();
    handle(
        &home,
        "user",
        &serde_json::json!({"action": "update", "id": id, "status": "cancelled"}),
    );
    assert_eq!(
        list_all(&home)[0].status,
        crate::task_events::TaskStatus::Cancelled
    );
    let all = list_all(&home);
    let columns = crate::render::task_board_columns(&all);
    let total: usize = columns.iter().map(|c| c.len()).sum();
    assert_eq!(total, 0, "cancelled task should not appear in any column");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn task_board_shift_d_marks_done() {
    // Test Shift+D (done action) from all 3 non-done columns
    for (label, setup) in [
        (
            "backlog",
            vec![(
                "create",
                r#"{"action":"create","title":"t","priority":"low"}"#,
            )],
        ),
        (
            "open",
            vec![(
                "create",
                r#"{"action":"create","title":"t","priority":"normal"}"#,
            )],
        ),
        (
            "in_progress",
            vec![
                (
                    "create",
                    r#"{"action":"create","title":"t","priority":"normal"}"#,
                ),
                ("claim", r#"{"action":"claim","id":"__ID__"}"#),
            ],
        ),
    ] {
        let home = tmp_home(&format!("shift_d_{label}"));
        let mut id = String::new();
        for (_, json_str) in &setup {
            let json_str = json_str.replace("__ID__", &id);
            let v: serde_json::Value = serde_json::from_str(&json_str).expect("test JSON literal");
            let r = handle(&home, "user", &v);
            if let Some(i) = r["id"].as_str() {
                id = i.to_string();
            }
        }
        if id.is_empty() {
            id = list_all(&home)[0].id.clone();
        }
        let r = handle(
            &home,
            "user",
            &serde_json::json!({"action": "done", "id": id}),
        );
        assert_eq!(r["status"], "done", "failed for {label}");
        assert_eq!(
            list_all(&home)[0].status,
            crate::task_events::TaskStatus::Done,
            "failed for {label}"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}

#[test]
fn test_concurrent_creates_unique_ids() {
    let home = tmp_home("concurrent_ids");
    let home_arc = std::sync::Arc::new(home.clone());
    let threads: Vec<_> = (0..20)
        .map(|i| {
            let h = home_arc.clone();
            std::thread::spawn(move || {
                handle(
                    &h,
                    &format!("agent-{i}"),
                    &serde_json::json!({"action": "create", "title": format!("task-{i}")}),
                )
            })
        })
        .collect();
    let ids: Vec<String> = threads
        .into_iter()
        .map(|h| {
            let r = h.join().expect("thread");
            assert_eq!(r["status"], "created");
            r["id"].as_str().expect("id").to_string()
        })
        .collect();
    let unique: std::collections::HashSet<&String> = ids.iter().collect();
    assert_eq!(
        unique.len(),
        20,
        "all 20 task IDs must be unique, got: {ids:?}"
    );
    let tasks = list_all(&home);
    assert_eq!(tasks.len(), 20);
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_task_blocked_when_dep_not_done() {
    let home = tmp_home("dep-blocked");
    // Create dep task (stays open)
    let r1 = handle(
        &home,
        "u",
        &serde_json::json!({"action": "create", "title": "dep"}),
    );
    let dep_id = r1["id"].as_str().unwrap().to_string();
    // Create task depending on dep
    handle(
        &home,
        "u",
        &serde_json::json!({
            "action": "create", "title": "child", "depends_on": [dep_id]
        }),
    );
    // List triggers eval → child should be blocked
    let listed = handle(&home, "u", &serde_json::json!({"action": "list"}));
    let tasks = listed["tasks"].as_array().unwrap();
    let child = tasks.iter().find(|t| t["title"] == "child").unwrap();
    assert_eq!(
        child["status"], "blocked",
        "task with open dep must be blocked"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_task_auto_unblock_when_all_deps_done() {
    let home = tmp_home("dep-unblock");
    let r1 = handle(
        &home,
        "u",
        &serde_json::json!({"action": "create", "title": "dep"}),
    );
    let dep_id = r1["id"].as_str().unwrap().to_string();
    handle(
        &home,
        "u",
        &serde_json::json!({
            "action": "create", "title": "child", "depends_on": [dep_id]
        }),
    );
    // List → child blocked
    let listed = handle(&home, "u", &serde_json::json!({"action": "list"}));
    let child = listed["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["title"] == "child")
        .unwrap();
    assert_eq!(child["status"], "blocked");

    // Complete dep → done triggers re-eval → child auto-unblocks
    handle(
        &home,
        "u",
        &serde_json::json!({"action": "done", "id": dep_id, "result": "ok"}),
    );
    let listed = handle(&home, "u", &serde_json::json!({"action": "list"}));
    let child = listed["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["title"] == "child")
        .unwrap();
    assert_eq!(
        child["status"], "open",
        "child must auto-unblock when dep is done"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// PR3 — depends_on is set in the Created event and immutable via
/// the MCP surface (no "depends_on update" event variant). Circular
/// deps can still arise from migration of a circular legacy
/// `tasks.json` or from forward-references in Created events
/// (Task A's `depends_on=[B]` written before Task B exists).
/// This test exercises the latter path via direct event_log writes.
#[test]
fn test_circular_dep_no_infinite_loop() {
    let home = tmp_home("dep-circular");
    let inst = crate::task_events::InstanceName::from("u");
    // Hand-craft two Created events with cross-references.
    crate::task_events::append(
        &home,
        &inst,
        crate::task_events::TaskEvent::Created {
            task_id: crate::task_events::TaskId("t-A".into()),
            title: "A".into(),
            description: String::new(),
            priority: "normal".into(),
            owner: None,
            due_at: None,
            branch: None,
            depends_on: vec![crate::task_events::TaskId("t-B".into())],
            routed_to: None,
            bind: None,
            eta_secs: None,
            tags: vec![],
            parent_id: None,
        },
    )
    .unwrap();
    crate::task_events::append(
        &home,
        &inst,
        crate::task_events::TaskEvent::Created {
            task_id: crate::task_events::TaskId("t-B".into()),
            title: "B".into(),
            description: String::new(),
            priority: "normal".into(),
            owner: None,
            due_at: None,
            branch: None,
            depends_on: vec![crate::task_events::TaskId("t-A".into())],
            routed_to: None,
            bind: None,
            eta_secs: None,
            tags: vec![],
            parent_id: None,
        },
    )
    .unwrap();
    // List must not hang — both should be blocked (neither is done).
    let listed = handle(&home, "u", &serde_json::json!({"action": "list"}));
    let tasks = listed["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 2, "must return without infinite loop");
    for t in tasks {
        assert_eq!(
            t["status"], "blocked",
            "circular dep tasks must be blocked: {}",
            t["title"]
        );
    }
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_task_create_accepts_due_at_iso() {
    let home = tmp_home("due-at-iso");
    let future = (chrono::Utc::now() + chrono::Duration::hours(2)).to_rfc3339();
    let result = handle(
        &home,
        "agent1",
        &serde_json::json!({"action": "create", "title": "timed", "due_at": future}),
    );
    assert_eq!(result["status"], "created");
    let listed = handle(&home, "agent1", &serde_json::json!({"action": "list"}));
    let task = &listed["tasks"][0];
    assert!(task["due_at"].is_string(), "due_at must be set");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_overdue_claimed_task_unclaimed_by_sweep() {
    let home = tmp_home("overdue-sweep");
    let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    let r = handle(
        &home,
        "agent1",
        &serde_json::json!({"action": "create", "title": "overdue", "due_at": past}),
    );
    let id = r["id"].as_str().unwrap().to_string();
    handle(
        &home,
        "agent1",
        &serde_json::json!({"action": "claim", "id": id}),
    );
    let unclaimed = sweep_overdue_claimed(&home);
    assert_eq!(unclaimed, vec![id.clone()]);
    let listed = handle(&home, "agent1", &serde_json::json!({"action": "list"}));
    let task = &listed["tasks"][0];
    assert_eq!(task["status"], "open");
    assert!(task["assignee"].is_null());
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_not_yet_due_not_touched() {
    let home = tmp_home("not-due");
    let future = (chrono::Utc::now() + chrono::Duration::hours(2)).to_rfc3339();
    let r = handle(
        &home,
        "agent1",
        &serde_json::json!({"action": "create", "title": "future", "due_at": future}),
    );
    let id = r["id"].as_str().unwrap().to_string();
    handle(
        &home,
        "agent1",
        &serde_json::json!({"action": "claim", "id": id}),
    );
    let unclaimed = sweep_overdue_claimed(&home);
    assert!(unclaimed.is_empty(), "future task must not be unclaimed");
    let listed = handle(&home, "agent1", &serde_json::json!({"action": "list"}));
    assert_eq!(listed["tasks"][0]["status"], "claimed");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_done_task_ignored_by_sweep() {
    let home = tmp_home("done-ignore");
    let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    let r = handle(
        &home,
        "agent1",
        &serde_json::json!({"action": "create", "title": "done-overdue", "due_at": past}),
    );
    let id = r["id"].as_str().unwrap().to_string();
    handle(
        &home,
        "agent1",
        &serde_json::json!({"action": "claim", "id": id}),
    );
    handle(
        &home,
        "agent1",
        &serde_json::json!({"action": "done", "id": id, "result": "finished"}),
    );
    let unclaimed = sweep_overdue_claimed(&home);
    assert!(unclaimed.is_empty(), "done task must not be unclaimed");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_daemon_maintenance_unclaims_overdue_and_logs_event() {
    let home = tmp_home("daemon-maint");
    let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    let r = handle(
        &home,
        "agent1",
        &serde_json::json!({"action": "create", "title": "overdue-maint", "due_at": past}),
    );
    let id = r["id"].as_str().unwrap().to_string();
    handle(
        &home,
        "agent1",
        &serde_json::json!({"action": "claim", "id": id}),
    );
    crate::daemon::run_task_maintenance(&home);
    let listed = handle(&home, "agent1", &serde_json::json!({"action": "list"}));
    assert_eq!(listed["tasks"][0]["status"], "open");
    assert!(listed["tasks"][0]["assignee"].is_null());
    let log_content = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
    assert!(
        log_content.contains("task_overdue_unclaimed"),
        "event_log must contain task_overdue_unclaimed entry"
    );
    assert!(
        log_content.contains(&id),
        "event_log must reference the task id"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── Mutation integrity tests ──

fn write_fleet_yaml(home: &std::path::Path, instances: &[&str]) {
    let entries: Vec<String> = instances
        .iter()
        .map(|n| format!("  {n}:\n    backend: claude"))
        .collect();
    let yaml = format!("instances:\n{}", entries.join("\n"));
    std::fs::write(crate::fleet::fleet_yaml_path(home), yaml).ok();
}

/// #2117 P1: two teams with distinct `source_repo`s get ISOLATED boards. A task
/// created by teamA's member lands on teamA's board and is invisible to teamB's
/// default (current-project) `list` — the new cross-board isolation. The
/// `task_index` routes a later `done` to the right board, and `scope=fleet`
/// aggregates both.
#[test]
fn two_projects_get_isolated_boards_2117() {
    let home = tmp_home("p1-board-isolation");
    let yaml = r#"
instances:
  devA:
    backend: claude
  devB:
    backend: claude
teams:
  teamA:
    members:
      - devA
    source_repo: /repos/orgA/projA
  teamB:
    members:
      - devB
    source_repo: /repos/orgB/projB
"#;
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();

    let ta = handle(
        &home,
        "devA",
        &serde_json::json!({"action": "create", "title": "TA", "assignee": "devA"}),
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    let tb = handle(
        &home,
        "devB",
        &serde_json::json!({"action": "create", "title": "TB", "assignee": "devB"}),
    )["id"]
        .as_str()
        .unwrap()
        .to_string();

    let list_ids = |caller: &str, args: serde_json::Value| -> Vec<String> {
        handle(&home, caller, &args)["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["id"].as_str().map(String::from))
            .collect()
    };

    // Default list = caller's current project → isolated.
    let a_ids = list_ids("devA", serde_json::json!({"action": "list"}));
    let b_ids = list_ids("devB", serde_json::json!({"action": "list"}));
    assert!(
        a_ids.contains(&ta) && !a_ids.contains(&tb),
        "teamA must see only its own task: {a_ids:?}"
    );
    assert!(
        b_ids.contains(&tb) && !b_ids.contains(&ta),
        "teamB must see only its own task: {b_ids:?}"
    );

    // Each task lands on its own board subtree; nothing leaks to the default board.
    assert!(home.join("boards/orgA_projA/task_events.jsonl").exists());
    assert!(home.join("boards/orgB_projB/task_events.jsonl").exists());
    assert!(
        !home.join("task_events.jsonl").exists(),
        "no task should land on the default/home board"
    );

    // scope=fleet aggregates across boards.
    let all_ids = list_ids(
        "devA",
        serde_json::json!({"action": "list", "scope": "fleet"}),
    );
    assert!(
        all_ids.contains(&ta) && all_ids.contains(&tb),
        "fleet scope must see both boards' tasks: {all_ids:?}"
    );

    // done routes via task_index to the task's board (cross-board O(1)).
    let done = handle(
        &home,
        "devA",
        &serde_json::json!({"action": "done", "id": ta}),
    );
    assert_eq!(
        done["event"], "done",
        "teamA task done must route to its board: {done}"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// #2117 P3a: a subtask (`parent_id` = composition, "A is composed of B/C/D")
/// MUST live in its parent's project — the DP4 board-isolation invariant.
/// `handle_create` rejects a cross-project `parent_id` fail-closed, so
/// `cascade_cancel_children` (same-board replay) can never silently orphan a
/// cross-project child on parent cancel. Same-project composition is allowed.
/// (`depends_on` — execution-order dependency, a cross-board *reference* the epic
/// allows — is deliberately NOT guarded here.)
#[test]
fn cross_project_parent_id_rejected_at_create_2117_p3a() {
    let home = tmp_home("p3a-parent-id-guard");

    let parent = handle(
        &home,
        "dev",
        &serde_json::json!({"action": "create", "title": "parent", "project": "orgA/projA"}),
    )["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Cross-project subtask → rejected (fail-closed).
    let bad = handle(
        &home,
        "dev",
        &serde_json::json!({"action": "create", "title": "child", "project": "orgB/projB", "parent_id": parent}),
    );
    assert!(
        bad["error"]
            .as_str()
            .unwrap_or_default()
            .contains("cross-project parent_id"),
        "cross-project parent_id must be rejected: {bad}"
    );
    // And nothing was written to teamB's board.
    assert!(
        super::list_all_at(&crate::task_events::board_root(&home, "orgB/projB")).is_empty(),
        "rejected subtask must not land on any board"
    );

    // Same-project subtask → allowed (control).
    let ok = handle(
        &home,
        "dev",
        &serde_json::json!({"action": "create", "title": "child2", "project": "orgA/projA", "parent_id": parent}),
    );
    assert!(
        ok["id"].is_string(),
        "same-project subtask must be allowed: {ok}"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// #2117 P3a (FM5 / board isolation): a task mutation resolves its board from the
/// task_id, so the per-board ACL must deny a caller acting in a DIFFERENT project
/// than the task's board. devA (teamA→projA) cannot `done`/`claim` a task on
/// teamB's (projB) board; devB (teamB, same board) can. Single-project never
/// triggers (caller and task both DEFAULT) — see the unchanged existing
/// done/claim/update tests.
#[test]
fn cross_board_mutation_denied_same_board_allowed_2117_p3a() {
    let home = tmp_home("p3a-board-acl");
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        r#"
instances:
  devA:
    backend: claude
  devB:
    backend: claude
teams:
  teamA:
    members:
      - devA
    source_repo: /repos/orgA/projA
  teamB:
    members:
      - devB
    source_repo: /repos/orgB/projB
"#,
    )
    .unwrap();

    // A task on teamB's board, created+owned by devB. No explicit `project` — it
    // resolves from devB's team `source_repo` (/repos/orgB/projB → slug
    // `orgB_projB`), so the recorded board id is exactly what
    // `resolve_current_project(devB)` yields (the production-consistent path; an
    // explicit raw `project: "orgB/projB"` would NOT match the slugged caller id).
    let tb = handle(
        &home,
        "devB",
        &serde_json::json!({"action": "create", "title": "B", "assignee": "devB"}),
    )["id"]
        .as_str()
        .unwrap()
        .to_string();

    // (a) devA (projA) mutating teamB's (projB) task → cross-board deny, on every
    // mutation path.
    for action in ["done", "claim", "update"] {
        let denied = handle(
            &home,
            "devA",
            &serde_json::json!({"action": action, "id": tb, "status": "blocked"}),
        );
        assert!(
            denied["error"]
                .as_str()
                .unwrap_or_default()
                .contains("cross-board mutation denied"),
            "devA must be denied cross-board on {action}: {denied}"
        );
    }
    // Nothing mutated — task still Open on board B (slug `orgB_projB`).
    let tb_status = super::list_all_at(&crate::task_events::board_root(&home, "orgB_projB"))
        .into_iter()
        .find(|t| t.id == tb)
        .unwrap()
        .status;
    assert_eq!(
        tb_status,
        crate::task_events::TaskStatus::Open,
        "a denied cross-board mutation must not change the task"
    );

    // (b) devB (projB, same board) → allowed (board gate passes; claim succeeds on
    // the Open task).
    let ok = handle(
        &home,
        "devB",
        &serde_json::json!({"action": "claim", "id": tb}),
    );
    assert_eq!(
        ok["event"], "claimed",
        "devB same-board mutation must succeed: {ok}"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// #2117 P3a (reviewer-4 #2133): a HARD fleet.yaml read/parse failure must
/// fail-CLOSED — the per-board ACL can't determine the caller's project, so it
/// DENIES rather than fall through to the default board (fail-open). Distinct from
/// a legitimate no-team caller (missing fleet.yaml = single-project → allow).
#[test]
fn fleet_read_failure_denies_mutation_fail_closed_2117_p3a() {
    let home = tmp_home("p3a-fleet-fail-closed");
    // Create the task with a VALID single-project fleet (lands on the default board).
    write_fleet_yaml(&home, &["dev"]);
    let t = handle(
        &home,
        "dev",
        &serde_json::json!({"action": "create", "title": "t", "assignee": "dev"}),
    )["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Now CORRUPT fleet.yaml (file PRESENT but unparseable → try_load_fleet = Err,
    // NOT the missing-file Ok(default) path).
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "{{{ not: : valid yaml ][",
    )
    .unwrap();

    // A non-system caller's mutation fail-closes (without the #2133 hardening the
    // unchecked resolver would return DEFAULT and ALLOW — fail-open).
    let denied = handle(
        &home,
        "dev",
        &serde_json::json!({"action": "done", "id": t}),
    );
    assert!(
        denied["error"]
            .as_str()
            .unwrap_or_default()
            .contains("cross-board mutation denied"),
        "a fleet.yaml hard read failure must fail-closed deny: {denied}"
    );

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_claim_unknown_instance_rejected() {
    let home = tmp_home("claim-unknown");
    write_fleet_yaml(&home, &["known-agent"]);
    let r = handle(
        &home,
        "known-agent",
        &serde_json::json!({"action": "create", "title": "t"}),
    );
    let id = r["id"].as_str().unwrap();
    // Unknown instance tries to claim
    let r = handle(
        &home,
        "phantom",
        &serde_json::json!({"action": "claim", "id": id}),
    );
    assert!(
        r["error"].as_str().unwrap().contains("not found in fleet"),
        "got: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_claim_already_claimed_by_other_rejected() {
    let home = tmp_home("claim-stolen");
    write_fleet_yaml(&home, &["agent-a", "agent-b"]);
    let r = handle(
        &home,
        "agent-a",
        &serde_json::json!({"action": "create", "title": "t"}),
    );
    let id = r["id"].as_str().unwrap();
    handle(
        &home,
        "agent-a",
        &serde_json::json!({"action": "claim", "id": id}),
    );
    // agent-b tries to steal
    let r = handle(
        &home,
        "agent-b",
        &serde_json::json!({"action": "claim", "id": id}),
    );
    assert!(
        r["error"].as_str().unwrap().contains("only 'open'"),
        "claimed task must not be claimable by others: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// §3.9 regression (#t-21 HIGH #2): N agents racing to claim the SAME Open task
/// — exactly ONE must win, the rest must be rejected, and the board must end up
/// Claimed by that single winner. Pre-fix, the claimable check ran before a
/// separate append lock that did NOT re-validate, so multiple racers all
/// appended `Claimed` events. Regression-proof: revert the claim arm to
/// `append` + outer pre-check and this FAILS (successes > 1).
#[test]
fn concurrent_claims_exactly_one_wins_t21() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let home = tmp_home("concurrent-claim");
    const N: usize = 8;
    let agents: Vec<String> = (0..N).map(|i| format!("agent-{i}")).collect();
    let agent_refs: Vec<&str> = agents.iter().map(|s| s.as_str()).collect();
    write_fleet_yaml(&home, &agent_refs);

    let r = handle(
        &home,
        "agent-0",
        &serde_json::json!({"action": "create", "title": "t"}),
    );
    let id = r["id"].as_str().unwrap().to_string();

    let successes = AtomicUsize::new(0);
    let winner: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    std::thread::scope(|s| {
        for agent in &agents {
            let (home, id, successes, winner) = (&home, &id, &successes, &winner);
            s.spawn(move || {
                let res = handle(
                    home,
                    agent,
                    &serde_json::json!({"action": "claim", "id": id}),
                );
                if res["status"] == "claimed" {
                    successes.fetch_add(1, Ordering::SeqCst);
                    *winner.lock().unwrap() = Some(agent.clone());
                } else {
                    assert!(
                        res["error"].as_str().unwrap_or("").contains("only 'open'"),
                        "a losing racer must get the not-open rejection, got: {res}"
                    );
                }
            });
        }
    });

    assert_eq!(
        successes.load(Ordering::SeqCst),
        1,
        "exactly one concurrent claim may succeed — the append-lock revalidation \
         must reject all racers that lost"
    );
    // Final board state must agree with the single reported winner.
    let winner = winner.lock().unwrap().clone().expect("a winner must exist");
    let tasks = list_all(&home);
    let task = tasks.iter().find(|t| t.id == id).expect("task exists");
    assert_eq!(
        task.status,
        crate::task_events::TaskStatus::Claimed,
        "board must be Claimed after the race"
    );
    assert_eq!(
        task.assignee.as_deref(),
        Some(winner.as_str()),
        "board assignee must be the claim that reported success"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_claim_self_reclaim_ok() {
    let home = tmp_home("claim-reclaim");
    write_fleet_yaml(&home, &["agent-a"]);
    let r = handle(
        &home,
        "agent-a",
        &serde_json::json!({"action": "create", "title": "t"}),
    );
    let id = r["id"].as_str().unwrap();
    handle(
        &home,
        "agent-a",
        &serde_json::json!({"action": "claim", "id": id}),
    );
    // Re-claim own task → ok
    let r = handle(
        &home,
        "agent-a",
        &serde_json::json!({"action": "claim", "id": id}),
    );
    assert_eq!(r["status"], "claimed", "self re-claim must succeed");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_done_non_assignee_rejected() {
    let home = tmp_home("done-non-assignee");
    write_fleet_yaml(&home, &["agent-a", "agent-b"]);
    let r = handle(
        &home,
        "agent-a",
        &serde_json::json!({"action": "create", "title": "t"}),
    );
    let id = r["id"].as_str().unwrap();
    handle(
        &home,
        "agent-a",
        &serde_json::json!({"action": "claim", "id": id}),
    );
    // agent-b tries to mark done
    let r = handle(
        &home,
        "agent-b",
        &serde_json::json!({"action": "done", "id": id}),
    );
    assert!(
        r["error"].as_str().unwrap().contains("not authorized"),
        "got: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_done_assignee_ok() {
    let home = tmp_home("done-assignee");
    write_fleet_yaml(&home, &["agent-a"]);
    let r = handle(
        &home,
        "agent-a",
        &serde_json::json!({"action": "create", "title": "t"}),
    );
    let id = r["id"].as_str().unwrap();
    handle(
        &home,
        "agent-a",
        &serde_json::json!({"action": "claim", "id": id}),
    );
    let r = handle(
        &home,
        "agent-a",
        &serde_json::json!({"action": "done", "id": id, "result": "ok"}),
    );
    assert_eq!(r["status"], "done");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_done_orchestrator_ok() {
    let home = tmp_home("done-orch");
    write_fleet_yaml(&home, &["lead", "worker"]);
    crate::teams::create(
        &home,
        &serde_json::json!({"name": "dev", "members": ["lead", "worker"], "orchestrator": "lead"}),
    );
    let r = handle(
        &home,
        "lead",
        &serde_json::json!({"action": "create", "title": "t", "assignee": "worker"}),
    );
    let id = r["id"].as_str().unwrap();
    handle(
        &home,
        "worker",
        &serde_json::json!({"action": "claim", "id": id}),
    );
    // Orchestrator marks done on behalf
    let r = handle(
        &home,
        "lead",
        &serde_json::json!({"action": "done", "id": id, "result": "merged"}),
    );
    assert_eq!(
        r["status"], "done",
        "orchestrator must be able to mark done"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_update_non_owner_rejected() {
    let home = tmp_home("update-non-owner");
    write_fleet_yaml(&home, &["agent-a", "agent-b"]);
    let r = handle(
        &home,
        "agent-a",
        &serde_json::json!({"action": "create", "title": "t"}),
    );
    let id = r["id"].as_str().unwrap();
    handle(
        &home,
        "agent-a",
        &serde_json::json!({"action": "claim", "id": id}),
    );
    // agent-b tries to change priority
    let r = handle(
        &home,
        "agent-b",
        &serde_json::json!({"action": "update", "id": id, "priority": "urgent"}),
    );
    assert!(
        r["error"].as_str().unwrap().contains("not authorized"),
        "got: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_update_orchestrator_ok() {
    let home = tmp_home("update-orch");
    write_fleet_yaml(&home, &["lead", "worker"]);
    crate::teams::create(
        &home,
        &serde_json::json!({"name": "dev", "members": ["lead", "worker"], "orchestrator": "lead"}),
    );
    let r = handle(
        &home,
        "lead",
        &serde_json::json!({"action": "create", "title": "t", "assignee": "worker"}),
    );
    let id = r["id"].as_str().unwrap();
    handle(
        &home,
        "worker",
        &serde_json::json!({"action": "claim", "id": id}),
    );
    let r = handle(
        &home,
        "lead",
        &serde_json::json!({"action": "update", "id": id, "priority": "urgent"}),
    );
    assert_eq!(
        r["status"], "updated",
        "orchestrator must be able to update"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_update_release_claim_ok() {
    let home = tmp_home("update-release");
    write_fleet_yaml(&home, &["agent-a"]);
    let r = handle(
        &home,
        "agent-a",
        &serde_json::json!({"action": "create", "title": "t"}),
    );
    let id = r["id"].as_str().unwrap();
    handle(
        &home,
        "agent-a",
        &serde_json::json!({"action": "claim", "id": id}),
    );
    // Release claim by setting status=open
    let r = handle(
        &home,
        "agent-a",
        &serde_json::json!({"action": "update", "id": id, "status": "open"}),
    );
    assert_eq!(r["status"], "updated");
    let tasks = list_all(&home);
    let t = tasks.iter().find(|t| t.id == id).unwrap();
    assert_eq!(t.status, crate::task_events::TaskStatus::Open);
    assert!(t.assignee.is_none(), "release must clear assignee");
    assert!(t.routed_to.is_none(), "release must clear routed_to");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_claim_blocked_task_rejected() {
    let home = tmp_home("claim-blocked");
    write_fleet_yaml(&home, &["agent-a"]);
    // Create dep (stays open) + child that depends on it (auto-blocked)
    let r1 = handle(
        &home,
        "agent-a",
        &serde_json::json!({"action": "create", "title": "dep"}),
    );
    let dep_id = r1["id"].as_str().unwrap().to_string();
    let r2 = handle(
        &home,
        "agent-a",
        &serde_json::json!({"action": "create", "title": "child", "depends_on": [dep_id]}),
    );
    let child_id = r2["id"].as_str().unwrap();
    // List triggers dep eval → child becomes blocked
    handle(&home, "agent-a", &serde_json::json!({"action": "list"}));
    // Try to claim blocked task
    let r = handle(
        &home,
        "agent-a",
        &serde_json::json!({"action": "claim", "id": child_id}),
    );
    assert!(
        r["error"].as_str().unwrap().contains("only 'open'"),
        "blocked task must not be claimable: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_update_non_owner_on_open_assigned_rejected() {
    let home = tmp_home("update-non-owner-assigned");
    write_fleet_yaml(&home, &["agent-a", "agent-b"]);
    // Create task assigned to agent-a (but not claimed yet → status open)
    let r = handle(
        &home,
        "agent-a",
        &serde_json::json!({"action": "create", "title": "t", "assignee": "agent-a"}),
    );
    let id = r["id"].as_str().unwrap();
    // agent-b tries to change priority on agent-a's assigned task
    let r = handle(
        &home,
        "agent-b",
        &serde_json::json!({"action": "update", "id": id, "priority": "urgent"}),
    );
    assert!(
        r["error"].as_str().unwrap().contains("not authorized"),
        "non-owner must not update assigned task: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

// --- Sprint 8 PR-M: Done TTL filter ---
// PR3 cutover note — backdating `updated_at` requires writing
// hand-crafted envelopes with old timestamps directly to
// `task_events.jsonl` (the public `task_events::append` always
// stamps `Utc::now()`). The TTL filter logic itself is exercised by
// every other test that creates Done tasks; this specific 14-day
// boundary check is gated behind a future invariant test that uses
// direct envelope writes.

#[test]
fn test_list_done_filter_returns_all() {
    let home = tmp_home("done-ttl-all");
    let r1 = handle(
        &home,
        "a",
        &serde_json::json!({"action": "create", "title": "old"}),
    );
    let id1 = r1["id"].as_str().unwrap().to_string();
    handle(
        &home,
        "a",
        &serde_json::json!({"action": "claim", "id": id1}),
    );
    handle(
        &home,
        "a",
        &serde_json::json!({"action": "done", "id": id1, "result": "ok"}),
    );

    let r2 = handle(
        &home,
        "a",
        &serde_json::json!({"action": "create", "title": "new"}),
    );
    let id2 = r2["id"].as_str().unwrap().to_string();
    handle(
        &home,
        "a",
        &serde_json::json!({"action": "claim", "id": id2}),
    );
    handle(
        &home,
        "a",
        &serde_json::json!({"action": "done", "id": id2, "result": "ok"}),
    );

    // #1608b: backdate id1 on the REAL event-sourced path. The `list` handler
    // reads `updated_at` from `task_events::replay` (the LATEST envelope's
    // timestamp), NOT `tasks.json` (which no read path consults) — so the old
    // `mutate_versioned(tasks.json)` backdate had zero effect and this test
    // could not fail for the regression it guards (#1614 fiction-test class).
    // Rewrite id1's envelopes in `task_events.jsonl` with a 15-day-old timestamp.
    {
        let path = home.join("task_events.jsonl");
        let content = std::fs::read_to_string(&path).unwrap();
        let old = (chrono::Utc::now() - chrono::Duration::days(15)).to_rfc3339();
        let rewritten = content
            .lines()
            .map(|line| {
                let mut v: serde_json::Value = serde_json::from_str(line).unwrap();
                if v["event"]["task_id"] == serde_json::json!(id1) {
                    v["timestamp"] = serde_json::json!(old);
                }
                serde_json::to_string(&v).unwrap()
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, format!("{rewritten}\n")).unwrap();
    }

    // Explicit filter_status=done returns ALL done tasks regardless of age (id1
    // is now genuinely aged past the 14-day done-TTL on the replay path).
    let listed = handle(
        &home,
        "a",
        &serde_json::json!({"action": "list", "filter_status": "done"}),
    );
    let tasks = listed["tasks"].as_array().unwrap();
    assert_eq!(
        tasks.len(),
        2,
        "filter_status=done must return all done tasks"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_list_non_done_always_returns() {
    let home = tmp_home("done-ttl-nondone");
    // Create open + claimed tasks — they should always appear
    handle(
        &home,
        "a",
        &serde_json::json!({"action": "create", "title": "open task"}),
    );
    let r2 = handle(
        &home,
        "a",
        &serde_json::json!({"action": "create", "title": "claimed task"}),
    );
    let id2 = r2["id"].as_str().unwrap().to_string();
    handle(
        &home,
        "a",
        &serde_json::json!({"action": "claim", "id": id2}),
    );

    let listed = handle(&home, "a", &serde_json::json!({"action": "list"}));
    let tasks = listed["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 2, "non-done tasks must always appear");
    std::fs::remove_dir_all(&home).ok();
}

// ── Sprint 24 P0 PR2 — bridge-phase migration tests ─────────────

/// Migration helper imports every legacy `tasks.json` task into the
/// event log: open / claimed / done / cancelled / blocked all reach
/// the same final status under replay() that they had on disk.
#[test]
fn migration_imports_legacy_tasks_to_event_log() {
    let home = tmp_home("migration_import");
    // Seed a legacy tasks.json with one task per status branch.
    crate::store::mutate_versioned(&store_path(&home), |store: &mut TaskStore| {
        for (id, status, assignee) in [
            ("t-mig-open", "open", None),
            ("t-mig-claimed", "claimed", Some("agent-a")),
            ("t-mig-in-prog", "in_progress", Some("agent-b")),
            ("t-mig-done", "done", Some("agent-c")),
            ("t-mig-cancelled", "cancelled", None),
            ("t-mig-blocked", "blocked", None),
        ] {
            let parsed_status: crate::task_events::TaskStatus =
                serde_json::from_value(serde_json::Value::String(status.to_string())).unwrap();
            store.tasks.push(Task {
                id: id.into(),
                title: format!("title {id}"),
                description: "legacy".into(),
                status: parsed_status,
                priority: crate::task_events::TaskPriority::Normal,
                assignee: assignee.map(String::from),
                routed_to: None,
                created_by: "operator".into(),
                depends_on: Vec::new(),
                result: if status == "done" {
                    Some("completed".into())
                } else {
                    None
                },
                created_at: chrono::Utc::now().to_rfc3339(),
                updated_at: chrono::Utc::now().to_rfc3339(),
                due_at: None,
                branch: None,
                started_at: None,
                eta_secs: None,
                tags: vec![],
                parent_id: None,
                auto_release_on_verdict: None,
                metadata: std::collections::BTreeMap::new(),
            });
        }
        Ok(())
    })
    .unwrap();

    let report = migrate_legacy_tasks_json_to_event_log(&home).unwrap();
    assert_eq!(report.migrated, 6, "all 6 legacy tasks imported");
    assert_eq!(report.skipped, 0);

    // Replay should observe 6 tasks at their pre-migration statuses.
    let state = crate::task_events::replay(&home).unwrap();
    assert_eq!(state.tasks.len(), 6);
    let lookup = |id: &str| {
        state
            .tasks
            .get(&crate::task_events::TaskId::from(id))
            .map(|t| t.status)
            .unwrap()
    };
    assert_eq!(lookup("t-mig-open"), crate::task_events::TaskStatus::Open);
    assert_eq!(
        lookup("t-mig-claimed"),
        crate::task_events::TaskStatus::Claimed
    );
    assert_eq!(
        lookup("t-mig-in-prog"),
        crate::task_events::TaskStatus::InProgress
    );
    assert_eq!(lookup("t-mig-done"), crate::task_events::TaskStatus::Done);
    assert_eq!(
        lookup("t-mig-cancelled"),
        crate::task_events::TaskStatus::Cancelled
    );
    assert_eq!(
        lookup("t-mig-blocked"),
        crate::task_events::TaskStatus::Blocked
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Migration is idempotent: re-running on already-migrated state is a
/// no-op (every task_id already in the event log → skipped).
#[test]
fn migration_idempotent_on_second_run() {
    let home = tmp_home("migration_idempotent");
    crate::store::mutate_versioned(&store_path(&home), |store: &mut TaskStore| {
        store.tasks.push(Task {
            id: "t-idp-1".into(),
            title: "idem".into(),
            description: String::new(),
            status: crate::task_events::TaskStatus::Open,
            priority: crate::task_events::TaskPriority::Normal,
            assignee: None,
            routed_to: None,
            created_by: "op".into(),
            depends_on: Vec::new(),
            result: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            due_at: None,
            branch: None,
            started_at: None,
            eta_secs: None,
            tags: vec![],
            parent_id: None,
            auto_release_on_verdict: None,
            metadata: std::collections::BTreeMap::new(),
        });
        Ok(())
    })
    .unwrap();

    let first = migrate_legacy_tasks_json_to_event_log(&home).unwrap();
    assert_eq!(first.migrated, 1);
    assert_eq!(first.skipped, 0);

    // PR4 — after a successful first migration the live `tasks.json`
    // is archived to `tasks.json.legacy_pre_v2`. The second run sees
    // an empty input store and reports zero work.
    let second = migrate_legacy_tasks_json_to_event_log(&home).unwrap();
    assert_eq!(
        second.migrated, 0,
        "second run finds no live tasks.json → no new emit"
    );
    assert_eq!(second.skipped, 0);
    // Archive sidecar should exist for archeology.
    assert!(
        home.join("tasks.json.legacy_pre_v2").exists(),
        "PR4 retire: live tasks.json archived to .legacy_pre_v2"
    );
    assert!(
        !home.join("tasks.json").exists(),
        "PR4 retire: live tasks.json removed from daemon's read path"
    );

    // Replay confirms exactly one TaskRecord (no duplicate Created
    // event accumulated across the two migration runs).
    let state = crate::task_events::replay(&home).unwrap();
    assert_eq!(state.tasks.len(), 1);
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_task_create_with_branch() {
    let home = tmp_home("branch");
    let result = handle(
        &home,
        "dev-lead",
        &serde_json::json!({"action": "create", "title": "Fix bug", "branch": "sprint-28-fix"}),
    );
    assert!(
        result.get("error").is_none(),
        "create must succeed: {result}"
    );
    let tasks = list_all(&home);
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].branch.as_deref(), Some("sprint-28-fix"));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_task_create_without_branch_defaults_none() {
    let home = tmp_home("no-branch");
    let result = handle(
        &home,
        "dev-lead",
        &serde_json::json!({"action": "create", "title": "Fix bug"}),
    );
    assert!(result.get("error").is_none());
    let tasks = list_all(&home);
    assert_eq!(tasks.len(), 1);
    assert!(tasks[0].branch.is_none());
    std::fs::remove_dir_all(&home).ok();
}

// --- M4 r1-fix: ACL allow-list + state guard tests ---

#[test]
fn system_identity_in_allow_list() {
    assert!(is_system_identity("system:auto_close"));
    assert!(is_system_identity("system:overdue_sweep"));
}

#[test]
fn non_system_identity_rejected() {
    assert!(!is_system_identity("random_agent"));
    assert!(!is_system_identity("system")); // bare "system" not in list
}

#[test]
fn done_action_honors_done_source_override() {
    let home = tmp_home("done_source");
    // Create a task, claim it, then done with custom done_source
    let created = handle(
        &home,
        "dev",
        &serde_json::json!({"action": "create", "title": "test task"}),
    );
    let id = created["id"].as_str().expect("task id");
    handle(
        &home,
        "dev",
        &serde_json::json!({"action": "claim", "id": id}),
    );
    let done = handle(
        &home,
        "dev",
        &serde_json::json!({
            "action": "done",
            "id": id,
            "done_source": {
                "via": "AutoCloseOnPrMerge",
                "branch": "feat/test",
                "merged_at": "2026-05-01T00:00:00Z"
            }
        }),
    );
    assert_eq!(done["status"], "done", "done should succeed: {done}");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn system_identity_can_mutate_others_task() {
    let home = tmp_home("sys_acl");
    // Create task owned by "dev"
    let created = handle(
        &home,
        "dev",
        &serde_json::json!({"action": "create", "title": "dev task"}),
    );
    let id = created["id"].as_str().expect("task id");
    handle(
        &home,
        "dev",
        &serde_json::json!({"action": "claim", "id": id}),
    );
    // system:auto_close should be able to close dev's task
    let done = handle(
        &home,
        "system:auto_close",
        &serde_json::json!({"action": "done", "id": id}),
    );
    assert_eq!(
        done["status"], "done",
        "system identity should bypass ACL: {done}"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ----------------------------------------------------------------------
// #789 anchor (§3.10 red→green) — task action=done triggers cleanup of
// empty init commits accumulated post-bind. Pre-C3: tasks::handle("done")
// does NOT invoke `clean_empty_init_commits`, so the backend-style
// empty inits ride along to push. Post-C3: handle("done") cleans up at
// the task-completion workflow boundary.
//
// Cross-platform: no `#[cfg(unix)]` per #785/#786 precedent + reviewer C6.
// ----------------------------------------------------------------------

#[test]
#[cfg(unix)]
fn task_done_cleans_post_lease_empty_init_commits() {
    // Setup: bind agent to a temp worktree that has accumulated 3
    // empty `init` commits between origin/main and HEAD (simulating
    // backend session-checkpoint heartbeat bursts post-lease).
    // Asserting that `task action=done` cleans them at the workflow
    // boundary.
    let home = tmp_home("789-task-done-cleanup");
    let worktree = home.join("worktree");
    std::fs::create_dir_all(&worktree).unwrap();
    let bypass = ("AGEND_GIT_BYPASS", "1");

    // git init + initial commit (origin/main reference)
    std::process::Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(&worktree)
        .env(bypass.0, bypass.1)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ])
        .current_dir(&worktree)
        .env(bypass.0, bypass.1)
        .output()
        .unwrap();
    // Snapshot HEAD into refs/remotes/origin/main so the cleanup's
    // `git log origin/main..HEAD` range resolves locally.
    let initial_sha = String::from_utf8(
        std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&worktree)
            .env(bypass.0, bypass.1)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    std::process::Command::new("git")
        .args(["update-ref", "refs/remotes/origin/main", &initial_sha])
        .current_dir(&worktree)
        .env(bypass.0, bypass.1)
        .output()
        .unwrap();

    // Accumulate 3 empty `init` commits on HEAD (post-lease pollution).
    for _ in 0..3 {
        std::process::Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ])
            .current_dir(&worktree)
            .env(bypass.0, bypass.1)
            .output()
            .unwrap();
    }

    // Write binding so tasks::handle("done") can locate the worktree.
    let runtime = crate::paths::runtime_dir(&home).join("dev");
    std::fs::create_dir_all(&runtime).ok();
    std::fs::write(
        runtime.join("binding.json"),
        serde_json::to_string(&serde_json::json!({
            "version": 1,
            "agent": "dev",
            "task_id": "T-1",
            "branch": "feat/p789",
            "worktree": worktree.display().to_string(),
            "source_repo": worktree.display().to_string(),
            "issued_at": "2026-01-01T00:00:00Z",
        }))
        .unwrap(),
    )
    .unwrap();

    // Confirm 3 empty inits sit between origin/main..HEAD pre-call.
    let pre_count = String::from_utf8(
        std::process::Command::new("git")
            .args(["log", "origin/main..HEAD", "--format=%H"])
            .current_dir(&worktree)
            .env(bypass.0, bypass.1)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .lines()
    .count();
    assert_eq!(pre_count, 3, "fixture must have 3 empty inits pre-call");

    // Task lifecycle: create + claim + done.
    let created = handle(
        &home,
        "dev",
        &serde_json::json!({"action": "create", "title": "p789 anchor"}),
    );
    let id = created["id"].as_str().expect("task id");
    handle(
        &home,
        "dev",
        &serde_json::json!({"action": "claim", "id": id}),
    );
    let done = handle(
        &home,
        "dev",
        &serde_json::json!({"action": "done", "id": id}),
    );
    assert_eq!(done["status"], "done", "task done must succeed: {done}");

    // §3.10 anchor assertion: post-done, the empty init commits are
    // cleaned (HEAD back to origin/main).
    let post_count = String::from_utf8(
        std::process::Command::new("git")
            .args(["log", "origin/main..HEAD", "--format=%H"])
            .current_dir(&worktree)
            .env(bypass.0, bypass.1)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .lines()
    .count();
    assert_eq!(
        post_count, 0,
        "task action=done MUST trigger clean_empty_init_commits on bound worktree \
         (pre-#789: handler does not call cleanup → 3 commits remain → test fails)"
    );

    std::fs::remove_dir_all(&home).ok();
}

// ── #808 ghost-owner ACL deadlock: auto-orphan + force flag tests ──

#[test]
fn test_ghost_owner_update_without_force_errors_with_acl() {
    // Baseline regression: today operator cannot cancel a task
    // whose owner is no longer in the fleet (ghost-owner ACL
    // deadlock the issue describes). The #808 force flag must NOT
    // change this behavior when force=false / absent — the ACL
    // gate stays load-bearing so accidental cancels still require
    // an explicit force opt-in.
    let home = tmp_home("ghost_acl_baseline");
    write_fleet_yaml(&home, &["operator"]);
    let r = handle(
        &home,
        "operator",
        &serde_json::json!({
            "action": "create",
            "title": "stuck",
            "assignee": "ghost-instance",
        }),
    );
    let id = r["id"].as_str().expect("id").to_string();
    let r = handle(
        &home,
        "operator",
        &serde_json::json!({
            "action": "update",
            "id": id,
            "status": "cancelled",
        }),
    );
    assert!(
        r["error"]
            .as_str()
            .map(|e| e.contains("not authorized"))
            .unwrap_or(false),
        "ghost-owned task cancel without force must surface ACL error, got: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_force_update_true_without_force_reason_rejected() {
    // Force-flag contract (mirrors comms.rs:200-218): force=true
    // without a non-empty force_reason must surface a validator
    // error, NOT fall through to ACL or succeed silently. The
    // grammar match is "force_reason" (the validator message names
    // the missing field) — pre-fix the handler ignores force and
    // returns the regular ACL error, so this test is RED until C2
    // ships.
    let home = tmp_home("force_no_reason");
    write_fleet_yaml(&home, &["operator"]);
    let r = handle(
        &home,
        "operator",
        &serde_json::json!({
            "action": "create",
            "title": "stuck",
            "assignee": "ghost-instance",
        }),
    );
    let id = r["id"].as_str().expect("id").to_string();
    let r = handle(
        &home,
        "operator",
        &serde_json::json!({
            "action": "update",
            "id": id,
            "status": "cancelled",
            "force": true,
        }),
    );
    assert!(
        r["error"]
            .as_str()
            .map(|e| e.contains("force_reason"))
            .unwrap_or(false),
        "force=true without force_reason must surface validator error, got: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_force_update_with_reason_cancels_ghost_and_logs_audit() {
    // GREEN test 1: force=true + non-empty force_reason on
    // `action=update status=cancelled` bypasses the ACL gate AND
    // pushes a `task_force_update` entry into event-log.jsonl
    // (cross-board audit) AND embeds the force_reason into the
    // per-task event's reason field (per-task replay audit).
    let home = tmp_home("force_cancel_ghost");
    write_fleet_yaml(&home, &["operator"]);
    let r = handle(
        &home,
        "operator",
        &serde_json::json!({
            "action": "create",
            "title": "stuck-by-ghost",
            "assignee": "ghost-instance",
        }),
    );
    let id = r["id"].as_str().expect("id").to_string();
    let r = handle(
        &home,
        "operator",
        &serde_json::json!({
            "action": "update",
            "id": id,
            "status": "cancelled",
            "force": true,
            "force_reason": "post-#808 board hygiene 2026-05-15",
        }),
    );
    assert_eq!(
        r["status"], "updated",
        "force=true + force_reason must succeed despite ghost owner, got: {r}"
    );
    let tasks = list_all(&home);
    let t = tasks
        .iter()
        .find(|t| t.id == id)
        .expect("task still present");
    assert_eq!(
        t.status,
        crate::task_events::TaskStatus::Cancelled,
        "task must be cancelled"
    );
    // Cross-board audit: event-log.jsonl records the force action.
    let log = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
    assert!(
        log.contains("task_force_update"),
        "event-log.jsonl must record task_force_update entry, got: {log}"
    );
    assert!(
        log.contains("post-#808 board hygiene"),
        "event-log.jsonl must capture the force_reason, got: {log}"
    );
    // Per-task replay audit: the Cancelled event's reason field
    // carries the forced marker.
    let task_log = std::fs::read_to_string(home.join("task_events.jsonl")).unwrap_or_default();
    assert!(
        task_log.contains("forced by 'operator'"),
        "Cancelled event reason must carry forced marker, got: {task_log}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_force_done_with_reason_succeeds_on_done_arm() {
    // BONUS test (mandatory per dispatch spec): force flag must
    // also gate the `action=done` arm — operators sometimes close
    // ghost-owned tasks as Done rather than Cancelled when the
    // work was effectively completed before the owner disbanded.
    let home = tmp_home("force_done_ghost");
    write_fleet_yaml(&home, &["operator"]);
    let r = handle(
        &home,
        "operator",
        &serde_json::json!({
            "action": "create",
            "title": "ghost-done",
            "assignee": "ghost-instance",
        }),
    );
    let id = r["id"].as_str().expect("id").to_string();
    // Without force, the done arm rejects (mirrors RED1 behavior).
    let r_blocked = handle(
        &home,
        "operator",
        &serde_json::json!({"action": "done", "id": id}),
    );
    assert!(
        r_blocked["error"]
            .as_str()
            .map(|e| e.contains("not authorized"))
            .unwrap_or(false),
        "done arm without force must reject ghost-owned task, got: {r_blocked}"
    );
    // With force + reason, the done arm proceeds and event-log
    // records the cross-board audit (task_force_done).
    let r = handle(
        &home,
        "operator",
        &serde_json::json!({
            "action": "done",
            "id": id,
            "result": "completed before owner disband",
            "force": true,
            "force_reason": "post-#808 done-arm hygiene",
        }),
    );
    assert_eq!(
        r["status"], "done",
        "force=true done must succeed, got: {r}"
    );
    let log = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
    assert!(
        log.contains("task_force_done"),
        "event-log.jsonl must record task_force_done entry, got: {log}"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── #806 list default filter (Part A) — RED test + scaffolding ──

#[test]
fn test_list_default_returns_only_actionable_statuses() {
    // #806 RED test 1: pre-fix `task action=list` with no filter
    // returns every status including `done` / `cancelled`,
    // producing the 504KB / 3847-line dump the issue documents.
    // Post-fix: default trim → {open,claimed,in_progress,blocked}.
    // A `filtered_default` flag on the response signals callers
    // that the trim fired so audit/forensic consumers can re-call
    // with `include_history=true` if needed.
    let home = tmp_home("list_default_actionable");
    write_fleet_yaml(&home, &["agent-a"]);
    let create_with_status = |title: &str, target_status: &str| {
        let r = handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "create", "title": title, "assignee": "agent-a"}),
        );
        let id = r["id"].as_str().expect("id").to_string();
        if target_status != "open" {
            let r = handle(
                &home,
                "agent-a",
                &serde_json::json!({"action": "update", "id": id, "status": target_status}),
            );
            assert_eq!(r["status"], "updated", "seed transition failed: {r}");
        }
        id
    };
    let open_id = create_with_status("open task", "open");
    let in_progress_id = create_with_status("active task", "in_progress");
    let cancelled_id = create_with_status("stale cancel", "cancelled");
    let done_id = create_with_status("stale done", "done");

    // include_history=true must surface everything (existing audit
    // consumers opt in this way).
    let r_history = handle(
        &home,
        "agent-a",
        &serde_json::json!({"action": "list", "include_history": true}),
    );
    let history_ids: std::collections::HashSet<_> = r_history["tasks"]
        .as_array()
        .expect("tasks")
        .iter()
        .filter_map(|t| t["id"].as_str().map(String::from))
        .collect();
    for id in [&open_id, &in_progress_id, &cancelled_id, &done_id] {
        assert!(
            history_ids.contains(id),
            "include_history=true must surface {id}, got {history_ids:?}"
        );
    }
    assert_eq!(
        r_history["filtered_default"], false,
        "include_history=true must report filtered_default=false"
    );

    // Default (no filter) — only actionable statuses.
    let r = handle(&home, "agent-a", &serde_json::json!({"action": "list"}));
    let tasks = r["tasks"].as_array().expect("tasks");
    let ids: std::collections::HashSet<_> = tasks
        .iter()
        .filter_map(|t| t["id"].as_str().map(String::from))
        .collect();
    assert!(
        ids.contains(&open_id),
        "open task must surface in default list, got {ids:?}"
    );
    assert!(
        ids.contains(&in_progress_id),
        "in_progress task must surface in default list, got {ids:?}"
    );
    assert!(
        !ids.contains(&cancelled_id),
        "cancelled task must NOT surface in default list, got {ids:?}"
    );
    assert!(
        !ids.contains(&done_id),
        "done task must NOT surface in default list, got {ids:?}"
    );
    assert_eq!(
        r["filtered_default"], true,
        "default list must report filtered_default=true"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_list_explicit_filter_status_overrides_default() {
    // GREEN 1b: an explicit `filter_status=cancelled` must still
    // return cancelled entries; the default trim only fires when
    // neither filter_status nor include_history is supplied.
    let home = tmp_home("list_explicit_filter");
    write_fleet_yaml(&home, &["a"]);
    let r = handle(
        &home,
        "a",
        &serde_json::json!({"action": "create", "title": "to-cancel", "assignee": "a"}),
    );
    let id = r["id"].as_str().expect("id").to_string();
    handle(
        &home,
        "a",
        &serde_json::json!({"action": "update", "id": id, "status": "cancelled"}),
    );
    let r = handle(
        &home,
        "a",
        &serde_json::json!({"action": "list", "filter_status": "cancelled"}),
    );
    let tasks = r["tasks"].as_array().expect("tasks");
    assert!(
        tasks.iter().any(|t| t["id"] == id),
        "explicit filter_status=cancelled must surface cancelled task: {tasks:?}"
    );
    assert!(
        tasks.iter().all(|t| t["status"] == "cancelled"),
        "filter_status=cancelled must restrict to cancelled only"
    );
    assert_eq!(
        r["filtered_default"], false,
        "explicit filter_status must report filtered_default=false"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_list_limit_truncates_newest_first() {
    // GREEN 1c: `limit=N` caps response newest-first by updated_at
    // for caller-visible pagination without cursors.
    let home = tmp_home("list_limit");
    write_fleet_yaml(&home, &["a"]);
    let mut ids = Vec::new();
    for title in ["oldest", "middle", "newest"] {
        let r = handle(
            &home,
            "a",
            &serde_json::json!({"action": "create", "title": title, "assignee": "a"}),
        );
        ids.push(r["id"].as_str().expect("id").to_string());
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let r = handle(
        &home,
        "a",
        &serde_json::json!({"action": "list", "limit": 2}),
    );
    let tasks = r["tasks"].as_array().expect("tasks");
    assert_eq!(
        tasks.len(),
        2,
        "limit=2 must cap response, got {}",
        tasks.len()
    );
    let returned: std::collections::HashSet<_> = tasks
        .iter()
        .filter_map(|t| t["id"].as_str().map(String::from))
        .collect();
    assert!(
        returned.contains(&ids[2]),
        "newest task (idx 2) must be in newest-first cap"
    );
    assert!(
        returned.contains(&ids[1]),
        "second-newest (idx 1) must be in newest-first cap"
    );
    assert!(
        !returned.contains(&ids[0]),
        "oldest task (idx 0) must NOT be in newest-first cap"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── #806 sweeper (Part B) — dry-run + apply tests ──

/// Stub PR lookup for sweep tests — bypasses `gh pr view`
/// shellout. Maps a static table of (repo, num) → PrState.
fn stub_pr_lookup(repo: &str, num: u32) -> Result<sweep::PrState, String> {
    match (repo, num) {
        ("test/repo", 999) => Ok(sweep::PrState::Merged {
            merged_at: "2026-04-01T00:00:00Z".to_string(),
        }),
        ("test/repo", 998) => Ok(sweep::PrState::Closed),
        _ => Ok(sweep::PrState::Unknown),
    }
}

/// Stub issue lookup for #2061 stale_open sweep tests — bypasses
/// `gh issue view`. 100 = open (in-flight → must NOT flag), 101 = closed
/// (terminal → flaggable); anything else = Unknown (non-terminal → must NOT flag).
fn stub_issue_lookup(repo: &str, num: u32) -> Result<sweep::IssueState, String> {
    match (repo, num) {
        ("test/repo", 100) => Ok(sweep::IssueState::Open),
        ("test/repo", 101) => Ok(sweep::IssueState::Closed),
        _ => Ok(sweep::IssueState::Unknown),
    }
}

#[test]
fn test_sweep_scan_identifies_team_disbanded_category() {
    // GREEN 2: scan_categories puts tasks owned by instances NOT
    // in live_instances AND aged > 30d into the team_disbanded
    // bucket. Fast-forward `now` 60 days into the future so the
    // freshly-created task crosses the 30d threshold without
    // needing event-log timestamp forgery.
    let home = tmp_home("sweep_disband");
    write_fleet_yaml(&home, &["alive"]);
    let r = handle(
        &home,
        "alive",
        &serde_json::json!({"action": "create", "title": "old work", "assignee": "ghost"}),
    );
    let task_id = r["id"].as_str().expect("id").to_string();
    let live: std::collections::HashSet<String> = ["alive".to_string()].into_iter().collect();
    let now = chrono::Utc::now() + chrono::Duration::days(60);
    let cats = sweep::scan_categories(&home, &live, &stub_pr_lookup, &stub_issue_lookup, None, now);
    assert_eq!(
        cats.team_disbanded.len(),
        1,
        "exactly one team_disbanded candidate expected, got {cats:?}"
    );
    assert_eq!(cats.team_disbanded[0].id, task_id);
    assert_eq!(cats.team_disbanded[0].owner.as_deref(), Some("ghost"));
    // No PR ref → other categories empty.
    assert!(cats.shipped.is_empty());
    assert!(cats.superseded.is_empty());
    assert!(cats.validation_leftovers.is_empty());
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_sweep_scan_identifies_shipped_via_pr_lookup_stub() {
    // GREEN 2a: a task whose title carries `PR #999` and whose
    // stubbed PR state is Merged lands in the shipped bucket
    // when aged > 7d. The PrLookup function-pointer abstraction
    // lets the test bypass the production `gh pr view` shellout.
    let home = tmp_home("sweep_shipped");
    write_fleet_yaml(&home, &["alive"]);
    let r = handle(
        &home,
        "alive",
        &serde_json::json!({
            "action": "create",
            "title": "shipped via PR #999",
            "assignee": "alive",
        }),
    );
    let task_id = r["id"].as_str().expect("id").to_string();
    let live: std::collections::HashSet<String> = ["alive".to_string()].into_iter().collect();
    // 14 days forward — past the 7d shipped threshold but under
    // the 30d team_disbanded threshold (which wouldn't fire
    // anyway because owner is alive).
    let now = chrono::Utc::now() + chrono::Duration::days(14);
    let cats = sweep::scan_categories(
        &home,
        &live,
        &stub_pr_lookup,
        &stub_issue_lookup,
        Some("test/repo"),
        now,
    );
    assert_eq!(
        cats.shipped.len(),
        1,
        "exactly one shipped candidate expected, got {cats:?}"
    );
    assert_eq!(cats.shipped[0].id, task_id);
    assert_eq!(cats.shipped[0].pr, Some(999));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_sweep_handler_dry_run_returns_categorized_plan() {
    // GREEN 2b: full handler path returns dry_run=true with
    // categories shaped + candidate_ids list. Uses
    // team_disbanded category which doesn't require gh shellout.
    let home = tmp_home("sweep_handler_dryrun");
    write_fleet_yaml(&home, &["alive"]);
    handle(
        &home,
        "alive",
        &serde_json::json!({"action": "create", "title": "stuck", "assignee": "ghost"}),
    );
    // Forge updated_at via direct event-log append of an
    // OwnerAssigned older than the 30d threshold — bypass would
    // be cleaner, but for handler test the live `now` of the
    // task is recent. We invoke sweep but expect zero candidates
    // (handler uses real now, task fresh). Still validates the
    // dry-run shape contract.
    let r = handle(&home, "alive", &serde_json::json!({"action": "sweep"}));
    assert_eq!(r["dry_run"], true, "dry-run response shape: {r}");
    assert!(r["categories"].is_object(), "categories must be object");
    assert!(
        r["categories"]["team_disbanded"].is_array(),
        "team_disbanded slot present"
    );
    assert!(
        r["categories"]["shipped"].is_array(),
        "shipped slot present"
    );
    assert!(r["candidate_ids"].is_array(), "candidate_ids array present");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_sweep_apply_without_confirm_ids_rejected() {
    // GREEN 3a: apply=true without confirm_ids must surface
    // a validator error (NOT a silent no-op cancel of every
    // candidate — explicit subset is the double-opt-in guard).
    let home = tmp_home("sweep_no_confirm");
    write_fleet_yaml(&home, &["a"]);
    let r = handle(
        &home,
        "a",
        &serde_json::json!({"action": "sweep", "apply": true}),
    );
    assert!(
        r["error"]
            .as_str()
            .map(|e| e.contains("confirm_ids"))
            .unwrap_or(false),
        "apply=true without confirm_ids must reject: {r}"
    );
    // Also: apply=true with confirm_ids but no audit_reason.
    let r = handle(
        &home,
        "a",
        &serde_json::json!({
            "action": "sweep",
            "apply": true,
            "confirm_ids": ["t-nonexistent"],
        }),
    );
    assert!(
        r["error"]
            .as_str()
            .map(|e| e.contains("audit_reason"))
            .unwrap_or(false),
        "apply=true without audit_reason must reject: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_sweep_apply_emits_cancelled_and_logs_audit() {
    // GREEN 3: apply path with explicit (categories, confirm_ids)
    // emits Cancelled events under system:task_sweep + writes
    // task_sweep_apply lines to event-log.jsonl. The sweep_impl
    // helper is exercised directly because the handler's
    // `categories` rebuild uses real-time `now` and we want a
    // deterministic candidate set.
    let home = tmp_home("sweep_apply");
    write_fleet_yaml(&home, &["alive"]);
    let r = handle(
        &home,
        "alive",
        &serde_json::json!({"action": "create", "title": "ghost task", "assignee": "ghost"}),
    );
    let task_id = r["id"].as_str().expect("id").to_string();
    let live: std::collections::HashSet<String> = ["alive".to_string()].into_iter().collect();
    let now = chrono::Utc::now() + chrono::Duration::days(60);
    let cats = sweep::scan_categories(&home, &live, &stub_pr_lookup, &stub_issue_lookup, None, now);
    assert_eq!(cats.team_disbanded.len(), 1);
    let confirm: std::collections::HashSet<String> = [task_id.clone()].into_iter().collect();
    let count = sweep::emit_cancelled_batch(&home, &cats, &confirm, "post-#806 sweep test fixture")
        .expect("emit_cancelled_batch");
    assert_eq!(count, 1, "exactly one Cancelled must be emitted");
    let listed = list_all(&home);
    let after = listed.iter().find(|t| t.id == task_id).expect("task");
    assert_eq!(
        after.status,
        crate::task_events::TaskStatus::Cancelled,
        "swept task must transition to cancelled"
    );
    let log = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
    assert!(
        log.contains("task_sweep_apply"),
        "event-log.jsonl must record task_sweep_apply entry, got: {log}"
    );
    assert!(
        log.contains("post-#806 sweep test fixture"),
        "event-log.jsonl must carry the audit_reason"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── #2061 stale_open category ──

/// Create one Open task owned by a LIVE agent (so team_disbanded never fires)
/// and return its id. `title` carries any issue/PR ref.
fn create_open_task(home: &Path, title: &str) -> String {
    let r = handle(
        home,
        "alive",
        &serde_json::json!({"action": "create", "title": title, "assignee": "alive"}),
    );
    r["id"].as_str().expect("id").to_string()
}

#[test]
fn test_sweep_stale_open_all_refs_terminal_flagged() {
    // #2061: an open task whose only referenced issue is CLOSED (terminal)
    // lands in stale_open. (#101 → Closed in stub_issue_lookup.)
    let home = tmp_home("sweep_stale_terminal");
    write_fleet_yaml(&home, &["alive"]);
    let id = create_open_task(&home, "follow-up for #101");
    let live: std::collections::HashSet<String> = ["alive".to_string()].into_iter().collect();
    let now = chrono::Utc::now() + chrono::Duration::days(2);
    let cats = sweep::scan_categories(
        &home,
        &live,
        &stub_pr_lookup,
        &stub_issue_lookup,
        Some("test/repo"),
        now,
    );
    assert!(
        cats.stale_open.iter().any(|c| c.id == id),
        "task with an all-terminal issue ref must be flagged stale_open, got {cats:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_sweep_stale_open_open_ref_not_flagged() {
    // #2061 conservative bias: an OPEN referenced issue means the task may be
    // in flight — must NOT flag even when very old. (#100 → Open.)
    let home = tmp_home("sweep_stale_openref");
    write_fleet_yaml(&home, &["alive"]);
    let id = create_open_task(&home, "blocked on #100");
    let live: std::collections::HashSet<String> = ["alive".to_string()].into_iter().collect();
    let now = chrono::Utc::now() + chrono::Duration::days(60);
    let cats = sweep::scan_categories(
        &home,
        &live,
        &stub_pr_lookup,
        &stub_issue_lookup,
        Some("test/repo"),
        now,
    );
    assert!(
        !cats.all_ids().contains(&id),
        "task with an OPEN issue ref must NOT be flagged (in flight), got {cats:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_sweep_stale_open_no_ref_old_flagged() {
    // #2061: a ref-less open task >14d stale lands in stale_open. Fires even
    // with NO repo (the no-ref fallback needs no GitHub query).
    let home = tmp_home("sweep_stale_noref_old");
    write_fleet_yaml(&home, &["alive"]);
    let id = create_open_task(&home, "plain stale task no refs");
    let live: std::collections::HashSet<String> = ["alive".to_string()].into_iter().collect();
    let now = chrono::Utc::now() + chrono::Duration::days(15);
    let cats = sweep::scan_categories(&home, &live, &stub_pr_lookup, &stub_issue_lookup, None, now);
    assert!(
        cats.stale_open.iter().any(|c| c.id == id),
        "ref-less open task >14d must be flagged stale_open (no repo needed), got {cats:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_sweep_stale_open_no_ref_fresh_not_flagged() {
    // #2061: a ref-less open task <14d must NOT be flagged.
    let home = tmp_home("sweep_stale_noref_fresh");
    write_fleet_yaml(&home, &["alive"]);
    let id = create_open_task(&home, "plain fresh task no refs");
    let live: std::collections::HashSet<String> = ["alive".to_string()].into_iter().collect();
    let now = chrono::Utc::now() + chrono::Duration::days(13);
    let cats = sweep::scan_categories(&home, &live, &stub_pr_lookup, &stub_issue_lookup, None, now);
    assert!(
        !cats.all_ids().contains(&id),
        "ref-less open task <14d must NOT be flagged, got {cats:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_sweep_stale_open_mixed_ref_not_flagged() {
    // #2061 ALL-quantifier (load-bearing conservative bias): one closed (#101)
    // + one OPEN (#100) ref — the open ref disqualifies the WHOLE task even
    // though another ref is terminal.
    let home = tmp_home("sweep_stale_mixed");
    write_fleet_yaml(&home, &["alive"]);
    let id = create_open_task(&home, "done with #101 but still #100");
    let live: std::collections::HashSet<String> = ["alive".to_string()].into_iter().collect();
    let now = chrono::Utc::now() + chrono::Duration::days(30);
    let cats = sweep::scan_categories(
        &home,
        &live,
        &stub_pr_lookup,
        &stub_issue_lookup,
        Some("test/repo"),
        now,
    );
    assert!(
        !cats.all_ids().contains(&id),
        "ANY non-terminal ref must disqualify the whole task, got {cats:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_sweep_stale_open_unknown_ref_not_flagged() {
    // #2061: an unresolvable ref (#404 → Unknown, e.g. gh query failed / bogus
    // number) is treated as possibly-live → must NOT flag (fail safe).
    let home = tmp_home("sweep_stale_unknown");
    write_fleet_yaml(&home, &["alive"]);
    let id = create_open_task(&home, "see #404");
    let live: std::collections::HashSet<String> = ["alive".to_string()].into_iter().collect();
    let now = chrono::Utc::now() + chrono::Duration::days(30);
    let cats = sweep::scan_categories(
        &home,
        &live,
        &stub_pr_lookup,
        &stub_issue_lookup,
        Some("test/repo"),
        now,
    );
    assert!(
        !cats.all_ids().contains(&id),
        "an Unknown (unresolvable) ref must NOT be flagged (fail safe), got {cats:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_sweep_stale_open_apply_labels_category() {
    // #2061: the apply leg labels a stale_open candidate correctly (NOT the
    // bare validation_leftovers fallback) in both the Cancelled.reason and the
    // task_sweep_apply audit line.
    let home = tmp_home("sweep_stale_apply");
    write_fleet_yaml(&home, &["alive"]);
    let id = create_open_task(&home, "ref-less stale task to cancel");
    let live: std::collections::HashSet<String> = ["alive".to_string()].into_iter().collect();
    let now = chrono::Utc::now() + chrono::Duration::days(20);
    let cats = sweep::scan_categories(&home, &live, &stub_pr_lookup, &stub_issue_lookup, None, now);
    assert!(
        cats.stale_open.iter().any(|c| c.id == id),
        "precondition: task must be a stale_open candidate, got {cats:?}"
    );
    let confirm: std::collections::HashSet<String> = [id.clone()].into_iter().collect();
    let count = sweep::emit_cancelled_batch(&home, &cats, &confirm, "stale_open sweep test")
        .expect("emit_cancelled_batch");
    assert_eq!(count, 1);
    let log = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
    assert!(
        log.contains("category=stale_open"),
        "audit line must label the category stale_open (not the fallback), got: {log}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_sweep_stale_open_overflow_ref_token_not_flagged() {
    // #2061 conservative bias: a `#N` token that overflows u32 is present-but-
    // unparseable. It must NOT make the task look ref-less and get age-flagged
    // (the task DOES reference work); it is skipped (saw_token, no parseable ref).
    let home = tmp_home("sweep_stale_overflow");
    write_fleet_yaml(&home, &["alive"]);
    let id = create_open_task(&home, "tracking upstream #99999999999999999999");
    let live: std::collections::HashSet<String> = ["alive".to_string()].into_iter().collect();
    let now = chrono::Utc::now() + chrono::Duration::days(20);
    let cats = sweep::scan_categories(&home, &live, &stub_pr_lookup, &stub_issue_lookup, None, now);
    assert!(
        !cats.all_ids().contains(&id),
        "an unparseable (overflow) #N ref must NOT be age-flagged as ref-less, got {cats:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_sweep_stale_open_url_ref_not_flagged() {
    // #2061 conservative bias: an issue named only by GitHub URL (no `#N`) is a
    // reference — must NOT be treated as ref-less and age-flagged. Skipped
    // (saw_token via the URL, no parseable ref to verify).
    let home = tmp_home("sweep_stale_url");
    write_fleet_yaml(&home, &["alive"]);
    let id = create_open_task(
        &home,
        "see https://github.com/suzuke/agend-terminal/issues/1234",
    );
    let live: std::collections::HashSet<String> = ["alive".to_string()].into_iter().collect();
    let now = chrono::Utc::now() + chrono::Duration::days(20);
    let cats = sweep::scan_categories(&home, &live, &stub_pr_lookup, &stub_issue_lookup, None, now);
    assert!(
        !cats.all_ids().contains(&id),
        "an issue referenced only by URL must NOT be age-flagged as ref-less, got {cats:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_sweep_stale_open_merged_pr_fresh_flagged() {
    // #2061: an Open task whose merged PR ref is younger than the shipped 7d
    // grace still lands in stale_open (all-terminal ⇒ done). Pins that the
    // shipped arm's `continue` does NOT fire for age<7d, and that stale_open has
    // no age gate on the all-terminal path. (PR #999 = Merged in stub.)
    let home = tmp_home("sweep_stale_merged_fresh");
    write_fleet_yaml(&home, &["alive"]);
    let id = create_open_task(&home, "PR #999 landed, wrapping up");
    let live: std::collections::HashSet<String> = ["alive".to_string()].into_iter().collect();
    let now = chrono::Utc::now() + chrono::Duration::days(3);
    let cats = sweep::scan_categories(
        &home,
        &live,
        &stub_pr_lookup,
        &stub_issue_lookup,
        Some("test/repo"),
        now,
    );
    assert!(
        cats.shipped.is_empty(),
        "shipped must NOT fire under its 7d grace, got {cats:?}"
    );
    assert!(
        cats.stale_open.iter().any(|c| c.id == id),
        "merged-PR open task must land in stale_open regardless of shipped grace, got {cats:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── #807 response shape consistency bundle — RED tests ──

#[test]
fn test_action_responses_carry_event_and_task_fields_with_lifecycle_status() {
    // #807 Item 1 RED: create/claim/update/done responses today
    // overload `status` with action-event names ("created" /
    // "updated") that don't match the task's lifecycle status
    // (which is "open" / unchanged). Post-fix: every action
    // response gains an `event` field (the action verb) AND a
    // `task` field (full Task object with the correct lifecycle
    // `status`). The legacy `status` field stays as a back-compat
    // alias per the dispatch spec (NOT removed).
    let home = tmp_home("action_response_shape");
    write_fleet_yaml(&home, &["agent-a"]);

    // create
    let r = handle(
        &home,
        "agent-a",
        &serde_json::json!({"action": "create", "title": "t1", "assignee": "agent-a"}),
    );
    let id = r["id"].as_str().expect("id").to_string();
    assert_eq!(
        r["event"], "created",
        "create must carry event=created: {r}"
    );
    assert_eq!(
        r["task"]["status"], "open",
        "create task object must carry lifecycle status=open: {r}"
    );
    assert_eq!(
        r["status"], "created",
        "back-compat status alias preserved (status=event for create): {r}"
    );

    // claim
    let r = handle(
        &home,
        "agent-a",
        &serde_json::json!({"action": "claim", "id": id}),
    );
    assert_eq!(r["event"], "claimed", "claim must carry event=claimed: {r}");
    assert_eq!(
        r["task"]["status"], "claimed",
        "claim task object must carry lifecycle status=claimed: {r}"
    );

    // update → in_progress
    let r = handle(
        &home,
        "agent-a",
        &serde_json::json!({"action": "update", "id": id, "status": "in_progress"}),
    );
    assert_eq!(
        r["event"], "updated",
        "update must carry event=updated: {r}"
    );
    assert_eq!(
        r["task"]["status"], "in_progress",
        "update task object must carry lifecycle status=in_progress (NOT 'updated'): {r}"
    );

    // done
    let r = handle(
        &home,
        "agent-a",
        &serde_json::json!({"action": "done", "id": id, "result": "shipped"}),
    );
    assert_eq!(r["event"], "done", "done must carry event=done: {r}");
    assert_eq!(
        r["task"]["status"], "done",
        "done task object must carry lifecycle status=done: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_task_started_at_replaces_dispatched_at_in_serialized_output() {
    // #807 Item 3 RED: today the Task struct field is named
    // `dispatched_at` but the value is stamped on first transition
    // to InProgress — so the operator's "when was this dispatched
    // from send()?" reading is misleading. Rename to `started_at`
    // (matches `claimed_at` naming, mental model honest). serde
    // alias `dispatched_at` preserves replay of old persisted logs.
    let home = tmp_home("started_at_rename");
    write_fleet_yaml(&home, &["a"]);
    let r = handle(
        &home,
        "a",
        &serde_json::json!({"action": "create", "title": "t", "assignee": "a"}),
    );
    let id = r["id"].as_str().expect("id").to_string();
    handle(
        &home,
        "a",
        &serde_json::json!({"action": "update", "id": id, "status": "in_progress"}),
    );
    let listed = handle(&home, "a", &serde_json::json!({"action": "list"}));
    let task = listed["tasks"][0].as_object().expect("task object in list");
    assert!(
        task.contains_key("started_at"),
        "task must serialize `started_at` (not `dispatched_at`), got keys: {:?}",
        task.keys().collect::<Vec<_>>()
    );
    assert!(
        task["started_at"].is_string(),
        "started_at must be an RFC3339 string, got: {:?}",
        task["started_at"]
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_task_deserialize_dispatched_at_alias_preserves_back_compat() {
    // #807 Item 3: persisted JSON carrying the old `dispatched_at`
    // field name MUST still deserialize into the renamed
    // `started_at` field via `#[serde(alias = "dispatched_at")]`.
    // Locks the contract that pre-rename task_events.jsonl files
    // and external test fixtures keep working post-migration.
    let raw_old = serde_json::json!({
        "id": "t-test-back-compat",
        "title": "old",
        "description": "",
        "status": "open",
        "priority": "normal",
        "assignee": null,
        "created_by": "test",
        "depends_on": [],
        "result": null,
        "created_at": "2026-04-01T00:00:00Z",
        "updated_at": "2026-04-01T00:00:00Z",
        "dispatched_at": "2026-04-02T00:00:00Z",
    });
    let t: Task = serde_json::from_value(raw_old).expect("deserialize old shape");
    assert_eq!(
        t.started_at.as_deref(),
        Some("2026-04-02T00:00:00Z"),
        "serde alias must map legacy `dispatched_at` → new `started_at`"
    );
}

/// #1147: task action=activity returns chronological timeline.
#[test]
fn activity_timeline_returns_events_for_task() {
    let home = tmp_home("activity-timeline");
    // Create a task.
    let create_result = super::handle(
        &home,
        "lead",
        &serde_json::json!({
            "action": "create",
            "title": "implement widget",
            "assignee": "dev",
            "branch": "feat/widget",
        }),
    );
    let task_id = create_result["id"].as_str().unwrap();
    // Claim it.
    super::handle(
        &home,
        "dev",
        &serde_json::json!({"action": "claim", "id": task_id}),
    );
    // Mark done.
    super::handle(
        &home,
        "dev",
        &serde_json::json!({"action": "done", "id": task_id, "result": "PR merged"}),
    );
    // Query activity timeline.
    let result = super::handle(
        &home,
        "lead",
        &serde_json::json!({"action": "activity", "id": task_id}),
    );
    assert_eq!(result["task_id"].as_str(), Some(task_id));
    let events = result["events"].as_array().expect("events array");
    assert_eq!(
        events.len(),
        3,
        "expected 3 events (created+claimed+done): {events:?}"
    );
    assert_eq!(events[0]["event_type"].as_str(), Some("created"));
    assert_eq!(events[1]["event_type"].as_str(), Some("claimed"));
    assert_eq!(events[2]["event_type"].as_str(), Some("done"));
    assert_eq!(events[1]["actor"].as_str(), Some("dev"));
    std::fs::remove_dir_all(&home).ok();
}

// ── #2037: param aliases + list filter aliases ──

/// #2037 (3): `task_id` accepted wherever `id` is canonical (send calls it
/// task_id — the highest-frequency cross-tool slip).
#[test]
fn task_id_alias_accepted_2037() {
    let home = tmp_home("2037-id-alias");
    let create = crate::tasks::handle(
        &home,
        "lead",
        &serde_json::json!({"action": "create", "title": "t"}),
    );
    let id = create["task"]["id"].as_str().expect("created").to_string();
    let claimed = crate::tasks::handle(
        &home,
        "dev",
        &serde_json::json!({"action": "claim", "task_id": id}),
    );
    assert_eq!(
        claimed["event"].as_str(),
        Some("claimed"),
        "task_id alias must work for claim: {claimed}"
    );
    // Missing both → error names the alias.
    let err = crate::tasks::handle(&home, "dev", &serde_json::json!({"action": "claim"}));
    assert!(
        err["error"]
            .as_str()
            .unwrap_or_default()
            .contains("alias: task_id"),
        "error must teach the alias: {err}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #2037 (1): `status`/`assignee` accepted as list-filter aliases of
/// `filter_status`/`filter_assignee`.
#[test]
fn list_filter_aliases_2037() {
    let home = tmp_home("2037-list-alias");
    for (t, who) in [("a", "alice"), ("b", "bob")] {
        let c = crate::tasks::handle(
            &home,
            "lead",
            &serde_json::json!({"action": "create", "title": t, "assignee": who}),
        );
        assert!(c["task"]["id"].is_string(), "{c}");
    }
    let by_alias = crate::tasks::handle(
        &home,
        "lead",
        &serde_json::json!({"action": "list", "assignee": "alice"}),
    );
    let tasks = by_alias["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks.len(), 1, "assignee alias filters: {by_alias}");
    assert_eq!(tasks[0]["assignee"].as_str(), Some("alice"));

    let by_status = crate::tasks::handle(
        &home,
        "lead",
        &serde_json::json!({"action": "list", "status": "open"}),
    );
    assert_eq!(
        by_status["tasks"].as_array().expect("array").len(),
        2,
        "status alias filters: {by_status}"
    );
    std::fs::remove_dir_all(&home).ok();
}
