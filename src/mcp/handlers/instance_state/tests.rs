use super::*;
use std::collections::HashMap;

#[allow(clippy::unwrap_used, clippy::expect_used)]
fn tmp_home_for_create_instance_team(tag: &str) -> std::path::PathBuf {
    let home = std::env::temp_dir().join(format!(
        "agend-create-instance-team-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&home).unwrap();
    home
}

/// #2454 residual RED: pure generated-member team mode must route a live MCP
/// RuntimeContext directly to the merged typed CREATE_TEAM owner. The missing
/// owner wire-up currently tries the API socket and therefore reports its
/// transport failure before the deliberately invalid backend can be observed.
#[test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
fn create_instance_team_runtime_some_reaches_typed_owner_without_api_2454() {
    let _guard = crate::mcp::handlers::fleet_test_guard();
    let home = tmp_home_for_create_instance_team("runtime-some");
    let runtime = crate::mcp::handlers::minimal_test_runtime();
    let result = handle_create_instance(
        &home,
        &serde_json::json!({
            "team": "runtime-team",
            "backends": ["/definitely-missing-agent-binary-2454"],
            "description": "runtime-owner-red",
            "topic_binding": "skip"
        }),
        "caller",
        Some(&runtime),
    );
    let error = result["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("all 1 spawns failed"),
        "runtime=Some pure team mode must reach team_ops::create without an API listener; got {result}"
    );
    assert!(
        !error.contains("API unavailable"),
        "runtime=Some must not use the legacy API loopback: {result}"
    );
    let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home))
        .expect("typed owner should persist its planned member before spawn");
    assert!(
        fleet.instances.contains_key("runtime-team-1"),
        "typed owner must preserve generated-member planning before the spawn failure: {fleet:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #2454 parity RED: the legacy CREATE_TEAM adapter normalizes `auto` and
/// other unsupported topic-binding values to `None`, preserving the default
/// topic-creation behavior. The direct RuntimeContext path must keep that
/// same boundary contract before handing the typed request to team_ops.
#[test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
fn create_instance_team_runtime_some_normalizes_topic_binding_like_api_2454() {
    let _guard = crate::mcp::handlers::fleet_test_guard();
    let runtime = crate::mcp::handlers::minimal_test_runtime();
    for mode in ["auto", "invalid"] {
        let home = tmp_home_for_create_instance_team("topic-binding-parity");
        let result = handle_create_instance(
            &home,
            &serde_json::json!({
                "team": "parity-team",
                "backends": ["/definitely-missing-agent-binary-2454"],
                "topic_binding": mode
            }),
            "caller",
            Some(&runtime),
        );
        assert!(
            result["error"]
                .as_str()
                .is_some_and(|error| error.contains("all 1 spawns failed")),
            "typed owner should still be reached for {mode}: {result}"
        );
        let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home))
            .expect("typed owner should persist its planned member");
        let entry = fleet
            .instances
            .get("parity-team-1")
            .expect("typed owner should persist the generated member");
        assert_eq!(
            entry.topic_binding_mode, None,
            "unsupported topic_binding={mode:?} must retain API auto-default semantics"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}

/// #2454 residual contract: a standalone bridge has no RuntimeContext and
/// keeps the isolated legacy API transport. With no listener, it fails closed
/// and must not create a team as a side effect.
#[test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
fn create_instance_team_runtime_none_keeps_legacy_bridge_2454() {
    let _guard = crate::mcp::handlers::fleet_test_guard();
    let home = tmp_home_for_create_instance_team("runtime-none");
    let result = handle_create_instance(
        &home,
        &serde_json::json!({
            "team": "legacy-team",
            "backends": ["/definitely-missing-agent-binary-2454"]
        }),
        "caller",
        None,
    );
    let error = result["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("API unavailable") || error.contains("daemon") || error.contains("run dir"),
        "runtime=None must retain the isolated legacy API transport failure: {result}"
    );
    let fleet_content =
        std::fs::read_to_string(crate::fleet::fleet_yaml_path(&home)).unwrap_or_default();
    assert!(
        !fleet_content.contains("legacy-team"),
        "legacy bridge failure must not create a team as a side effect: {fleet_content}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #2454 residual RED seam: the executable RED test above omits `task` so it
/// never sleeps. This structural pin keeps the existing delayed task capture
/// intact until GREEN rewires only the CREATE_TEAM leaf.
#[test]
fn create_instance_team_delayed_injection_captures_runtime_and_spawned_2454() {
    let source = include_str!("mod.rs");
    let marker = source
        .find("\"team_task_inject\"")
        .expect("team task injection thread marker");
    let region_start = marker.saturating_sub(1_000);
    let region_end = (marker + 1_200).min(source.len());
    let region = &source[region_start..region_end];
    for needle in [
        "let names = spawned.clone()",
        "runtime.map",
        "Arc::clone(&rt.registry)",
        "Arc::clone(&rt.externals)",
        "inject_with_routing",
        "Duration::from_secs(3)",
    ] {
        assert!(
            region.contains(needle),
            "team task injection must preserve `{needle}` capture/routing seam"
        );
    }
}

/// #2855 gate 4: `full_delete_instance_with_runtime` must reject an invalid
/// (traversal) name with canonical `validate_name` semantics BEFORE the
/// LifecyclePermit / deleting-mark / any filesystem or store mutation. The
/// adjacent sentinel derived from `workspace_dir(home)/join(name)` must stay
/// intact.
#[test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
fn full_delete_rejects_invalid_name_before_permit_or_mutation_2855() {
    let _guard = crate::mcp::handlers::fleet_test_guard();
    let home = tmp_home_for_create_instance_team("full-delete-invalid-2855");
    let sentinel_dir = crate::paths::workspace_dir(&home).join("../g4v-2855");
    std::fs::create_dir_all(&sentinel_dir).unwrap();
    let marker = sentinel_dir.join("marker.txt");
    std::fs::write(&marker, "sentinel").unwrap();

    let result = crate::mcp::handlers::instance_state::lifecycle::full_delete_instance_with_runtime(
        &home,
        "../g4v-2855",
        None,
    );

    assert!(
        result
            .as_ref()
            .err()
            .is_some_and(|e| e.contains("invalid characters")),
        "#2855: traversal name must be rejected with validate_name semantics \
         before any permit/mutation; got {result:?}"
    );
    assert!(
        marker.exists(),
        "#2855: rejected delete must not touch the adjacent sentinel"
    );
    std::fs::remove_dir_all(&home).ok();
}

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

/// #ACL-creator audit (review finding F1): a creator force-delete override
/// must write a durable `fleet_events.jsonl` entry — `tracing::warn!` alone
/// is process-log-only and not queryable audit trail. Mirrors the
/// `merge_force_bypass` pattern in `ci/merge.rs`.
#[test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
fn delete_instance_creator_force_writes_fleet_events_audit() {
    let home = tmp_home_for_creator_acl("audit");
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
    assert_eq!(resp["name"], "victim", "force-delete must succeed: {resp}");

    let events_path = home.join("fleet_events.jsonl");
    let content = std::fs::read_to_string(&events_path)
        .unwrap_or_else(|e| panic!("fleet_events.jsonl must exist and be readable: {e}"));
    let event: serde_json::Value = content
        .lines()
        .find_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .expect("fleet_events.jsonl must contain a parseable JSON line");
    assert_eq!(event["kind"], "creator_force_delete");
    assert_eq!(event["agent"], "creator");
    assert_eq!(event["target"], "victim");
    assert_eq!(
        event["force_reason"],
        "retiring my own spawn to change its model"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// #ACL-creator audit fail-closed (review finding F1): if the durable audit
/// write fails, the delete must be REFUSED — a permission override that
/// can't be recorded must not proceed, same fail-closed semantics as
/// `merge_force_bypass`. Simulated by making `home` itself unwritable for
/// the new file (a directory in place of `fleet_events.jsonl`).
#[test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
fn delete_instance_creator_force_fails_closed_when_audit_write_fails() {
    let home = tmp_home_for_creator_acl("audit-fail");
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  victim:\n    created_by: creator\n",
    )
    .unwrap();
    seed_claimed_task(&home, "victim");
    // A directory at the audit log's path makes the OpenOptions::append open
    // fail (EISDIR), simulating a write failure without touching permissions.
    std::fs::create_dir_all(home.join("fleet_events.jsonl")).unwrap();

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
    assert!(
        resp.get("error").is_some(),
        "audit write failure must refuse the delete: {resp}"
    );
    assert_ne!(
        resp["name"], "victim",
        "a fail-closed refusal must not report the same success shape: {resp}"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// #ACL-creator safety valve (review finding F3): an active worktree binding
/// (no task involved) must ALSO gate the creator path — production checks
/// `has_binding || has_active_task`, but prior tests only exercised the
/// claimed-task half.
#[test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
fn delete_instance_creator_requires_force_when_target_has_active_binding() {
    let home = tmp_home_for_creator_acl("active-binding");
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  victim:\n    created_by: creator\n",
    )
    .unwrap();
    let wt = dirty_worktree("acl-binding");
    bind_worktree(&home, "victim", &wt);

    let creator = crate::identity::Sender::new("creator");
    let denied =
        handle_delete_instance(&home, &serde_json::json!({"instance": "victim"}), &creator);
    assert_eq!(
        denied["code"], "creator_delete_requires_force",
        "creator delete of a bound target without force must be denied: {denied}"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&wt).ok();
}

/// #ACL-creator safety valve (review finding F3): `force` + `force_reason`
/// also overrides the active-binding half of the valve.
#[test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
fn delete_instance_creator_force_succeeds_when_target_has_active_binding() {
    let home = tmp_home_for_creator_acl("active-binding-force");
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  victim:\n    created_by: creator\n",
    )
    .unwrap();
    let wt = dirty_worktree("acl-binding-force");
    bind_worktree(&home, "victim", &wt);

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
        "force + force_reason must override the active-binding guard: {resp}"
    );
    assert_eq!(resp["name"], "victim");

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&wt).ok();
}

/// #ACL-creator restart-persistence (review finding F2): `created_by` must
/// survive a daemon-restart-equivalent reload — written through the REAL
/// spawn-time path (`add_instance_to_yaml`, not a hand-typed YAML fixture),
/// then re-resolved via a fresh `FleetConfig::load` (no in-memory state
/// carried over), and the creator ACL must still recognize it. Directly
/// motivated by the same-night team-persistence bug investigation
/// (t-20260702132159428591-56872-1) — a field that LOOKS persisted in a
/// single process can still fail to round-trip.
#[test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
fn delete_instance_creator_acl_survives_restart_equivalent_reload() {
    let home = tmp_home_for_creator_acl("restart-persist");
    std::fs::write(crate::fleet::fleet_yaml_path(&home), "instances: {}\n").unwrap();

    // The REAL write path a spawn takes (spawn.rs stamps this exact field).
    crate::fleet::add_instance_to_yaml(
        &home,
        "victim",
        &crate::fleet::InstanceYamlEntry {
            backend: Some("claude".into()),
            created_by: Some("creator".into()),
            ..Default::default()
        },
    )
    .expect("write instance entry");

    // "Restart-equivalent": a FRESH load off disk, no cached/in-memory state
    // from the write above (FleetConfig::load has no such cache to begin
    // with, but this asserts the round-trip explicitly rather than assuming
    // it).
    let reloaded = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home))
        .expect("reload fleet.yaml");
    assert_eq!(
        reloaded
            .instances
            .get("victim")
            .and_then(|i| i.created_by.clone()),
        Some("creator".to_string()),
        "created_by must round-trip through the real write + a fresh reload"
    );

    // The ACL path itself (handle_delete_instance) does its own fresh
    // `FleetConfig::load` internally — this proves the end-to-end behavior,
    // not just the struct round-trip above.
    let creator = crate::identity::Sender::new("creator");
    let resp = handle_delete_instance(&home, &serde_json::json!({"instance": "victim"}), &creator);
    assert_ne!(
        resp.get("code").and_then(|v| v.as_str()),
        Some("not_owner_or_orchestrator"),
        "creator ACL must resolve from persisted state after reload: {resp}"
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

// ── #991 handle_bind_topic (MCP JSON-envelope mapping) ──────────────────
// `bind_topic_for_instance`'s own outcomes are pinned in
// `channel::telegram::topic_registry`'s tests; these tests cover the layer
// on top — args validation and the `BindTopicOutcome` → JSON `code`/`error`
// mapping the handler itself owns.

#[allow(clippy::unwrap_used, clippy::expect_used)]
fn tmp_home_for_bind_topic(tag: &str) -> std::path::PathBuf {
    let home = std::env::temp_dir().join(format!(
        "agend-991-bind-topic-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&home).unwrap();
    home
}

#[test]
fn bind_topic_missing_instance_arg_991() {
    let home = tmp_home_for_bind_topic("missing-arg");
    let resp = handle_bind_topic(&home, &json!({}));
    assert_eq!(resp["error"], "missing 'instance'");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn bind_topic_rejects_invalid_instance_name_991() {
    let home = tmp_home_for_bind_topic("invalid-name");
    let resp = handle_bind_topic(&home, &json!({"instance": "bad name!"}));
    let err = resp["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("invalid characters"),
        "expected an invalid-characters error, got: {resp}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn bind_topic_rejects_unsupported_channel_991() {
    let home = tmp_home_for_bind_topic("bad-channel");
    let resp = handle_bind_topic(
        &home,
        &json!({"instance": "someagent", "channel": "discord"}),
    );
    assert_eq!(resp["code"], "channel_not_supported");
    assert!(
        resp["error"]
            .as_str()
            .unwrap_or_default()
            .contains("discord"),
        "error should name the rejected channel: {resp}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
fn bind_topic_instance_not_found_991() {
    let home = tmp_home_for_bind_topic("not-found");
    std::fs::write(crate::fleet::fleet_yaml_path(&home), "instances: {}\n").unwrap();
    let resp = handle_bind_topic(&home, &json!({"instance": "ghost"}));
    assert_eq!(resp["code"], "instance_not_found");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
fn bind_topic_skip_mode_via_handler_991() {
    let home = tmp_home_for_bind_topic("skip-mode");
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  internal-only:\n    backend: claude\n    topic_binding_mode: skip\n",
    )
    .unwrap();
    let resp = handle_bind_topic(&home, &json!({"instance": "internal-only"}));
    assert_eq!(resp["code"], "not_eligible");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
fn bind_topic_already_bound_via_handler_991() {
    let home = tmp_home_for_bind_topic("already-bound");
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  deferred-agent:\n    backend: claude\n    topic_binding_mode: deferred\n",
    )
    .unwrap();
    crate::channel::telegram::register_topic(&home, 501, "deferred-agent").unwrap();
    // Explicit channel="telegram" must behave identically to the default.
    let resp = handle_bind_topic(
        &home,
        &json!({"instance": "deferred-agent", "channel": "telegram"}),
    );
    assert_eq!(resp["bound"], true);
    assert_eq!(resp["topic_id"], 501);
    assert_eq!(resp["already_bound"], true);
    std::fs::remove_dir_all(&home).ok();
}

#[test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
fn bind_topic_channel_unavailable_via_handler_991() {
    let home = tmp_home_for_bind_topic("channel-unavailable");
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  deferred-agent:\n    backend: claude\n    topic_binding_mode: deferred\n",
    )
    .unwrap();
    // No `channel:` section in fleet.yaml → resolve_channel_only_from errors.
    let resp = handle_bind_topic(&home, &json!({"instance": "deferred-agent"}));
    assert_eq!(resp["code"], "channel_unavailable");
    std::fs::remove_dir_all(&home).ok();
}

// ── t-95913-5: defer fresh/resume restart while the operator has an unsent
// draft in the target pane's input line. The pure `restart_draft_gate` covers
// the whole decision matrix deterministically (no clock, no sleep). ──

#[test]
fn restart_draft_gate_proceeds_when_no_live_draft() {
    // No unsent draft → kill immediately (gate is a no-op), any elapsed.
    assert_eq!(
        restart_draft_gate(false, false, std::time::Duration::ZERO, RESTART_DRAFT_GRACE),
        DraftGate::Proceed
    );
}

#[test]
fn restart_draft_gate_defers_live_draft_within_grace() {
    // Live draft, still inside the grace window → defer the kill.
    assert_eq!(
        restart_draft_gate(
            false,
            true,
            std::time::Duration::from_secs(10),
            RESTART_DRAFT_GRACE
        ),
        DraftGate::Defer
    );
}

#[test]
fn restart_draft_gate_proceeds_when_grace_ceiling_reached() {
    // Live draft but grace elapsed → force the kill so continuous typing can't
    // defer forever. Boundary: elapsed == grace proceeds.
    assert_eq!(
        restart_draft_gate(false, true, RESTART_DRAFT_GRACE, RESTART_DRAFT_GRACE),
        DraftGate::Proceed
    );
    assert_eq!(
        restart_draft_gate(
            false,
            true,
            std::time::Duration::from_secs(61),
            RESTART_DRAFT_GRACE
        ),
        DraftGate::Proceed
    );
}

#[test]
fn restart_draft_gate_force_bypasses_live_draft() {
    // force:true → kill immediately even with a live draft inside the window.
    assert_eq!(
        restart_draft_gate(true, true, std::time::Duration::ZERO, RESTART_DRAFT_GRACE),
        DraftGate::Proceed
    );
}

#[test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
fn operator_has_live_draft_reflects_unsent_keystrokes() {
    let home = std::env::temp_dir().join(format!(
        "agend-live-draft-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&home).unwrap();
    // No metadata → no draft.
    assert!(!crate::inbox::notify::operator_has_live_draft(&home, "a"));
    // A keystroke with no following submit → a live unsent draft.
    crate::notification_queue::record_input_activity(&home, "a");
    assert!(crate::inbox::notify::operator_has_live_draft(&home, "a"));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
fn await_unsent_draft_or_grace_returns_fast_without_a_draft() {
    // No draft → the fast path returns immediately (no block). A regression that
    // dropped the no-draft check would hang this test to the nextest timeout.
    let home = std::env::temp_dir().join(format!(
        "agend-await-nodraft-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&home).unwrap();
    await_unsent_draft_or_grace(&home, "a", false);
    std::fs::remove_dir_all(&home).ok();
}
