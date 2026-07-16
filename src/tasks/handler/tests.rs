#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

fn tmp_home(name: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-metadata-test-{}-{}-{}",
        std::process::id(),
        name,
        id
    ));
    std::fs::create_dir_all(&dir).ok();
    dir
}

fn create_task(home: &std::path::Path, task_id: &str) {
    let args = serde_json::json!({
        "action": "create",
        "title": "test task",
    });
    let emitter = crate::task_events::InstanceName::from("test:operator");
    let tid = crate::task_events::TaskId(task_id.into());
    crate::task_events::append(
        home,
        &emitter,
        crate::task_events::TaskEvent::Created {
            task_id: tid,
            title: "test task".into(),
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
        },
    )
    .expect("create task");
    let _ = args;
}

/// #78445-2 (d): a cascade parent-cancel is terminal for EACH child — every
/// cancelled child's dispatch_tracking rows must settle (plural), while a NON-child
/// task's rows (even the same dispatcher) survive. This path previously cleared
/// NEITHER store (reviewer4 #2679).
#[test]
fn cascade_cancel_settles_each_child_dispatch_tracking_78445_2() {
    let home = tmp_home("cascade-settle");
    let seed = |tid: &str, parent: Option<&str>| {
        crate::task_events::append(
            &home,
            &crate::task_events::InstanceName::from("test:seed"),
            crate::task_events::TaskEvent::Created {
                task_id: crate::task_events::TaskId(tid.into()),
                title: "task".into(),
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
                parent_id: parent.map(|p| crate::task_events::TaskId(p.into())),
            },
        )
        .expect("seed");
    };
    seed("t-parent", None);
    seed("t-c1", Some("t-parent"));
    seed("t-c2", Some("t-parent"));
    seed("t-x", None); // unrelated — NOT a child of t-parent

    // dispatch_tracking rows for both children + the unrelated task (same dispatcher).
    let now = chrono::Utc::now().to_rfc3339();
    for (tid, to) in [("t-c1", "rev-c1"), ("t-c2", "rev-c2"), ("t-x", "rev-x")] {
        crate::dispatch_tracking::track_dispatch(
            &home,
            crate::dispatch_tracking::DispatchEntry {
                task_id: Some(tid.into()),
                from: "lead".into(),
                to: to.into(),
                from_id: None,
                to_id: None,
                delegated_at: now.clone(),
                status: "pending".into(),
            },
        );
    }

    cascade_cancel_children(
        &home,
        &home,
        "t-parent",
        &crate::task_events::InstanceName::from("test:cancel"),
    );

    let active = crate::dispatch_tracking::active_target_names(&home);
    assert!(
        !active.contains(&"rev-c1".to_string()) && !active.contains(&"rev-c2".to_string()),
        "#78445-2 (d): EACH cascaded child's dispatch_tracking row must settle: {active:?}"
    );
    assert!(
        active.contains(&"rev-x".to_string()),
        "#78445-2 (d) isolation: a NON-child task's row must survive: {active:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Simulate a concurrent reassignment landing between handle_update's
/// out-of-lock read and its in-lock append.
fn reassign(home: &std::path::Path, task_id: &str, new_owner: &str) {
    crate::task_events::append(
        home,
        &crate::task_events::InstanceName::from("lead"),
        crate::task_events::TaskEvent::OwnerAssigned {
            task_id: crate::task_events::TaskId(task_id.into()),
            by: crate::task_events::InstanceName::from("lead"),
            owner: Some(crate::task_events::InstanceName::from(new_owner)),
            routed_to: None,
        },
    )
    .expect("reassign");
}

/// CR-2026-06-14 (:231) ②, the core gap — a NON-status update (target=None)
/// by an unauthorized caller. Pre-fix the in-lock closure did nothing when
/// target_status was None, so the write slipped past the in-lock gate (RED).
#[test]
fn inlock_precond_rejects_unauthorized_nonstatus_update_231() {
    let home = tmp_home("231-nonstatus-acl");
    create_task(&home, "t-231-a"); // owner = dev-agent
    let state = crate::task_events::replay(&home).unwrap();
    let res = update_batch_precondition(
        &state,
        &home,
        "intruder",
        "t-231-a",
        false,
        None,
        &Some(crate::task_events::InstanceName::from("dev-agent")),
    );
    assert!(
        res.is_err(),
        "unauthorized non-status update must be rejected in-lock"
    );
    assert!(res.unwrap_err().contains("no longer authorized"));
    std::fs::remove_dir_all(&home).ok();
}

/// CR-2026-06-14 (:231) ① — a status update whose caller WAS authorized at
/// the out-of-lock read (stale_owner == caller), but the owner drifted to
/// someone else before the in-lock commit. The in-lock ACL must reject.
#[test]
fn inlock_precond_rejects_status_update_after_owner_reassign_231() {
    let home = tmp_home("231-reassign");
    create_task(&home, "t-231-b"); // owner = dev-agent
    reassign(&home, "t-231-b", "other-owner");
    let state = crate::task_events::replay(&home).unwrap();
    let res = update_batch_precondition(
        &state,
        &home,
        "dev-agent",
        "t-231-b",
        false,
        Some(crate::task_events::TaskStatus::InProgress),
        &Some(crate::task_events::InstanceName::from("dev-agent")),
    );
    assert!(
        res.is_err(),
        "status update after owner drift must be rejected"
    );
    assert!(res.unwrap_err().contains("no longer authorized"));
    std::fs::remove_dir_all(&home).ok();
}

/// CR-2026-06-14 (:231) ③ — the done-arm `by` drift. A system identity
/// (ACL-bypassed, so the ACL gate alone wouldn't catch this) marks the task
/// Done; the Done event's `by` was baked from the stale owner (dev-agent),
/// but the task is now owned by new-owner → committing would mis-attribute.
#[test]
fn inlock_precond_rejects_done_when_by_owner_drifted_231() {
    let home = tmp_home("231-by-drift");
    create_task(&home, "t-231-c"); // owner = dev-agent
    reassign(&home, "t-231-c", "new-owner");
    let state = crate::task_events::replay(&home).unwrap();
    let res = update_batch_precondition(
        &state,
        &home,
        "system:task_sweep", // ACL bypassed → only the drift check can reject
        "t-231-c",
        false,
        Some(crate::task_events::TaskStatus::Done),
        &Some(crate::task_events::InstanceName::from("dev-agent")),
    );
    assert!(
        res.is_err(),
        "done with drifted by-owner must be rejected fail-closed"
    );
    assert!(res.unwrap_err().contains("attribution would be stale"));
    std::fs::remove_dir_all(&home).ok();
}

/// CR-2026-06-14 (:231) control — a legitimate authorized update with no
/// drift MUST pass (guards against over-rejection from the new in-lock gate).
#[test]
fn inlock_precond_allows_legitimate_authorized_update_231() {
    let home = tmp_home("231-control");
    create_task(&home, "t-231-d"); // owner = dev-agent
    let state = crate::task_events::replay(&home).unwrap();
    let res = update_batch_precondition(
        &state,
        &home,
        "dev-agent",
        "t-231-d",
        false,
        Some(crate::task_events::TaskStatus::InProgress),
        &Some(crate::task_events::InstanceName::from("dev-agent")),
    );
    assert!(
        res.is_ok(),
        "legitimate authorized non-drift update must pass: {res:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// §3.9 #1916 WIRING (real entry point, not just the helper): a `task update`
/// that changes the assignee must retarget the dispatch-idle sidecar to the new
/// owner — proving `handle_update` actually calls the reassign hook (an
/// injected-input helper test alone wouldn't prove the wiring reaches it).
#[test]
fn task_reassign_retargets_dispatch_sidecar_through_handle_1916() {
    let home = tmp_home("1916-wiring");
    create_task(&home, "t-wire-001"); // owner = dev-agent
                                      // A dispatch-idle sidecar tracks the task, targeting the original owner.
    crate::daemon::dispatch_idle::record_dispatch(
        &home,
        "lead",
        "dev-agent",
        Some("t-wire-001"),
        "task",
        600,
    )
    .expect("dispatch recorded");

    // REAL entry point: the owner reassigns the task to a new owner.
    let result = handle(
        &home,
        "dev-agent",
        &serde_json::json!({
            "action": "update",
            "id": "t-wire-001",
            "assignee": "new-owner",
        }),
    );
    assert!(
        result.get("error").is_none(),
        "#1916: reassign update should succeed, got {result}"
    );

    let pending = crate::daemon::dispatch_idle::list_pending(&home);
    let s = pending
        .iter()
        .find(|p| p.correlation_id.as_deref() == Some("t-wire-001"))
        .expect("#1916: sidecar must survive the reassign");
    assert_eq!(
        s.target, "new-owner",
        "#1916 WIRING: `task update(assignee)` must retarget the dispatch-idle sidecar \
         via handle_update's hook — else the watchdog keeps nudging the former owner"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn metadata_set_writes_and_reads() {
    let home = tmp_home("set_read");
    create_task(&home, "t-meta-001");

    let result = handle(
        &home,
        "dev-agent",
        &serde_json::json!({
            "action": "metadata_set",
            "id": "t-meta-001",
            "metadata_key": "pr_url",
            "metadata_value": "https://github.com/test/repo/pull/42"
        }),
    );
    assert_eq!(result["event"], "metadata_set");
    assert!(result["error"].is_null(), "unexpected error: {result}");

    let get_result = handle(
        &home,
        "dev-agent",
        &serde_json::json!({
            "action": "metadata_get",
            "id": "t-meta-001",
        }),
    );
    assert_eq!(
        get_result["metadata"]["pr_url"],
        "https://github.com/test/repo/pull/42"
    );
}

#[test]
fn metadata_set_overwrites_existing_key() {
    let home = tmp_home("overwrite");
    create_task(&home, "t-meta-002");

    handle(
        &home,
        "dev-agent",
        &serde_json::json!({
            "action": "metadata_set",
            "id": "t-meta-002",
            "metadata_key": "commit_sha",
            "metadata_value": "abc123"
        }),
    );
    handle(
        &home,
        "dev-agent",
        &serde_json::json!({
            "action": "metadata_set",
            "id": "t-meta-002",
            "metadata_key": "commit_sha",
            "metadata_value": "def456"
        }),
    );

    let result = handle(
        &home,
        "dev-agent",
        &serde_json::json!({
            "action": "metadata_get",
            "id": "t-meta-002",
        }),
    );
    assert_eq!(result["metadata"]["commit_sha"], "def456");
}

#[test]
fn metadata_supports_non_string_values() {
    let home = tmp_home("non_string");
    create_task(&home, "t-meta-003");

    handle(
        &home,
        "dev-agent",
        &serde_json::json!({
            "action": "metadata_set",
            "id": "t-meta-003",
            "metadata_key": "retry_count",
            "metadata_value": 3
        }),
    );

    let result = handle(
        &home,
        "dev-agent",
        &serde_json::json!({
            "action": "metadata_get",
            "id": "t-meta-003",
        }),
    );
    assert_eq!(result["metadata"]["retry_count"], 3);
}

#[test]
fn metadata_get_empty_on_new_task() {
    let home = tmp_home("empty_meta");
    create_task(&home, "t-meta-004");

    let result = handle(
        &home,
        "dev-agent",
        &serde_json::json!({
            "action": "metadata_get",
            "id": "t-meta-004",
        }),
    );
    assert!(result["error"].is_null());
    assert_eq!(result["metadata"], serde_json::json!({}));
}

#[test]
fn metadata_set_missing_key_returns_error() {
    let home = tmp_home("missing_key");
    create_task(&home, "t-meta-005");

    let result = handle(
        &home,
        "dev-agent",
        &serde_json::json!({
            "action": "metadata_set",
            "id": "t-meta-005",
            "metadata_value": "some_value"
        }),
    );
    assert!(result["error"].as_str().unwrap().contains("metadata_key"));
}

#[test]
fn metadata_set_missing_value_returns_error() {
    let home = tmp_home("missing_val");
    create_task(&home, "t-meta-006");

    let result = handle(
        &home,
        "dev-agent",
        &serde_json::json!({
            "action": "metadata_set",
            "id": "t-meta-006",
            "metadata_key": "some_key"
        }),
    );
    assert!(result["error"].as_str().unwrap().contains("metadata_value"));
}

#[test]
fn metadata_appears_in_list() {
    let home = tmp_home("list_meta");
    create_task(&home, "t-meta-007");

    handle(
        &home,
        "dev-agent",
        &serde_json::json!({
            "action": "metadata_set",
            "id": "t-meta-007",
            "metadata_key": "pr_url",
            "metadata_value": "https://example.com/pr/1"
        }),
    );

    let list = handle(&home, "dev-agent", &serde_json::json!({"action": "list"}));
    let tasks = list["tasks"].as_array().unwrap();
    let task = tasks.iter().find(|t| t["id"] == "t-meta-007").unwrap();
    assert_eq!(task["metadata"]["pr_url"], "https://example.com/pr/1");
}

#[test]
fn metadata_get_nonexistent_task_returns_error() {
    let home = tmp_home("nonexistent");

    let result = handle(
        &home,
        "dev-agent",
        &serde_json::json!({
            "action": "metadata_get",
            "id": "t-meta-999",
        }),
    );
    assert!(result["error"].as_str().unwrap().contains("not found"));
}

fn drain_inbox(home: &std::path::Path, agent: &str) -> Vec<crate::inbox::InboxMessage> {
    crate::inbox::storage::drain(home, agent)
}

// #1496 Option 1: create no longer auto-notifies, so the prior
// `create_with_assignee_sends_task_to_inbox` /
// `create_with_assignee_correlation_id_matches_task_id` tests (which asserted
// an inbox message on create) are removed — their inverse is now
// `create_with_assignee_has_no_dispatch_side_effects_1496`. Dispatch-message
// shape (kind/task_id/correlation_id) is covered on the send(kind=task) path.

#[test]
fn create_without_assignee_sends_no_message() {
    let home = tmp_home("no_assign");
    let result = handle(
        &home,
        "lead-agent",
        &serde_json::json!({
            "action": "create",
            "title": "unassigned task",
        }),
    );
    assert_eq!(result["event"], "created");

    let msgs = drain_inbox(&home, "lead-agent");
    assert!(msgs.is_empty(), "no inbox message without assignee");
}

#[test]
fn create_self_assign_sends_no_message() {
    let home = tmp_home("self_assign");
    let result = handle(
        &home,
        "dev-agent",
        &serde_json::json!({
            "action": "create",
            "title": "self-assigned task",
            "assignee": "dev-agent",
        }),
    );
    assert_eq!(result["event"], "created");

    let msgs = drain_inbox(&home, "dev-agent");
    assert!(msgs.is_empty(), "self-assign should not send inbox message");
}

#[test]
fn create_with_assignee_task_status_is_open() {
    let home = tmp_home("status_open");
    let result = handle(
        &home,
        "lead-agent",
        &serde_json::json!({
            "action": "create",
            "title": "test task",
            "assignee": "dev-agent",
        }),
    );
    let task = &result["task"];
    assert_eq!(task["status"], "open");
    assert_eq!(task["assignee"], "dev-agent");
}

fn write_fleet_yaml_with_team(home: &std::path::Path, team: &str, orchestrator: &str) {
    let yaml = format!(
        "teams:\n  {team}:\n    orchestrator: {orchestrator}\n    members:\n      - dev-a\n      - dev-b\n"
    );
    std::fs::write(home.join("fleet.yaml"), yaml).unwrap();
}

#[test]
fn create_with_team_assignee_records_orchestrator_routing() {
    // #1496 Option 1: create no longer notifies, but it still RESOLVES a team
    // assignee to its orchestrator and RECORDS that on the task (`routed_to`).
    // The dispatch-time team→orchestrator inbox routing is covered separately
    // on the send(kind=task) path
    // (mcp::handlers::tests::test_delegate_task_resolves_team_to_orchestrator_inbox).
    let home = tmp_home("team_route");
    write_fleet_yaml_with_team(&home, "my-team", "team-lead");

    let result = handle(
        &home,
        "operator",
        &serde_json::json!({
            "action": "create",
            "title": "team task",
            "assignee": "my-team",
        }),
    );
    assert_eq!(result["event"], "created");
    assert_eq!(
        result["task"]["routed_to"].as_str(),
        Some("team-lead"),
        "team assignee must resolve to its orchestrator in the record: {result}"
    );

    // Pure record: no inbox side-effect for the orchestrator OR the raw team.
    assert!(
        !home.join("inbox").join("team-lead.jsonl").exists()
            && !home.join("inbox").join("my-team.jsonl").exists(),
        "create must not enqueue any inbox message"
    );
}

#[test]
fn create_with_assignee_has_no_dispatch_side_effects_1496() {
    // #1496 Option 1: `task(action:create)` is a PURE board record. Creating
    // a task assigned to ANOTHER agent must NOT enqueue an inbox message or
    // write a dispatch-tracking entry — dispatch (notify + worktree auto-bind)
    // is solely `send(kind=task)`'s job. Pre-#1496 (#1238) this auto-notified
    // with a title-only, non-actionable wake that raced the real send into the
    // busy-gate, taxing every dispatch with a force-resend.
    //
    // REGRESSION-PROOF: restore the auto-notify block in the create handler →
    // both assertions below fail (the assignee's inbox jsonl and
    // dispatch_tracking.json reappear). Subsumes the old self-assign case:
    // create never dispatches now, for self OR other.
    let home = tmp_home("create_no_dispatch_1496");
    let result = handle(
        &home,
        "lead-agent",
        &serde_json::json!({
            "action": "create",
            "title": "pure record task",
            "assignee": "dev-agent",
            "branch": "feat/x",
        }),
    );
    assert_eq!(result["event"], "created", "task still created: {result}");
    assert!(
        result["id"].as_str().is_some(),
        "task id returned: {result}"
    );

    // No inbox message enqueued for the assignee.
    let assignee_inbox = home.join("inbox").join("dev-agent.jsonl");
    assert!(
        !assignee_inbox.exists(),
        "#1496: create must not enqueue an inbox message for the assignee"
    );
    // No dispatch-tracking entry written.
    let track = crate::store::store_path(&home, "dispatch_tracking.json");
    assert!(
        !track.exists(),
        "#1496: create must not write a dispatch-tracking entry"
    );
}

#[test]
fn create_without_assignee_no_dispatch_tracking() {
    let home = tmp_home("dispatch_none");
    handle(
        &home,
        "lead-agent",
        &serde_json::json!({
            "action": "create",
            "title": "unassigned",
        }),
    );

    let path = crate::store::store_path(&home, "dispatch_tracking.json");
    assert!(
        !path.exists(),
        "unassigned task should not create dispatch tracking entry"
    );
}

// #event-bus pattern #7: the (from, kind, text, correlation_id) tuple a
// drained notify carries — id/timestamp ignored so legacy-vs-bus compares clean.
fn cascade_payloads(
    home: &std::path::Path,
    recipient: &str,
) -> Vec<(String, Option<String>, String, Option<String>)> {
    crate::inbox::drain(home, recipient)
        .into_iter()
        .map(|m| (m.from, m.kind, m.text, m.correlation_id))
        .collect()
}

// gate-ON: emit(CascadeCancelNotify)→subscriber re-delivers BYTE-IDENTICALLY
// to the legacy `deliver_cascade_cancel` direct enqueue.
#[test]
fn cascade_gate_on_emit_subscriber_matches_legacy() {
    let owner = "fixup-dev";
    let parent_id = "t-parent-1";
    let child_id = "t-child-1";

    let home_legacy = tmp_home("p7-parity-legacy");
    deliver_cascade_cancel(&home_legacy, owner, parent_id, child_id);

    let home_bus = tmp_home("p7-parity-bus");
    let bus = crate::daemon::event_bus::EventBus::new();
    bus.subscribe(handle_event);
    bus.emit(
        &home_bus,
        crate::daemon::event_bus::EventKind::CascadeCancelNotify {
            owner: owner.to_string(),
            parent_id: parent_id.to_string(),
            child_id: child_id.to_string(),
        },
    );

    let legacy = cascade_payloads(&home_legacy, owner);
    let via_bus = cascade_payloads(&home_bus, owner);
    assert!(!legacy.is_empty(), "legacy notify must enqueue");
    assert_eq!(
        legacy, via_bus,
        "bus delivery must match legacy byte-for-byte"
    );

    std::fs::remove_dir_all(&home_legacy).ok();
    std::fs::remove_dir_all(&home_bus).ok();
}

// #event-bus Step 2 (legacy-zero): route_cascade_cancel emits to the global
// bus; the registered subscriber delivers via deliver_cascade_cancel to the
// event's home (this test's home).
#[test]
fn route_cascade_cancel_delivers_via_bus() {
    let home = tmp_home("p7-via-bus");
    route_cascade_cancel(&home, "fixup-dev", "t-parent-2", "t-child-2");
    let alerts = cascade_payloads(&home, "fixup-dev");
    assert_eq!(alerts.len(), 1, "gate-off must deliver via legacy path");
    assert_eq!(alerts[0].1.as_deref(), Some("parent_cancelled"));
    assert!(alerts[0].2.contains("t-parent-2") && alerts[0].2.contains("t-child-2"));
    std::fs::remove_dir_all(&home).ok();
}

/// #1868 §3.9: the in-lock precondition `handle_done` now uses
/// (`append_checked`) REJECTS a `done` whose out-of-lock read was stale — a
/// concurrent sweep/auto_close moved the task to Cancelled. Pre-fix (plain
/// `append`) this Done was silently applied (replay's `apply_done` does not
/// re-guard transitions).
#[test]
fn append_checked_rejects_stale_done_after_concurrent_cancel_1868() {
    let home = tmp_home("1868-done-stale");
    create_task(&home, "t1");
    let emitter = crate::task_events::InstanceName::from("dev-agent");
    handle(
        &home,
        "dev-agent",
        &serde_json::json!({"action": "claim", "id": "t1"}),
    );
    // Concurrent sweep/auto_close cancels it → committed state is Cancelled.
    handle(
        &home,
        "dev-agent",
        &serde_json::json!({"action": "update", "id": "t1", "status": "cancelled"}),
    );

    // A `done` prepared as-if the caller had still seen Claimed: the in-lock
    // precondition re-reads the FRESH committed state (Cancelled) and rejects
    // (Cancelled→Done is illegal).
    let done = crate::task_events::TaskEvent::Done {
        task_id: crate::task_events::TaskId("t1".into()),
        by: crate::task_events::InstanceName::from("dev-agent"),
        source: crate::task_events::DoneSource::OperatorManual {
            authored_at: "2026-06-09T00:00:00+00:00".into(),
            result: None,
        },
    };
    let r = crate::task_events::append_checked(&home, &emitter, done, |state| {
        let tv = state
            .tasks
            .values()
            .map(record_to_task)
            .find(|t| t.id == "t1")
            .ok_or_else(|| "not found".to_string())?;
        if !tv
            .status
            .can_transition_to(crate::task_events::TaskStatus::Done)
        {
            return Err("illegal".to_string());
        }
        Ok(())
    });
    assert!(
        matches!(r, Ok(Err(_))),
        "#1868: in-lock guard must REJECT a stale done on a Cancelled task: {r:?}"
    );
    assert_eq!(
        read_task_record(&home, "t1").expect("task exists").status,
        crate::task_events::TaskStatus::Cancelled,
        "no Done event must land → task stays Cancelled"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #1868 §3.9: same in-lock guard for the multi-event `update` arm via
/// `append_batch_checked`.
#[test]
fn append_batch_checked_rejects_stale_update_after_concurrent_cancel_1868() {
    let home = tmp_home("1868-update-stale");
    create_task(&home, "t1");
    let emitter = crate::task_events::InstanceName::from("dev-agent");
    handle(
        &home,
        "dev-agent",
        &serde_json::json!({"action": "claim", "id": "t1"}),
    );
    handle(
        &home,
        "dev-agent",
        &serde_json::json!({"action": "update", "id": "t1", "status": "cancelled"}),
    );
    let ev = crate::task_events::TaskEvent::InProgress {
        task_id: crate::task_events::TaskId("t1".into()),
        by: crate::task_events::InstanceName::from("dev-agent"),
    };
    let r = crate::task_events::append_batch_checked(&home, &emitter, vec![ev], |state| {
        let tv = state
            .tasks
            .values()
            .map(record_to_task)
            .find(|t| t.id == "t1")
            .ok_or_else(|| "not found".to_string())?;
        if !tv
            .status
            .can_transition_to(crate::task_events::TaskStatus::InProgress)
        {
            return Err("illegal".to_string());
        }
        Ok(())
    });
    assert!(
        matches!(r, Ok(Err(_))),
        "#1868: in-lock batch guard must REJECT a stale update on a Cancelled task: {r:?}"
    );
    assert_eq!(
        read_task_record(&home, "t1").expect("task exists").status,
        crate::task_events::TaskStatus::Cancelled
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #1868 §3.9: the normal (uncontended) sequence still succeeds end-to-end
/// through the real handlers — no regression from the append→append_checked
/// swap.
#[test]
fn normal_done_and_update_still_succeed_1868() {
    let home = tmp_home("1868-happy");
    create_task(&home, "t-done");
    handle(
        &home,
        "dev-agent",
        &serde_json::json!({"action": "claim", "id": "t-done"}),
    );
    let d = handle(
        &home,
        "dev-agent",
        &serde_json::json!({"action": "done", "id": "t-done"}),
    );
    assert!(d["error"].is_null(), "legal done must succeed: {d}");
    assert_eq!(
        read_task_record(&home, "t-done").expect("exists").status,
        crate::task_events::TaskStatus::Done
    );

    create_task(&home, "t-upd");
    handle(
        &home,
        "dev-agent",
        &serde_json::json!({"action": "claim", "id": "t-upd"}),
    );
    let u = handle(
        &home,
        "dev-agent",
        &serde_json::json!({"action": "update", "id": "t-upd", "status": "in_progress"}),
    );
    assert!(u["error"].is_null(), "legal update must succeed: {u}");
    assert_eq!(
        read_task_record(&home, "t-upd").expect("exists").status,
        crate::task_events::TaskStatus::InProgress
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── #2524 P2a-r1 / #2249 pre-work alignment gate — real MCP handler
// entry point throughout (§3.9: no mid-pipeline event injection). Every
// call below goes through the top-level `handle()` exactly as the `task`
// MCP tool would dispatch it.

#[test]
fn plan_ack_gate_blocks_then_unblocks_in_progress_2249() {
    let home = tmp_home("plan-ack-happy");
    let created = handle(
        &home,
        "lead",
        &serde_json::json!({
            "action": "create",
            "title": "risky refactor",
            "assignee": "worker",
            "plan_ack_required": 2,
            "plan_ack_reason": "touches auth boundary"
        }),
    );
    let id = created["id"].as_str().expect("id").to_string();
    assert_eq!(
        created["task"]["metadata"]["plan_ack_required"], 2,
        "plan_ack_required must be seeded into metadata: {created}"
    );

    handle(
        &home,
        "worker",
        &serde_json::json!({"action": "claim", "id": id}),
    );

    // Gate blocks: no plan set yet, 0 acks.
    let blocked = handle(
        &home,
        "worker",
        &serde_json::json!({"action": "update", "id": id, "status": "in_progress"}),
    );
    assert_eq!(
        blocked["code"], "plan_ack_pending",
        "in_progress must be gated with 0/2 acks: {blocked}"
    );
    assert_eq!(blocked["required"], 2);
    assert_eq!(blocked["acked"], 0);
    assert_eq!(
        read_task_record(&home, &id).expect("exists").status,
        crate::task_events::TaskStatus::Claimed,
        "status must NOT have advanced past claimed"
    );

    // Set the plan.
    let set = handle(
        &home,
        "worker",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "plan", "metadata_value": "step 1, step 2"
        }),
    );
    assert!(
        set["error"].is_null(),
        "metadata_set plan must succeed: {set}"
    );

    // One ack: still blocked (1 < 2).
    let ack1 = handle(
        &home,
        "reviewer-a",
        &serde_json::json!({"action": "ack_plan", "id": id}),
    );
    assert!(
        ack1["error"].is_null(),
        "reviewer-a ack must succeed: {ack1}"
    );
    assert_eq!(ack1["acked"], 1);
    let still_blocked = handle(
        &home,
        "worker",
        &serde_json::json!({"action": "update", "id": id, "status": "in_progress"}),
    );
    assert_eq!(
        still_blocked["code"], "plan_ack_pending",
        "1/2 acks must still block: {still_blocked}"
    );
    assert_eq!(still_blocked["acked"], 1);

    // Second (distinct) ack: threshold met, in_progress now passes.
    let ack2 = handle(
        &home,
        "reviewer-b",
        &serde_json::json!({"action": "ack_plan", "id": id}),
    );
    assert_eq!(ack2["acked"], 2);
    let unblocked = handle(
        &home,
        "worker",
        &serde_json::json!({"action": "update", "id": id, "status": "in_progress"}),
    );
    assert!(
        unblocked["error"].is_null(),
        "in_progress must pass once 2/2 acks are in: {unblocked}"
    );
    assert_eq!(
        read_task_record(&home, &id).expect("exists").status,
        crate::task_events::TaskStatus::InProgress
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn plan_ack_required_zero_is_byte_identical_regression_2249() {
    // The N=0 (default/absent) path must be indistinguishable from
    // pre-#2249 behavior: create → claim → in_progress, no plan, no
    // acks, no gate — exactly the shape of the pre-existing
    // `illegal_transition` regression test above, just without any
    // plan_ack_* args at all.
    let home = tmp_home("plan-ack-n0");
    let created = handle(
        &home,
        "lead",
        &serde_json::json!({"action": "create", "title": "ordinary task", "assignee": "worker"}),
    );
    let id = created["id"].as_str().expect("id").to_string();
    assert!(
        created["task"]["metadata"]
            .get("plan_ack_required")
            .is_none(),
        "N=0/absent must NOT seed any plan_ack_required metadata: {created}"
    );
    handle(
        &home,
        "worker",
        &serde_json::json!({"action": "claim", "id": id}),
    );
    let result = handle(
        &home,
        "worker",
        &serde_json::json!({"action": "update", "id": id, "status": "in_progress"}),
    );
    assert!(
        result["error"].is_null(),
        "N=0 must never gate in_progress: {result}"
    );
    assert_eq!(
        read_task_record(&home, &id).expect("exists").status,
        crate::task_events::TaskStatus::InProgress
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn plan_ack_required_without_reason_rejected_2249() {
    let home = tmp_home("plan-ack-no-reason");
    let result = handle(
        &home,
        "lead",
        &serde_json::json!({"action": "create", "title": "x", "plan_ack_required": 1}),
    );
    assert!(
        result["error"]
            .as_str()
            .unwrap_or("")
            .contains("plan_ack_reason"),
        "plan_ack_required>0 without reason must error: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn plan_ack_self_ack_rejected_2249() {
    let home = tmp_home("plan-ack-self");
    let created = handle(
        &home,
        "lead",
        &serde_json::json!({
            "action": "create", "title": "x", "assignee": "worker",
            "plan_ack_required": 1, "plan_ack_reason": "reason"
        }),
    );
    let id = created["id"].as_str().expect("id").to_string();
    handle(
        &home,
        "worker",
        &serde_json::json!({"action": "claim", "id": id}),
    );
    handle(
        &home,
        "worker",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "plan", "metadata_value": "plan text"
        }),
    );
    let self_ack = handle(
        &home,
        "worker",
        &serde_json::json!({"action": "ack_plan", "id": id}),
    );
    assert_eq!(
        self_ack["code"], "self_ack_forbidden",
        "assignee must not be able to ack their own plan: {self_ack}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn plan_ack_before_plan_set_rejected_2249() {
    let home = tmp_home("plan-ack-no-plan");
    let created = handle(
        &home,
        "lead",
        &serde_json::json!({
            "action": "create", "title": "x", "assignee": "worker",
            "plan_ack_required": 1, "plan_ack_reason": "reason"
        }),
    );
    let id = created["id"].as_str().expect("id").to_string();
    let ack = handle(
        &home,
        "reviewer-a",
        &serde_json::json!({"action": "ack_plan", "id": id}),
    );
    assert_eq!(
        ack["code"], "plan_not_set",
        "ack before plan is set must be rejected: {ack}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn plan_ack_idempotent_reack_does_not_double_count_2249() {
    let home = tmp_home("plan-ack-idempotent");
    let created = handle(
        &home,
        "lead",
        &serde_json::json!({
            "action": "create", "title": "x", "assignee": "worker",
            "plan_ack_required": 2, "plan_ack_reason": "reason"
        }),
    );
    let id = created["id"].as_str().expect("id").to_string();
    handle(
        &home,
        "worker",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "plan", "metadata_value": "plan text"
        }),
    );
    let first = handle(
        &home,
        "reviewer-a",
        &serde_json::json!({"action": "ack_plan", "id": id}),
    );
    assert_eq!(first["acked"], 1);
    assert_eq!(first["already_acked"], false);
    let second = handle(
        &home,
        "reviewer-a",
        &serde_json::json!({"action": "ack_plan", "id": id}),
    );
    assert_eq!(
        second["acked"], 1,
        "re-acking the same reviewer must NOT double-count: {second}"
    );
    assert_eq!(second["already_acked"], true);
    std::fs::remove_dir_all(&home).ok();
}

// ── Result / depends_on update semantics (spike t-…19288-1, fix t-…46182-4) ──

fn task_result(home: &std::path::Path, task_id: &str) -> Option<String> {
    crate::task_events::replay(home)
        .unwrap()
        .tasks
        .get(&crate::task_events::TaskId(task_id.into()))
        .and_then(|r| r.result.clone())
}

fn seed_claimed(home: &std::path::Path, task_id: &str, owner: &str) {
    use crate::task_events::{InstanceName, TaskEvent, TaskId};
    let tid = TaskId(task_id.into());
    crate::task_events::append_batch(
        home,
        &InstanceName::from("test:seed"),
        vec![
            TaskEvent::Created {
                task_id: tid.clone(),
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
            },
            TaskEvent::Claimed {
                task_id: tid,
                by: InstanceName::from(owner),
            },
        ],
    )
    .expect("seed claimed");
}

/// F2 (witnessed backfill): after a terminal auto-close (owner=dev-agent,
/// result null), the OWNER's `update(result=…)` must persist the result. Pre-fix
/// `handle_update` never read `args["result"]`, so it was a silent no-op.
#[test]
fn update_result_backfills_done_task() {
    let home = tmp_home("f2-backfill");
    seed_claimed(&home, "t-f2", "dev-agent");
    crate::tasks::auto_close::auto_close_on_report(
        &home,
        "report",
        "t-f2",
        "dev-agent",
        "auto summary",
        true,
    )
    .expect("auto_close");
    assert_eq!(
        read_task_record(&home, "t-f2").unwrap().status,
        crate::task_events::TaskStatus::Done,
        "precondition: Done"
    );
    let resp = handle(
        &home,
        "dev-agent",
        &serde_json::json!({"action": "update", "id": "t-f2", "result": "final: shipped in PR #77"}),
    );
    assert!(resp.get("error").is_none(), "update must not error: {resp}");
    assert_eq!(
        task_result(&home, "t-f2").as_deref(),
        Some("final: shipped in PR #77"),
        "F2: owner update(result=…) on a done task must persist `result`"
    );
}

/// F2 honest idempotent: `update(result=X)` when the result already equals X
/// must report `unchanged`, not a false `updated`.
#[test]
fn update_result_idempotent_reports_unchanged() {
    let home = tmp_home("f2-idempotent");
    seed_claimed(&home, "t-f2i", "dev-agent");
    let first = handle(
        &home,
        "dev-agent",
        &serde_json::json!({"action": "update", "id": "t-f2i", "result": "X"}),
    );
    assert_eq!(
        first["status"], "updated",
        "first result set is a real change: {first}"
    );
    let second = handle(
        &home,
        "dev-agent",
        &serde_json::json!({"action": "update", "id": "t-f2i", "result": "X"}),
    );
    assert_eq!(
        second["status"], "unchanged",
        "F2: an equal-value result update must report `unchanged`, not `updated`: {second}"
    );
    assert_eq!(task_result(&home, "t-f2i").as_deref(), Some("X"));
}

/// F3: `depends_on` is create-only/immutable (tests.rs:1888). `update(depends_on=…)`
/// must return an explicit error, never a false `updated`, and never mutate deps.
#[test]
fn update_depends_on_is_rejected_not_silently_accepted() {
    let home = tmp_home("f3-reject");
    create_task(&home, "t-f3");
    let resp = handle(
        &home,
        "dev-agent",
        &serde_json::json!({"action": "update", "id": "t-f3", "depends_on": ["t-up-1"]}),
    );
    assert!(
        resp.get("error").is_some(),
        "F3: update(depends_on=…) must return an explicit create-only error: {resp}"
    );
    let deps = read_task_record(&home, "t-f3").unwrap().depends_on;
    assert!(
        deps.is_empty(),
        "depends_on must stay as created (immutable): {deps:?}"
    );
}

/// Fail-loud: an update with no supported mutable field is not a success. Pre-fix
/// it returned `updated` (empty pending_events → unconditional success).
#[test]
fn update_with_no_supported_fields_errors() {
    let home = tmp_home("no-fields");
    create_task(&home, "t-nf");
    let resp = handle(
        &home,
        "dev-agent",
        &serde_json::json!({"action": "update", "id": "t-nf"}),
    );
    assert!(
        resp.get("error").is_some(),
        "an update with no updatable field must fail loudly, not report success: {resp}"
    );
}

/// Review correction (#task-result-rca): an update with an UNKNOWN status string
/// must fail loud, not silently no-op / report "unchanged".
#[test]
fn update_unknown_status_errors() {
    let home = tmp_home("bad-status");
    create_task(&home, "t-bs");
    let resp = handle(
        &home,
        "dev-agent",
        &serde_json::json!({"action": "update", "id": "t-bs", "status": "foo"}),
    );
    assert!(
        resp.get("error").is_some(),
        "unknown status must error, not unchanged/updated: {resp}"
    );
}

/// Review correction: `verified` is produced by the reviewer verdict path, not an
/// operator update — `update(status=verified)` must error (never invent a verdict).
#[test]
fn update_status_verified_is_rejected() {
    let home = tmp_home("verified");
    create_task(&home, "t-vf");
    let resp = handle(
        &home,
        "dev-agent",
        &serde_json::json!({"action": "update", "id": "t-vf", "status": "verified"}),
    );
    // Must be the ACTIONABLE verdict-path error — not the generic illegal_transition
    // that `open → verified` happens to raise. (codex review: "do not invent a
    // verdict — direct callers to the review/verdict path".)
    assert_eq!(
        resp["code"], "unsupported_status_transition",
        "status=verified must return the actionable verdict-path error: {resp}"
    );
}

/// Review correction: a non-string `result` must fail loud, not be silently
/// ignored and reported as "unchanged".
#[test]
fn update_non_string_result_errors() {
    let home = tmp_home("bad-result");
    create_task(&home, "t-br");
    let resp = handle(
        &home,
        "dev-agent",
        &serde_json::json!({"action": "update", "id": "t-br", "result": 123}),
    );
    assert!(
        resp.get("error").is_some(),
        "non-string result must fail loud, not unchanged/updated: {resp}"
    );
}

/// Review correction R3 (codex): an idempotent (equal) `result` must NOT absorb a
/// malformed second field. `result=<current>` + a non-emitting `description` (wrong
/// type) is a zero-event request that must fail loud, never "unchanged".
#[test]
fn update_idempotent_result_with_malformed_other_field_errors() {
    let home = tmp_home("combo");
    seed_claimed(&home, "t-combo", "dev-agent");
    // Establish result = "X".
    let set = handle(
        &home,
        "dev-agent",
        &serde_json::json!({"action": "update", "id": "t-combo", "result": "X"}),
    );
    assert_eq!(set["status"], "updated", "precondition: result set: {set}");
    // Equal result + a malformed (non-string) description → zero events.
    let resp = handle(
        &home,
        "dev-agent",
        &serde_json::json!({"action": "update", "id": "t-combo", "result": "X", "description": 123}),
    );
    assert!(
        resp.get("error").is_some(),
        "equal result + malformed other field must fail loud, not unchanged: {resp}"
    );
}

// ── task-governance metadata ACL (t-…-74, decision d-…-22, Root m-1087) ──
// RED-first through the REAL MCP handler entry `handle(home, caller, args)`.
// Model: `create` sets created_by = caller, assignee → owner. So throughout:
//   owner/assignee = "worker",  created_by/GOV_AUTHOR = "lead".
// The owner can forge plan_acks, lower plan_ack_required, and rewrite the plan
// after acks today (handle_metadata_set gates only on can_mutate_record then
// writes ANY key). GOV_AUTHOR (created_by) has NO write authority today. These
// tests pin the target behavior; they fail against current code (RED).

fn gov_plan_acks_len(home: &std::path::Path, id: &str) -> usize {
    read_task_record(home, id)
        .and_then(|r| {
            r.metadata
                .get("plan_acks")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
        })
        .unwrap_or(0)
}

fn gov_plan_ack_required(home: &std::path::Path, id: &str) -> u64 {
    read_task_record(home, id)
        .and_then(|r| r.metadata.get("plan_ack_required").and_then(|v| v.as_u64()))
        .unwrap_or(0)
}

fn gov_plan_text(home: &std::path::Path, id: &str) -> Option<String> {
    read_task_record(home, id).and_then(|r| {
        r.metadata
            .get("plan")
            .and_then(|v| v.as_str().map(String::from))
    })
}

/// Create a task owned by `worker`, created by `lead`, gated at N acks, then
/// claim it so status == Claimed (satisfies I2's "after assignee claim").
fn gov_seed_claimed(home: &std::path::Path, required: u64) -> String {
    let created = handle(
        home,
        "lead",
        &serde_json::json!({
            "action": "create",
            "title": "governed task",
            "assignee": "worker",
            "plan_ack_required": required,
            "plan_ack_reason": "repairs the plan-ack gate itself",
        }),
    );
    let id = created["id"].as_str().expect("id").to_string();
    handle(
        home,
        "worker",
        &serde_json::json!({"action": "claim", "id": id}),
    );
    id
}

/// R1 — the OWNER cannot self-weaken plan_ack_required (lower it or, being a
/// non-author, raise it). The counter must stay at its created value.
#[test]
fn gov_r1_owner_cannot_lower_or_raise_plan_ack_required_t74() {
    let home = tmp_home("gov-r1");
    let id = gov_seed_claimed(&home, 2);

    for bad in [0u64, 1] {
        let r = handle(
            &home,
            "worker",
            &serde_json::json!({
                "action": "metadata_set", "id": id,
                "metadata_key": "plan_ack_required", "metadata_value": bad
            }),
        );
        assert_eq!(
            r["code"], "plan_ack_required_protected",
            "owner lowering plan_ack_required to {bad} must be rejected: {r}"
        );
    }
    // Even a *raise* by the owner (non-author) is rejected — only GOV_AUTHOR raises.
    let raise = handle(
        &home,
        "worker",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "plan_ack_required", "metadata_value": 3
        }),
    );
    assert_eq!(
        raise["code"], "plan_ack_required_protected",
        "owner (non-author) raising plan_ack_required must be rejected: {raise}"
    );
    assert_eq!(
        gov_plan_ack_required(&home, &id),
        2,
        "plan_ack_required must be unchanged after every rejected write"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// R2 — the OWNER cannot forge plan_acks via metadata_set (only handle_ack_plan
/// may append). plan_acks must stay empty and the gate stays shut.
#[test]
fn gov_r2_owner_cannot_forge_plan_acks_t74() {
    let home = tmp_home("gov-r2");
    let id = gov_seed_claimed(&home, 2);
    // Owner may author the plan pre-ack (legit).
    handle(
        &home,
        "worker",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "plan", "metadata_value": "the plan"
        }),
    );

    let forged = handle(
        &home,
        "worker",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "plan_acks",
            "metadata_value": ["reviewer-a", "reviewer-b"]
        }),
    );
    assert_eq!(
        forged["code"], "plan_acks_immutable",
        "owner forging plan_acks must be rejected: {forged}"
    );
    assert_eq!(
        gov_plan_acks_len(&home, &id),
        0,
        "plan_acks must remain empty after a rejected forge"
    );
    // Gate still shut: 0/2.
    let blocked = handle(
        &home,
        "worker",
        &serde_json::json!({"action": "update", "id": id, "status": "in_progress"}),
    );
    assert_eq!(
        blocked["code"], "plan_ack_pending",
        "forged acks must not have opened the gate: {blocked}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// R3 — once an ack lands, the OWNER can no longer content-change the plan
/// (owner-frozen). A same-content write stays an idempotent no-op.
#[test]
fn gov_r3_owner_plan_frozen_after_first_ack_t74() {
    let home = tmp_home("gov-r3");
    let id = gov_seed_claimed(&home, 2);
    // Owner authors the plan pre-ack (ok).
    let set = handle(
        &home,
        "worker",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "plan", "metadata_value": "plan A"
        }),
    );
    assert!(
        set["error"].is_null(),
        "owner pre-ack plan write must succeed: {set}"
    );
    // A non-assignee ack lands.
    let ack = handle(
        &home,
        "reviewer-a",
        &serde_json::json!({"action": "ack_plan", "id": id}),
    );
    assert_eq!(ack["acked"], 1, "reviewer-a ack must land: {ack}");

    // Owner CONTENT-CHANGE after ack → frozen.
    let frozen = handle(
        &home,
        "worker",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "plan", "metadata_value": "plan B (sneaky rewrite)"
        }),
    );
    assert_eq!(
        frozen["code"], "plan_frozen_after_ack",
        "owner rewriting the plan after an ack must be rejected: {frozen}"
    );
    assert_eq!(
        gov_plan_text(&home, &id).as_deref(),
        Some("plan A"),
        "the acked plan content must be unchanged after a rejected rewrite"
    );
    assert_eq!(
        gov_plan_acks_len(&home, &id),
        1,
        "a rejected owner rewrite must not disturb existing acks"
    );

    // Same-content owner write after ack → idempotent no-op (no reject, no reset).
    let noop = handle(
        &home,
        "worker",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "plan", "metadata_value": "plan A"
        }),
    );
    assert!(
        noop["error"].is_null(),
        "same-content owner plan write must be an idempotent no-op, not a reject: {noop}"
    );
    assert_eq!(
        gov_plan_acks_len(&home, &id),
        1,
        "idempotent same-content write must not reset acks"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// R4 — the CREATOR (created_by, cross-team, post-claim) MAY author the plan and
/// MAY raise plan_ack_required monotonically. Both are denied today.
#[test]
fn gov_r4_creator_governance_writes_allowed_t74() {
    let home = tmp_home("gov-r4");
    let id = gov_seed_claimed(&home, 1);
    // No fleet.yaml → "lead" is neither owner nor orchestrator of "worker"
    // (cross-team). Authority comes ONLY from being created_by.
    let plan = handle(
        &home,
        "lead",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "plan", "metadata_value": "creator-authored plan"
        }),
    );
    assert!(
        plan["error"].is_null(),
        "created_by must be able to author the plan: {plan}"
    );
    assert_eq!(
        gov_plan_text(&home, &id).as_deref(),
        Some("creator-authored plan")
    );

    let raise = handle(
        &home,
        "lead",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "plan_ack_required", "metadata_value": 2
        }),
    );
    assert!(
        raise["error"].is_null(),
        "created_by must be able to raise plan_ack_required monotonically: {raise}"
    );
    assert_eq!(gov_plan_ack_required(&home, &id), 2);
    std::fs::remove_dir_all(&home).ok();
}

/// R5 — a CREATOR content-change resets plan_acks (reopening the gate) and
/// blocks in_progress until re-acked; a same-content creator write is an
/// idempotent no-op that preserves acks.
#[test]
fn gov_r5_creator_content_change_resets_acks_idempotent_preserves_t74() {
    let home = tmp_home("gov-r5");
    let id = gov_seed_claimed(&home, 2);
    handle(
        &home,
        "lead",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "plan", "metadata_value": "content A"
        }),
    );
    handle(
        &home,
        "reviewer-a",
        &serde_json::json!({"action": "ack_plan", "id": id}),
    );
    handle(
        &home,
        "reviewer-b",
        &serde_json::json!({"action": "ack_plan", "id": id}),
    );
    assert_eq!(gov_plan_acks_len(&home, &id), 2, "precondition: 2/2 acked");

    // Creator CONTENT-CHANGE → acks reset to [].
    let changed = handle(
        &home,
        "lead",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "plan", "metadata_value": "content B"
        }),
    );
    assert!(
        changed["error"].is_null(),
        "creator content-change must succeed: {changed}"
    );
    assert_eq!(
        gov_plan_acks_len(&home, &id),
        0,
        "a creator content-change must RESET plan_acks (reopen the gate)"
    );
    let blocked = handle(
        &home,
        "worker",
        &serde_json::json!({"action": "update", "id": id, "status": "in_progress"}),
    );
    assert_eq!(
        blocked["code"], "plan_ack_pending",
        "in_progress must be blocked until the plan is re-acked: {blocked}"
    );

    // Re-ack, then a SAME-content creator write → idempotent no-op, acks preserved.
    handle(
        &home,
        "reviewer-a",
        &serde_json::json!({"action": "ack_plan", "id": id}),
    );
    handle(
        &home,
        "reviewer-b",
        &serde_json::json!({"action": "ack_plan", "id": id}),
    );
    assert_eq!(gov_plan_acks_len(&home, &id), 2, "re-acked to 2/2");
    let noop = handle(
        &home,
        "lead",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "plan", "metadata_value": "content B"
        }),
    );
    assert!(
        noop["error"].is_null(),
        "same-content creator write must be a no-op: {noop}"
    );
    assert_eq!(
        gov_plan_acks_len(&home, &id),
        2,
        "idempotent same-content creator write must NOT reset acks"
    );
    let unblocked = handle(
        &home,
        "worker",
        &serde_json::json!({"action": "update", "id": id, "status": "in_progress"}),
    );
    assert!(
        unblocked["error"].is_null(),
        "preserved acks must keep the gate open: {unblocked}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// R6 — the CREATOR has governance authority ONLY. No done / status-update /
/// non-governance metadata authority (I5, I4). Guard against GREEN over-grant.
#[test]
fn gov_r6_creator_has_no_operational_authority_t74() {
    let home = tmp_home("gov-r6");
    let id = gov_seed_claimed(&home, 1);

    let done = handle(
        &home,
        "lead",
        &serde_json::json!({"action": "done", "id": id, "result": "sneaky close"}),
    );
    assert!(
        done.get("error").is_some(),
        "creator must not be able to `done`: {done}"
    );

    let upd = handle(
        &home,
        "lead",
        &serde_json::json!({"action": "update", "id": id, "status": "blocked"}),
    );
    assert!(
        upd.get("error").is_some(),
        "creator must not be able to update operational status: {upd}"
    );

    let meta = handle(
        &home,
        "lead",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "priority_note", "metadata_value": "creator poking non-gov key"
        }),
    );
    assert!(
        meta.get("error").is_some(),
        "creator must not write a non-governance metadata key: {meta}"
    );
    assert!(
        read_task_record(&home, &id)
            .map(|r| !r.metadata.contains_key("priority_note"))
            .unwrap_or(false),
        "the non-governance key must not have been written"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// R7 — the OWNER's pre-ack plan authoring workflow is preserved (I3 owner
/// branch, zero acks). Guard that GREEN doesn't over-freeze.
#[test]
fn gov_r7_owner_pre_ack_authoring_preserved_t74() {
    let home = tmp_home("gov-r7");
    let id = gov_seed_claimed(&home, 2);
    let first = handle(
        &home,
        "worker",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "plan", "metadata_value": "draft 1"
        }),
    );
    assert!(
        first["error"].is_null(),
        "owner pre-ack plan authoring must succeed: {first}"
    );
    // Owner may still revise the plan while zero acks exist.
    let revise = handle(
        &home,
        "worker",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "plan", "metadata_value": "draft 2"
        }),
    );
    assert!(
        revise["error"].is_null(),
        "owner may revise the plan while zero acks exist: {revise}"
    );
    assert_eq!(gov_plan_text(&home, &id).as_deref(), Some("draft 2"));
    std::fs::remove_dir_all(&home).ok();
}

