//! t-20260713015904150648-15764-33: a COMMITTED terminal→open
//! `TaskEvent::Reopened` must hand the preserved owner exactly one fresh
//! durable `task_reopened` inbox row (a team-owned task routes to its recorded
//! `routed_to` orchestrator) — and nothing else may enqueue one: Released
//! (Claimed/InProgress→open), a failed in-lock precondition, a metadata-only
//! update, and a NON-terminal Reopened (Backlog→open) all stay silent.
//! Every test drives the real MCP `handle` surface end-to-end.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::tasks::handle;

fn tmp_home(name: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-reopened-notify-test-{}-{}-{}",
        std::process::id(),
        name,
        id
    ));
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// Drain `agent`'s inbox and keep only `task_reopened` rows.
fn reopened_rows(
    home: &std::path::Path,
    agent: &str,
) -> Vec<crate::inbox::message::InboxMessage> {
    crate::inbox::storage::drain(home, agent)
        .into_iter()
        .filter(|m| m.kind.as_deref() == Some("task_reopened"))
        .collect()
}

/// Layout-independent negative probe: true if ANY file under `home` contains a
/// serialized `task_reopened` row. Negative tests must not depend on guessing
/// which principal's inbox a buggy implementation might have targeted.
fn any_task_reopened_row_anywhere(dir: &std::path::Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if any_task_reopened_row_anywhere(&path) {
                return true;
            }
        } else if let Ok(body) = std::fs::read_to_string(&path) {
            if body.contains("\"task_reopened\"") {
                return true;
            }
        }
    }
    false
}

fn create_task(home: &std::path::Path, assignee: Option<&str>) -> String {
    let mut args = serde_json::json!({
        "action": "create",
        "title": "reopen notify probe",
    });
    if let Some(a) = assignee {
        args["assignee"] = serde_json::json!(a);
    }
    let created = handle(home, "lead", &args);
    created["id"]
        .as_str()
        .unwrap_or_else(|| panic!("create failed: {created}"))
        .to_string()
}

fn set_status(
    home: &std::path::Path,
    caller: &str,
    id: &str,
    status: &str,
) -> serde_json::Value {
    handle(
        home,
        caller,
        &serde_json::json!({"action": "update", "id": id, "status": status}),
    )
}

fn assert_updated(result: &serde_json::Value) {
    assert!(
        result.get("error").is_none(),
        "transition unexpectedly failed: {result}"
    );
}

