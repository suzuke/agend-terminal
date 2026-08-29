//! #2760 (frozen-plan d-…-7) — strict routed task authority, RED-first.
//!
//! These pin the fail-closed contract of [`super::load_routed`]: it resolves the
//! ONE board that authoritatively holds an id and NEVER falls back to the default
//! board on a miss. They are PROVEN-FAILING against the checkpoint stub (which
//! reaches only the default board, the same reach as the `load_by_id` seam it
//! replaces); the catalog-backed strict-resolution body turns them green.
//!
//! The two guard tests (default legacy; unknown → NotFound) already
//! pass against the stub — they pin the byte-identical default-board behaviour so
//! GREEN cannot regress it.

use super::{link_branch_to_task, load_routed, TaskRouteError};
use crate::task_events::{append_batch_at, board_root, InstanceName, TaskEvent, TaskId};
use std::path::{Path, PathBuf};

fn tmp_home(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!(
        "agend-routing-red-2760-{}-{}-{tag}",
        std::process::id(),
        n
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Seed a real (Created) task onto `project`'s board. `DEFAULT_PROJECT`/"default"
/// → the home board.
fn seed_task_on_board(home: &Path, project: &str, task_id: &str) {
    append_batch_at(
        &board_root(home, project),
        &InstanceName::from("test:seed"),
        vec![TaskEvent::Created {
            task_id: TaskId(task_id.to_string()),
            title: "t".into(),
            description: String::new(),
            priority: "normal".into(),
            owner: None,
            due_at: None,
            depends_on: Vec::new(),
            routed_to: None,
            branch: None,
            bind: None,
            eta_secs: None,
            tags: vec![],
            parent_id: None,
            governing_decision_id: None,
            review_class: None,
        }],
    )
    .expect("seed task");
}

/// RED: a task living on a NON-DEFAULT project board must route strictly to that
/// board. Pre-fix (stub / `load_by_id`) reads only the default board → the id is
/// invisible → `NotFound`. This is the routing bug behind the t-…-35 live failure.
#[test]
fn load_routed_finds_task_on_non_default_board_2760() {
    let home = tmp_home("non-default");
    seed_task_on_board(&home, "proj-x", "t-2760-x");

    let routed = load_routed(&home, "t-2760-x");
    match routed {
        Ok(rt) => assert_eq!(
            rt.task.id, "t-2760-x",
            "a project-board task must route to its own board"
        ),
        Err(e) => panic!(
            "load_routed must FIND a project-board task, got {e:?} — pre-fix the \
             default-only seam cannot see per-project boards (t-…-35)"
        ),
    }
}

/// RED: the SAME id present on two boards has no single authority → `Ambiguous`,
/// never a silent default pick. Pre-fix the stub finds the default copy and
/// returns `Ok`, mis-authorizing one of two boards.
#[test]
fn load_routed_duplicate_id_across_boards_is_ambiguous_2760() {
    let home = tmp_home("dup");
    seed_task_on_board(&home, "default", "t-2760-dup");
    seed_task_on_board(&home, "proj-b", "t-2760-dup");

    match load_routed(&home, "t-2760-dup") {
        Err(TaskRouteError::Ambiguous { .. }) => {}
        other => {
            panic!("a duplicate id across boards must fail closed as Ambiguous, got {other:?}")
        }
    }
}

/// RED: an unreadable board during the resolution scan means uniqueness cannot be
/// proven → `Unreadable`, never a default guess. The task is uniquely on the
/// default board, but a project board whose event log is unreadable might ALSO
/// hold it, so the route must fail closed. Pre-fix the stub reads only the default
/// board and returns `Ok` (blind to the unreadable board).
#[test]
fn load_routed_unreadable_board_fails_closed_2760() {
    let home = tmp_home("unread");
    seed_task_on_board(&home, "default", "t-2760-unread");
    // A project board whose event log is a DIRECTORY → `replay_at` errors → the
    // scan cannot prove the id is unique to the default board.
    let bad = board_root(&home, "proj-unread");
    std::fs::create_dir_all(bad.join("task_events.jsonl")).unwrap();

    match load_routed(&home, "t-2760-unread") {
        Err(TaskRouteError::Unreadable { .. }) => {}
        other => panic!(
            "an unreadable board that blocks a uniqueness proof must fail closed as \
             Unreadable, got {other:?}"
        ),
    }
}

/// Guard: a legacy task on the default board remains catalog-routable.
#[test]
fn load_routed_default_legacy_task_is_found_2760() {
    let home = tmp_home("default-legacy");
    seed_task_on_board(&home, "default", "t-2760-legacy");

    let routed = load_routed(&home, "t-2760-legacy").expect("default legacy task must resolve");
    assert_eq!(routed.task.id, "t-2760-legacy");
}

/// Guard (passes against the stub): an id present on no board is a definitive
/// `NotFound` — the strict router must not invent a route for an unknown id.
#[test]
fn load_routed_unknown_id_is_notfound_2760() {
    let home = tmp_home("unknown");
    seed_task_on_board(&home, "default", "t-2760-present");

    match load_routed(&home, "t-2760-absent") {
        Err(TaskRouteError::NotFound) => {}
        other => panic!("an unknown id must be NotFound, got {other:?}"),
    }
}

/// #2760: `link_branch_to_task` must write `BranchLinked` to the task's
/// AUTHORITATIVE (project) board via the strict route — NOT the default board.
/// Pre-#2760 the body read `replay(home)` (default board only), so a project-board
/// task's branch link silently no-op'd (`Ok(false)`) and any write would have
/// landed on the wrong board (the "branch-link same-route write" forcing proof).
#[test]
fn link_branch_to_task_writes_to_project_board_not_default_2760() {
    let home = tmp_home("branch-link-proj");
    seed_task_on_board(&home, "proj-bl", "t-2760-bl");

    let linked = link_branch_to_task(&home, "t-2760-bl", "feat/2760-bl").expect("link ok");
    assert!(
        linked,
        "branch link must SUCCEED for a project-board task — pre-#2760 the default-only \
         replay returned Ok(false)"
    );

    // BranchLinked landed on the PROJECT board.
    let on_proj = crate::task_events::replay_at(&board_root(&home, "proj-bl"))
        .expect("replay proj board")
        .tasks
        .get(&TaskId("t-2760-bl".to_string()))
        .and_then(|r| r.branch.clone());
    assert_eq!(
        on_proj.as_deref(),
        Some("feat/2760-bl"),
        "branch recorded on the task's authoritative project board"
    );

    // The default board has NO copy — the write went ONLY to the routed board.
    let default_has_it =
        crate::task_events::replay_at(&board_root(&home, crate::task_events::DEFAULT_PROJECT))
            .map(|s| s.tasks.contains_key(&TaskId("t-2760-bl".to_string())))
            .unwrap_or(false);
    assert!(
        !default_has_it,
        "no default-board copy — branch-link must not write to the default board"
    );
}

/// #2760 idempotency guard: a second `link_branch_to_task` with the SAME branch is
/// a no-op (`Ok(false)`) — the checked append's precondition rejects a re-link.
#[test]
fn link_branch_to_task_same_branch_is_idempotent_noop_2760() {
    let home = tmp_home("branch-link-idem");
    seed_task_on_board(&home, "proj-bl2", "t-2760-bl2");
    assert!(link_branch_to_task(&home, "t-2760-bl2", "feat/x").expect("first link"));
    assert!(
        !link_branch_to_task(&home, "t-2760-bl2", "feat/x").expect("second link"),
        "re-linking the SAME branch is an idempotent no-op"
    );
}

// ── #2760 item 1: route-local STRICT complete-record proof + locked EOF repair ──
//
// The router treats any complete malformed task-event record as `Unreadable`.
// A final unterminated fragment is excluded from the durable prefix; the next
// writer repairs it under the board lock.

/// Append raw bytes to `path` verbatim (no trailing newline added) — used to inject
/// a COMPLETE malformed record (with `\n`) or a torn EOF fragment (without `\n`).
fn append_raw(path: &Path, bytes: &str) {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open raw append");
    write!(f, "{bytes}").expect("raw append");
}

fn events_log(home: &Path, project: &str) -> PathBuf {
    board_root(home, project).join("task_events.jsonl")
}

/// RED: a COMPLETE (newline-terminated) malformed task-event record makes the
/// board's history unprovable → `Unreadable`. Pre-item-1 the router used the
/// fleet-wide `replay_at`, which SKIPS the corrupt line (#1988) — so the id still
/// resolved `Ok`, silently trusting a partial board (the skipped line could have
/// been the very Created/Cancelled event that decides the route).
#[test]
fn route_task_complete_malformed_event_record_is_unreadable_2760() {
    let home = tmp_home("evt-malformed");
    seed_task_on_board(&home, "default", "t-2760-evtbad");
    // A complete (has trailing '\n') non-JSON record mid-log.
    append_raw(&events_log(&home, "default"), "{not valid json at all\n");

    match load_routed(&home, "t-2760-evtbad") {
        Err(TaskRouteError::Unreadable { .. }) => {}
        other => panic!(
            "a complete-malformed task-event record must fail closed as Unreadable \
             (never silently skipped), got {other:?}"
        ),
    }
}

/// RED: a valid-JSON record with a `schema_version` newer than supported is a
/// forward-incompat hazard → `Unreadable` (same as the fleet-wide fail-closed abort,
/// but surfaced as a typed route error).
#[test]
fn route_task_future_schema_event_record_is_unreadable_2760() {
    let home = tmp_home("evt-future");
    seed_task_on_board(&home, "default", "t-2760-evtfut");
    append_raw(
        &events_log(&home, "default"),
        "{\"schema_version\":9999,\"seq\":1,\"event\":{}}\n",
    );

    match load_routed(&home, "t-2760-evtfut") {
        Err(TaskRouteError::Unreadable { .. }) => {}
        other => panic!("a future-schema task-event record must be Unreadable, got {other:?}"),
    }
}

/// A final unterminated EOF fragment is not part of the durable prefix. Authority
/// reads resolve from the complete prefix without taking the writer lock; the
/// next writer repairs the fragment before appending.
#[test]
fn route_task_ignores_final_eof_fragment_then_resolves_2760() {
    let home = tmp_home("evt-eof");
    seed_task_on_board(&home, "default", "t-2760-eof");
    let log = events_log(&home, "default");
    // A torn tail: non-JSON AND no trailing newline (unterminated).
    append_raw(&log, "{torn half-written tail no newline");
    assert!(
        !std::fs::read_to_string(&log).unwrap().ends_with('\n'),
        "precondition: log ends in an unterminated fragment"
    );

    let routed = load_routed(&home, "t-2760-eof")
        .expect("a torn EOF fragment must not hide the durable task prefix");
    assert_eq!(routed.task.id, "t-2760-eof");

    let after = std::fs::read_to_string(&log).unwrap();
    assert!(
        after.contains("torn half-written tail"),
        "authority reads must not mutate storage or take the writer lock"
    );

    append_batch_at(
        &board_root(&home, "default"),
        &InstanceName::from("test:writer"),
        vec![TaskEvent::DescriptionUpdated {
            task_id: TaskId("t-2760-eof".to_string()),
            by: InstanceName::from("test:writer"),
            description: "after repair".to_string(),
        }],
    )
    .expect("the next writer repairs the torn tail before appending");

    let repaired = std::fs::read_to_string(&log).unwrap();
    assert!(!repaired.contains("torn half-written tail"));
    assert!(repaired.ends_with('\n'));
    let routed = load_routed(&home, "t-2760-eof").expect("repaired log remains readable");
    assert_eq!(routed.task.description, "after repair");
}

// ── #2760 items 2+3+4: per-id lock + write-time revalidation + narrow ops ──
//
// These pin the SLICE-A authority-mutation contract: every per-id mutation resolves
// its board strictly, re-validates the route under the per-id lock, and writes ONLY
// to that board (never the default board). The narrow external ops (usage-limit,
// reclaim) carry no board path — they take a task-bound guard and return an outcome.

/// Seed a Created+Claimed task OWNED by `owner` with `branch`, on `project`'s board,
/// the starting state for a reclaim / usage-limit mutation (status `Claimed`, owner
/// set, branch set).
fn seed_owned_claimed(home: &Path, project: &str, id: &str, owner: &str, branch: &str) {
    let board = board_root(home, project);
    append_batch_at(
        &board,
        &InstanceName::from("test:seed"),
        vec![
            TaskEvent::Created {
                task_id: TaskId(id.to_string()),
                title: "t".into(),
                description: String::new(),
                priority: "normal".into(),
                owner: Some(InstanceName::from(owner)),
                due_at: None,
                depends_on: Vec::new(),
                routed_to: None,
                branch: Some(branch.to_string()),
                bind: None,
                eta_secs: None,
                tags: vec![],
                parent_id: None,
                governing_decision_id: None,
                review_class: None,
            },
            TaskEvent::Claimed {
                task_id: TaskId(id.to_string()),
                by: InstanceName::from(owner),
            },
        ],
    )
    .expect("seed owned+claimed task");
}

fn record_on_board(home: &Path, project: &str, id: &str) -> crate::task_events::TaskRecord {
    crate::task_events::replay_at(&board_root(home, project))
        .expect("replay board")
        .tasks
        .get(&TaskId(id.to_string()))
        .cloned()
        .unwrap_or_else(|| panic!("task '{id}' must exist on board '{project}'"))
}

fn default_has_task(home: &Path, id: &str) -> bool {
    crate::task_events::replay_at(&board_root(home, crate::task_events::DEFAULT_PROJECT))
        .map(|s| s.tasks.contains_key(&TaskId(id.to_string())))
        .unwrap_or(false)
}

/// RED (reclaim default-board BUG fix): a reclaimed PROJECT-board task's `Released`
/// event must land on ITS board — the pre-#2760 body appended to the DEFAULT board
/// unconditionally, so the task's own board never saw the release and it stayed
/// Claimed forever.
#[test]
fn release_reclaimed_task_writes_to_project_board_not_default_2760() {
    let home = tmp_home("reclaim-proj");
    seed_owned_claimed(&home, "proj-rc", "t-2760-rc", "dev", "feat/rc");

    let released =
        super::release_reclaimed_task(&home, "t-2760-rc", "reclaimed: test".into()).expect("ok");
    assert!(released, "a claimed project-board task must be released");

    let rec = record_on_board(&home, "proj-rc", "t-2760-rc");
    assert_eq!(
        rec.status,
        crate::task_events::TaskStatus::Open,
        "Released → Open on the PROJECT board"
    );
    assert!(rec.owner.is_none(), "Released clears the owner");
    assert!(
        !default_has_task(&home, "t-2760-rc"),
        "reclaim Released must NOT write to the default board (#2760 BUG fix)"
    );
}

/// RED (item 4 narrow op): a usage-limit block lands `Blocked` on the task's PROJECT
/// board and is idempotent (a re-block for the same episode → `AlreadyApplied`, no
/// second event); never a default-board write.
#[test]
fn apply_usage_limit_block_targets_project_board_and_is_idempotent_2760() {
    let home = tmp_home("ul-block-proj");
    seed_owned_claimed(&home, "proj-ul", "t-2760-ul", "dev", "feat/ul");
    let guard = super::UsageLimitGuard {
        task_id: "t-2760-ul".into(),
        source: "dev".into(),
        branch: "feat/ul".into(),
        episode_id: "ep-123".into(),
    };
    // The reason payload embeds the episode id (as the production caller builds it),
    // so the idempotency pre-check can recognise a prior block for this episode.
    let reason = r#"{"episode_id":"ep-123"}"#.to_string();

    assert_eq!(
        super::apply_usage_limit_block(&home, &guard, reason.clone()).expect("block ok"),
        super::ApplyOutcome::Applied
    );
    let rec = record_on_board(&home, "proj-ul", "t-2760-ul");
    assert_eq!(rec.status, crate::task_events::TaskStatus::Blocked);
    assert!(rec
        .block_reason
        .as_deref()
        .is_some_and(|r| r.contains("ep-123")));
    assert!(
        !default_has_task(&home, "t-2760-ul"),
        "no default-board write"
    );

    // Re-block for the SAME episode → idempotent no-op.
    assert_eq!(
        super::apply_usage_limit_block(&home, &guard, reason).expect("re-block ok"),
        super::ApplyOutcome::AlreadyApplied
    );
}

/// RED (item 4 narrow op): usage-limit recovery unblocks → InProgress on the PROJECT
/// board.
#[test]
fn recover_usage_limit_block_targets_project_board_2760() {
    let home = tmp_home("ul-recover-proj");
    seed_owned_claimed(&home, "proj-ur", "t-2760-ur", "dev", "feat/ur");
    let guard = super::UsageLimitGuard {
        task_id: "t-2760-ur".into(),
        source: "dev".into(),
        branch: "feat/ur".into(),
        episode_id: "ep-9".into(),
    };
    assert_eq!(
        super::apply_usage_limit_block(&home, &guard, r#"{"episode_id":"ep-9"}"#.into())
            .expect("block"),
        super::ApplyOutcome::Applied
    );
    assert_eq!(
        super::recover_usage_limit_block(&home, &guard).expect("recover"),
        super::ApplyOutcome::Applied
    );
    let rec = record_on_board(&home, "proj-ur", "t-2760-ur");
    assert_eq!(
        rec.status,
        crate::task_events::TaskStatus::InProgress,
        "recovery → Unblocked+InProgress on the project board"
    );
}

/// RED (item 4 guard): a usage-limit block whose guard branch no longer matches the
/// task is `Stale` — NO event is written (deny-on-mismatch, no side effect).
#[test]
fn apply_usage_limit_block_stale_on_branch_mismatch_2760() {
    let home = tmp_home("ul-stale");
    seed_owned_claimed(&home, "proj-st", "t-2760-st", "dev", "feat/real");
    let guard = super::UsageLimitGuard {
        task_id: "t-2760-st".into(),
        source: "dev".into(),
        branch: "feat/WRONG".into(),
        episode_id: "ep".into(),
    };
    assert_eq!(
        super::apply_usage_limit_block(&home, &guard, r#"{"episode_id":"ep"}"#.into()).expect("ok"),
        super::ApplyOutcome::Stale,
        "a guard/branch mismatch → Stale"
    );
    assert_eq!(
        record_on_board(&home, "proj-st", "t-2760-st").status,
        crate::task_events::TaskStatus::Claimed,
        "no Blocked event written on a guard mismatch"
    );
}

/// RED (core item 2+3): `with_revalidated_board` re-resolves the strict route UNDER
/// the per-id lock and REFUSES the write — without running the closure — when the
/// route is no longer unique (a concurrent duplicate-create raced the mutation in).
/// This is the write-time revalidation that a stale pre-lock route cannot give.
#[test]
fn with_revalidated_board_refuses_when_route_became_ambiguous_2760() {
    let home = tmp_home("reval-ambig");
    seed_task_on_board(&home, "proj-rv", "t-2760-rv");
    let routed = load_routed(&home, "t-2760-rv").expect("resolve");

    // A concurrent create lands the SAME id on another board → route now Ambiguous.
    seed_task_on_board(&home, "proj-other", "t-2760-rv");

    let ran = std::cell::Cell::new(false);
    let result = routed.with_revalidated_board(&home, |_board| ran.set(true));
    assert!(
        result.is_err(),
        "revalidation must refuse when the route is no longer unique"
    );
    assert!(
        !ran.get(),
        "the write closure must NOT run when revalidation fails (zero side effect)"
    );
}