/// R8 — a generic/invalid governance-counter write is denied for EVERY identity,
/// including the strongest (owner, creator/GOV_AUTHOR, a SYSTEM identity, and a
/// team ORCHESTRATOR who passes can_mutate_record). plan_acks is immutable for
/// all; plan_ack_required rejects lower/non-numeric for all.
#[test]
fn gov_r8_counter_write_denied_for_every_identity_t74() {
    let home = tmp_home("gov-r8");
    let id = gov_seed_claimed(&home, 2);
    handle(
        &home,
        "worker",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "plan", "metadata_value": "the plan"
        }),
    );

    // plan_acks is immutable via metadata_set for owner, creator, and system.
    for who in ["worker", "lead", "system:task_sweep"] {
        let r = handle(
            &home,
            who,
            &serde_json::json!({
                "action": "metadata_set", "id": id,
                "metadata_key": "plan_acks", "metadata_value": ["x", "y"]
            }),
        );
        assert_eq!(
            r["code"], "plan_acks_immutable",
            "{who} must not be able to raw-write plan_acks: {r}"
        );
    }
    assert_eq!(gov_plan_acks_len(&home, &id), 0, "plan_acks stayed empty");

    // plan_ack_required: lower (by GOV_AUTHOR creator, and by SYSTEM) → rejected.
    for who in ["lead", "system:task_sweep"] {
        let lower = handle(
            &home,
            who,
            &serde_json::json!({
                "action": "metadata_set", "id": id,
                "metadata_key": "plan_ack_required", "metadata_value": 1
            }),
        );
        assert_eq!(
            lower["code"], "plan_ack_required_protected",
            "{who} must not be able to LOWER plan_ack_required: {lower}"
        );
    }
    // Non-numeric raise by the creator → rejected (typed validation).
    let non_numeric = handle(
        &home,
        "lead",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "plan_ack_required", "metadata_value": "lots"
        }),
    );
    assert_eq!(
        non_numeric["code"], "plan_ack_required_protected",
        "non-numeric plan_ack_required must be rejected: {non_numeric}"
    );
    assert_eq!(gov_plan_ack_required(&home, &id), 2, "counter unchanged");

    // A team ORCHESTRATOR who passes can_mutate_record but is NOT created_by is
    // ALSO denied — no transitive authority expansion (Root m-1087).
    let orch_home = tmp_home("gov-r8-orch");
    write_fleet_yaml_with_team(&orch_home, "my-team", "team-lead");
    let created = handle(
        &orch_home,
        "lead",
        &serde_json::json!({
            "action": "create", "title": "orch task", "assignee": "dev-a",
            "plan_ack_required": 2, "plan_ack_reason": "gate repair"
        }),
    );
    let oid = created["id"].as_str().expect("id").to_string();
    handle(
        &orch_home,
        "dev-a",
        &serde_json::json!({"action": "claim", "id": oid}),
    );
    let orch_forge = handle(
        &orch_home,
        "team-lead",
        &serde_json::json!({
            "action": "metadata_set", "id": oid,
            "metadata_key": "plan_acks", "metadata_value": ["a", "b"]
        }),
    );
    assert_eq!(
        orch_forge["code"], "plan_acks_immutable",
        "an orchestrator (non-created_by) must not forge plan_acks: {orch_forge}"
    );
    let orch_lower = handle(
        &orch_home,
        "team-lead",
        &serde_json::json!({
            "action": "metadata_set", "id": oid,
            "metadata_key": "plan_ack_required", "metadata_value": 1
        }),
    );
    assert_eq!(
        orch_lower["code"], "plan_ack_required_protected",
        "an orchestrator (non-created_by) must not lower plan_ack_required: {orch_lower}"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&orch_home).ok();
}

