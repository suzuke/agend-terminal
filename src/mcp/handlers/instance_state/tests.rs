use super::*;
use std::collections::HashMap;

#[test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
fn delete_instance_denies_non_owner_non_orchestrator_audit2_002() {
    let home = std::env::temp_dir().join(format!(
        "agend-2002-del-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&home).unwrap();

    // A peer that is neither the target nor its orchestrator is denied — the
    // ACL fires before any teardown / existence check.
    let attacker = crate::identity::Sender::new("attacker");
    let denied =
        handle_delete_instance(&home, &serde_json::json!({"instance": "victim"}), &attacker);
    assert_eq!(
        denied["code"], "not_owner_or_orchestrator",
        "non-owner delete must be denied: {denied}"
    );

    std::fs::remove_dir_all(&home).ok();
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
fn tmp_home_for_creator_acl(tag: &str) -> std::path::PathBuf {
    let home = std::env::temp_dir().join(format!(
        "agend-creator-acl-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&home).unwrap();
    home
}

/// Seed a `claimed` task assigned to `assignee` directly in the event log
/// — the in-flight condition the creator-ACL safety valve checks for.
#[allow(clippy::unwrap_used, clippy::expect_used)]
fn seed_claimed_task(home: &std::path::Path, assignee: &str) {
    use crate::task_events::{InstanceName, TaskEvent, TaskId};
    let tid = TaskId("t-creator-acl-1".into());
    let emitter = InstanceName::from("test:creator_acl");
    crate::task_events::append_batch(
        home,
        &emitter,
        vec![
            TaskEvent::Created {
                task_id: tid.clone(),
                title: "in-flight work".into(),
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
                by: InstanceName::from(assignee),
            },
        ],
    )
    .expect("seed claimed task");
}

/// #ACL-creator: a creator with NO in-flight work on its target may delete
/// it — the pain point being fixed is "creator had to build a team just to
/// gain orchestrator authority" for a clean, no-longer-needed spawn.
#[test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
fn delete_instance_allows_creator_with_no_inflight_work() {
    let home = tmp_home_for_creator_acl("clean");
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  victim:\n    created_by: creator\n",
    )
    .unwrap();

    let creator = crate::identity::Sender::new("creator");
    let resp = handle_delete_instance(&home, &serde_json::json!({"instance": "victim"}), &creator);
    assert_ne!(
        resp.get("code").and_then(|v| v.as_str()),
        Some("not_owner_or_orchestrator"),
        "creator must not be denied as a non-owner: {resp}"
    );
    assert_eq!(resp["name"], "victim");

    std::fs::remove_dir_all(&home).ok();
}

/// #ACL-creator safety valve: a claimed/in_progress task on the target
/// blocks the creator path unless `force=true` + a non-empty
/// `force_reason` is supplied — in-flight work must not be casually reaped.
#[test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
fn delete_instance_creator_requires_force_when_target_has_active_task() {
    let home = tmp_home_for_creator_acl("active-task");
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  victim:\n    created_by: creator\n",
    )
    .unwrap();
    seed_claimed_task(&home, "victim");

    let creator = crate::identity::Sender::new("creator");
    let denied =
        handle_delete_instance(&home, &serde_json::json!({"instance": "victim"}), &creator);
    assert_eq!(
        denied["code"], "creator_delete_requires_force",
        "creator delete of an in-flight target without force must be denied: {denied}"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// #ACL-creator safety valve: `force=true` + `force_reason` lets the
/// creator override the in-flight guard (audit-logged at the call site).
#[test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
fn delete_instance_creator_force_succeeds_when_target_has_active_task() {
    let home = tmp_home_for_creator_acl("force");
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  victim:\n    created_by: creator\n",
    )
    .unwrap();
    seed_claimed_task(&home, "victim");

    let creator = crate::identity::Sender::new("creator");
    let resp = handle_delete_instance(
        &home,
        &serde_json::json!({
            "instance": "victim",
            "force": true,
            "force_reason": "retiring my own spawn to change its model",
        }),
        &creator,
    );
    assert_ne!(
        resp.get("code").and_then(|v| v.as_str()),
        Some("creator_delete_requires_force"),
        "force + force_reason must override the in-flight guard: {resp}"
    );
    assert_eq!(resp["name"], "victim");

    std::fs::remove_dir_all(&home).ok();
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
fn dirty_worktree(tag: &str) -> std::path::PathBuf {
    let wt = std::env::temp_dir().join(format!(
        "agend-2476-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&wt).unwrap();
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&wt)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .expect("git");
    };
    git(&["init", "-b", "main"]);
    // Untracked file → `git status --porcelain` non-empty → work-at-risk.
    std::fs::write(wt.join("wip.txt"), "uncommitted groundwork").unwrap();
    wt
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
fn bind_worktree(home: &std::path::Path, agent: &str, wt: &std::path::Path) {
    let dir = crate::paths::runtime_dir(home).join(agent);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("binding.json"),
        serde_json::json!({"worktree": wt.display().to_string()}).to_string(),
    )
    .unwrap();
}

/// #2476: a `fresh` restart must refuse when the bound worktree has
/// uncommitted changes (context drop would strand them), unless `force`.
/// `resume` keeps context so it is never guarded.
#[test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
fn fresh_restart_guards_uncommitted_worktree_2476() {
    let home = std::env::temp_dir().join(format!(
        "agend-2476-home-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&home).unwrap();
    let wt = dirty_worktree("wt");
    bind_worktree(&home, "dev", &wt);

    // fresh + dirty + no force → refused at the guard (before any spawn).
    let refused = handle_restart_instance(
        &home,
        &serde_json::json!({"instance": "dev", "mode": "fresh"}),
    );
    assert_eq!(
        refused["code"], "uncommitted_work_at_risk",
        "got: {refused}"
    );

    // force bypasses the guard (proceeds past it — a later error is NOT the guard).
    let forced = handle_restart_instance(
        &home,
        &serde_json::json!({"instance": "dev", "mode": "fresh", "force": true}),
    );
    assert_ne!(
        forced["code"], "uncommitted_work_at_risk",
        "force must bypass: {forced}"
    );

    // resume keeps context → never guarded.
    let resumed = handle_restart_instance(
        &home,
        &serde_json::json!({"instance": "dev", "mode": "resume"}),
    );
    assert_ne!(
        resumed["code"], "uncommitted_work_at_risk",
        "resume must not guard: {resumed}"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&wt).ok();
}

// #1625: every restart, regardless of mode, must carry the same-tab layout
// hint so the respawned pane returns to its original tab (the fresh path
// previously omitted it and fell out into a new tab).
#[test]
fn restart_spawn_params_carries_same_tab_fresh() {
    let env = HashMap::new();
    let p = restart_spawn_params("dev", "claude", &[], None, &env, "fresh");
    assert_eq!(p["layout"], "same-tab");
    // fresh must NOT request a resume.
    assert!(p.get("mode").is_none());
    // fresh restart arms the daemon self-kick (the independent flag).
    assert_eq!(p["self_kick_on_ready"].as_bool(), Some(true));
}

#[test]
fn restart_spawn_params_carries_same_tab_resume() {
    let env = HashMap::new();
    let p = restart_spawn_params("dev", "claude", &[], None, &env, "resume");
    assert_eq!(p["layout"], "same-tab");
    assert_eq!(p["mode"], "resume");
    // resume preserves context → must NOT self-kick.
    assert!(p.get("self_kick_on_ready").is_none());
}

/// must-follow ②: the self-kick flag is INDEPENDENT — set ONLY by the
/// fresh-restart path, NEVER derived from `SpawnMode::Fresh` (initial fleet
/// spawns / create_instance / team-spawn are Fresh too but never set it). The
/// SPAWN handler reads `self_kick_on_ready` with `unwrap_or(false)`, so any
/// spawn-params shape that lacks the flag (every non-restart-fresh spawn)
/// gates the self-kick OUT, fail-safe.
#[test]
fn self_kick_flag_set_only_by_fresh_restart_fail_safe_default() {
    let env = HashMap::new();
    // fresh restart → flag present + true.
    let fresh = restart_spawn_params("dev", "claude", &[], None, &env, "fresh");
    assert!(fresh["self_kick_on_ready"].as_bool().unwrap_or(false));
    // resume restart → no flag → reads false.
    let resume = restart_spawn_params("dev", "claude", &[], None, &env, "resume");
    assert!(!resume["self_kick_on_ready"].as_bool().unwrap_or(false));
    // a generic spawn-params object (the initial-fleet / create_instance shape,
    // which also maps to SpawnMode::Fresh) carries no flag → reads false.
    let initial = json!({"name": "dev", "backend": "claude", "layout": "tab"});
    assert!(!initial["self_kick_on_ready"].as_bool().unwrap_or(false));
}
