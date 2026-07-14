use super::*;

fn tmp_home(suffix: &str) -> std::path::PathBuf {
    let h = std::env::temp_dir().join(format!(
        "agend-p0x-handler-{}-{}",
        std::process::id(),
        suffix
    ));
    std::fs::create_dir_all(&h).ok();
    h
}

#[test]
fn handler_rejects_missing_agent() {
    let home = tmp_home("no-agent");
    let result = handle_release_worktree(&home, &json!({}), &None);
    assert_eq!(
        result["error"].as_str(),
        Some("missing 'instance'"),
        "missing instance must surface clear error: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn handler_rejects_invalid_agent_name() {
    let home = tmp_home("bad-name");
    // Agent names with `..` are rejected by validate_name.
    let result = handle_release_worktree(&home, &json!({"instance": "../etc/passwd"}), &None);
    assert!(
        result.get("error").is_some(),
        "invalid agent name must error: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn handler_idempotent_no_binding_returns_success_noop() {
    // #1465: no binding → idempotent SUCCESS no-op (released:true,
    // already_released:true, no error; was released:false pre-#1465).
    let home = tmp_home("idem-no-binding");
    let result = handle_release_worktree(&home, &json!({"instance": "ghost"}), &None);
    assert_eq!(result["released"].as_bool(), Some(true), "{result}");
    assert_eq!(result["already_released"].as_bool(), Some(true), "{result}");
    assert!(
        result.get("error").is_none(),
        "no-op must not error: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── #2548 PR-2: release_worktree(force:true) tests ──────────────────
//
// Absorbed from the former standalone `force_release_worktree` tool
// (`force_release/mod.rs`'s pre-#2548 test suite) — same behavior,
// now exercised through `handle_release_worktree(..., "force": true)`.

/// Write a daemon-managed worktree dir at the canonical path with the
/// `.agend-managed` marker, mirroring the daemon's real bind-then-crash
/// stale-state scenario.
fn seed_force_worktree(home: &std::path::Path, agent: &str, branch: &str) -> std::path::PathBuf {
    let dir = home.join("worktrees").join(agent).join(branch);
    std::fs::create_dir_all(&dir).unwrap();
    let source_repo = home.join("source-repo");
    std::fs::create_dir_all(&source_repo).unwrap();
    std::fs::write(
        dir.join(".agend-managed"),
        format!(
            "agent={agent}\nbranch={branch}\nsource_repo={}\n",
            source_repo.display()
        ),
    )
    .unwrap();
    std::fs::write(dir.join("sample.txt"), "leftover").unwrap();
    dir
}

#[test]
fn force_cleans_existing_dir() {
    let home = tmp_home("force-clean-existing");
    let dir = seed_force_worktree(&home, "dev", "feature/x");
    assert!(dir.exists(), "seeded dir must exist pre-call");
    let result = handle_release_worktree(
        &home,
        &json!({"instance": "dev", "branch": "feature/x", "force": true}),
        &None,
    );
    assert_eq!(result["released"].as_bool(), Some(true), "{result}");
    assert_eq!(result["dir_existed"].as_bool(), Some(true), "{result}");
    assert_eq!(result["dir_removed"].as_bool(), Some(true), "{result}");
    assert!(!dir.exists(), "dir must be cleaned post-call");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn force_idempotent_on_missing_dir() {
    let home = tmp_home("force-idempotent");
    let source = home.join("source-repo");
    std::fs::create_dir_all(&source).unwrap();
    let result = handle_release_worktree(
        &home,
        &json!({"instance": "dev", "branch": "feature/never-existed", "repository_path": source, "force": true}),
        &None,
    );
    assert_eq!(result["released"].as_bool(), Some(true), "{result}");
    assert_eq!(result["dir_existed"].as_bool(), Some(false), "{result}");
    assert_eq!(result["dir_removed"].as_bool(), Some(false), "{result}");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn force_releases_binding_too() {
    let home = tmp_home("force-releases-binding");
    seed_force_worktree(&home, "dev", "feature/y");
    let result = handle_release_worktree(
        &home,
        &json!({"instance": "dev", "branch": "feature/y", "force": true}),
        &None,
    );
    let outcome = &result["binding_outcome"];
    assert!(outcome.is_object(), "{result}");
    // No prior binding → #1465 idempotent SUCCESS no-op.
    assert_eq!(outcome["released"].as_bool(), Some(true), "{result}");
    assert_eq!(
        outcome["already_released"].as_bool(),
        Some(true),
        "{result}"
    );
    assert!(outcome["error"].is_null(), "no-op must not error: {result}");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn force_rejects_path_outside_worktree_pool() {
    // Defense-in-depth: empty branch is caught by the missing-branch
    // check first, but exercises the input-rejection path without
    // manipulating anything outside the pool.
    let home = tmp_home("force-outside-pool-reject");
    let outside = home.join("config");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("important.json"), "data").unwrap();
    let result = handle_release_worktree(
        &home,
        &json!({"instance": "dev", "branch": "", "force": true}),
        &None,
    );
    assert!(
        result["error"].is_string(),
        "empty branch must error: {result}"
    );
    assert!(outside.join("important.json").exists());
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn force_rejects_invalid_agent_name() {
    let home = tmp_home("force-invalid-agent");
    let result = handle_release_worktree(
        &home,
        &json!({"instance": "../etc/passwd", "branch": "feature/x", "force": true}),
        &None,
    );
    assert!(result["error"].is_string());
    assert_eq!(result["code"].as_str(), Some("invalid_agent"), "{result}");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn force_rejects_invalid_branch_name() {
    let home = tmp_home("force-invalid-branch");
    let result = handle_release_worktree(
        &home,
        &json!({"instance": "dev", "branch": "../../escape", "force": true}),
        &None,
    );
    assert!(result["error"].is_string());
    assert_eq!(result["code"].as_str(), Some("invalid_branch"), "{result}");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn force_rejects_missing_branch() {
    let home = tmp_home("force-missing-branch");
    let result = handle_release_worktree(&home, &json!({"instance": "dev", "force": true}), &None);
    assert_eq!(
        result["error"].as_str(),
        Some("missing 'branch'"),
        "{result}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn force_preserves_other_branches() {
    let home = tmp_home("force-preserves-siblings");
    let dir_x = seed_force_worktree(&home, "dev", "feature/x");
    let dir_y = seed_force_worktree(&home, "dev", "feature/y");
    let result = handle_release_worktree(
        &home,
        &json!({"instance": "dev", "branch": "feature/x", "force": true}),
        &None,
    );
    assert_eq!(result["dir_removed"].as_bool(), Some(true), "{result}");
    assert!(!dir_x.exists(), "target branch dir cleaned");
    assert!(
        dir_y.exists(),
        "sibling branch dir preserved: {}",
        dir_y.display()
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn force_preserves_other_agents() {
    let home = tmp_home("force-preserves-agents");
    let dir_dev = seed_force_worktree(&home, "dev", "feature/x");
    let dir_lead = seed_force_worktree(&home, "lead", "feature/x");
    let result = handle_release_worktree(
        &home,
        &json!({"instance": "dev", "branch": "feature/x", "force": true}),
        &None,
    );
    assert_eq!(result["dir_removed"].as_bool(), Some(true), "{result}");
    assert!(!dir_dev.exists());
    assert!(
        dir_lead.exists(),
        "lead's dir preserved: {}",
        dir_lead.display()
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── S2 force/rebase authority matrix (RED-first) ─────────────────────

fn write_raw_binding(home: &std::path::Path, agent: &str, body: &str) {
    let dir = crate::paths::runtime_dir(home).join(agent);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("binding.json"), body).unwrap();
}

fn seed_binding_target(
    home: &std::path::Path,
    agent: &str,
    branch: &str,
    source_repo: &std::path::Path,
) -> std::path::PathBuf {
    let target = home.join("worktrees").join(agent).join(branch);
    std::fs::create_dir_all(&target).unwrap();
    std::fs::create_dir_all(source_repo).unwrap();
    std::fs::write(
        target.join(crate::worktree_pool::MANAGED_MARKER),
        format!(
            "agent={agent}\nbranch={branch}\nsource_repo={}\n",
            source_repo.display()
        ),
    )
    .unwrap();
    crate::binding::bind_full(home, agent, "", branch, &target, source_repo, false).unwrap();
    target
}

#[test]
fn s2_force_known_branch_mismatch_refuses_without_mutation() {
    let home = tmp_home("s2-known-mismatch");
    let source = home.join("repo-a");
    std::fs::create_dir_all(&source).unwrap();
    let target = seed_binding_target(&home, "agent", "feature/live", &source);
    let before = std::fs::read_to_string(crate::paths::binding_path(&home, "agent")).unwrap();
    let result = handle_release_worktree(
        &home,
        &json!({"instance":"agent", "branch":"feature/other", "force":true}),
        &None,
    );
    assert!(
        result["error"].is_string(),
        "mismatched known binding must refuse: {result}"
    );
    assert!(target.exists(), "mismatch must not remove target");
    assert_eq!(
        std::fs::read_to_string(crate::paths::binding_path(&home, "agent")).unwrap(),
        before,
        "mismatch must preserve binding evidence"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn s2_force_opaque_binding_refuses_without_mutation() {
    let home = tmp_home("s2-opaque");
    let target = seed_force_worktree(&home, "agent", "feature/opaque");
    write_raw_binding(&home, "agent", "{ definitely not json");
    let result = handle_release_worktree(
        &home,
        &json!({"instance":"agent", "branch":"feature/opaque", "force":true}),
        &None,
    );
    assert!(
        result["error"].is_string(),
        "opaque binding must refuse: {result}"
    );
    assert!(target.exists(), "opaque state must not remove target");
    assert_eq!(
        std::fs::read_to_string(crate::paths::binding_path(&home, "agent")).unwrap(),
        "{ definitely not json"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn s2_force_absent_unmanaged_orphan_refuses_with_operator_route() {
    let home = tmp_home("s2-absent-unmanaged");
    let target = home.join("worktrees").join("agent").join("feature/orphan");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("important.txt"), "operator data").unwrap();
    let result = handle_release_worktree(
        &home,
        &json!({"instance":"agent", "branch":"feature/orphan", "force":true}),
        &None,
    );
    let error = result["error"].as_str().unwrap_or("");
    assert!(
        error.contains("GC archive") || error.contains("operator"),
        "refusal must name sanctioned recovery channel: {result}"
    );
    assert!(target.exists(), "unmanaged orphan must be preserved");
    assert!(target.join("important.txt").exists());
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn s2_force_absent_exact_managed_target_succeeds() {
    let home = tmp_home("s2-absent-managed");
    let target = seed_force_worktree(&home, "agent", "feature/managed");
    let result = handle_release_worktree(
        &home,
        &json!({"instance":"agent", "branch":"feature/managed", "force":true}),
        &None,
    );
    assert_eq!(result["released"].as_bool(), Some(true), "{result}");
    assert_eq!(result["dir_removed"].as_bool(), Some(true), "{result}");
    assert!(!target.exists(), "exact managed target must be removed");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn s2_force_explicit_repo_disagreement_refuses_without_mutation() {
    let home = tmp_home("s2-repo-disagreement");
    let source = home.join("repo-owner");
    let other = home.join("repo-explicit");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    let target = seed_binding_target(&home, "agent", "feature/owned", &source);
    let result = handle_release_worktree(
        &home,
        &json!({
            "instance":"agent",
            "branch":"feature/owned",
            "repository_path":other,
            "force":true
        }),
        &None,
    );
    assert!(
        result["error"].is_string(),
        "explicit repo disagreement must refuse: {result}"
    );
    assert!(target.exists(), "repo disagreement must not remove target");
    std::fs::remove_dir_all(&home).ok();
}

#[cfg(unix)]
#[test]
fn s2_force_canonical_repo_aliases_share_one_branch_lease() {
    let home = tmp_home("s2-repo-alias");
    let source = home.join("repo-owner");
    let alias = home.join("repo-alias");
    std::fs::create_dir_all(&source).unwrap();
    std::os::unix::fs::symlink(&source, &alias).unwrap();
    let target = seed_binding_target(&home, "agent", "feature/alias", &source);
    let result = handle_release_worktree(
        &home,
        &json!({
            "instance":"agent",
            "branch":"feature/alias",
            "repository_path":alias,
            "force":true
        }),
        &None,
    );
    assert_eq!(result["released"].as_bool(), Some(true), "{result}");
    assert!(
        !target.exists(),
        "canonical aliases must resolve one target"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn s2_force_racing_new_generation_preserves_new_binding_and_target() {
    let home = tmp_home("s2-generation-race");
    let source = home.join("repo-race");
    std::fs::create_dir_all(&source).unwrap();
    let target = seed_binding_target(&home, "agent", "feature/race", &source);
    let replacement = serde_json::json!({
        "version": 1,
        "agent":"agent",
        "task_id":"new",
        "branch":"feature/race",
        "worktree":target.display().to_string(),
        "source_repo":source.display().to_string(),
        "issued_at":"2099-01-01T00:00:00Z"
    });
    let hook_home = home.clone();
    let _hook = crate::worktree_pool::release_test_seam::install(move |phase| {
        if phase == crate::worktree_pool::ReleaseTestPhase::AfterBindingSnapshot {
            write_raw_binding(
                &hook_home,
                "agent",
                &serde_json::to_string(&replacement).unwrap(),
            );
        }
    });
    let result = handle_release_worktree(
        &home,
        &json!({"instance":"agent", "branch":"feature/race", "force":true}),
        &None,
    );
    assert!(
        result["error"].is_string(),
        "new generation must win the race: {result}"
    );
    assert!(
        target.exists(),
        "new generation race must not remove target"
    );
    let live: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(crate::paths::binding_path(&home, "agent")).unwrap(),
    )
    .unwrap();
    assert_eq!(live["task_id"], "new");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn s2_rebase_live_opaque_binding_refuses_without_overwrite() {
    let home = tmp_home("s2-rebase-opaque-live");
    let source = home.join("repo-rebase-opaque");
    let target = seed_binding_target(&home, "agent", "feature/rebase-opaque", &source);
    crate::binding::unbind(&home, "agent");
    write_raw_binding(&home, "agent", "{ definitely not json");
    let result = handle_bind_self(
        &home,
        &json!({
            "branch":"feature/rebase-opaque",
            "repository_path":source,
            "rebase_mode":true
        }),
        &crate::identity::Sender::new("agent"),
    );
    assert_eq!(
        result["code"].as_str(),
        Some("rebind_repair_blocked"),
        "{result}"
    );
    assert!(target.exists(), "opaque live target must be preserved");
    assert_eq!(
        std::fs::read_to_string(crate::paths::binding_path(&home, "agent")).unwrap(),
        "{ definitely not json"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn s2_force_absent_metadata_race_refuses_new_binding() {
    let home = tmp_home("s2-absent-metadata-race");
    let source = home.join("repo-absent-race");
    std::fs::create_dir_all(&source).unwrap();
    let replacement = serde_json::json!({
        "version": 1,
        "agent":"agent",
        "task_id":"new",
        "branch":"feature/absent-race",
        "worktree":home.join("worktrees/agent/feature/absent-race").display().to_string(),
        "source_repo":source.display().to_string(),
        "issued_at":"2099-01-01T00:00:00Z"
    });
    let hook_home = home.clone();
    let _hook = crate::worktree_pool::release_test_seam::install(move |phase| {
        if phase == crate::worktree_pool::ReleaseTestPhase::AfterBindingSnapshot {
            write_raw_binding(
                &hook_home,
                "agent",
                &serde_json::to_string(&replacement).unwrap(),
            );
        }
    });
    let result = handle_release_worktree(
        &home,
        &json!({
            "instance":"agent",
            "branch":"feature/absent-race",
            "repository_path":source,
            "force":true
        }),
        &None,
    );
    assert!(
        result["error"].is_string(),
        "new bind must win absent race: {result}"
    );
    assert!(crate::binding::read(&home, "agent").is_some());
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn s2_force_absent_marker_branch_drift_refuses() {
    let home = tmp_home("s2-absent-marker-branch-drift");
    let source = home.join("repo-marker-drift");
    std::fs::create_dir_all(&source).unwrap();
    let target = home.join("worktrees/agent/feature/marker-drift");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(
        target.join(crate::worktree_pool::MANAGED_MARKER),
        format!(
            "agent=agent\nbranch=feature/marker-drift\nsource_repo={}\n",
            source.display()
        ),
    )
    .unwrap();
    let hook_target = target.clone();
    let _hook = crate::worktree_pool::release_test_seam::install(move |phase| {
        if phase == crate::worktree_pool::ReleaseTestPhase::AfterBindingSnapshot {
            std::fs::write(
                hook_target.join(crate::worktree_pool::MANAGED_MARKER),
                "agent=agent\nbranch=feature/drifted\nsource_repo=/wrong\n",
            )
            .unwrap();
        }
    });
    let result = handle_release_worktree(
        &home,
        &json!({
            "instance":"agent",
            "branch":"feature/marker-drift",
            "repository_path":source,
            "force":true
        }),
        &None,
    );
    assert!(
        result["error"].is_string(),
        "marker drift must refuse: {result}"
    );
    assert!(target.exists(), "marker drift must preserve target");
    std::fs::remove_dir_all(&home).ok();
}

#[cfg(unix)]
#[test]
fn s2_force_opaque_target_metadata_refuses() {
    let home = tmp_home("s2-opaque-target");
    let source = home.join("repo-opaque-target");
    std::fs::create_dir_all(&source).unwrap();
    let target = home.join("worktrees/agent/feature/opaque-target");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("missing-target", &target).unwrap();
    let result = handle_release_worktree(
        &home,
        &json!({
            "instance":"agent",
            "branch":"feature/opaque-target",
            "repository_path":source,
            "force":true
        }),
        &None,
    );
    let error = result["error"].as_str().unwrap_or("");
    assert!(
        error.contains("opaque target"),
        "typed opaque target refusal: {result}"
    );
    assert!(
        target.symlink_metadata().is_ok(),
        "opaque target must be preserved"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn s2_legacy_soft_release_has_no_production_entry_point() {
    let source = include_str!("../../../worktree_pool.rs");
    assert!(
        !source.contains("pub fn release("),
        "legacy soft release must be deleted; force/rebase must use guarded transaction"
    );
}

/// S2 authority anchor: a marker-less orphan is never guessed to be daemon
/// state. The sanctioned recovery paths are the GC archive or operator channel.
#[test]
fn force_refuses_dir_without_marker() {
    let home = tmp_home("force-no-marker");
    let dir = home.join("worktrees").join("dev").join("feature/x");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("leftover"), "data").unwrap();
    assert!(
        !dir.join(".agend-managed").exists(),
        "fixture has no marker"
    );
    let result = handle_release_worktree(
        &home,
        &json!({"instance": "dev", "branch": "feature/x", "force": true}),
        &None,
    );
    let error = result["error"].as_str().unwrap_or("");
    assert!(
        error.contains("GC archive") || error.contains("operator"),
        "{result}"
    );
    assert!(dir.exists(), "marker-less orphan must be preserved");
    std::fs::remove_dir_all(&home).ok();
}

/// AUDIT2-002 pinning test (RED-first): a peer that is neither the
/// worktree's own agent nor its team orchestrator must be denied.
#[test]
fn force_denies_non_owner_non_orchestrator() {
    let home = tmp_home("force-audit2-002-deny");
    seed_force_worktree(&home, "victim", "feat/x");
    let attacker = crate::identity::Sender::new("attacker");
    let result = handle_release_worktree(
        &home,
        &json!({"instance": "victim", "branch": "feat/x", "force": true}),
        &attacker,
    );
    assert_eq!(
        result["code"].as_str(),
        Some("not_owner_or_orchestrator"),
        "non-owner must be denied: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// AUDIT2-002: the worktree's own agent may force-release itself.
#[test]
fn force_allows_owner() {
    let home = tmp_home("force-audit2-002-owner");
    let dir = seed_force_worktree(&home, "victim", "feat/x");
    let owner = crate::identity::Sender::new("victim");
    let result = handle_release_worktree(
        &home,
        &json!({"instance": "victim", "branch": "feat/x", "force": true}),
        &owner,
    );
    assert_ne!(result["code"], "not_owner_or_orchestrator", "{result}");
    assert!(
        !dir.exists(),
        "owner-initiated force-release must clean the dir"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// AUDIT2-002: the target agent's team orchestrator may force-release
/// on its behalf.
#[test]
fn force_allows_team_orchestrator() {
    let home = tmp_home("force-audit2-002-orchestrator");
    let dir = seed_force_worktree(&home, "victim", "feat/x");
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  victim:\n    backend: claude\n  lead:\n    backend: claude\n\
         teams:\n  squad:\n    orchestrator: lead\n    members:\n      - victim\n",
    )
    .unwrap();
    let orchestrator = crate::identity::Sender::new("lead");
    let result = handle_release_worktree(
        &home,
        &json!({"instance": "victim", "branch": "feat/x", "force": true}),
        &orchestrator,
    );
    assert_ne!(result["code"], "not_owner_or_orchestrator", "{result}");
    assert!(
        !dir.exists(),
        "orchestrator-initiated force-release must clean the dir"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Zero-behavior-change anchor: `force:false` (the default, including
/// when the field is simply absent) must behave byte-identically to
/// pre-#2548 `release_worktree` — an extra `branch` arg (only
/// meaningful under `force:true`) must be silently ignored, and the
/// binding-driven path must run exactly as before.
#[test]
fn force_false_is_unaffected_by_branch_arg() {
    let home = tmp_home("force-false-unaffected");
    // No binding for "ghost" → idempotent no-op, same as the existing
    // `handler_idempotent_no_binding_returns_success_noop` behavior,
    // even with a stray `branch` arg present.
    let result = handle_release_worktree(
        &home,
        &json!({"instance": "ghost", "branch": "feature/x", "force": false}),
        &None,
    );
    assert_eq!(result["released"].as_bool(), Some(true), "{result}");
    assert_eq!(result["already_released"].as_bool(), Some(true), "{result}");
    assert!(result.get("error").is_none(), "{result}");
    // And the force-only response fields must NOT appear on this path.
    assert!(result.get("git_metadata_pruned").is_none(), "{result}");
    std::fs::remove_dir_all(&home).ok();
}

// ── Sprint 54 P1-7: bind_self handler tests ─────────────────────────
//
// These exercise `handle_bind_self` directly — same path the MCP layer
// uses. The helper sets up a real git repo + fleet.yaml entry so
// `worktree_pool::lease` can actually create the worktree (matches
// dispatch_hook/tests.rs setup_test_repo).
//
// Regression-proof anchor: replace the body of
// `dispatch_auto_bind_lease` with `Ok(())` (skip the actual bind) →
// `bind_self_creates_binding_and_worktree` fails because binding.json
// never gets written. PR description carries the captured FAIL
// signature.

fn p17_setup_repo(home: &std::path::Path, agent: &str) -> std::path::PathBuf {
    let repo = crate::paths::workspace_dir(home).join(agent);
    std::fs::create_dir_all(&repo).ok();
    std::process::Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(&repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .ok();
    std::process::Command::new("git")
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=t@t",
            "commit",
            "--allow-empty",
            "-m",
            "init",
        ])
        .current_dir(&repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .ok();
    // #781 Phase 3 r1 (Path A): populate `refs/remotes/origin/main`
    // for strict `ensure_branch_exists`; file:/// URL so
    // derive_repo returns None.
    let git = |a: &[&str]| -> Option<std::process::Output> {
        std::process::Command::new("git")
            .args(a)
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .ok()
    };
    git(&["remote", "add", "origin", "file:///dev/null/agend-fix"]);
    if let Some(o) = git(&["rev-parse", "HEAD"]).filter(|o| o.status.success()) {
        let sha = String::from_utf8_lossy(&o.stdout).trim().to_string();
        git(&["update-ref", "refs/remotes/origin/main", &sha]);
    }
    std::fs::write(
        crate::fleet::fleet_yaml_path(home),
        format!(
            "instances:\n  {agent}:\n    backend: claude\n    working_directory: {}\n",
            repo.display()
        ),
    )
    .ok();
    repo
}

fn sender_for(name: &str) -> Option<crate::identity::Sender> {
    crate::identity::Sender::new(name)
}

fn rebase_git(repo: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("git command");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn seed_live_rebase_binding(
    home: &std::path::Path,
    agent: &str,
    current_branch: &str,
    target_branches: &[&str],
) -> std::path::PathBuf {
    let repo = home.join("rebase-live-repo");
    std::fs::create_dir_all(&repo).unwrap();
    rebase_git(&repo, &["init", "-q", "-b", current_branch]);
    std::fs::write(repo.join(".gitignore"), ".agend-managed\n").unwrap();
    rebase_git(
        &repo,
        &[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@test",
            "add",
            ".gitignore",
        ],
    );
    rebase_git(
        &repo,
        &[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@test",
            "commit",
            "--allow-empty",
            "-q",
            "-m",
            "init",
        ],
    );
    for branch in target_branches {
        rebase_git(&repo, &["branch", branch]);
    }
    std::fs::write(
        repo.join(crate::worktree_pool::MANAGED_MARKER),
        format!(
            "agent={agent}\nbranch={current_branch}\nsource_repo={}\n",
            repo.display()
        ),
    )
    .unwrap();
    crate::binding::bind_full(home, agent, "", current_branch, &repo, &repo, true)
        .expect("seed binding");
    repo
}

fn marker_value(path: &std::path::Path, field: &str) -> String {
    std::fs::read_to_string(path.join(crate::worktree_pool::MANAGED_MARKER))
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{field}=")).map(str::to_string))
        .expect("marker field")
}

fn binding_value(home: &std::path::Path, agent: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(crate::paths::binding_path(home, agent)).unwrap())
        .unwrap()
}

#[test]
fn r3_bind_self_rebase_metadata_only_is_end_to_end() {
    let home = tmp_home("r3-rebase-metadata-only");
    let repo = seed_live_rebase_binding(&home, "agent", "main", &[]);
    let response = handle_bind_self(
        &home,
        &json!({"repository_path":repo, "branch":"main", "rebase_mode":true}),
        &sender_for("agent"),
    );
    assert_eq!(response["bound"].as_bool(), Some(true), "{response}");
    assert_eq!(
        response["repair_action"].as_str(),
        Some("metadata_only"),
        "{response}"
    );
    assert_eq!(
        crate::git_helpers::git_cmd(&repo, &["branch", "--show-current"]).unwrap(),
        "main"
    );
    assert_eq!(marker_value(&repo, "agent"), "agent");
    assert_eq!(marker_value(&repo, "branch"), "main");
    assert_eq!(binding_value(&home, "agent")["branch"], "main");
    assert!(crate::binding::signature_valid(&home, "agent"));
    assert_eq!(
        binding_value(&home, "agent")["worktree"],
        repo.display().to_string()
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn r3_bind_self_rebase_switched_branch_is_end_to_end() {
    let home = tmp_home("r3-rebase-switched");
    let repo = seed_live_rebase_binding(&home, "agent", "main", &["feature/r3"]);
    let response = handle_bind_self(
        &home,
        &json!({"repository_path":repo, "branch":"feature/r3", "rebase_mode":true}),
        &sender_for("agent"),
    );
    assert_eq!(response["bound"].as_bool(), Some(true), "{response}");
    assert_eq!(
        response["repair_action"].as_str(),
        Some("switched_branch"),
        "{response}"
    );
    assert_eq!(
        crate::git_helpers::git_cmd(&repo, &["branch", "--show-current"]).unwrap(),
        "feature/r3"
    );
    assert_eq!(marker_value(&repo, "agent"), "agent");
    assert_eq!(marker_value(&repo, "branch"), "feature/r3");
    assert_eq!(binding_value(&home, "agent")["branch"], "feature/r3");
    assert!(crate::binding::signature_valid(&home, "agent"));
    assert_eq!(
        binding_value(&home, "agent")["worktree"],
        repo.display().to_string()
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn r3_concurrent_rebase_has_one_bind_guard_transaction() {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Barrier, Condvar, Mutex,
    };
    use std::thread;

    let home = tmp_home("r3-rebase-concurrent");
    let repo = seed_live_rebase_binding(&home, "agent", "main", &["feature/a", "feature/b"]);
    let entered = Arc::new((Mutex::new(false), Condvar::new()));
    let green_barrier = Arc::new(Barrier::new(2));
    let first_callback = Arc::new(AtomicBool::new(true));
    let first_home = home.clone();
    let first_repo = repo.clone();
    let first_entered = entered.clone();
    let first_green = green_barrier.clone();
    let first_callback_gate = first_callback.clone();
    let first = thread::spawn(move || {
        let _hook = crate::mcp::handlers::force_release::rebase_test_seam::install(move |phase| {
            if phase != crate::mcp::handlers::force_release::RebaseTestPhase::BeforeRepair {
                return None;
            }
            let (flag, cv) = &*first_entered;
            *flag.lock().unwrap() = true;
            cv.notify_one();
            if first_callback_gate.swap(false, Ordering::SeqCst) {
                first_green.wait();
            }
            None
        });
        handle_bind_self(
            &first_home,
            &json!({"repository_path":first_repo, "branch":"feature/a", "rebase_mode":true}),
            &sender_for("agent"),
        )
    });
    let (flag, cv) = &*entered;
    let mut entered_guard = flag.lock().unwrap();
    while !*entered_guard {
        entered_guard = cv.wait(entered_guard).unwrap();
    }
    drop(entered_guard);
    let second_home = home.clone();
    let second_repo = repo.clone();
    let second_entered = entered.clone();
    let second_green = green_barrier.clone();
    let second_callback_gate = first_callback.clone();
    let second = thread::spawn(move || {
        let _hook = crate::mcp::handlers::force_release::rebase_test_seam::install(move |phase| {
            if phase != crate::mcp::handlers::force_release::RebaseTestPhase::BeforeRepair {
                return None;
            }
            let (flag, cv) = &*second_entered;
            *flag.lock().unwrap() = true;
            cv.notify_one();
            if second_callback_gate.swap(false, Ordering::SeqCst) {
                second_green.wait();
            }
            None
        });
        handle_bind_self(
            &second_home,
            &json!({"repository_path":second_repo, "branch":"feature/b", "rebase_mode":true}),
            &sender_for("agent"),
        )
    });
    green_barrier.wait();
    let first_response = first.join().unwrap();
    let second_response = second.join().unwrap();
    let successes = [first_response.clone(), second_response.clone()]
        .into_iter()
        .filter(|response| response["bound"].as_bool() == Some(true))
        .count();
    assert_eq!(
        successes, 1,
        "exactly one concurrent rebase may bind: {first_response} / {second_response}"
    );
    assert!(
        [first_response, second_response]
            .iter()
            .any(|response| response["code"].as_str() == Some("rebind_repair_blocked")),
        "losing rebase must be blocked by BindGuard"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn r3_marker_commit_failure_restores_coherent_state() {
    let home = tmp_home("r3-marker-failure");
    let repo = seed_live_rebase_binding(&home, "agent", "main", &["feature/fault"]);
    let _hook = crate::mcp::handlers::force_release::rebase_test_seam::install(|phase| {
        (phase == crate::mcp::handlers::force_release::RebaseTestPhase::BeforeMarkerCommit)
            .then(|| "injected marker commit failure".to_string())
    });
    let response = handle_bind_self(
        &home,
        &json!({"repository_path":repo, "branch":"feature/fault", "rebase_mode":true}),
        &sender_for("agent"),
    );
    assert!(
        response["error"]
            .as_str()
            .is_some_and(|error| error.contains("injected marker commit failure")),
        "marker fault must reach the rebase transaction: {response}"
    );
    assert_eq!(
        crate::git_helpers::git_cmd(&repo, &["branch", "--show-current"]).unwrap(),
        "main"
    );
    assert_eq!(marker_value(&repo, "branch"), "main");
    assert_eq!(binding_value(&home, "agent")["branch"], "main");
    assert!(crate::binding::signature_valid(&home, "agent"));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn r3_binding_commit_failure_restores_coherent_state() {
    let home = tmp_home("r3-binding-failure");
    let repo = seed_live_rebase_binding(&home, "agent", "main", &["feature/bind-fault"]);
    let _hook = crate::mcp::handlers::dispatch_hook::bind_test_seam::install(|| {
        Some("injected binding commit failure".to_string())
    });
    let response = handle_bind_self(
        &home,
        &json!({"repository_path":repo, "branch":"feature/bind-fault", "rebase_mode":true}),
        &sender_for("agent"),
    );
    assert!(
        response["error"]
            .as_str()
            .is_some_and(|error| error.contains("injected binding commit failure")),
        "binding fault must reach the rebase transaction: {response}"
    );
    assert_eq!(
        crate::git_helpers::git_cmd(&repo, &["branch", "--show-current"]).unwrap(),
        "main"
    );
    assert_eq!(marker_value(&repo, "branch"), "main");
    assert_eq!(binding_value(&home, "agent")["branch"], "main");
    assert!(crate::binding::signature_valid(&home, "agent"));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn r3_locked_metadata_cleanup_is_exact_or_fail_closed() {
    let home = tmp_home("r3-locked-metadata");
    let repo = home.join("metadata-owner");
    let target = home.join("worktrees").join("agent").join("feature/locked");
    std::fs::create_dir_all(&repo).unwrap();
    rebase_git(&repo, &["init", "-q", "-b", "main"]);
    rebase_git(
        &repo,
        &[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@test",
            "commit",
            "--allow-empty",
            "-q",
            "-m",
            "init",
        ],
    );
    rebase_git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature/locked",
            target.to_str().unwrap(),
        ],
    );
    std::fs::write(
        target.join(crate::worktree_pool::MANAGED_MARKER),
        format!(
            "agent=agent\nbranch=feature/locked\nsource_repo={}\n",
            repo.display()
        ),
    )
    .unwrap();
    let metadata_root = repo.join(".git").join("worktrees");
    let metadata = std::fs::read_dir(&metadata_root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.join("gitdir").exists())
        .expect("worktree metadata");
    std::fs::remove_dir_all(&target).unwrap();
    std::fs::write(metadata.join("locked"), "r3 test lock\n").unwrap();
    let response = handle_release_worktree(
        &home,
        &json!({"instance":"agent", "branch":"feature/locked", "repository_path":repo, "force":true}),
        &None,
    );
    let metadata_exists = metadata.exists();
    assert!(
        (response["released"].as_bool() == Some(true) && !metadata_exists)
            || (response["released"].as_bool() != Some(true) && metadata_exists),
        "locked metadata must be removed exactly or fail closed with evidence preserved: {response}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn r4_release_cannot_complete_between_repair_and_bind_commit() {
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread;

    let home = tmp_home("r4-release-during-rebase");
    let repo = seed_live_rebase_binding(&home, "agent", "main", &["feature/release-race"]);
    let entered = Arc::new((Mutex::new(false), Condvar::new()));
    let resume = Arc::new(std::sync::Barrier::new(2));
    let hook_entered = entered.clone();
    let hook_resume = resume.clone();
    let bind_home = home.clone();
    let bind_repo = repo.clone();
    let bind = thread::spawn(move || {
        let _hook = crate::mcp::handlers::dispatch_hook::bind_test_seam::install(move || {
            let (flag, cv) = &*hook_entered;
            *flag.lock().unwrap() = true;
            cv.notify_one();
            hook_resume.wait();
            None
        });
        handle_bind_self(
            &bind_home,
            &json!({"repository_path":bind_repo, "branch":"feature/release-race", "rebase_mode":true}),
            &sender_for("agent"),
        )
    });
    let (flag, cv) = &*entered;
    let mut guard = flag.lock().unwrap();
    while !*guard {
        guard = cv.wait(guard).unwrap();
    }
    drop(guard);
    let release = handle_release_worktree(&home, &json!({"instance":"agent"}), &None);
    assert_eq!(release["released"].as_bool(), Some(false), "{release}");
    assert!(
        release["error"]
            .as_str()
            .is_some_and(|error| error.contains("bind/rebase in flight")),
        "release must refuse while rebase owns lifecycle authority: {release}"
    );
    assert!(repo.exists(), "release must not remove the live worktree");
    resume.wait();
    let bind_result = bind.join().unwrap();
    assert_eq!(bind_result["bound"].as_bool(), Some(true), "{bind_result}");
    assert!(binding_value(&home, "agent")["worktree"]
        .as_str()
        .is_some_and(|path| std::path::Path::new(path).exists()));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn r4_rebase_rollback_preserves_newer_binding_generation() {
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread;

    let home = tmp_home("r4-newer-generation");
    let repo = seed_live_rebase_binding(&home, "agent", "main", &["feature/newer-generation"]);
    let entered = Arc::new((Mutex::new(false), Condvar::new()));
    let resume = Arc::new(std::sync::Barrier::new(2));
    let hook_entered = entered.clone();
    let hook_resume = resume.clone();
    let bind_home = home.clone();
    let bind_repo = repo.clone();
    let bind = thread::spawn(move || {
        let _hook = crate::mcp::handlers::dispatch_hook::bind_test_seam::install(move || {
            let (flag, cv) = &*hook_entered;
            *flag.lock().unwrap() = true;
            cv.notify_one();
            hook_resume.wait();
            Some("injected post-repair failure".to_string())
        });
        handle_bind_self(
            &bind_home,
            &json!({"repository_path":bind_repo, "branch":"feature/newer-generation", "rebase_mode":true}),
            &sender_for("agent"),
        )
    });
    let (flag, cv) = &*entered;
    let mut guard = flag.lock().unwrap();
    while !*guard {
        guard = cv.wait(guard).unwrap();
    }
    drop(guard);
    crate::binding::bind_full(
        &home,
        "agent",
        "new-generation",
        "feature/newer-generation",
        &repo,
        &repo,
        true,
    )
    .expect("new generation bind must succeed while rebase is paused");
    resume.wait();
    let bind_result = bind.join().unwrap();
    assert!(bind_result["error"].is_string(), "{bind_result}");
    assert_eq!(binding_value(&home, "agent")["task_id"], "new-generation");
    assert_eq!(
        binding_value(&home, "agent")["branch"],
        "feature/newer-generation"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn r4_exact_metadata_list_failure_is_opaque_and_preserves_binding() {
    let home = tmp_home("r4-metadata-list-opaque");
    let repo = home.join("metadata-list-repo");
    let target = home
        .join("worktrees")
        .join("agent")
        .join("feature/list-failure");
    std::fs::create_dir_all(&repo).unwrap();
    rebase_git(&repo, &["init", "-q", "-b", "main"]);
    rebase_git(
        &repo,
        &[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@test",
            "commit",
            "--allow-empty",
            "-q",
            "-m",
            "init",
        ],
    );
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    rebase_git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature/list-failure",
            target.to_str().unwrap(),
        ],
    );
    std::fs::write(
        target.join(crate::worktree_pool::MANAGED_MARKER),
        format!(
            "agent=agent\nbranch=feature/list-failure\nsource_repo={}\n",
            repo.display()
        ),
    )
    .unwrap();
    crate::binding::bind_full(
        &home,
        "agent",
        "",
        "feature/list-failure",
        &target,
        &repo,
        true,
    )
    .unwrap();
    std::fs::remove_dir_all(&target).unwrap();
    let _hook = crate::mcp::handlers::force_release::gc_test_seam::install(|| {
        Some("injected metadata list failure".to_string())
    });
    let result = handle_release_worktree(
        &home,
        &json!({
            "instance":"agent",
            "branch":"feature/list-failure",
            "repository_path":repo,
            "force":true
        }),
        &None,
    );
    assert_ne!(result["released"].as_bool(), Some(true), "{result}");
    assert!(
        result["error"]
            .as_str()
            .is_some_and(|error| error.contains("injected metadata list failure")),
        "metadata list failure must preserve evidence: {result}"
    );
    assert!(crate::binding::read(&home, "agent").is_some());
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn bind_self_creates_binding_and_worktree() {
    // Gate 1: a successful bind_self produces binding.json + worktree
    // dir + .agend-managed marker. Mirrors the dispatch hook's
    // happy path because we go through the same helper.
    //
    // EMPIRICAL REGRESSION-PROOF ANCHOR: replacing
    // `dispatch_auto_bind_lease` body with `Ok(())` makes this test
    // fail with "binding.json must exist after bind_self".
    let home = std::env::temp_dir().join(format!("agend-p17-self-{}-ok", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    p17_setup_repo(&home, "agent-self");

    let resp = handle_bind_self(
        &home,
        &json!({"repository": "owner/name", "branch": "feat/p17"}),
        &sender_for("agent-self"),
    );
    assert_eq!(
        resp["bound"].as_bool(),
        Some(true),
        "bind_self must succeed: {resp}"
    );
    let worktree_path = resp["worktree_path"]
        .as_str()
        .expect("worktree_path in success response");
    assert!(!worktree_path.is_empty(), "worktree_path must be populated");

    let binding_path = crate::paths::runtime_dir(&home)
        .join("agent-self")
        .join("binding.json");
    assert!(
        binding_path.exists(),
        "binding.json must exist after bind_self"
    );
    let v: Value =
        serde_json::from_str(&std::fs::read_to_string(&binding_path).expect("read binding"))
            .expect("parse binding");
    assert_eq!(v["branch"].as_str(), Some("feat/p17"));
    assert_eq!(
        v["task_id"].as_str(),
        Some(""),
        "self-bind without task_id arg must record empty task_id"
    );

    // Worktree dir + .agend-managed marker per P0-X / P1-7.
    let wt = std::path::Path::new(worktree_path);
    assert!(wt.exists(), "worktree dir must exist: {worktree_path}");
    assert!(
        wt.join(".agend-managed").exists(),
        ".agend-managed marker must exist: {worktree_path}"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// #2550 W3 Wave2 pin: the response's `worktree_path` must be an EXACT
/// match for the `worktree` field the bind just wrote to binding.json (not
/// just non-empty) — locks the read-back value before converging the raw
/// `read_to_string`+parse onto `binding::read()`.
#[test]
fn bind_self_response_worktree_path_matches_written_binding_2550_w3() {
    let home = tmp_home("resp-matches-binding");
    p17_setup_repo(&home, "agent-match");

    let resp = handle_bind_self(
        &home,
        &json!({"repository": "owner/name", "branch": "feat/w3-match"}),
        &sender_for("agent-match"),
    );
    assert_eq!(
        resp["bound"].as_bool(),
        Some(true),
        "bind_self must succeed: {resp}"
    );
    let resp_worktree = resp["worktree_path"]
        .as_str()
        .expect("worktree_path in success response");

    let binding_path = crate::paths::runtime_dir(&home)
        .join("agent-match")
        .join("binding.json");
    let v: Value =
        serde_json::from_str(&std::fs::read_to_string(&binding_path).expect("read binding"))
            .expect("parse binding");
    let disk_worktree = v["worktree"].as_str().expect("worktree field on disk");

    assert_eq!(
        resp_worktree, disk_worktree,
        "response worktree_path must exactly match the just-written binding.json worktree field"
    );

    // Response JSON structure: exactly the documented success shape (no
    // extra/missing keys for the non-rebase_mode path).
    let mut keys: Vec<&str> = resp
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        vec!["bound", "branch", "worktree_path"],
        "success response shape must be unchanged: {resp}"
    );

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn bind_self_idempotent_same_agent_same_branch() {
    // Gate 2: a second bind_self call from the same agent on the
    // same branch is idempotent. The first lease creates the
    // worktree; the second sees the existing daemon-managed
    // worktree on the matching branch and succeeds without
    // mutating state.
    let home = std::env::temp_dir().join(format!("agend-p17-self-{}-idem", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    p17_setup_repo(&home, "agent-idem");

    let args = json!({"repository": "owner/name", "branch": "feat/idem"});
    let r1 = handle_bind_self(&home, &args, &sender_for("agent-idem"));
    assert_eq!(r1["bound"].as_bool(), Some(true), "first bind: {r1}");
    let r2 = handle_bind_self(&home, &args, &sender_for("agent-idem"));
    assert_eq!(
        r2["bound"].as_bool(),
        Some(true),
        "second bind on same branch must be idempotent: {r2}"
    );
    assert_eq!(
        r1["worktree_path"], r2["worktree_path"],
        "worktree path must be stable across idempotent calls"
    );

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn bind_self_rejects_main_branch_with_e4_5() {
    // Gate 3: protected-branch invariant. Calling bind_self on
    // 'main' returns the E4.5 rejection from worktree_pool::lease,
    // mapped to a stable code so agents can branch on it.
    let home = std::env::temp_dir().join(format!("agend-p17-self-{}-e45", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    p17_setup_repo(&home, "agent-e45");

    let resp = handle_bind_self(
        &home,
        &json!({"repository": "owner/name", "branch": "main"}),
        &sender_for("agent-e45"),
    );
    assert!(
        resp.get("error").is_some(),
        "main branch must error: {resp}"
    );
    assert_eq!(
        resp["code"].as_str(),
        Some("e4_5_protected_branch"),
        "error code must surface E4.5 class: {resp}"
    );

    // No side-effects on rejection.
    let binding = crate::paths::runtime_dir(&home)
        .join("agent-e45")
        .join("binding.json");
    assert!(
        !binding.exists(),
        "rejected bind_self must not write binding.json"
    );

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn bind_self_rejects_cross_agent_branch_conflict() {
    // Gate 4: P0-1.5 cross-agent registry — agent A binds, agent B
    // attempts the same (source_repo, branch) → B is rejected.
    // #2117 P3b: the lease key is (source_repo, branch). Both agents claim the
    // SAME repo (via `repository_path`) and branch — the same-repo conflict P3b
    // preserves. (Cross-repo independence is covered by the dispatch-side
    // `cross_repo_same_branch_independent_p3b` test.)
    let home = std::env::temp_dir().join(format!("agend-p17-self-{}-cross", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    let shared = p17_setup_repo(&home, "shared-repo");
    let shared_path = shared.display().to_string();

    let r1 = handle_bind_self(
        &home,
        &json!({"repository_path": shared_path, "branch": "feat/cross"}),
        &sender_for("agent-A"),
    );
    assert_eq!(r1["bound"].as_bool(), Some(true), "A binds first: {r1}");

    let r2 = handle_bind_self(
        &home,
        &json!({"repository_path": shared_path, "branch": "feat/cross"}),
        &sender_for("agent-B"),
    );
    assert!(
        r2.get("error").is_some(),
        "B must be rejected on the same (repo, branch): {r2}"
    );
    assert_eq!(
        r2["code"].as_str(),
        Some("cross_agent_conflict"),
        "code must be cross_agent_conflict: {r2}"
    );

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn bind_self_then_release_worktree_clean_state() {
    // Gate 5: lifecycle round-trip. bind_self creates state;
    // release_worktree clears it. binding.json + worktree dir +
    // .agend-managed marker all gone after release.
    let home = std::env::temp_dir().join(format!("agend-p17-self-{}-cycle", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    p17_setup_repo(&home, "agent-cycle");

    let resp = handle_bind_self(
        &home,
        &json!({"repository": "owner/name", "branch": "feat/cycle"}),
        &sender_for("agent-cycle"),
    );
    assert_eq!(resp["bound"].as_bool(), Some(true));
    let worktree_path = resp["worktree_path"]
        .as_str()
        .expect("worktree path")
        .to_string();
    let binding = home
        .join("runtime")
        .join("agent-cycle")
        .join("binding.json");
    assert!(binding.exists());

    let release = handle_release_worktree(&home, &json!({"instance": "agent-cycle"}), &None);
    assert_eq!(
        release["released"].as_bool(),
        Some(true),
        "release must succeed: {release}"
    );

    assert!(!binding.exists(), "binding.json must be gone after release");
    assert!(
        !std::path::Path::new(&worktree_path).exists(),
        "worktree dir must be gone after release: {worktree_path}"
    );

    std::fs::remove_dir_all(&home).ok();
}