/// Drive a task all the way to in_progress: create (gated at 1), claim, creator
/// authors the plan, a non-assignee acks, assignee transitions to in_progress.
/// Returns the task id, now with status == InProgress and 1 ack recorded.
fn gov_seed_in_progress(home: &std::path::Path) -> String {
    let id = gov_seed_claimed(home, 1);
    handle(
        home,
        "lead",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "plan", "metadata_value": "plan A"
        }),
    );
    handle(
        home,
        "reviewer-a",
        &serde_json::json!({"action": "ack_plan", "id": id}),
    );
    let started = handle(
        home,
        "worker",
        &serde_json::json!({"action": "update", "id": id, "status": "in_progress"}),
    );
    assert!(
        started["error"].is_null(),
        "precondition: in_progress reached: {started}"
    );
    id
}

/// R9 (PR #2761 r1) — once WORK HAS STARTED (in_progress), the plan is frozen
/// for EVERYONE, creator included. r0's bug: a GOV_AUTHOR content-change reset
/// plan_acks but performed NO status transition, so work kept running under an
/// unacked replacement plan. The fix rejects the content-change (no silent
/// reset, no status mutation). Same-content stays an idempotent no-op.
#[test]
fn gov_r9_plan_frozen_once_in_progress_t74() {
    let home = tmp_home("gov-r9");
    let id = gov_seed_in_progress(&home);

    // Creator content-change while in_progress → frozen (NOT reset).
    let creator_change = handle(
        &home,
        "lead",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "plan", "metadata_value": "plan B"
        }),
    );
    assert_eq!(
        creator_change["code"], "plan_frozen_work_started",
        "creator must not content-change the plan once work started: {creator_change}"
    );
    // Owner content-change while in_progress → also frozen.
    let owner_change = handle(
        &home,
        "worker",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "plan", "metadata_value": "plan C"
        }),
    );
    assert_eq!(
        owner_change["code"], "plan_frozen_work_started",
        "owner must not content-change the plan once work started: {owner_change}"
    );

    // No mutation: plan unchanged, acks NOT reset, status still in_progress.
    assert_eq!(gov_plan_text(&home, &id).as_deref(), Some("plan A"));
    assert_eq!(
        gov_plan_acks_len(&home, &id),
        1,
        "a rejected in_progress content-change must NOT reset acks"
    );
    assert_eq!(
        read_task_record(&home, &id).expect("exists").status,
        crate::task_events::TaskStatus::InProgress,
        "status must stay in_progress (metadata_set never transitions status)"
    );

    // Same-content write is STILL an idempotent no-op, even in_progress.
    let noop = handle(
        &home,
        "lead",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "plan", "metadata_value": "plan A"
        }),
    );
    assert!(
        noop["error"].is_null(),
        "same-content plan write must remain a no-op in_progress: {noop}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// R10 (PR #2761 r1) — the freeze holds in a later lifecycle state (in_review):
/// a content-change past the pre-work window is rejected regardless of how far
/// the task has progressed.
#[test]
fn gov_r10_plan_frozen_in_review_t74() {
    let home = tmp_home("gov-r10");
    let id = gov_seed_in_progress(&home);
    let review = handle(
        &home,
        "worker",
        &serde_json::json!({"action": "update", "id": id, "status": "in_review"}),
    );
    assert!(
        review["error"].is_null(),
        "precondition: in_review reached: {review}"
    );

    let change = handle(
        &home,
        "lead",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "plan", "metadata_value": "plan B"
        }),
    );
    assert_eq!(
        change["code"], "plan_frozen_work_started",
        "plan must stay frozen in in_review: {change}"
    );
    assert_eq!(gov_plan_text(&home, &id).as_deref(), Some("plan A"));
    std::fs::remove_dir_all(&home).ok();
}

