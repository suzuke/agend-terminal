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
    std::fs::write(
        dir.join(".agend-managed"),
        format!("agent={agent}\nbranch={branch}\n"),
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
    let result = handle_release_worktree(
        &home,
        &json!({"instance": "dev", "branch": "feature/never-existed", "force": true}),
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

/// Regression anchor: unlike `force:false` (which refuses to remove a
/// dir lacking the `.agend-managed` marker), `force:true` cleans it
/// unconditionally — this is the stale-state recovery semantics the
/// path exists for, and merging must not accidentally add the marker
/// check here.
#[test]
fn force_deletes_dir_without_marker() {
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
    assert_eq!(result["dir_removed"].as_bool(), Some(true), "{result}");
    assert!(!dir.exists(), "force:true must remove a dir with no marker");
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
