#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Sprint 54 P1-B Bug 1 fix: residual-store audit + transactional-or-loud
//! `full_delete_instance`. Tests cover the audit fn's per-store
//! detection (clean / each-store-positive / multi-source) and the
//! delete fn's Result-return contract (Err on residual,
//! Ok on clean). `full_delete_instance` reaches into the daemon's
//! `api::call` which fails harmlessly with no daemon — we exercise
//! the post-cleanup audit branch by pre-seeding residual state
//! directly, mirroring the silent-drop class production scenario.

use super::name_residual_anywhere;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

fn tmp_home(tag: &str) -> PathBuf {
    static C: AtomicU32 = AtomicU32::new(0);
    let id = C.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-p1b-bug1-test-{}-{}-{}",
        std::process::id(),
        tag,
        id,
    ));
    std::fs::create_dir_all(&dir).ok();
    dir
}

#[test]
fn name_residual_anywhere_clean_home_returns_empty() {
    let home = tmp_home("clean");
    assert!(name_residual_anywhere(&home, "ghost", None).is_empty());
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn name_residual_anywhere_detects_fleet_yaml_instance_residual() {
    let home = tmp_home("fleet_yaml_inst");
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  zombie:\n    backend: claude\n",
    )
    .unwrap();
    let sources = name_residual_anywhere(&home, "zombie", None);
    assert!(
        sources.contains(&"fleet.yaml"),
        "fleet.yaml instances residual must surface, got {sources:?}"
    );
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn name_residual_anywhere_detects_fleet_yaml_team_member_residual() {
    // Sprint 54 PR #507 unification: teams live in fleet.yaml; a
    // delete that misses team membership cleanup leaves the name
    // resolvable as a team member, which the audit must surface
    // separately from the instances: stanza.
    let home = tmp_home("fleet_yaml_team");
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "teams:\n  ops:\n    members: [zombie, alice]\n    orchestrator: alice\n",
    )
    .unwrap();
    let sources = name_residual_anywhere(&home, "zombie", None);
    assert!(
        sources.contains(&"fleet.yaml/teams"),
        "team-member residual must surface, got {sources:?}"
    );
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn name_residual_anywhere_detects_metadata_residual() {
    let home = tmp_home("metadata");
    let meta_dir = home.join("metadata");
    std::fs::create_dir_all(&meta_dir).unwrap();
    std::fs::write(meta_dir.join("zombie.json"), "{}").unwrap();
    let sources = name_residual_anywhere(&home, "zombie", None);
    assert!(sources.contains(&"metadata"), "got {sources:?}");
    std::fs::remove_dir_all(home).ok();
}

/// #1682 (defect-1, codex): the metadata residual lives at the id-resolved
/// `<uuid>.json`, and `full_delete_instance` removes fleet.yaml BEFORE the
/// audit — so a fleet-reloading resolver can no longer map name→id and the
/// audit goes blind to the stale `<uuid>.json`. The captured id must be passed
/// so the audit checks the id path directly. This is the exact post-deletion
/// shape: fleet.yaml ABSENT + `<uuid>.json` present → audit must still report
/// "metadata". (Fails if the id-direct check is dropped — the name path alone
/// never sees `<uuid>.json`.)
#[test]
fn name_residual_anywhere_detects_uuid_metadata_after_fleet_yaml_removed_1682() {
    let home = tmp_home("uuid_residual_postdelete");
    std::fs::remove_dir_all(&home).ok();
    std::fs::create_dir_all(&home).unwrap();
    let id = crate::types::InstanceId::new();
    // The resolved metadata file exists ONLY as `<uuid>.json` (what every
    // post-#1680 reader/writer uses) — and NO `<name>.json`.
    let meta_dir = home.join("metadata");
    std::fs::create_dir_all(&meta_dir).unwrap();
    std::fs::write(meta_dir.join(format!("{}.json", id.full())), "{}").unwrap();
    // fleet.yaml is ABSENT (already removed, as in full_delete_instance) — so a
    // name→id resolver cannot find the uuid; only the captured id can.
    assert!(
        !crate::fleet::fleet_yaml_path(&home).exists(),
        "precondition: fleet.yaml must be absent (post-delete state)"
    );
    let sources = name_residual_anywhere(&home, "zombie", Some(&id.full()));
    assert!(
        sources.contains(&"metadata"),
        "#1682 defect-1: id-resolved metadata residual must surface even with \
             fleet.yaml gone, got {sources:?}"
    );
    std::fs::remove_dir_all(home).ok();
}

/// #1923 G12 (verify-only): `full_delete_instance` removes the escalation-persist
/// store, so a same-name redeploy does NOT rehydrate the deleted instance's
/// escalation state. The daemon rehydrates from this store on (re)spawn
/// (`rehydrate_escalation`); without the delete-time remove a fresh instance
/// reusing the name would inherit the dead one's crash budget / paged latch.
/// Confirms the existing `escalation_persist::remove` in `full_delete_instance`
/// mitigates the gap — no prod change, regression guard.
#[test]
fn full_delete_removes_escalation_store_no_stale_rehydrate_1923_g12() {
    let home = tmp_home("g12_escalation");
    std::fs::create_dir_all(&home).expect("mkdir home");
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  victim:\n    command: /bin/cat\n",
    )
    .expect("seed fleet.yaml");
    crate::daemon::escalation_persist::persist(
        &home,
        "victim",
        &crate::health::PersistedEscalation::default(),
    );
    assert!(
        crate::daemon::escalation_persist::load_for(&home, "victim").is_some(),
        "precondition: escalation store seeded for victim"
    );

    let _ = super::full_delete_instance(&home, "victim");

    assert!(
        crate::daemon::escalation_persist::load_for(&home, "victim").is_none(),
        "#1923 G12: full_delete_instance must remove the escalation store — else a \
         same-name redeploy rehydrates the deleted instance's escalation state"
    );
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn name_residual_anywhere_detects_inbox_residual() {
    let home = tmp_home("inbox");
    let inbox_dir = home.join("inbox");
    std::fs::create_dir_all(&inbox_dir).unwrap();
    std::fs::write(inbox_dir.join("zombie.jsonl"), "").unwrap();
    let sources = name_residual_anywhere(&home, "zombie", None);
    assert!(sources.contains(&"inbox"), "got {sources:?}");
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn name_residual_anywhere_detects_uuid_inbox_residual_1902() {
    // #1902: the current inbox is UUID-based, but the audit only checked the
    // name path → a `<uuid>.jsonl` inbox was a SILENT leak (past the audit).
    // Write a REAL uuid inbox file (via the id-direct path, NOT a synthetic
    // name file) and pass the id — the audit must now flag "inbox".
    let home = tmp_home("uuid_inbox_audit");
    let id = crate::types::InstanceId::new();
    let inbox_dir = home.join("inbox");
    std::fs::create_dir_all(&inbox_dir).unwrap();
    std::fs::write(crate::inbox::storage::inbox_path_for_id(&home, &id), "").unwrap();
    // No name-based file exists — pre-#1902 this returned clean (false信心).
    let sources = name_residual_anywhere(&home, "zombie", Some(&id.full()));
    assert!(
        sources.contains(&"inbox"),
        "uuid inbox residual must be detected, got {sources:?}"
    );
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn full_delete_instance_removes_uuid_inbox_1902() {
    // #1902 §3.9 (a): a single delete must remove the instance's REAL uuid
    // inbox (driven through inbox::enqueue → inbox_path_resolved, the
    // production path), not just a name-based file.
    let home = tmp_home("uuid_inbox_delete");
    let id = crate::types::InstanceId::new();
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        format!(
            "instances:\n  doomed:\n    backend: claude\n    id: {}\n",
            id.full()
        ),
    )
    .unwrap();
    // Enqueue via the resolver → lands at inbox/<uuid>.jsonl (the prod path).
    let msg = crate::inbox::InboxMessage::new_system("system:test", "update", "hi".to_string());
    crate::inbox::enqueue(&home, "doomed", msg).expect("enqueue");
    let uuid_inbox = crate::inbox::storage::inbox_path_for_id(&home, &id);
    assert!(
        uuid_inbox.exists(),
        "pre: uuid inbox must exist at {uuid_inbox:?}"
    );
    let result = super::full_delete_instance(&home, "doomed");
    assert!(result.is_ok(), "delete must return Ok, got {result:?}");
    assert!(
        !uuid_inbox.exists(),
        "uuid inbox leaked — full_delete_instance must delete it: {uuid_inbox:?}"
    );
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn name_residual_anywhere_detects_usage_limit_notify_residual_1906() {
    // #1906 Leak 2: the usage-limit notify-dedup store was a teardown audit
    // blind spot — the audit must now flag a leftover name entry.
    let home = tmp_home("usage_audit");
    std::fs::write(
        home.join("usage_limit_notify.json"),
        r#"{"zombie":{"unlock_at":null,"notified_at":"2026-01-01T00:00:00+00:00"}}"#,
    )
    .unwrap();
    let sources = name_residual_anywhere(&home, "zombie", None);
    assert!(
        sources.contains(&"usage_limit_notify"),
        "usage_limit_notify residual must be detected, got {sources:?}"
    );
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn name_residual_anywhere_detects_worktree_residual_1906() {
    // #1906 Leak 1: a leftover physical worktree (`worktrees/<name>/`) must be
    // flagged — the audit previously only saw runtime/<name>/binding.json.
    let home = tmp_home("wt_audit");
    std::fs::create_dir_all(
        crate::worktree_pool::daemon_managed_worktree_root(&home)
            .join("zombie")
            .join("feat-x"),
    )
    .unwrap();
    let sources = name_residual_anywhere(&home, "zombie", None);
    assert!(
        sources.contains(&"worktree"),
        "physical worktree residual must be detected, got {sources:?}"
    );
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn full_delete_instance_removes_usage_limit_notify_1906() {
    // #1906 Leak 2 §3.9: a single delete must drop the instance's
    // usage-limit notify-dedup entry so a same-name redeploy doesn't inherit
    // stale suppression.
    let home = tmp_home("usage_delete");
    std::fs::write(
        home.join("usage_limit_notify.json"),
        r#"{"doomed":{"unlock_at":null,"notified_at":"2026-01-01T00:00:00+00:00"}}"#,
    )
    .unwrap();
    assert!(
        crate::daemon::supervisor::usage_limit_notify_has(&home, "doomed"),
        "pre: usage_limit_notify entry must exist"
    );
    let result = super::full_delete_instance(&home, "doomed");
    assert!(result.is_ok(), "delete must return Ok, got {result:?}");
    assert!(
        !crate::daemon::supervisor::usage_limit_notify_has(&home, "doomed"),
        "usage_limit_notify entry leaked — full_delete must drop it"
    );
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn full_delete_instance_releases_worktree_1906() {
    // #1906 Leak 1 §3.9: a single delete must release the PHYSICAL worktree
    // (not just the binding). Seed a daemon-managed worktree + a binding
    // pointing at it (raw binding.json, mirroring binding.rs tests); a
    // same-name redeploy must not collide with an orphan worktree. ISOLATED
    // home — never touches ~/.agend-terminal.
    let home = tmp_home("wt_release");
    let wt = crate::worktree_pool::daemon_managed_worktree_root(&home)
        .join("doomed")
        .join("feat-y");
    std::fs::create_dir_all(&wt).unwrap();
    std::fs::write(wt.join(".agend-managed"), "").unwrap();
    let rt = crate::paths::runtime_dir(&home).join("doomed");
    std::fs::create_dir_all(&rt).unwrap();
    std::fs::write(
        rt.join("binding.json"),
        serde_json::json!({"worktree": wt.to_str().unwrap(), "branch": "feat-y"}).to_string(),
    )
    .unwrap();
    assert!(wt.exists(), "pre: managed worktree must exist");

    let result = super::full_delete_instance(&home, "doomed");
    assert!(result.is_ok(), "delete must return Ok, got {result:?}");
    assert!(
        !wt.exists(),
        "physical worktree leaked — full_delete must release it (release_full): {wt:?}"
    );
    // The agent dir under worktrees/ is gone too → audit clean.
    assert!(
        !crate::worktree_pool::daemon_managed_worktree_root(&home)
            .join("doomed")
            .exists(),
        "worktrees/doomed/ dir must be gone post-release"
    );
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn name_residual_anywhere_detects_runtime_binding_residual() {
    let home = tmp_home("binding");
    let dir = crate::paths::runtime_dir(&home).join("zombie");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("binding.json"), "{}").unwrap();
    let sources = name_residual_anywhere(&home, "zombie", None);
    assert!(sources.contains(&"runtime/binding.json"), "got {sources:?}");
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn name_residual_anywhere_detects_notification_queue_residual() {
    let home = tmp_home("nq");
    let dir = home.join("notification-queue");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("zombie.jsonl"), "").unwrap();
    let sources = name_residual_anywhere(&home, "zombie", None);
    assert!(sources.contains(&"notification-queue"), "got {sources:?}");
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn name_residual_anywhere_returns_multi_source_when_several_stores_dirty() {
    // Regression-proof: dropping the per-store check must surface
    // as a missing entry in this list, not as a silent skip.
    let home = tmp_home("multi");
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  zombie:\n    backend: claude\n",
    )
    .unwrap();
    std::fs::create_dir_all(home.join("metadata")).unwrap();
    std::fs::write(home.join("metadata").join("zombie.json"), "{}").unwrap();
    std::fs::create_dir_all(home.join("inbox")).unwrap();
    std::fs::write(home.join("inbox").join("zombie.jsonl"), "").unwrap();
    let sources = name_residual_anywhere(&home, "zombie", None);
    for expected in ["fleet.yaml", "metadata", "inbox"] {
        assert!(
            sources.contains(&expected),
            "multi-source audit must include {expected}, got {sources:?}"
        );
    }
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn full_delete_instance_returns_err_when_residual_remains_post_cleanup() {
    // Pre-seed a notification-queue file before delete. The daemon API is
    // unreachable in the test process, so `api::call(DELETE)` — the step that
    // clears the queue via the registry — fails silently, and the disk-cleanup
    // path in `full_delete_instance` does NOT touch notification-queue. So it
    // genuinely survives, and the post-cleanup audit must surface it as residual
    // → Err (the transactional-or-loud contract).
    //
    // #1907: the prior `metadata/zombie.json` seed is now correctly cleaned by
    // `cleanup_working_dir`'s "always clean metadata" tail — which now runs even
    // for entries with no explicit `working_directory` (previously skipped,
    // leaking the default-workspace metadata). It can no longer prove the Err
    // path; notification-queue is the residual that still does.
    let home = tmp_home("full_residual");
    std::fs::create_dir_all(home.join("notification-queue")).unwrap();
    std::fs::write(home.join("notification-queue").join("zombie.jsonl"), "").unwrap();
    let result = super::full_delete_instance(&home, "zombie");
    let err = result.expect_err(
        "notification-queue residual after cleanup must surface as Err — silent-drop class blocked",
    );
    assert!(
        err.contains("notification-queue"),
        "Err detail must name the residual store, got: {err:?}"
    );
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn full_delete_instance_returns_ok_when_no_residual() {
    // Clean home: no fleet.yaml, no metadata, no inbox — every
    // cleanup step is a no-op AND the audit reports clean.
    // `api::call` failure during DELETE is harmless because there's
    // nothing to clean and the audit returns empty.
    let home = tmp_home("full_clean");
    let result = super::full_delete_instance(&home, "ghost");
    assert!(
        result.is_ok(),
        "clean home + clean post-audit must return Ok, got: {result:?}"
    );
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn full_delete_instance_orphans_owned_tasks() {
    // #808 GREEN test 2: deleting an instance must orphan the
    // tasks it owns so the ACL gate (`tasks::can_mutate_record`)
    // doesn't lock survivors out. Pre-fix: tasks keep ghost
    // owner → operator gets "not authorized" on cancel.
    // Post-fix: orphan_tasks_for_owner clears assignee before
    // the residual audit so the survivor can mutate.
    let home = tmp_home("orphan_on_delete");
    // Create a task owned by the doomed instance via the public
    // handle entry — this exercises the same event-log flow the
    // MCP `task` tool uses in production.
    let r = crate::tasks::handle(
        &home,
        "doomed",
        &serde_json::json!({"action": "create", "title": "owned task", "assignee": "doomed"}),
    );
    let task_id = r["id"].as_str().expect("task id").to_string();
    // Sanity: pre-delete ownership recorded.
    let pre_tasks = crate::tasks::list_all(&home);
    let pre = pre_tasks
        .iter()
        .find(|t| t.id == task_id)
        .expect("task exists");
    assert_eq!(
        pre.assignee.as_deref(),
        Some("doomed"),
        "pre-delete sanity: task owner must be 'doomed'"
    );
    // Run the full teardown. `api::call` is unreachable in test
    // context (harmless) and there's no fleet.yaml / metadata so
    // the residual audit returns clean.
    let result = super::full_delete_instance(&home, "doomed");
    assert!(
        result.is_ok(),
        "delete on clean home must return Ok, got: {result:?}"
    );
    // Orphan side-effect: assignee cleared.
    let post_tasks = crate::tasks::list_all(&home);
    let post = post_tasks
        .iter()
        .find(|t| t.id == task_id)
        .expect("task still exists post-delete");
    assert!(
        post.assignee.is_none(),
        "owned task must be orphaned after full_delete_instance, got assignee={:?}",
        post.assignee
    );
    // #1903 §3.9 (c): a SINGLE delete must leave the task ORPHAN-OPEN (owner
    // cleared, still Open for re-dispatch) — NOT Cancelled. Cancellation is
    // reserved for the team-disband path; single delete preserves the
    // ACL-unlock-but-survivable semantics.
    assert_eq!(
        post.status,
        crate::task_events::TaskStatus::Open,
        "single delete must keep the task Open (orphan), not cancel it"
    );
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn full_delete_instance_removes_activity_sidecar() {
    let home = tmp_home("activity_cleanup");
    crate::daemon::idle_watchdog::touch_agent_activity(&home, "doomed");
    let activity_file = home.join("agent-activity").join("doomed.json");
    assert!(activity_file.exists(), "pre-delete sanity: sidecar exists");
    let _ = super::full_delete_instance(&home, "doomed");
    assert!(
        !activity_file.exists(),
        "activity sidecar must be removed after full_delete_instance"
    );
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn full_delete_instance_cascades_bound_services() {
    // #1488: deleting an instance must cascade-clean its schedules,
    // dispatch_tracking entries, and CI watches in one teardown.
    let home = tmp_home("cascade");
    // schedule targeting the doomed instance
    std::fs::write(
        home.join("schedules.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "schema_version": 2,
            "schedules": [{"id": "s-d", "message": "m", "target": "doomed",
                "trigger": {"kind": "cron", "expr": "0 9 * * *"}, "enabled": true,
                "timezone": "UTC", "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:00:00Z", "run_history": []}]
        }))
        .unwrap(),
    )
    .unwrap();
    // dispatch entry to the doomed instance
    crate::dispatch_tracking::track_dispatch(
        &home,
        crate::dispatch_tracking::DispatchEntry {
            task_id: Some("t-d".into()),
            from: "lead".into(),
            to: "doomed".into(),
            from_id: None,
            to_id: None,
            delegated_at: chrono::Utc::now().to_rfc3339(),
            status: "pending".into(),
        },
    );
    // ci_watch with the doomed instance as sole subscriber
    let ciw = crate::daemon::ci_watch::ci_watches_dir(&home);
    std::fs::create_dir_all(&ciw).unwrap();
    std::fs::write(
        ciw.join("w.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "repo": "o/r", "branch": "feat",
            "subscribers": [{"instance": "doomed"}],
        }))
        .unwrap(),
    )
    .unwrap();

    let result = super::full_delete_instance(&home, "doomed");
    assert!(
        result.is_ok(),
        "clean cascade must return Ok, got {result:?}"
    );

    // schedule disabled + marked orphaned (NOT deleted)
    let sched = &crate::schedules::load(&home).schedules[0];
    assert!(!sched.enabled, "schedule must be disabled by cascade");
    assert!(sched
        .run_history
        .last()
        .is_some_and(|r| r.status.contains("orphaned")));
    // dispatch entry GC'd
    let store: serde_json::Value =
        crate::store::load(&crate::store::store_path(&home, "dispatch_tracking.json"));
    assert!(
        store["entries"].as_array().unwrap().is_empty(),
        "dispatch entry to deleted instance must be GC'd"
    );
    // ci watch removed (sole subscriber gone)
    assert!(
        !ciw.join("w.json").exists(),
        "ci watch with only the deleted subscriber must be removed"
    );
    std::fs::remove_dir_all(home).ok();
}

/// §3.9 #1879 (BIND-1): deleting a BOUND agent (one that ran bind_self / repo
/// checkout) must clear its binding and succeed — pre-fix the binding was the
/// one store `full_delete_instance` never cleaned, so the residual audit
/// flagged `runtime/<name>/binding.json` and the teardown returned Err while
/// also leaking the binding. Regression-proof: revert the `binding::unbind`
/// call and the residual audit fails the delete.
#[test]
fn full_delete_clears_binding_and_succeeds_1879() {
    let home = tmp_home("1879-bind-delete");
    // Simulate a bound agent.
    crate::binding::bind_full(
        &home,
        "agent-b",
        "",
        "feat/x",
        std::path::Path::new("/tmp/wt-agent-b"),
        std::path::Path::new("/tmp/repo-agent-b"),
    )
    .expect("bind_full");
    assert!(
        crate::binding::read(&home, "agent-b").is_some(),
        "pre: binding exists"
    );

    let result = super::full_delete_instance(&home, "agent-b");

    assert!(
        result.is_ok(),
        "#1879 BIND-1: teardown of a bound agent must succeed (no residual Err), got: {result:?}"
    );
    assert!(
        crate::binding::read(&home, "agent-b").is_none(),
        "#1879 BIND-1: the binding must be cleared"
    );
    assert!(
        !home
            .join("runtime")
            .join("agent-b")
            .join("binding.json")
            .exists(),
        "#1879 BIND-1: binding.json must be gone"
    );

    std::fs::remove_dir_all(&home).ok();
}