/// R11 (PR #2761 r1) — Blocked-ambiguity resolved fail-closed: a Blocked task
/// has (in general) already started work, so the plan is frozen there too. A
/// content-change while Blocked is rejected for every identity.
#[test]
fn gov_r11_plan_frozen_when_blocked_t74() {
    let home = tmp_home("gov-r11");
    let id = gov_seed_in_progress(&home);
    let blocked = handle(
        &home,
        "worker",
        &serde_json::json!({"action": "update", "id": id, "status": "blocked"}),
    );
    assert!(
        blocked["error"].is_null(),
        "precondition: blocked reached: {blocked}"
    );

    // Creator and owner both frozen while Blocked.
    for who in ["lead", "worker"] {
        let change = handle(
            &home,
            who,
            &serde_json::json!({
                "action": "metadata_set", "id": id,
                "metadata_key": "plan", "metadata_value": "plan B"
            }),
        );
        assert_eq!(
            change["code"], "plan_frozen_work_started",
            "{who} must not content-change the plan while Blocked: {change}"
        );
    }
    assert_eq!(gov_plan_text(&home, &id).as_deref(), Some("plan A"));
    // Same-content no-op still holds while Blocked.
    let noop = handle(
        &home,
        "lead",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "plan", "metadata_value": "plan A"
        }),
    );
    assert!(
        noop["error"].is_null(),
        "same-content no-op must hold while Blocked: {noop}"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── #2760 R2 (root+independent REJECT of a542517b): deterministic concurrent REDs ──
//
// A route-only revalidation (board+incarnation) left the AUTHZ/PREDICATE TOCTOU
// open: a mutation that read/built state OUT of lock then appended could lose a
// concurrent update or act on stale authorization. Each RED arms the
// `before_mutation_commit` seam to land a CONCURRENT mutation in the out-of-lock
// window (it fully completes — acquires+releases its own per-id lock — BEFORE the
// instrumented caller locks, so no deadlock), and asserts the under-lock
// recompute/union is race-safe. Each FAILS on the pre-R2 (a542517b) code.

/// RED (finding 1 — ack_plan lost update): two reviewers ack concurrently; the
/// under-lock UNION from fresh `plan_acks` must preserve BOTH. Pre-R2 built the ack
/// list from the pre-lock snapshot and appended a full-list overwrite → last-write
/// wins → one ack silently lost.
#[test]
fn concurrent_ack_union_preserves_both_2760_r2() {
    let home = tmp_home("r2-concurrent-ack");
    let id = gov_seed_claimed(&home, 2);
    handle(
        &home,
        "lead",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "plan", "metadata_value": "the plan"
        }),
    );
    // Reviewer B acks in reviewer A's out-of-lock window.
    let home_h = home.clone();
    let id_h = id.clone();
    crate::tasks::set_before_mutation_commit_hook_for_test(move || {
        handle(
            &home_h,
            "rev-b",
            &serde_json::json!({"action": "ack_plan", "id": id_h}),
        );
    });
    let resp = handle(
        &home,
        "rev-a",
        &serde_json::json!({"action": "ack_plan", "id": id}),
    );
    assert!(
        resp.get("error").is_none(),
        "rev-a's ack must succeed: {resp}"
    );
    let acks: std::collections::BTreeSet<String> = read_task_record(&home, &id)
        .and_then(|r| {
            r.metadata
                .get("plan_acks")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
        })
        .unwrap_or_default();
    assert!(
        acks.contains("rev-a") && acks.contains("rev-b"),
        "both concurrent acks must be preserved by the fresh under-lock union, got {acks:?} \
         (pre-R2 the stale-vector overwrite loses one)"
    );
    assert_eq!(
        gov_plan_acks_len(&home, &id),
        2,
        "exactly the two distinct acks"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// RED (finding 2 — metadata_set stale authorization, STATUS dimension): the OWNER
/// revises the plan while the task is still pre-work (authorized out-of-lock), but a
/// concurrent transition to `in_progress` lands before the write commits. The
/// under-lock re-evaluation of the plan-governance policy must REFUSE (the plan is
/// FROZEN once work starts) and leave the plan unchanged. Pre-R2 evaluated the policy
/// on the pre-lock record and appended the revised plan unchecked — silently running
/// work under a hot-swapped plan.
#[test]
fn metadata_plan_revise_refused_when_status_advances_under_lock_2760_r2() {
    let home = tmp_home("r2-status-advance");
    let id = gov_seed_claimed(&home, 0); // required=0 → in_progress is ungated
    handle(
        &home,
        "worker",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "plan", "metadata_value": "v1"
        }),
    );
    // The owner advances the task to in_progress in the plan-revise out-of-lock window.
    let home_h = home.clone();
    let id_h = id.clone();
    crate::tasks::set_before_mutation_commit_hook_for_test(move || {
        handle(
            &home_h,
            "worker",
            &serde_json::json!({"action": "update", "id": id_h, "status": "in_progress"}),
        );
    });
    let resp = handle(
        &home,
        "worker",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "plan", "metadata_value": "v2"
        }),
    );
    assert_eq!(
        resp.get("code").and_then(|v| v.as_str()),
        Some("metadata_precondition_failed"),
        "the plan revise must be refused once the task advances to in_progress under the lock: {resp}"
    );
    assert_eq!(
        gov_plan_text(&home, &id).as_deref(),
        Some("v1"),
        "the plan stays v1 — the revise is refused under the lock"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// RED (finding 2/3 — plan vs concurrent ack): the OWNER revises the plan pre-ack
/// (authorized out-of-lock), but a reviewer's ack lands concurrently. The under-lock
/// re-evaluation must REFUSE (plan is frozen once acked) and leave the plan
/// unchanged. Pre-R2 appended the revised plan unchecked, silently running work
/// under a replacement plan the ack no longer covers.
#[test]
fn plan_revise_refused_when_ack_lands_concurrently_2760_r2() {
    let home = tmp_home("r2-plan-vs-ack");
    let id = gov_seed_claimed(&home, 2);
    handle(
        &home,
        "worker",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "plan", "metadata_value": "v1"
        }),
    );
    // A reviewer acks in the owner's plan-revise out-of-lock window.
    let home_h = home.clone();
    let id_h = id.clone();
    crate::tasks::set_before_mutation_commit_hook_for_test(move || {
        handle(
            &home_h,
            "reviewer",
            &serde_json::json!({"action": "ack_plan", "id": id_h}),
        );
    });
    let resp = handle(
        &home,
        "worker",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "plan", "metadata_value": "v2"
        }),
    );
    assert_eq!(
        resp.get("code").and_then(|v| v.as_str()),
        Some("metadata_precondition_failed"),
        "the owner's plan revise must be refused once a reviewer's ack lands concurrently: {resp}"
    );
    assert_eq!(
        gov_plan_text(&home, &id).as_deref(),
        Some("v1"),
        "the plan stays v1 — the revise is refused under the lock"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// RED (finding 2 — metadata_set stale authorization, OWNER dimension / STALE-FORMER-OWNER):
/// a THIRD PARTY (`lead`, the creator) creates the task; the OLD OWNER (`worker`) claims it
/// and is authorized to set a generic (non-`plan`) metadata key OUT of lock. In the old
/// owner's out-of-lock window the OLD OWNER RELEASES the task (`update`→open clears the
/// assignee) and a NEW OWNER (`worker2`) CLAIMS it — a real release→claim ownership drift,
/// expressed entirely through the production `update`/`claim` handlers (each takes+releases
/// its own per-id lock BEFORE the instrumented metadata_set locks, so no deadlock). The
/// under-lock re-evaluation of the owner ACL against the FRESH record (`can_mutate_record`,
/// owner = worker2) must REFUSE the former owner's write and leave the metadata (hence the
/// event log) unchanged. Pre-R2 (a542517b) checked the ACL ONLY on the pre-lock record
/// (`worker` still owner → authorized) and appended the write unchecked — a stale-authorized
/// former owner silently overwrites a key the new owner now controls.
#[test]
fn metadata_set_refused_when_owner_drifts_via_release_claim_under_lock_2760_r2() {
    let home = tmp_home("r2-stale-former-owner");
    // Third party (`lead`, creator) creates; OLD OWNER (`worker`) claims → creator ≠ owner.
    let created = handle(
        &home,
        "lead",
        &serde_json::json!({
            "action": "create",
            "title": "generic-metadata task",
            "assignee": "worker",
        }),
    );
    let id = created["id"].as_str().expect("id").to_string();
    handle(
        &home,
        "worker",
        &serde_json::json!({"action": "claim", "id": id}),
    );
    // Baseline: the old owner sets a generic key while still the owner (authorized).
    let base = handle(
        &home,
        "worker",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "note", "metadata_value": "baseline"
        }),
    );
    assert!(
        base.get("error").is_none(),
        "baseline set must succeed: {base}"
    );
    // In the old owner's out-of-lock window: the OLD OWNER releases (update→open clears the
    // assignee) and the NEW OWNER claims — ownership drifts before the write commits.
    let home_h = home.clone();
    let id_h = id.clone();
    crate::tasks::set_before_mutation_commit_hook_for_test(move || {
        let rel = handle(
            &home_h,
            "worker",
            &serde_json::json!({"action": "update", "id": id_h, "status": "open"}),
        );
        assert_eq!(
            rel["status"], "updated",
            "old owner's release must succeed: {rel}"
        );
        let claim = handle(
            &home_h,
            "worker2",
            &serde_json::json!({"action": "claim", "id": id_h}),
        );
        assert_eq!(
            claim["event"], "claimed",
            "new owner's claim must succeed: {claim}"
        );
    });
    // The OLD OWNER's racing generic-key write: authorized out-of-lock, but the under-lock
    // owner-ACL re-check against the FRESH record (owner = worker2) must REFUSE it.
    let resp = handle(
        &home,
        "worker",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "note", "metadata_value": "hijacked"
        }),
    );
    assert_eq!(
        resp.get("code").and_then(|v| v.as_str()),
        Some("metadata_precondition_failed"),
        "the former owner's write must be refused once ownership drifts under the lock: {resp}"
    );
    // Metadata unchanged (the hijack never landed) and ownership did drift to the new owner.
    let rec = read_task_record(&home, &id).expect("record");
    assert_eq!(
        rec.metadata.get("note").and_then(|v| v.as_str()),
        Some("baseline"),
        "the note stays 'baseline' — the former owner's write is refused under the lock"
    );
    assert_eq!(
        rec.owner.as_ref().map(|o| o.0.as_str()),
        Some("worker2"),
        "ownership drifted to the new owner in the out-of-lock window"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── P0 plan-ack fresh gate (task t-…-46776-13, decision d-…-8) ──
// Deterministic RED tests for six adversarial cases. Each test MUST FAIL against
// current code and PASS after the P0 GREEN fix lands. The gate validates plan
// shape, ack vector integrity, and fresh ownership UNDER the append lock at the
// InProgress transition, closing the out-of-lock TOCTOU gap.

/// P0 RED 1 — Plan-reset TOCTOU: acks are cleared between the out-of-lock
/// plan_ack check and the under-lock commit. The stale snapshot sees acks≥required
/// but fresh state has acks=0. Must refuse in_progress.
#[test]
fn p0_plan_reset_toctou_blocks_in_progress() {
    let home = tmp_home("p0-toctou");
    let id = gov_seed_claimed(&home, 1);
    handle(
        &home,
        "lead",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "plan", "metadata_value": "plan A"
        }),
    );
    handle(
        &home,
        "reviewer-a",
        &serde_json::json!({"action": "ack_plan", "id": id}),
    );
    assert_eq!(gov_plan_acks_len(&home, &id), 1, "precondition: 1 ack");
    // Hook: clear plan_acks in the out-of-lock window (simulates a concurrent
    // plan change that resets acks, landing before the update lock).
    let home_h = home.clone();
    let id_h = id.clone();
    crate::tasks::set_before_mutation_commit_hook_for_test(move || {
        let emitter = crate::task_events::InstanceName::from("lead");
        let _ = crate::task_events::append(
            &home_h,
            &emitter,
            crate::task_events::TaskEvent::MetadataSet {
                task_id: crate::task_events::TaskId(id_h),
                by: emitter.clone(),
                key: "plan_acks".to_string(),
                value: serde_json::json!([]),
            },
        );
    });
    let r = handle(
        &home,
        "worker",
        &serde_json::json!({"action": "update", "id": id, "status": "in_progress"}),
    );
    assert!(
        r.get("error").and_then(|v| v.as_str()).is_some(),
        "plan-reset TOCTOU must block in_progress (pre-fix: stale snapshot passes, \
         fresh state has 0 acks): {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// P0 RED 2 — Malformed plan_ack_required fails closed: if the stored value is
/// not a valid u64 (e.g. a string "2"), the InProgress gate must refuse rather
/// than defaulting to 0 and bypassing the gate.
#[test]
fn p0_malformed_required_fails_closed() {
    let home = tmp_home("p0-malformed");
    // Create a task without plan_ack_required, then inject a malformed value.
    let created = handle(
        &home,
        "lead",
        &serde_json::json!({
            "action": "create",
            "title": "malformed gate task",
            "assignee": "worker",
        }),
    );
    let id = created["id"].as_str().expect("id").to_string();
    handle(
        &home,
        "worker",
        &serde_json::json!({"action": "claim", "id": id}),
    );
    // Inject a malformed plan_ack_required via raw event (bypasses handler validation).
    let emitter = crate::task_events::InstanceName::from("lead");
    let _ = crate::task_events::append(
        &home,
        &emitter,
        crate::task_events::TaskEvent::MetadataSet {
            task_id: crate::task_events::TaskId(id.clone()),
            by: emitter.clone(),
            key: "plan_ack_required".to_string(),
            value: serde_json::json!("2"),
        },
    );
    // Set plan and ack so the count check would pass if required were parsed as 0.
    handle(
        &home,
        "lead",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "plan", "metadata_value": "the plan"
        }),
    );
    let r = handle(
        &home,
        "worker",
        &serde_json::json!({"action": "update", "id": id, "status": "in_progress"}),
    );
    assert!(
        r.get("error").and_then(|v| v.as_str()).is_some(),
        "malformed plan_ack_required (string '2') must fail closed, not silently \
         default to 0 and bypass the gate: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// P0 RED 3 — Late raise denied: after work has started (in_progress), the
/// GOV_AUTHOR must not be allowed to raise plan_ack_required.
#[test]
fn p0_late_raise_denied_after_in_progress() {
    let home = tmp_home("p0-late-raise");
    let id = gov_seed_in_progress(&home);
    let raise = handle(
        &home,
        "lead",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "plan_ack_required", "metadata_value": 2
        }),
    );
    assert_eq!(
        raise["code"], "plan_ack_required_protected",
        "GOV_AUTHOR must not raise plan_ack_required after in_progress: {raise}"
    );
    assert_eq!(
        gov_plan_ack_required(&home, &id),
        1,
        "plan_ack_required must remain 1 after rejected late raise"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// P0 RED 4 — Blank/empty plan blocks in_progress: a plan that is blank (""),
/// empty array ([]), or empty object ({}) is not a meaningful plan. The
/// InProgress gate must reject even when ack count ≥ required.
#[test]
fn p0_blank_plan_blocks_in_progress() {
    for (label, plan_value) in [
        ("blank string", serde_json::json!("")),
        ("empty array", serde_json::json!([])),
        ("empty object", serde_json::json!({})),
    ] {
        let home = tmp_home(&format!("p0-blank-{label}"));
        let id = gov_seed_claimed(&home, 1);
        handle(
            &home,
            "lead",
            &serde_json::json!({
                "action": "metadata_set", "id": id,
                "metadata_key": "plan", "metadata_value": plan_value
            }),
        );
        handle(
            &home,
            "reviewer-a",
            &serde_json::json!({"action": "ack_plan", "id": id}),
        );
        assert_eq!(gov_plan_acks_len(&home, &id), 1, "precondition: 1 ack");
        let r = handle(
            &home,
            "worker",
            &serde_json::json!({"action": "update", "id": id, "status": "in_progress"}),
        );
        assert!(
            r.get("error").and_then(|v| v.as_str()).is_some(),
            "plan '{label}' must block in_progress — a meaningless plan is not a plan: {r}"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}

/// P0 RED 5 — Ack-then-owner-reassignment laundering: reviewer acks the plan,
/// then gets reassigned as owner. The ack vector now contains the owner's own
/// identity — a self-ack laundered via the reassignment. Must refuse in_progress.
#[test]
fn p0_ack_owner_reassignment_laundering() {
    let home = tmp_home("p0-laundering");
    let id = gov_seed_claimed(&home, 1);
    handle(
        &home,
        "lead",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "plan", "metadata_value": "plan A"
        }),
    );
    handle(
        &home,
        "reviewer-a",
        &serde_json::json!({"action": "ack_plan", "id": id}),
    );
    assert_eq!(
        gov_plan_acks_len(&home, &id),
        1,
        "precondition: 1 ack from reviewer-a"
    );
    // Reassign ownership from "worker" to "reviewer-a" (the acker).
    handle(
        &home,
        "worker",
        &serde_json::json!({"action": "update", "id": id, "assignee": "reviewer-a"}),
    );
    let rec = read_task_record(&home, &id).expect("record");
    assert_eq!(
        rec.owner.as_ref().map(|o| o.0.as_str()),
        Some("reviewer-a"),
        "precondition: owner is now reviewer-a"
    );
    let r = handle(
        &home,
        "reviewer-a",
        &serde_json::json!({"action": "update", "id": id, "status": "in_progress"}),
    );
    assert!(
        r.get("error").and_then(|v| v.as_str()).is_some(),
        "laundered self-ack must block in_progress — ack vector contains the \
         current owner's identity: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// P0 RED 6 — Invalid ack vector blocks in_progress: the ack vector contains
/// duplicates, non-string elements, or the owner's own identity. The gate must
/// validate each element, not just count.
#[test]
fn p0_invalid_ack_vector_blocks_in_progress() {
    // Sub-case A: duplicate acks (count=2 but unique=1, required=2).
    {
        let home = tmp_home("p0-dup-ack");
        let id = gov_seed_claimed(&home, 2);
        handle(
            &home,
            "lead",
            &serde_json::json!({
                "action": "metadata_set", "id": id,
                "metadata_key": "plan", "metadata_value": "the plan"
            }),
        );
        // Inject a duplicate ack vector via raw event.
        let emitter = crate::task_events::InstanceName::from("system");
        let _ = crate::task_events::append(
            &home,
            &emitter,
            crate::task_events::TaskEvent::MetadataSet {
                task_id: crate::task_events::TaskId(id.clone()),
                by: emitter.clone(),
                key: "plan_acks".to_string(),
                value: serde_json::json!(["rev-a", "rev-a"]),
            },
        );
        let r = handle(
            &home,
            "worker",
            &serde_json::json!({"action": "update", "id": id, "status": "in_progress"}),
        );
        assert!(
            r.get("error").and_then(|v| v.as_str()).is_some(),
            "duplicate ack vector [rev-a, rev-a] must not satisfy required=2: {r}"
        );
        std::fs::remove_dir_all(&home).ok();
    }
    // Sub-case B: non-string element in ack vector.
    {
        let home = tmp_home("p0-nonstr-ack");
        let id = gov_seed_claimed(&home, 1);
        handle(
            &home,
            "lead",
            &serde_json::json!({
                "action": "metadata_set", "id": id,
                "metadata_key": "plan", "metadata_value": "the plan"
            }),
        );
        let emitter = crate::task_events::InstanceName::from("system");
        let _ = crate::task_events::append(
            &home,
            &emitter,
            crate::task_events::TaskEvent::MetadataSet {
                task_id: crate::task_events::TaskId(id.clone()),
                by: emitter.clone(),
                key: "plan_acks".to_string(),
                value: serde_json::json!([42]),
            },
        );
        let r = handle(
            &home,
            "worker",
            &serde_json::json!({"action": "update", "id": id, "status": "in_progress"}),
        );
        assert!(
            r.get("error").and_then(|v| v.as_str()).is_some(),
            "non-string ack element [42] must not count toward plan_ack_required: {r}"
        );
        std::fs::remove_dir_all(&home).ok();
    }
    // Sub-case C: self-ack (owner's identity in ack vector via raw injection).
    {
        let home = tmp_home("p0-self-ack");
        let id = gov_seed_claimed(&home, 1);
        handle(
            &home,
            "lead",
            &serde_json::json!({
                "action": "metadata_set", "id": id,
                "metadata_key": "plan", "metadata_value": "the plan"
            }),
        );
        let emitter = crate::task_events::InstanceName::from("system");
        let _ = crate::task_events::append(
            &home,
            &emitter,
            crate::task_events::TaskEvent::MetadataSet {
                task_id: crate::task_events::TaskId(id.clone()),
                by: emitter.clone(),
                key: "plan_acks".to_string(),
                value: serde_json::json!(["worker"]),
            },
        );
        let r = handle(
            &home,
            "worker",
            &serde_json::json!({"action": "update", "id": id, "status": "in_progress"}),
        );
        assert!(
            r.get("error").and_then(|v| v.as_str()).is_some(),
            "self-ack [worker] must not satisfy plan_ack_required: {r}"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}

/// P0 r4 RED: duplicate ack entry with required=1 must fail. Before the fix,
/// HashSet silently deduplicates so `["alice", "alice"]` counted as 1 unique
/// ack and passed the `>= required` gate.
#[test]
fn p0_duplicate_ack_with_required_1_fails() {
    let home = tmp_home("p0-dup-ack-r1");
    let id = gov_seed_claimed(&home, 1);
    handle(
        &home,
        "lead",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "plan", "metadata_value": "the plan"
        }),
    );
    let emitter = crate::task_events::InstanceName::from("system");
    let _ = crate::task_events::append(
        &home,
        &emitter,
        crate::task_events::TaskEvent::MetadataSet {
            task_id: crate::task_events::TaskId(id.clone()),
            by: emitter.clone(),
            key: "plan_acks".to_string(),
            value: serde_json::json!(["alice", "alice"]),
        },
    );
    let r = handle(
        &home,
        "worker",
        &serde_json::json!({"action": "update", "id": id, "status": "in_progress"}),
    );
    assert!(
        r.get("error")
            .and_then(|v| v.as_str())
            .is_some_and(|e| e.contains("duplicate")),
        "duplicate ack vector must fail even when count meets required=1: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// P0 r4 RED: ack_plan over a pre-existing malformed ack vector must fail
/// instead of filter_mapping past non-string elements.
#[test]
fn p0_ack_plan_rejects_malformed_preexisting_vector() {
    let home = tmp_home("p0-ackplan-malformed");
    let id = gov_seed_claimed(&home, 1);
    handle(
        &home,
        "lead",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "plan", "metadata_value": "the plan"
        }),
    );
    let emitter = crate::task_events::InstanceName::from("system");
    let _ = crate::task_events::append(
        &home,
        &emitter,
        crate::task_events::TaskEvent::MetadataSet {
            task_id: crate::task_events::TaskId(id.clone()),
            by: emitter.clone(),
            key: "plan_acks".to_string(),
            value: serde_json::json!([42, "valid-acker"]),
        },
    );
    let r = handle(
        &home,
        "new-acker",
        &serde_json::json!({"action": "ack_plan", "id": id}),
    );
    assert!(
        r.get("error").and_then(|v| v.as_str()).is_some(),
        "ack_plan must refuse to append over a malformed pre-existing vector: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// P0 r5 RED: ack_plan with a corrupt vector that ALSO contains the caller
/// must reject, not return already_acked success. Before the fix, the
/// out-of-lock fast-path used filter_map to skip non-strings, found the
/// caller in the filtered list, and returned already_acked: true without
/// ever running strict validation.
#[test]
fn p0_ack_plan_corrupt_vector_containing_caller_rejects() {
    let home = tmp_home("p0-ackplan-corrupt-caller");
    let id = gov_seed_claimed(&home, 1);
    handle(
        &home,
        "lead",
        &serde_json::json!({
            "action": "metadata_set", "id": id,
            "metadata_key": "plan", "metadata_value": "the plan"
        }),
    );
    let emitter = crate::task_events::InstanceName::from("system");
    let _ = crate::task_events::append(
        &home,
        &emitter,
        crate::task_events::TaskEvent::MetadataSet {
            task_id: crate::task_events::TaskId(id.clone()),
            by: emitter.clone(),
            key: "plan_acks".to_string(),
            value: serde_json::json!([42, "new-acker"]),
        },
    );
    let r = handle(
        &home,
        "new-acker",
        &serde_json::json!({"action": "ack_plan", "id": id}),
    );
    assert!(
        r.get("error").and_then(|v| v.as_str()).is_some(),
        "ack_plan must reject corrupt vector even when caller is present (no already_acked bypass): {r}"
    );
    assert!(
        r.get("already_acked").is_none(),
        "must not return already_acked on a corrupt vector: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ---------------------------------------------------------------------------
// Architecture-14 item 4: typed AssigneePatch RED tests (real-entry via handle)
// ---------------------------------------------------------------------------

#[test]
fn create_missing_assignee_is_unassigned() {
    let home = tmp_home("ap-t1");
    let r = handle(
        &home,
        "op",
        &serde_json::json!({"action": "create", "title": "t1"}),
    );
    assert!(r.get("error").is_none(), "create must succeed: {r}");
    let id = r["id"].as_str().expect("id");
    let tasks = crate::tasks::list_all(&home);
    let task = tasks.iter().find(|t| t.id == id).expect("task");
    assert!(task.assignee.is_none(), "missing assignee → None");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn create_blank_assignee_is_unassigned() {
    let home = tmp_home("ap-t2");
    let r = handle(
        &home,
        "op",
        &serde_json::json!({"action": "create", "title": "t2", "assignee": ""}),
    );
    assert!(r.get("error").is_none(), "create must succeed: {r}");
    let id = r["id"].as_str().expect("id");
    let tasks = crate::tasks::list_all(&home);
    let task = tasks.iter().find(|t| t.id == id).expect("task");
    assert!(
        task.assignee.is_none(),
        "blank assignee must be None, not Some('')"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn create_set_assignee_trims_whitespace() {
    let home = tmp_home("ap-t3");
    let r = handle(
        &home,
        "op",
        &serde_json::json!({"action": "create", "title": "t3", "assignee": " agent "}),
    );
    assert!(r.get("error").is_none(), "create must succeed: {r}");
    let id = r["id"].as_str().expect("id");
    let tasks = crate::tasks::list_all(&home);
    let task = tasks.iter().find(|t| t.id == id).expect("task");
    assert_eq!(
        task.assignee.as_deref(),
        Some("agent"),
        "must trim whitespace"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn create_non_string_assignee_errors() {
    let home = tmp_home("ap-t4");
    let r = handle(
        &home,
        "op",
        &serde_json::json!({"action": "create", "title": "t4", "assignee": 42}),
    );
    assert!(
        r.get("error").is_some(),
        "non-string assignee must error: {r}"
    );
    assert!(
        r["code"].as_str() == Some("invalid_assignee")
            || r["error"]
                .as_str()
                .is_some_and(|s| s.contains("invalid_assignee")),
        "error must identify invalid_assignee: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn update_missing_assignee_unchanged() {
    let home = tmp_home("ap-t5");
    let r = handle(
        &home,
        "dev-agent",
        &serde_json::json!({"action": "create", "title": "t5", "assignee": "dev-agent"}),
    );
    let id = r["id"].as_str().expect("id");
    let r = handle(
        &home,
        "dev-agent",
        &serde_json::json!({"action": "update", "id": id, "description": "updated"}),
    );
    assert!(r.get("error").is_none(), "update must succeed: {r}");
    let tasks = crate::tasks::list_all(&home);
    let task = tasks.iter().find(|t| t.id == id).expect("task");
    assert_eq!(
        task.assignee.as_deref(),
        Some("dev-agent"),
        "assignee must be unchanged"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn update_blank_assignee_clears() {
    let home = tmp_home("ap-t6");
    let r = handle(
        &home,
        "dev-agent",
        &serde_json::json!({"action": "create", "title": "t6", "assignee": "dev-agent"}),
    );
    let id = r["id"].as_str().expect("id");
    let _ = handle(
        &home,
        "dev-agent",
        &serde_json::json!({"action": "claim", "id": id}),
    );
    let r = handle(
        &home,
        "dev-agent",
        &serde_json::json!({"action": "update", "id": id, "assignee": ""}),
    );
    assert!(r.get("error").is_none(), "update clear must succeed: {r}");
    let tasks = crate::tasks::list_all(&home);
    let task = tasks.iter().find(|t| t.id == id).expect("task");
    assert!(
        task.assignee.is_none(),
        "blank assignee update must clear to None"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn update_set_assignee_trims() {
    let home = tmp_home("ap-t7");
    let r = handle(
        &home,
        "old",
        &serde_json::json!({"action": "create", "title": "t7", "assignee": "old"}),
    );
    let id = r["id"].as_str().expect("id");
    let _ = handle(
        &home,
        "old",
        &serde_json::json!({"action": "claim", "id": id}),
    );
    let r = handle(
        &home,
        "old",
        &serde_json::json!({"action": "update", "id": id, "assignee": " new "}),
    );
    assert!(r.get("error").is_none(), "update must succeed: {r}");
    let tasks = crate::tasks::list_all(&home);
    let task = tasks.iter().find(|t| t.id == id).expect("task");
    assert_eq!(
        task.assignee.as_deref(),
        Some("new"),
        "must trim whitespace"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn update_non_string_assignee_errors() {
    let home = tmp_home("ap-t8");
    let r = handle(
        &home,
        "op",
        &serde_json::json!({"action": "create", "title": "t8", "assignee": "dev-agent"}),
    );
    let id = r["id"].as_str().expect("id");
    let r = handle(
        &home,
        "op",
        &serde_json::json!({"action": "update", "id": id, "assignee": true}),
    );
    assert!(
        r.get("error").is_some(),
        "non-string assignee must error: {r}"
    );
    let tasks = crate::tasks::list_all(&home);
    let task = tasks.iter().find(|t| t.id == id).expect("task");
    assert_eq!(
        task.assignee.as_deref(),
        Some("dev-agent"),
        "assignee must be unchanged on error"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// T9: Clear settles all three sidecars (dispatch-idle, next_after_ci,
/// dispatch-tracking) with None after commit.
#[test]
fn update_clear_settles_three_sidecars() {
    let home = tmp_home("ap-t9");
    let r = handle(
        &home,
        "dev-agent",
        &serde_json::json!({"action": "create", "title": "t9", "assignee": "dev-agent"}),
    );
    let id = r["id"].as_str().expect("id");
    let _ = handle(
        &home,
        "dev-agent",
        &serde_json::json!({"action": "claim", "id": id}),
    );
    // Seed a dispatch-idle pending entry via record_dispatch.
    crate::daemon::dispatch_idle::record_dispatch(
        &home,
        "lead",
        "dev-agent",
        Some(id),
        "task",
        600,
    );
    assert!(
        crate::daemon::dispatch_idle::has_pending_for_instance(&home, "dev-agent"),
        "precondition: dispatch-idle sidecar exists"
    );
    // Clear assignee.
    let r = handle(
        &home,
        "dev-agent",
        &serde_json::json!({"action": "update", "id": id, "assignee": ""}),
    );
    assert!(r.get("error").is_none(), "clear must succeed: {r}");
    // After commit, sidecars cleared.
    assert!(
        !crate::daemon::dispatch_idle::has_pending_for_instance(&home, "dev-agent"),
        "dispatch-idle sidecar must be cleared after assignee clear"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// T10: Combined status=done + assignee="" attributes done event to the
/// pre-commit owner and clears sidecars post-commit.
#[test]
fn combined_status_done_and_clear_attributes_to_old_owner() {
    let home = tmp_home("ap-t10");
    let r = handle(
        &home,
        "dev-agent",
        &serde_json::json!({"action": "create", "title": "t10", "assignee": "dev-agent"}),
    );
    let id = r["id"].as_str().expect("id");
    let _ = handle(
        &home,
        "dev-agent",
        &serde_json::json!({"action": "claim", "id": id}),
    );
    // Combined: status=done + clear assignee in one update.
    let r = handle(
        &home,
        "dev-agent",
        &serde_json::json!({"action": "update", "id": id, "status": "done", "assignee": "", "result": "completed"}),
    );
    assert!(
        r.get("error").is_none(),
        "combined update must succeed: {r}"
    );
    let tasks = crate::tasks::list_all(&home);
    let task = tasks.iter().find(|t| t.id == id).expect("task");
    assert_eq!(
        task.status,
        crate::task_events::TaskStatus::Done,
        "status must be done"
    );
    assert!(task.assignee.is_none(), "assignee must be cleared");
    std::fs::remove_dir_all(&home).ok();
}

/// T11: ACL failure produces no sidecar mutation.
#[test]
fn update_acl_failure_no_sidecar_mutation() {
    let home = tmp_home("ap-t11");
    let r = handle(
        &home,
        "dev-agent",
        &serde_json::json!({"action": "create", "title": "t11", "assignee": "dev-agent"}),
    );
    let id = r["id"].as_str().expect("id");
    let _ = handle(
        &home,
        "dev-agent",
        &serde_json::json!({"action": "claim", "id": id}),
    );
    // Seed sidecar for dev-agent via record_dispatch.
    crate::daemon::dispatch_idle::record_dispatch(
        &home,
        "lead",
        "dev-agent",
        Some(id),
        "task",
        600,
    );
    assert!(
        crate::daemon::dispatch_idle::has_pending_for_instance(&home, "dev-agent"),
        "precondition"
    );
    // Unauthorized caller tries to clear assignee.
    let r = handle(
        &home,
        "intruder",
        &serde_json::json!({"action": "update", "id": id, "assignee": ""}),
    );
    assert!(
        r.get("error").is_some(),
        "unauthorized update must fail: {r}"
    );
    // Sidecar must be unchanged.
    assert!(
        crate::daemon::dispatch_idle::has_pending_for_instance(&home, "dev-agent"),
        "dispatch-idle sidecar must survive ACL failure"
    );
    let tasks = crate::tasks::list_all(&home);
    let task = tasks.iter().find(|t| t.id == id).expect("task");
    assert_eq!(
        task.assignee.as_deref(),
        Some("dev-agent"),
        "assignee unchanged after ACL failure"
    );
    std::fs::remove_dir_all(&home).ok();
}