/// RED 1 (owned terminal→open ⇒ one row): done→open enqueues exactly one
/// `task_reopened` row to the preserved owner, carrying task_id/correlation_id,
/// the board/project source, and status/result reconciliation guidance. A
/// second reopen generation gets its own FRESH row (no reuse, no dedup across
/// generations).
#[test]
fn owned_done_to_open_enqueues_exactly_one_row_15764_33() {
    let home = tmp_home("done-open-one-row");
    let id = create_task(&home, Some("dev-agent"));
    handle(
        &home,
        "dev-agent",
        &serde_json::json!({
            "action": "update", "id": id,
            "status": "done", "result": "shipped v1",
        }),
    );
    assert_updated(&set_status(&home, "dev-agent", &id, "open"));

    let rows = reopened_rows(&home, "dev-agent");
    assert_eq!(rows.len(), 1, "exactly one task_reopened row: {rows:?}");
    let row = &rows[0];
    assert_eq!(row.task_id.as_deref(), Some(id.as_str()), "{row:?}");
    assert_eq!(row.correlation_id.as_deref(), Some(id.as_str()), "{row:?}");
    assert_eq!(row.from, "system:task_board", "{row:?}");
    for needle in [
        id.as_str(),
        "done → open",
        "default", // DEFAULT_PROJECT board source
        "claim",
        "result",
        "shipped v1",
    ] {
        assert!(
            row.text.contains(needle),
            "row text missing {needle:?}: {}",
            row.text
        );
    }

    // Second reopen generation → its own fresh row (drain above consumed the
    // first, so exactly one NEW row must appear now).
    assert_updated(&set_status(&home, "dev-agent", &id, "done"));
    assert_updated(&set_status(&home, "dev-agent", &id, "open"));
    let second = reopened_rows(&home, "dev-agent");
    assert_eq!(second.len(), 1, "fresh row per reopen generation: {second:?}");
    assert_ne!(
        second[0].id, row.id,
        "second generation must be a NEW durable row, not a reuse"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// RED 1b: cancelled→open is the other reachable terminal reopen
/// (Superseded→open is rejected by `can_transition_to`) and must also notify.
#[test]
fn owned_cancelled_to_open_enqueues_one_row_15764_33() {
    let home = tmp_home("cancelled-open-one-row");
    let id = create_task(&home, Some("dev-agent"));
    assert_updated(&set_status(&home, "dev-agent", &id, "cancelled"));
    assert_updated(&set_status(&home, "dev-agent", &id, "open"));
    let rows = reopened_rows(&home, "dev-agent");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(
        rows[0].text.contains("cancelled → open"),
        "{}",
        rows[0].text
    );
    std::fs::remove_dir_all(&home).ok();
}

/// RED 2 (ownerless ⇒ none): a terminal→open reopen of an unowned task has no
/// one to notify — no row may appear anywhere.
#[test]
fn ownerless_reopen_enqueues_nothing_15764_33() {
    let home = tmp_home("ownerless-none");
    let id = create_task(&home, None);
    assert_updated(&set_status(&home, "lead", &id, "done"));
    assert_updated(&set_status(&home, "lead", &id, "open"));
    assert!(
        !any_task_reopened_row_anywhere(&home),
        "ownerless reopen must enqueue nothing"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// RED 3 (Claimed/InProgress Released ⇒ none): claim-release emits `Released`
/// (owner cleared), not a reopen — no notification.
#[test]
fn released_claimed_and_in_progress_enqueue_nothing_15764_33() {
    let home = tmp_home("released-none");
    let a = create_task(&home, Some("dev-agent"));
    assert_updated(&set_status(&home, "dev-agent", &a, "claimed"));
    assert_updated(&set_status(&home, "dev-agent", &a, "open"));
    let b = create_task(&home, Some("dev-agent"));
    assert_updated(&set_status(&home, "dev-agent", &b, "claimed"));
    assert_updated(&set_status(&home, "dev-agent", &b, "in_progress"));
    assert_updated(&set_status(&home, "dev-agent", &b, "open"));
    assert!(
        !any_task_reopened_row_anywhere(&home),
        "Released transitions must enqueue nothing"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// RED 4 (failed/precondition race ⇒ none): a concurrent owner change between
/// the out-of-lock ACL read and the in-lock commit fails the batch precondition
/// (#1868/:231) — the reopen does NOT commit and nothing may be enqueued.
#[test]
fn failed_precondition_race_enqueues_nothing_15764_33() {
    let home = tmp_home("race-none");
    let id = create_task(&home, Some("dev-agent"));
    assert_updated(&set_status(&home, "dev-agent", &id, "done"));
    let hook_home = home.clone();
    let hook_id = id.clone();
    crate::tasks::set_before_mutation_commit_hook_for_test(move || {
        // Concurrent writer wins the race: reassign the owner before the
        // reopener's batch takes the append lock.
        let r = handle(
            &hook_home,
            "dev-agent",
            &serde_json::json!({
                "action": "update", "id": hook_id, "assignee": "other-agent",
            }),
        );
        assert!(r.get("error").is_none(), "hook reassign failed: {r}");
    });
    let result = set_status(&home, "dev-agent", &id, "open");
    assert!(
        result.get("error").is_some(),
        "owner drift must fail the reopen closed: {result}"
    );
    assert!(
        !any_task_reopened_row_anywhere(&home),
        "a failed reopen must enqueue nothing"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// RED 5 (metadata-only ⇒ none): result/priority updates without a status
/// transition — including a result backfill on an already-done task — must not
/// notify.
#[test]
fn metadata_only_update_enqueues_nothing_15764_33() {
    let home = tmp_home("metadata-none");
    let id = create_task(&home, Some("dev-agent"));
    assert_updated(&set_status(&home, "dev-agent", &id, "done"));
    let r = handle(
        &home,
        "dev-agent",
        &serde_json::json!({
            "action": "update", "id": id,
            "result": "backfilled outcome", "priority": "high",
        }),
    );
    assert!(r.get("error").is_none(), "metadata update failed: {r}");
    assert!(
        !any_task_reopened_row_anywhere(&home),
        "metadata-only updates must enqueue nothing"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// RED 5b (non-terminal Reopened ⇒ none): Backlog→open COMMITS a real
/// `Reopened` event but the task was never terminal — the notification gate
/// must discriminate on the prior status, not on the event kind.
#[test]
fn backlog_to_open_reopened_enqueues_nothing_15764_33() {
    let home = tmp_home("backlog-none");
    let emitter = crate::task_events::InstanceName::from("test:operator");
    let id = "t-15764-33-backlog";
    crate::task_events::append(
        &home,
        &emitter,
        crate::task_events::TaskEvent::Created {
            task_id: crate::task_events::TaskId(id.into()),
            title: "backlog probe".into(),
            description: String::new(),
            priority: "normal".into(),
            owner: Some(crate::task_events::InstanceName::from("dev-agent")),
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
        },
    )
    .expect("seed created");
    crate::task_events::append(
        &home,
        &emitter,
        crate::task_events::TaskEvent::MovedToBacklog {
            task_id: crate::task_events::TaskId(id.into()),
        },
    )
    .expect("seed backlog");
    assert_updated(&set_status(&home, "dev-agent", id, "open"));
    assert!(
        !any_task_reopened_row_anywhere(&home),
        "a non-terminal Reopened must enqueue nothing"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// RED 6 (team route): a team-owned task's reopen row goes to the recorded
/// `routed_to` orchestrator — never to the team name itself.
#[test]
fn team_owned_reopen_routes_to_orchestrator_15764_33() {
    let home = tmp_home("team-route");
    let created = crate::teams::create(
        &home,
        &serde_json::json!({
            "name": "team-x",
            "members": ["worker-1", "orch-1"],
            "orchestrator": "orch-1",
        }),
    );
    assert!(created.get("error").is_none(), "team create failed: {created}");
    let id = create_task(&home, Some("team-x"));
    assert_updated(&set_status(&home, "orch-1", &id, "done"));
    assert_updated(&set_status(&home, "orch-1", &id, "open"));
    let orch_rows = reopened_rows(&home, "orch-1");
    assert_eq!(orch_rows.len(), 1, "orchestrator gets the row: {orch_rows:?}");
    assert!(
        orch_rows[0].text.contains("team-x"),
        "row names the preserved team owner: {}",
        orch_rows[0].text
    );
    assert!(
        reopened_rows(&home, "team-x").is_empty(),
        "the team name itself must not receive the row"
    );
    std::fs::remove_dir_all(&home).ok();
}
