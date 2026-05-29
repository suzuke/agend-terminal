// ── Sprint 53 P0-1: dispatch_auto_bind_lease tests ───────────────
// These call the PRODUCTION function directly (§1.4 compliance).

fn setup_test_repo(home: &std::path::Path, agent: &str) -> std::path::PathBuf {
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
    // #781 Phase 3 r1 (Path A — strict mode): pre-#781 fixtures relied
    // on `worktree::create -b` for missing-branch creation. With #781's
    // strict `ensure_branch_exists`, dispatch_auto_bind_lease creates
    // the branch from `origin/main` BEFORE lease. Legacy local-only
    // fixtures must register an origin URL + populate
    // `refs/remotes/origin/main` so the fast path resolves without
    // network I/O. `file:///dev/null/agend-fixture` chosen so
    // `parse_github_owner_repo` returns None — preserves the pre-#781
    // assertion in `delegate_task_without_repo_no_ci_watch` that no
    // ci-watch fires when repo is undeterminable. Tests that need
    // github-style ci-watch arming (e.g. `…_preserves_p0b_tail_ops`)
    // use a separate fixture with `https://github.com/...` URL.
    let _ = std::process::Command::new("git")
        .args([
            "remote",
            "add",
            "origin",
            "file:///dev/null/agend-fixture-no-derive",
        ])
        .current_dir(&repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output();
    let main_sha = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    if !main_sha.is_empty() {
        let _ = std::process::Command::new("git")
            .args(["update-ref", "refs/remotes/origin/main", &main_sha])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output();
    }
    // Write fleet.yaml so resolve_instance works.
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

#[test]
fn dispatch_with_branch_creates_binding_and_worktree() {
    let home = std::env::temp_dir().join(format!("agend-s53-prod-{}-bind", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    setup_test_repo(&home, "test-agent");

    // Call PRODUCTION function.
    let result = super::dispatch_auto_bind_lease(&home, "test-agent", "T-100", "feat/test", None);
    assert!(result.is_ok(), "dispatch must succeed: {:?}", result.err());

    let binding_path = crate::paths::runtime_dir(&home)
        .join("test-agent")
        .join("binding.json");
    assert!(binding_path.exists(), "binding.json must exist");
    let content = std::fs::read_to_string(&binding_path).expect("read");
    let v: serde_json::Value = serde_json::from_str(&content).expect("parse");
    assert_eq!(v["branch"], "feat/test");
    assert_eq!(v["task_id"], "T-100");

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn main_branch_rejects_dispatch() {
    let home = std::env::temp_dir().join(format!("agend-s53-prod-{}-main", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    setup_test_repo(&home, "test-agent");

    // Call PRODUCTION function — must reject main branch.
    let result = super::dispatch_auto_bind_lease(&home, "test-agent", "T-1", "main", None);
    assert!(result.is_err(), "main branch must REJECT");
    let err = result.expect_err("must be error");
    assert!(
        err.message.contains("E4.5"),
        "error must mention E4.5: {err:?}"
    );

    // No binding should exist.
    let binding_path = crate::paths::runtime_dir(&home)
        .join("test-agent")
        .join("binding.json");
    assert!(
        !binding_path.exists(),
        "rejected dispatch must not create binding"
    );

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn lease_conflict_rejects_dispatch() {
    let home = std::env::temp_dir().join(format!("agend-s53-prod-{}-conflict", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    // CRITICAL: each agent has its OWN working_directory (production topology).
    setup_test_repo(&home, "agent-a");
    setup_test_repo(&home, "agent-b");
    std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            format!("instances:\n  agent-a:\n    backend: claude\n    working_directory: {}\n  agent-b:\n    backend: claude\n    working_directory: {}\n",
                crate::paths::workspace_dir(&home).join("agent-a").display(),
                crate::paths::workspace_dir(&home).join("agent-b").display()),
        ).ok();

    // First dispatch succeeds in agent-a's clone.
    let r1 = super::dispatch_auto_bind_lease(&home, "agent-a", "T-1", "feat/shared", None);
    assert!(r1.is_ok(), "first dispatch must succeed: {:?}", r1.err());

    // Second dispatch SAME branch DIFFERENT agent → REJECT via central registry.
    let r2 = super::dispatch_auto_bind_lease(&home, "agent-b", "T-2", "feat/shared", None);
    assert!(
        r2.is_err(),
        "central registry must REJECT cross-agent same-branch, got: {:?}",
        r2
    );
    let err = r2.expect_err("must err");
    assert!(
        err.message.contains("already leased by 'agent-a'"),
        "error must name leasing agent: {err:?}"
    );

    // Agent-b binding must NOT exist (Q2: REJECT = no side effects).
    let binding_b = crate::paths::runtime_dir(&home)
        .join("agent-b")
        .join("binding.json");
    assert!(
        !binding_b.exists(),
        "rejected dispatch must not create binding"
    );

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn same_agent_re_dispatch_idempotent() {
    let home = std::env::temp_dir().join(format!("agend-s53-prod-{}-idem", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    setup_test_repo(&home, "agent-x");

    // First dispatch.
    let r1 = super::dispatch_auto_bind_lease(&home, "agent-x", "T-1", "feat/test", None);
    assert!(r1.is_ok(), "first dispatch must succeed: {:?}", r1.err());

    // Same agent re-dispatch same branch with different task_id → idempotent.
    let r2 = super::dispatch_auto_bind_lease(&home, "agent-x", "T-2", "feat/test", None);
    assert!(
        r2.is_ok(),
        "same-agent re-dispatch must be idempotent: {:?}",
        r2.err()
    );

    // Binding should exist with new task_id.
    let binding = crate::paths::runtime_dir(&home)
        .join("agent-x")
        .join("binding.json");
    let content = std::fs::read_to_string(&binding).expect("read");
    let v: serde_json::Value = serde_json::from_str(&content).expect("parse");
    assert_eq!(v["task_id"], "T-2", "task_id must update on re-dispatch");

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn bind_file_error_rolls_back_worktree() {
    // #1310: bind file write error → worktree is rolled back (no orphan).
    // Inject error by making runtime/<agent> a regular file (not dir).
    let home = std::env::temp_dir().join(format!("agend-s53-prod-{}-graceful", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    setup_test_repo(&home, "test-agent");

    // Block bind_full by creating runtime/test-agent as a file (not dir).
    let runtime_parent = crate::paths::runtime_dir(&home);
    std::fs::create_dir_all(&runtime_parent).ok();
    let runtime_agent = runtime_parent.join("test-agent");
    std::fs::write(&runtime_agent, "blocking file").ok();

    // Lease succeeds but bind_full fails → error returned + worktree rolled back.
    let result = super::dispatch_auto_bind_lease(&home, "test-agent", "T-1", "feat/graceful", None);
    let err = result.expect_err("dispatch must return Err when bind_full fails (#1324)");
    assert_eq!(err.code, super::ErrorCode::BindFailed);
    assert_eq!(err.stage, super::Stage::Bind);

    // #1310: worktree should NOT exist after rollback.
    let wt = home
        .join("worktrees")
        .join("test-agent")
        .join("feat/graceful");
    assert!(
        !wt.exists(),
        "worktree must be rolled back when bind fails (no orphan)"
    );

    std::fs::remove_dir_all(&home).ok();
}

// (Earlier "does_not_deliver_to_inbox" tests deleted — superseded by the
// delegate_task integration tests below, which exercise the production
// dispatch path and are regression-proof against gate-after-send.)

// ── Integration tests: delegate_task ordering proof ──────────────
//
// These call the production task-dispatch entry point
// (`handle_delegate_task`) — the same function MCP `send` with
// `request_kind: "task"` lands in via `handle_unified_send`.
//
// Regression-proof property: the lease gate sits BEFORE the
// `api::call(SEND)` block. When the gate trips (E4.5 main-branch
// rejection or LeaseError::Conflict), `handle_delegate_task` must
// return an error AND must not deliver the `[delegate_task] ...`
// message to the target's inbox via the fallback_deliver path.
//
// If the gate is moved back after the SEND block, the api::call
// returns `Err` in test (no daemon) and `fallback_deliver` writes
// the rendered `[delegate_task] implement feature X` line to
// `inbox/<target>.jsonl`. Both assertions below trip in that case
// (verified manually by removing the gate and re-running).

#[test]
fn delegate_task_main_branch_rejects_without_delivering() {
    use crate::identity::Sender;

    let home = std::env::temp_dir().join(format!(
        "agend-s53-integration-{}-main-order",
        std::process::id()
    ));
    std::fs::create_dir_all(&home).ok();
    setup_test_repo(&home, "target-agent");

    let args = serde_json::json!({
        "instance": "target-agent",
        "task": "implement feature X",
        "task_id": "T-999",
        "branch": "main",  // ← E4.5 rejection trigger
    });
    let sender = Some(Sender::new("lead").expect("sender"));

    let result = super::super::comms::handle_delegate_task(&home, &args, &sender);

    assert!(
        result.get("error").is_some(),
        "handle_delegate_task must return error when lease rejects main: {result}"
    );
    assert!(
        result["error"]
            .as_str()
            .unwrap_or("")
            .contains("dispatch rejected"),
        "error must indicate dispatch rejection (not generic): {result}"
    );

    let inbox_path = home.join("inbox").join("target-agent.jsonl");
    let inbox_content = std::fs::read_to_string(&inbox_path).unwrap_or_default();
    assert!(
        !inbox_content.contains("implement feature X"),
        "rejected dispatch must NOT deliver message to target inbox. Got: {inbox_content}"
    );

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn delegate_task_lease_conflict_rejects_without_delivering() {
    use crate::identity::Sender;

    let home = std::env::temp_dir().join(format!(
        "agend-s53-integration-{}-conflict-order",
        std::process::id()
    ));
    std::fs::create_dir_all(&home).ok();
    setup_test_repo(&home, "agent-a");

    let repo = crate::paths::workspace_dir(&home).join("agent-a");
    std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            // allow: shared-source_repo — this test exercises the central
            // lease registry's cross-agent collision rejection, which fires
            // precisely when two agents are bound to the same source repo
            // and dispatch the same branch. The shared `repo.display()` is
            // the bug condition under test, not an oversight; production
            // topology is one-clone-per-agent.
            format!("instances:\n  agent-a:\n    backend: claude\n    working_directory: {}\n  agent-b:\n    backend: claude\n    working_directory: {}\n", repo.display(), repo.display()),
        ).ok();

    // First lease seeds the worktree pool with feat/end2end for agent-a.
    let r1 = super::dispatch_auto_bind_lease(&home, "agent-a", "T-1", "feat/end2end", None);
    assert!(r1.is_ok(), "first lease must succeed: {r1:?}");

    let args = serde_json::json!({
        "instance": "agent-b",
        "task": "implement feature Y",
        "task_id": "T-2",
        "branch": "feat/end2end",
    });
    let sender = Some(Sender::new("lead").expect("sender"));

    let result = super::super::comms::handle_delegate_task(&home, &args, &sender);

    assert!(
        result.get("error").is_some(),
        "handle_delegate_task must return error on lease conflict: {result}"
    );
    assert!(
        result["error"]
            .as_str()
            .unwrap_or("")
            .contains("dispatch rejected"),
        "error must indicate dispatch rejection: {result}"
    );

    let inbox_path = home.join("inbox").join("agent-b.jsonl");
    let inbox_content = std::fs::read_to_string(&inbox_path).unwrap_or_default();
    assert!(
        !inbox_content.contains("implement feature Y"),
        "rejected dispatch must NOT deliver to agent-b inbox. Got: {inbox_content}"
    );

    std::fs::remove_dir_all(&home).ok();
}

// ── P0-1.6 + Sprint 57 Wave 4 (#546 Item 4): same agent + different ──
// branch must reject — across the architectural-layer shift.
//
// Pre-Wave-4 scenario: agent-x leased feat/A, then operator (or another
// dispatcher) sent a second task with feat/B. worktree::create silently
// reused the existing `.worktrees/agent-x` dir and echoed feat/B back as
// the lease branch. dispatch_auto_bind_lease saw Ok and proceeded; the
// smoke message landed in agent-x's inbox even though the worktree was
// still on feat/A.
//
// P0-1.6 fix: worktree::create runs `git branch --show-current` on the
// existing dir; mismatch returns None → lease fails → dispatch rejects.
//
// Sprint 57 Wave 4 (#546 Item 4) architectural-layer shift: worktrees
// now live at `$AGEND_HOME/worktrees/<agent>/<branch>/` external to the
// source repo. With branch-segmented paths, each (agent, branch) pair
// occupies a DISTINCT path — so the P0-1.6 reuse-path-rejection guard
// at `worktree::create` no longer fires (different branch → different
// dir → no existing-dir-with-mismatch state to detect).
//
// The conflict semantic is preserved EXPLICITLY at the binding layer
// in `dispatch_auto_bind_lease_with_source` (Wave 4 PR #555):
//   if let Some(existing) = crate::binding::read(home, target) {
//       if existing.branch != requested_branch { return Err(...) }
//   }
// Same outcome (same-agent-different-branch dispatch rejects), different
// implementation. The existing tests below are agnostic to which layer
// enforces the guard — they pin the OUTCOME, not the mechanism — so they
// continue to pass post-Wave-4. Sprint 58 Wave 1 PR-3 (#15) adds
// explicit Wave-4-layer pins below to make the new dispatch-layer check
// architecturally addressable for future maintainers.

#[test]
fn same_agent_different_branch_rejects() {
    let home = std::env::temp_dir().join(format!(
        "agend-p01-6-{}-same-agent-diff-branch",
        std::process::id()
    ));
    std::fs::create_dir_all(&home).ok();
    setup_test_repo(&home, "agent-x");

    // First lease establishes the worktree on feat/A.
    let r1 = super::dispatch_auto_bind_lease(&home, "agent-x", "T-1", "feat/A", None);
    assert!(r1.is_ok(), "first lease must succeed: {r1:?}");

    // Second dispatch with a DIFFERENT branch must reject.
    let r2 = super::dispatch_auto_bind_lease(&home, "agent-x", "T-2", "feat/B", None);
    assert!(
        r2.is_err(),
        "same-agent different-branch dispatch must reject (P0-1.6): {r2:?}"
    );

    // Binding still reflects feat/A (T-1) — the rejected dispatch must
    // not have overwritten it.
    let binding = crate::paths::runtime_dir(&home)
        .join("agent-x")
        .join("binding.json");
    let content = std::fs::read_to_string(&binding).expect("read binding");
    let v: serde_json::Value = serde_json::from_str(&content).expect("parse binding");
    assert_eq!(
        v["branch"], "feat/A",
        "rejected dispatch must NOT overwrite binding to feat/B"
    );
    assert_eq!(v["task_id"], "T-1", "task_id must remain T-1");

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn delegate_task_same_agent_different_branch_without_delivering() {
    use crate::identity::Sender;

    let home = std::env::temp_dir().join(format!(
        "agend-p01-6-integration-{}-same-agent-diff-branch",
        std::process::id()
    ));
    std::fs::create_dir_all(&home).ok();
    setup_test_repo(&home, "agent-x");

    // Seed: agent-x already leased on feat/A.
    let r1 = super::dispatch_auto_bind_lease(&home, "agent-x", "T-1", "feat/A", None);
    assert!(r1.is_ok(), "first lease must succeed: {r1:?}");

    // Production-realistic: a second delegate_task targeting agent-x with
    // a different branch must trip the gate before SEND, not deliver the
    // [delegate_task] message to the inbox.
    let args = serde_json::json!({
        "instance": "agent-x",
        "task": "implement feature B",
        "task_id": "T-2",
        "branch": "feat/B",
    });
    let sender = Some(Sender::new("lead").expect("sender"));
    let result = super::super::comms::handle_delegate_task(&home, &args, &sender);

    assert!(
        result.get("error").is_some(),
        "handle_delegate_task must error on same-agent different-branch: {result}"
    );
    assert!(
        result["error"]
            .as_str()
            .unwrap_or("")
            .contains("dispatch rejected"),
        "error must indicate dispatch rejection: {result}"
    );

    let inbox_path = home.join("inbox").join("agent-x.jsonl");
    let inbox_content = std::fs::read_to_string(&inbox_path).unwrap_or_default();
    assert!(
        !inbox_content.contains("implement feature B"),
        "rejected dispatch must NOT deliver to agent-x inbox. Got: {inbox_content}"
    );

    std::fs::remove_dir_all(&home).ok();
}

// ── Sprint 53 P0-2: dispatch-time auto-watch_ci tests ───────────────
//
// Covers all three Hotfix C #451 deletion-proof paths per PLAN §6 R4:
// - explicit repo arg path (operator-side dispatch convention)
// - missing-repo path (graceful skip, no false watch entry)
// - idempotent re-dispatch path (poll state preservation)
// - production smoke gate via handle_delegate_task (agent-to-agent send equivalence)
//
// Regression-proof: comment out the auto-watch_ci block in
// `dispatch_auto_bind_lease` and `delegate_task_with_repo_creates_ci_watch`
// FAILS (no watch file). Restore → PASS. See commit message §regression-proof.

#[test]
fn delegate_task_with_repo_creates_ci_watch() {
    let home = std::env::temp_dir().join(format!("agend-s53-p02-{}-with-repo", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    setup_test_repo(&home, "test-agent");

    // Explicit repo arg → watch_ci must fire.
    let result = super::dispatch_auto_bind_lease(
        &home,
        "test-agent",
        "T-1",
        "feat/p02-with-repo",
        Some("owner/repo"),
    );
    assert!(result.is_ok(), "dispatch must succeed: {:?}", result.err());

    let filename = crate::daemon::ci_watch::watch_filename("owner/repo", "feat/p02-with-repo");
    let watch_path = crate::daemon::ci_watch::ci_watches_dir(&home).join(&filename);
    assert!(
        watch_path.exists(),
        "watch file must exist at {}",
        watch_path.display()
    );
    let watch: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&watch_path).expect("read watch"))
            .expect("parse watch");
    assert_eq!(watch["repo"], "owner/repo");
    assert_eq!(watch["branch"], "feat/p02-with-repo");
    assert_eq!(watch["instance"], "test-agent");

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn delegate_task_without_repo_no_ci_watch() {
    // No explicit repo arg AND test repo has no `origin` remote (setup_test_repo
    // never adds one) → derive_repo_from_remote returns None → no watch entry.
    // This is the graceful-skip path — better than writing a stale watch
    // the poller can't act on.
    let home = std::env::temp_dir().join(format!("agend-s53-p02-{}-no-repo", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    setup_test_repo(&home, "test-agent");

    let result =
        super::dispatch_auto_bind_lease(&home, "test-agent", "T-1", "feat/p02-no-repo", None);
    assert!(result.is_ok(), "dispatch must succeed: {:?}", result.err());

    // Bind/lease still happened (load-bearing).
    let binding_path = crate::paths::runtime_dir(&home)
        .join("test-agent")
        .join("binding.json");
    assert!(
        binding_path.exists(),
        "binding.json must exist (lease succeeded)"
    );

    // But ci-watches dir must be empty / non-existent (no auto-watch fired).
    let ci_dir = crate::daemon::ci_watch::ci_watches_dir(&home);
    let entries: Vec<_> = std::fs::read_dir(&ci_dir)
        .ok()
        .map(|rd| rd.flatten().collect())
        .unwrap_or_default();
    assert!(
        entries.is_empty(),
        "no watch must be created when repo undeterminable. Got: {:?}",
        entries.iter().map(|e| e.path()).collect::<Vec<_>>()
    );

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn delegate_task_idempotent_existing_watch() {
    // Re-dispatch on same agent + branch must reuse the existing watch entry.
    // The idempotent guard preserves poll state (last_run_id, head_sha,
    // last_polled_at) so an active poll loop isn't reset mid-flight.
    let home = std::env::temp_dir().join(format!("agend-s53-p02-{}-idem", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    setup_test_repo(&home, "test-agent");

    let r1 = super::dispatch_auto_bind_lease(
        &home,
        "test-agent",
        "T-1",
        "feat/p02-idem",
        Some("owner/repo"),
    );
    assert!(r1.is_ok(), "first dispatch must succeed: {:?}", r1.err());

    let filename = crate::daemon::ci_watch::watch_filename("owner/repo", "feat/p02-idem");
    let watch_path = crate::daemon::ci_watch::ci_watches_dir(&home).join(&filename);
    assert!(watch_path.exists(), "first dispatch must create watch");

    // Mutate watch state to simulate an in-flight poll, then re-dispatch.
    let mut state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&watch_path).expect("read")).unwrap();
    state["last_run_id"] = serde_json::json!(42);
    state["head_sha"] = serde_json::json!("abc123");
    std::fs::write(&watch_path, serde_json::to_string_pretty(&state).unwrap()).unwrap();

    let r2 = super::dispatch_auto_bind_lease(
        &home,
        "test-agent",
        "T-2",
        "feat/p02-idem",
        Some("owner/repo"),
    );
    assert!(r2.is_ok(), "re-dispatch must succeed: {:?}", r2.err());

    // ci-watches dir must still contain exactly one entry.
    let ci_dir = crate::daemon::ci_watch::ci_watches_dir(&home);
    let entry_count = std::fs::read_dir(&ci_dir)
        .expect("read ci-watches")
        .flatten()
        .count();
    assert_eq!(entry_count, 1, "must remain exactly one watch entry");

    // Poll state must be preserved (idempotent, not overwritten).
    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&watch_path).expect("read")).unwrap();
    assert_eq!(
        after["last_run_id"], 42,
        "re-dispatch must NOT reset poll state"
    );
    assert_eq!(after["head_sha"], "abc123");

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn delegate_task_with_repo_creates_ci_watch_via_handle_delegate_task() {
    // Production smoke gate (§5): exercise the actual dispatch entry point
    // — `handle_delegate_task`, the same function MCP `send` with
    // `request_kind: "task"` lands in via `handle_unified_send`. This is
    // the regression-proof check against the failure mode the smoke test
    // caught: lead-to-dev `send` with branch produced no ci-watches entry.
    use crate::identity::Sender;

    let home = std::env::temp_dir().join(format!(
        "agend-s53-p02-integration-{}-with-repo",
        std::process::id()
    ));
    std::fs::create_dir_all(&home).ok();
    setup_test_repo(&home, "target-agent");

    let args = serde_json::json!({
        "instance": "target-agent",
        "task": "implement feature X",
        "task_id": "T-100",
        "branch": "feat/p02-integration",
        "repository": "owner/repo",
    });
    let sender = Some(Sender::new("lead").expect("sender"));

    let result = super::super::comms::handle_delegate_task(&home, &args, &sender);

    // Dispatch should NOT carry the lease-rejection error path.
    if let Some(err) = result.get("error").and_then(|v| v.as_str()) {
        // The api::call(SEND) returns Err in test (no daemon), but
        // fallback_deliver writes to inbox and the wrapper still reports OK
        // via the dispatch_tracking branch. The lease itself must succeed.
        assert!(
            !err.contains("dispatch rejected"),
            "lease must not reject in this scenario: {err}"
        );
    }

    // Production-smoke assertion: ci-watches entry must exist post-dispatch.
    let filename = crate::daemon::ci_watch::watch_filename("owner/repo", "feat/p02-integration");
    let watch_path = crate::daemon::ci_watch::ci_watches_dir(&home).join(&filename);
    assert!(
        watch_path.exists(),
        "handle_delegate_task end-to-end must create ci-watches entry. \
             Path: {} — this is the Hotfix C non-fire regression check.",
        watch_path.display()
    );

    std::fs::remove_dir_all(&home).ok();
}

// ─────────────────────────────────────────────────────────────────
// Sprint 58 Wave 1 PR-3 (#15) — explicit post-Wave-4 dispatch-layer
// guard pins.
//
// Wave 4 (#546 Item 4 / PR #555) shifted the same-agent-different-
// branch conflict guard from worktree::create's reuse-path
// rejection (P0-1.6 era) to dispatch_auto_bind_lease_with_source's
// binding::read branch-mismatch check. The P0-1.6 tests above
// continue to pass because they pin the OUTCOME, but the IMPLEMENT-
// ATION-layer pin is missing. These tests close that audit gap.
// ─────────────────────────────────────────────────────────────────

#[test]
fn dispatch_auto_bind_lease_rejects_same_agent_different_branch_post_wave_4() {
    // Architectural pin: post-Wave-4 the conflict guard fires at
    // the BINDING layer (binding::read branch mismatch), NOT at the
    // worktree::create reuse-path. The two layers produce the same
    // outcome but the regression-proof is layer-specific.
    //
    // If a future refactor were to remove the binding-layer check
    // assuming worktree::create still has it, this test would fail
    // catastrophically because Wave 4's branch-segmented paths put
    // each (agent, branch) at a DISTINCT location — so the
    // worktree::create guard CANNOT fire (no existing-dir for
    // feat/B when feat/A is bound, they're separate dirs).
    let home = std::env::temp_dir().join(format!(
        "agend-s58-w1pr3-{}-binding-layer-pin",
        std::process::id()
    ));
    std::fs::create_dir_all(&home).ok();
    setup_test_repo(&home, "agent-y");

    // Lease feat/A — establishes binding.json + worktree at
    // <home>/worktrees/agent-y/feat/A/ per Wave 4 layout.
    let r1 = super::dispatch_auto_bind_lease(&home, "agent-y", "T-1", "feat/A", None);
    assert!(r1.is_ok(), "first lease must succeed: {r1:?}");

    // Pre-Wave-4 was a single-path-per-agent layout, so a second
    // lease on feat/B would have hit worktree::create's existing-
    // dir guard. Post-Wave-4, feat/B's path is DIFFERENT from
    // feat/A's path — the worktree::create layer can't see the
    // conflict. The dispatch layer's binding::read check is what
    // catches it.
    let r2 = super::dispatch_auto_bind_lease(&home, "agent-y", "T-2", "feat/B", None);
    assert!(
        r2.is_err(),
        "Wave 4 architectural pin: dispatch-layer guard MUST reject \
         same-agent-different-branch even though worktree paths are now distinct: {r2:?}"
    );

    // Error message must mention the rejection cause for operator
    // diagnostics — preserves the human-readable error contract
    // across the architectural shift.
    let err = r2.unwrap_err();
    assert!(
        err.message.contains("agent-y") && err.message.contains("feat/A"),
        "rejection error must mention the existing binding's agent + branch: {err:?}"
    );

    // Binding still reflects feat/A — the rejected dispatch must
    // NOT have overwritten it.
    let binding = crate::paths::runtime_dir(&home)
        .join("agent-y")
        .join("binding.json");
    let content = std::fs::read_to_string(&binding).expect("read binding");
    let v: serde_json::Value = serde_json::from_str(&content).expect("parse binding");
    assert_eq!(
        v["branch"], "feat/A",
        "rejected dispatch must NOT overwrite binding to feat/B"
    );

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn dispatch_auto_bind_lease_idempotent_same_agent_same_branch_post_wave_4() {
    // Confirmatory positive case: same agent + SAME branch must
    // remain idempotent across the architectural shift. Wave 4's
    // branch-segmented path puts the second-call's would-be
    // worktree at the SAME location as the first, so worktree::
    // create reuses it; the binding-layer check sees identical
    // branch, allows the dispatch through.
    let home = std::env::temp_dir().join(format!(
        "agend-s58-w1pr3-{}-idem-positive",
        std::process::id()
    ));
    std::fs::create_dir_all(&home).ok();
    setup_test_repo(&home, "agent-z");

    let r1 = super::dispatch_auto_bind_lease(&home, "agent-z", "T-1", "feat/idem", None);
    assert!(r1.is_ok(), "first lease must succeed: {r1:?}");

    // Same agent + same branch + new task_id — must be idempotent
    // (re-dispatch landing on the same binding).
    let r2 = super::dispatch_auto_bind_lease(&home, "agent-z", "T-2", "feat/idem", None);
    assert!(
        r2.is_ok(),
        "same-agent same-branch must remain idempotent post-Wave-4: {r2:?}"
    );

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn release_worktree_then_rebind_different_branch_succeeds_post_wave_4() {
    // Defensive bonus pin: after release_worktree, a subsequent
    // dispatch on a DIFFERENT branch must succeed (no orphan
    // binding blocks it). Wave 4 + Sprint 56 Track G's
    // unsubscribe_all_ci_watches_for_agent / release_full path
    // should fully clean up the agent's binding so a fresh
    // different-branch dispatch lands cleanly.
    //
    // This pins the release-then-rebind cycle that operators
    // use during Sprint-closeout transitions (e.g. release Track A
    // worktree, immediately bind Track B on a different branch).
    let home = std::env::temp_dir().join(format!(
        "agend-s58-w1pr3-{}-release-rebind",
        std::process::id()
    ));
    std::fs::create_dir_all(&home).ok();
    setup_test_repo(&home, "agent-cycle");

    // Initial bind to feat/A
    let r1 = super::dispatch_auto_bind_lease(&home, "agent-cycle", "T-1", "feat/A", None);
    assert!(r1.is_ok(), "initial lease must succeed: {r1:?}");

    // Release the worktree (mirrors operator's release_worktree
    // MCP tool call between sprints).
    let outcome = crate::worktree_pool::release_full(&home, "agent-cycle", false);
    assert!(outcome.released, "release_full must succeed: {outcome:?}");

    // Now re-bind to a DIFFERENT branch. Must succeed because the
    // prior binding is gone.
    let r2 = super::dispatch_auto_bind_lease(&home, "agent-cycle", "T-2", "feat/B", None);
    assert!(
        r2.is_ok(),
        "release-then-rebind cycle must succeed across different branches: {r2:?}"
    );

    // Binding now reflects feat/B (the fresh state).
    let binding = home
        .join("runtime")
        .join("agent-cycle")
        .join("binding.json");
    let content = std::fs::read_to_string(&binding).expect("read binding");
    let v: serde_json::Value = serde_json::from_str(&content).expect("parse binding");
    assert_eq!(
        v["branch"], "feat/B",
        "post-release rebind must establish new branch binding"
    );

    std::fs::remove_dir_all(&home).ok();
}

// ----------------------------------------------------------------------
// #781 dispatch_auto_bind_lease structural tests. Pins Piece 2 (Bug B,
// worktree::create exit-code-128 gate too strict) + Piece 6 (source_repo
// tier observability) + Piece 7 (structured DispatchOutcome).
//
// Source of truth: decision d-20260514124732379010-0 (amended) +
// d-20260514130311646510-1 (Piece 1 micro-amend).
//
// Empirical anchor (§3.10 red→green): comment out the Bug B fix in
// worktree.rs:228-230 OR revert the SourceRepoTier wiring in C4 →
// `dispatch_auto_bind_lease_with_pre_existing_branch_in_team_source_repo_succeeds_via_fallback`
// fails (at C3 HEAD both regressions still active: lease fails with exit
// 255 unmatched + source_repo_tier placeholder mismatches TeamSourceRepo).
//
// Cross-platform: all happy-path tests `#[cfg(unix)]` per §3.7 — Windows
// runner git-subprocess concurrency observed unstable, see #780/#778
// fixture history. Backlog D tracks Windows CI smoke test.
// ----------------------------------------------------------------------

/// Fixture: canonical repo + fleet.yaml team source_repo + simulated
/// `refs/remotes/origin/main`. Branch parameter pre-created on canonical
/// so `worktree::create`'s `-b` path hits the duplicate-branch fallback
/// (this is the Bug B trigger).
#[cfg(unix)]
fn p781_canonical_with_team_source_repo(
    parent: &std::path::Path,
    branch: &str,
    pre_create_branch: bool,
    team: &str,
    members: &[&str],
) -> (std::path::PathBuf, std::path::PathBuf) {
    let home = parent.join(format!(
        "home-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&home).ok();
    let canonical = parent.join(format!(
        "canonical-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&canonical).ok();
    let bypass = ("AGEND_GIT_BYPASS", "1");
    std::process::Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(&canonical)
        .env(bypass.0, bypass.1)
        .output()
        .unwrap();
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
        .current_dir(&canonical)
        .env(bypass.0, bypass.1)
        .output()
        .unwrap();
    // Register origin remote with a github-style URL so
    // `derive_repo_from_remote` resolves to `owner/repo` for the
    // ci_watches arming downstream — mirrors production canonicals.
    std::process::Command::new("git")
        .args([
            "remote",
            "add",
            "origin",
            "https://github.com/owner/repo.git",
        ])
        .current_dir(&canonical)
        .env(bypass.0, bypass.1)
        .output()
        .unwrap();
    // Simulate fetched origin/main so #781 ensure_branch_exists fast path
    // (zero network on `git branch X origin/main` where origin/main is
    // already a valid local ref) fires without needing real network I/O.
    let main_sha = String::from_utf8(
        std::process::Command::new("git")
            .args(["rev-parse", "main"])
            .current_dir(&canonical)
            .env(bypass.0, bypass.1)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    std::process::Command::new("git")
        .args(["update-ref", "refs/remotes/origin/main", &main_sha])
        .current_dir(&canonical)
        .env(bypass.0, bypass.1)
        .output()
        .unwrap();
    if pre_create_branch {
        std::process::Command::new("git")
            .args(["branch", branch, "main"])
            .current_dir(&canonical)
            .env(bypass.0, bypass.1)
            .output()
            .unwrap();
    }
    let members_yaml = members
        .iter()
        .map(|m| format!("      - {m}"))
        .collect::<Vec<_>>()
        .join("\n");
    let yaml = format!(
        "instances: {{}}\nteams:\n  {team}:\n    members:\n{members_yaml}\n    source_repo: {}\n",
        canonical.display()
    );
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
    (home, canonical)
}

#[test]
#[cfg(unix)]
fn dispatch_auto_bind_lease_with_pre_existing_branch_in_team_source_repo_succeeds_via_fallback() {
    // ANCHOR (§3.10 red→green). Pre-C4 HEAD this FAILs on:
    //   (a) `worktree::create` -b path returns exit 255 with stderr
    //       "already exists"; current exit-code-128 gate misses
    //       fallback → lease returns Err → dispatch surfaces
    //       Err(DispatchError { stage: WorktreeLeaseConflict }).
    //   (b) Even if lease succeeded, C2's placeholder
    //       `SourceRepoTier::Stub` mismatches the asserted
    //       `TeamSourceRepo` until C4 wires `resolve_source_repo`.
    // C4 fixes both: Bug B (stderr-substring fallback) + Piece 6
    // (tier wiring) → test PASSes.
    let parent = std::env::temp_dir().join(format!(
        "agend-p781-anchor-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let (home, canonical) = p781_canonical_with_team_source_repo(
        &parent,
        "feat/p781-anchor",
        true, // pre-create branch on canonical → triggers Bug B fallback path
        "val",
        &["val-dev"],
    );

    let result = super::dispatch_auto_bind_lease(&home, "val-dev", "T-1", "feat/p781-anchor", None);

    let outcome = result.expect("dispatch must succeed via stderr-substring fallback");
    assert_eq!(
        outcome.source_repo_tier,
        super::SourceRepoTier::TeamSourceRepo,
        "source_repo_tier must observe Tier 2.5 (team source_repo): {outcome:?}"
    );

    // Binding written with canonical source_repo (Tier 2.5 resolution
    // actually wins downstream — no Tier 4 stub).
    let binding = home.join("runtime").join("val-dev").join("binding.json");
    assert!(binding.exists(), "binding.json must be written");
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&binding).unwrap()).unwrap();
    let observed = std::path::PathBuf::from(v["source_repo"].as_str().unwrap());
    assert_eq!(
        observed.canonicalize().unwrap_or(observed.clone()),
        canonical.canonicalize().unwrap_or(canonical.clone()),
        "binding.source_repo must be canonical (Tier 2.5), not workspace stub"
    );

    std::fs::remove_dir_all(&parent).ok();
}

#[test]
#[cfg(unix)]
fn dispatch_auto_bind_lease_with_team_source_repo_missing_branch_auto_creates() {
    // Test 2: branch missing on canonical → ensure_branch_exists's fast
    // path (zero network on `git branch feat/X origin/main` where
    // origin/main is already locally populated) creates the branch
    // pre-lease. Observable via DispatchOutcome.auto_created_branch.
    let parent = std::env::temp_dir().join(format!(
        "agend-p781-auto-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let (home, _canonical) = p781_canonical_with_team_source_repo(
        &parent,
        "feat/p781-auto",
        false, // branch missing → auto-create path
        "val",
        &["val-dev"],
    );
    let result = super::dispatch_auto_bind_lease(&home, "val-dev", "T-1", "feat/p781-auto", None);
    let outcome = result.expect("dispatch must succeed");
    assert_eq!(
        outcome.source_repo_tier,
        super::SourceRepoTier::TeamSourceRepo
    );
    assert!(
        outcome.auto_created_branch,
        "branch missing pre-call must surface auto_created_branch=true: {outcome:?}"
    );
    assert!(
        !outcome.fetch_attempted,
        "origin/main pre-populated → no fetch fired: {outcome:?}"
    );
    std::fs::remove_dir_all(&parent).ok();
}

#[test]
#[cfg(unix)]
fn dispatch_auto_bind_lease_existing_branch_ignores_from_ref() {
    // Test 3: pre-existing branch short-circuits ensure_branch_exists
    // — from_ref is not consulted, neither create nor fetch fire.
    // Pins the back-compat path so callers that pre-create branches
    // (gh PR checkout, manual setup) get auto_created_branch=false.
    let parent = std::env::temp_dir().join(format!(
        "agend-p781-exist-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let (home, _canonical) = p781_canonical_with_team_source_repo(
        &parent,
        "feat/p781-exist",
        true, // pre-create
        "val",
        &["val-dev"],
    );
    let result = super::dispatch_auto_bind_lease(&home, "val-dev", "T-1", "feat/p781-exist", None);
    let outcome = result.expect("dispatch must succeed");
    assert!(
        !outcome.auto_created_branch,
        "pre-existing branch must NOT report auto-created: {outcome:?}"
    );
    assert!(
        !outcome.fetch_attempted,
        "pre-existing branch short-circuits before any fetch: {outcome:?}"
    );
    std::fs::remove_dir_all(&parent).ok();
}

#[test]
#[cfg(unix)]
fn dispatch_auto_bind_lease_invalid_from_ref_returns_structured_error() {
    // Test 4: ensure_branch_exists's `validate_branch(from_ref)` gate
    // rejects option-injection / charset-illegal `from_ref` values at
    // the daemon API boundary. The hard-coded production `from_ref` is
    // `origin/main` which always passes, so this test drives the
    // helper directly with a malicious value to pin the structured-
    // error surface.
    let parent = std::env::temp_dir().join(format!(
        "agend-p781-inv-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let (home, canonical) =
        p781_canonical_with_team_source_repo(&parent, "feat/p781-inv", false, "val", &["val-dev"]);
    // Drive ensure_branch_exists directly with an option-injection
    // attempt — `--upload-pack=...` fails `validate_branch`'s leading-`-`
    // guard.
    let err = super::ensure_branch_exists(
        &home,
        &canonical,
        "feat/p781-inv",
        "--upload-pack=/bin/sh",
        "val-dev",
    )
    .expect_err("validate_from_ref must reject option-injection");
    assert_eq!(err.code, super::ErrorCode::InvalidFromRef);
    assert_eq!(err.stage, super::Stage::ValidateFromRef);
    assert!(!err.fetch_attempted);
    std::fs::remove_dir_all(&parent).ok();
}

#[test]
#[cfg(unix)]
fn dispatch_auto_bind_lease_concurrent_different_targets_race_idempotent() {
    // Test 5: two concurrent dispatches on the SAME source_repo + SAME
    // branch + DIFFERENT targets (avoids per-target BindGuard rejection
    // which fires for same-target collisions).
    //
    // Race semantics being pinned (#781 Phase 3 r1, reviewer constraint #2
    // — distinguish `CreateBranch` stage from `WorktreeLeaseConflict`
    // stage in race-loser error path):
    //
    // - At least one caller succeeds (Ok). The other either also succeeds
    //   (if it observed the branch already existing AND grabbed a distinct
    //   worktree path) or errors. Worktree path is segmented per agent
    //   (`<home>/worktrees/<agent>/<branch>/`) so technically both could
    //   reuse the same branch — but `worktree::create`'s existing-branch
    //   guard rejects a second `worktree add <branch>` since git's
    //   per-branch single-checkout invariant holds.
    // - The loser, if it errored, MUST surface `Stage::WorktreeLeaseConflict`
    //   (the failure landed at the worktree-add step, after the race-
    //   absorbed `git branch` already-exists fall-through). It must NOT
    //   surface `Stage::CreateBranch` (= `git branch` non-`already-exists`
    //   failure) and must NOT carry `ErrorCode::BranchCreateFailed`.
    // - At most one Ok observes `auto_created_branch=true`. Branch
    //   creation race winner is determined by `git branch` ordering; the
    //   loser's `git branch` hits `already exists` stderr → fall-through
    //   with `auto_created_branch=false`. We don't require exactly-one-
    //   true: when the branch-create winner subsequently LOSES the
    //   worktree-add race, its `DispatchError` does not carry
    //   `auto_created_branch` (omitted from error shape per Piece 7
    //   schema), so the signal is observable only from the Ok variant.
    //   `at_most_one` is the strict invariant; `exactly_one` would flake.
    let parent = std::env::temp_dir().join(format!(
        "agend-p781-race-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let (home, _canonical) = p781_canonical_with_team_source_repo(
        &parent,
        "feat/p781-race",
        false,
        "val",
        &["val-dev-a", "val-dev-b"],
    );
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let mut handles = Vec::new();
    for target in ["val-dev-a", "val-dev-b"] {
        let barrier = std::sync::Arc::clone(&barrier);
        let home_c = home.clone();
        let target = target.to_string();
        // fire-and-forget: test-only race harness; JoinHandle stored in
        // `handles` and explicitly joined below.
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            super::dispatch_auto_bind_lease(&home_c, &target, "T-1", "feat/p781-race", None)
        }));
    }
    let outcomes: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    let ok_count = outcomes.iter().filter(|o| o.is_ok()).count();
    assert!(
        ok_count >= 1,
        "at least one race winner must succeed (branch-create race must absorb the loser via stderr fall-through): {outcomes:?}"
    );

    // Stage / code differentiation in error path (reviewer constraint #2):
    // race losers MUST land on WorktreeLeaseConflict, NEVER on CreateBranch.
    for err in outcomes.iter().filter_map(|o| o.as_ref().err()) {
        assert_eq!(
            err.stage,
            super::Stage::WorktreeLeaseConflict,
            "race loser must fail at WorktreeLeaseConflict stage (NOT CreateBranch — race must be absorbed at git-branch layer), got stage={:?}: {err:?}",
            err.stage
        );
        assert_ne!(
            err.code,
            super::ErrorCode::BranchCreateFailed,
            "race must NOT surface BranchCreateFailed — already-exists stderr is the fall-through signal: {err:?}"
        );
    }

    // At most one Ok caller observes auto_created_branch=true. When the
    // branch-create winner subsequently loses worktree-add, its
    // DispatchError omits auto_created_branch (per Piece 7 shape).
    let true_count = outcomes
        .iter()
        .filter(|r| matches!(r, Ok(o) if o.auto_created_branch))
        .count();
    assert!(
        true_count <= 1,
        "at most one Ok caller may report auto_created_branch=true: {outcomes:?}"
    );

    std::fs::remove_dir_all(&parent).ok();
}

#[test]
#[cfg(unix)]
fn dispatch_auto_bind_lease_stress_50_iter_branch_create_no_flaky_parse() {
    // Test 6: stress-loop 50 iterations of the missing-branch
    // auto-create path. Catches flaky stderr-substring matching across
    // git versions / locales (we rely on "already exists" / "is already
    // checked out" / "not a valid object name" — if git rewords any
    // the failure surfaces here, not in production). Each iter is a
    // fresh home + canonical.
    let parent = std::env::temp_dir().join(format!(
        "agend-p781-stress-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    for i in 0..50 {
        let sub = parent.join(format!("iter-{i}"));
        std::fs::create_dir_all(&sub).ok();
        let branch = format!("feat/p781-stress-{i}");
        let (home, _canonical) =
            p781_canonical_with_team_source_repo(&sub, &branch, false, "val", &["val-dev"]);
        let r = super::dispatch_auto_bind_lease(&home, "val-dev", "T-1", &branch, None);
        let outcome = r.expect("each iter must succeed");
        assert!(
            outcome.auto_created_branch,
            "iter {i}: fresh branch must report auto_created_branch=true"
        );
        assert!(
            !outcome.fetch_attempted,
            "iter {i}: origin/main pre-populated → no fetch"
        );
        std::fs::remove_dir_all(&home).ok();
    }
    std::fs::remove_dir_all(&parent).ok();
}

#[test]
#[cfg(unix)]
fn dispatch_auto_bind_lease_auto_create_path_preserves_p0b_tail_ops() {
    // Test 7: regression guard. Sprint 53 P0-1+P0-2 tail-ops (binding
    // write + ci_watches arming via derive_repo_from_remote) must STILL
    // fire when the auto-create path runs. Easy to regress if
    // ensure_branch_exists short-circuits the post-lease block.
    let parent = std::env::temp_dir().join(format!(
        "agend-p781-tail-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let (home, _canonical) =
        p781_canonical_with_team_source_repo(&parent, "feat/p781-tail", false, "val", &["val-dev"]);
    // Fixture's origin is `https://github.com/owner/repo.git` so
    // `derive_repo_from_remote_pub` resolves owner/repo for ci_watches.
    let r = super::dispatch_auto_bind_lease(&home, "val-dev", "T-77", "feat/p781-tail", None);
    let outcome = r.expect("dispatch must succeed");
    assert!(outcome.auto_created_branch);

    // binding.json present + branch + task_id
    let binding = home.join("runtime").join("val-dev").join("binding.json");
    assert!(binding.exists());
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&binding).unwrap()).unwrap();
    assert_eq!(v["branch"], "feat/p781-tail");
    assert_eq!(v["task_id"], "T-77");

    // ci_watches armed via owner/repo derivation
    let watch_path = crate::daemon::ci_watch::ci_watches_dir(&home).join(
        crate::daemon::ci_watch::watch_filename("owner/repo", "feat/p781-tail"),
    );
    assert!(
        watch_path.exists(),
        "ci_watches must be armed post-auto-create — derive_repo_from_remote produced owner/repo"
    );
    std::fs::remove_dir_all(&parent).ok();
}

#[test]
#[cfg(unix)]
fn dispatch_auto_bind_lease_sets_next_after_ci_from_dispatch_hook_931() {
    // #931 Fix 2 (H5a): the dispatch chain knows who the next agent in
    // the workflow is (e.g. lead dispatches dev with
    // `next_after_ci=reviewer`). Pre-#931, `dispatch_auto_bind_lease`
    // armed the ci-watch via `handle_watch_ci(repo, branch)` with no
    // `next_after_ci` arg, so the chain handoff `[ci-ready-for-action]`
    // never fired unless someone explicitly re-called `ci action=watch`
    // with the field set. Combined with the release-time subscriber
    // sweep (#931 Fix 1), this caused 4-in-a-row PR stalls.
    //
    // Post-#931, the new `dispatch_auto_bind_lease_with_chain` wrapper
    // propagates `next_after_ci` from the dispatcher down to the
    // auto-armed watch JSON. Reviewer (the chain target) receives
    // `[ci-ready-for-action]` on CI pass without any manual re-watch.
    //
    // REGRESSION-PROOF: revert Fix 2 (drop the next_after_ci wiring in
    // `_with_source` → `handle_watch_ci`) → this test FAILS because
    // `watch["next_after_ci"]` reads as `null` instead of `"reviewer"`.
    let parent = std::env::temp_dir().join(format!(
        "agend-931-h5a-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let (home, _canonical) = p781_canonical_with_team_source_repo(
        &parent,
        "feat/931-h5a-chain",
        true,
        "val",
        &["val-dev"],
    );

    // Lead-style dispatch: dev is the implementer, reviewer is the chain target.
    let r = super::dispatch_auto_bind_lease_with_chain(
        &home,
        "val-dev",
        "T-931-h5a",
        "feat/931-h5a-chain",
        None,
        Some("reviewer"),
    );
    assert!(
        r.is_ok(),
        "dispatch_auto_bind_lease_with_chain must succeed: {:?}",
        r.err()
    );

    let watch_path = crate::daemon::ci_watch::ci_watches_dir(&home).join(
        crate::daemon::ci_watch::watch_filename("owner/repo", "feat/931-h5a-chain"),
    );
    assert!(watch_path.exists(), "ci_watches armed by dispatch");

    let watch: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&watch_path).expect("read watch"))
            .expect("parse watch");
    assert_eq!(
        watch["next_after_ci"].as_str(),
        Some("reviewer"),
        "#931 Fix 2 (H5a) GREEN: dispatch chain MUST propagate next_after_ci into auto-armed watch JSON. Got: {watch}"
    );

    std::fs::remove_dir_all(&parent).ok();
}

#[test]
#[cfg(unix)]
fn dispatch_persists_task_id_into_ci_watch_sidecar_1031() {
    // #1031: when `send(kind=task, task_id=T)` triggers the dispatch
    // auto-arm, the task_id is persisted into the ci-watch sidecar's
    // `task_id` field. The ci_check_repo emit site reads it back to
    // enrich `[ci-ready-for-action]` payloads — closing the
    // dispatcher→reviewer back-link that pre-#1031 required manual
    // inbox-archaeology.
    //
    // REGRESSION-PROOF: revert the `watch_args["task_id"]` write in
    // dispatch_hook/mod.rs → this assertion FAILS because
    // `watch["task_id"]` reads as null instead of `"T-1031-persist"`.
    let parent = std::env::temp_dir().join(format!(
        "agend-1031-persist-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let (home, _canonical) = p781_canonical_with_team_source_repo(
        &parent,
        "feat/1031-persist",
        true,
        "val",
        &["val-dev", "val-reviewer"],
    );

    let r = super::dispatch_auto_bind_lease_with_chain(
        &home,
        "val-dev",
        "T-1031-persist",
        "feat/1031-persist",
        None,
        Some("val-reviewer"),
    );
    assert!(r.is_ok(), "dispatch must succeed: {:?}", r.err());

    let watch_path = crate::daemon::ci_watch::ci_watches_dir(&home).join(
        crate::daemon::ci_watch::watch_filename("owner/repo", "feat/1031-persist"),
    );
    let watch: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&watch_path).expect("read watch"))
            .expect("parse watch");
    assert_eq!(
        watch["task_id"].as_str(),
        Some("T-1031-persist"),
        "#1031: dispatch must persist task_id into ci-watch sidecar; got: {watch}"
    );

    std::fs::remove_dir_all(&parent).ok();
}

#[test]
#[cfg(unix)]
fn dispatch_auto_derives_next_after_ci_from_team_reviewer_convention_1037() {
    // #1037 RED→GREEN: when the dispatcher does NOT explicitly pass
    // `next_after_ci`, the daemon auto-derives the chain target from
    // the target's team using the `<team>-reviewer` naming convention.
    //
    // Empirical motivation (lead 2026-05-21): the post-#1030 wake-aware
    // fix never triggered on real fixup-team PRs because lead's
    // send(kind=task) MCP calls never set `next_after_ci=fixup-reviewer`
    // in the args. The wire path propagates the field correctly (per
    // `dispatch_auto_bind_lease_sets_next_after_ci_from_dispatch_hook_931`)
    // — the gap is the caller didn't know to set it. Auto-derive
    // closes the loop without requiring a per-call burden.
    //
    // REGRESSION-PROOF: revert the auto-derive lookup → this test
    // FAILS because `watch["next_after_ci"]` reads as `null` instead
    // of `"val-reviewer"` (the team's reviewer-suffixed member).
    let parent = std::env::temp_dir().join(format!(
        "agend-1037-auto-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let (home, _canonical) = p781_canonical_with_team_source_repo(
        &parent,
        "feat/1037-auto-derive",
        true,
        "val",
        // val-reviewer present in team → convention match.
        &["val-dev", "val-reviewer"],
    );

    // Dispatch WITHOUT explicit next_after_ci. Pre-#1037 this leaves
    // the sidecar with no next_after_ci field; post-#1037 the daemon
    // scans val-dev's team members, finds "val-reviewer" by the
    // `-reviewer` suffix convention, and propagates it.
    let r = super::dispatch_auto_bind_lease_with_chain(
        &home,
        "val-dev",
        "T-1037-auto",
        "feat/1037-auto-derive",
        None,
        None, // ← no explicit next_after_ci — the field this test pins.
    );
    assert!(
        r.is_ok(),
        "dispatch_auto_bind_lease_with_chain must succeed: {:?}",
        r.err()
    );

    let watch_path = crate::daemon::ci_watch::ci_watches_dir(&home).join(
        crate::daemon::ci_watch::watch_filename("owner/repo", "feat/1037-auto-derive"),
    );
    assert!(watch_path.exists(), "ci_watches armed by dispatch");

    let watch: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&watch_path).expect("read watch"))
            .expect("parse watch");
    assert_eq!(
        watch["next_after_ci"].as_str(),
        Some("val-reviewer"),
        "#1037 GREEN: dispatch must auto-derive next_after_ci from \
         `<team>-reviewer` convention when caller omits the arg. \
         Got: {watch}"
    );

    std::fs::remove_dir_all(&parent).ok();
}

#[test]
#[cfg(unix)]
fn dispatch_auto_derive_no_reviewer_in_team_leaves_next_after_ci_none_1037() {
    // #1037 negative case: if the target's team has NO member matching
    // the `<team>-reviewer` convention, auto-derive leaves
    // next_after_ci as None. This preserves the pre-#1037 behavior for
    // teams that don't follow the naming convention — no behavior
    // regression for legacy / atypical team layouts.
    let parent = std::env::temp_dir().join(format!(
        "agend-1037-no-reviewer-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let (home, _canonical) = p781_canonical_with_team_source_repo(
        &parent,
        "feat/1037-no-reviewer",
        true,
        "val",
        // No -reviewer member in team. Auto-derive should leave the
        // field absent rather than picking a non-reviewer member.
        &["val-dev", "val-lead"],
    );

    let r = super::dispatch_auto_bind_lease_with_chain(
        &home,
        "val-dev",
        "T-1037-no-reviewer",
        "feat/1037-no-reviewer",
        None,
        None,
    );
    assert!(r.is_ok(), "dispatch must succeed: {:?}", r.err());

    let watch_path = crate::daemon::ci_watch::ci_watches_dir(&home).join(
        crate::daemon::ci_watch::watch_filename("owner/repo", "feat/1037-no-reviewer"),
    );
    let watch: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&watch_path).expect("read watch"))
            .expect("parse watch");
    assert!(
        watch["next_after_ci"].as_str().is_none(),
        "#1037: team without `-reviewer` member must leave \
         next_after_ci unset; got: {watch}"
    );

    std::fs::remove_dir_all(&parent).ok();
}

#[test]
#[cfg(unix)]
fn dispatch_explicit_next_after_ci_overrides_auto_derive_1037() {
    // #1037 precedence: when caller explicitly passes next_after_ci,
    // that value wins over the auto-derive fallback. The auto-derive
    // is a fallback for the "caller didn't bother" case, not a
    // mandate that overrides explicit intent.
    let parent = std::env::temp_dir().join(format!(
        "agend-1037-override-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let (home, _canonical) = p781_canonical_with_team_source_repo(
        &parent,
        "feat/1037-override",
        true,
        "val",
        &["val-dev", "val-reviewer"],
    );

    // Explicit arg differs from the convention-match. Explicit wins.
    let r = super::dispatch_auto_bind_lease_with_chain(
        &home,
        "val-dev",
        "T-1037-override",
        "feat/1037-override",
        None,
        Some("val-lead"),
    );
    assert!(r.is_ok(), "dispatch must succeed: {:?}", r.err());

    let watch_path = crate::daemon::ci_watch::ci_watches_dir(&home).join(
        crate::daemon::ci_watch::watch_filename("owner/repo", "feat/1037-override"),
    );
    let watch: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&watch_path).expect("read watch"))
            .expect("parse watch");
    assert_eq!(
        watch["next_after_ci"].as_str(),
        Some("val-lead"),
        "#1037: explicit next_after_ci arg must override auto-derive; got: {watch}"
    );

    std::fs::remove_dir_all(&parent).ok();
}

#[test]
fn teams_json_migration_preserves_existing_fleet_yaml_source_repo() {
    // Test 9 (invariant guard, Piece 1): the migration must NOT
    // overwrite an existing fleet.yaml team entry — if operator hand-
    // edited a team into fleet.yaml with source_repo set, and
    // teams.json contains an entry with the same name (legacy holdover),
    // the migration short-circuit (`add_team_to_yaml` returns Ok(false)
    // on duplicate) must preserve the fleet.yaml source_repo.
    let parent = std::env::temp_dir().join(format!(
        "agend-p781-mig-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&parent).ok();

    // Pre-existing fleet.yaml with team `val` + source_repo set.
    let canonical = std::path::PathBuf::from("/tmp/p781-fake-canonical");
    let yaml = format!(
        "teams:\n  val:\n    members:\n      - val-dev\n    source_repo: {}\n",
        canonical.display()
    );
    std::fs::write(crate::fleet::fleet_yaml_path(&parent), yaml).unwrap();

    // Legacy teams.json with same team name, no source_repo field.
    let teams_json = parent.join("teams.json");
    std::fs::write(
        &teams_json,
        serde_json::to_string(&serde_json::json!({
            "teams": [{
                "name": "val",
                "members": ["val-dev"],
                "orchestrator": "val-dev",
                "description": null,
                "created_at": "2026-01-01T00:00:00Z"
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    crate::fleet::migrate_teams_json_to_yaml(&parent).expect("migration");

    // Verify fleet.yaml's source_repo survived.
    let post: crate::fleet::FleetConfig =
        crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&parent)).unwrap();
    let team = post.teams.get("val").expect("team val present");
    assert_eq!(
        team.source_repo.as_ref().map(|p| p.display().to_string()),
        Some(canonical.display().to_string()),
        "fleet.yaml-side source_repo must survive teams.json migration"
    );

    // teams.json renamed → .migrated
    assert!(parent.join("teams.json.migrated").exists());
    assert!(!teams_json.exists());

    std::fs::remove_dir_all(&parent).ok();
}

// ── #814 clean_empty_init_commits stale-rebase-merge recovery ──

/// Spawn a temp git repo + worktree pair scoped to `tag`. The repo
/// has an initial commit + `refs/remotes/origin/main` so the
/// helper's `git log origin/main..HEAD` query has a baseline. The
/// worktree branches off `main` so subsequent commits land in
/// `origin/main..HEAD`. Returns `(repo_dir, worktree_dir)`.
fn setup_repo_and_worktree(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let base = std::env::temp_dir().join(format!("agend-814-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&base).ok();
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).ok();
    let git_run = |dir: &std::path::Path, args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("AGEND_GIT_BYPASS", "1")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git ran")
    };
    git_run(&repo, &["init", "-b", "main"]);
    // #1452: disable git auto-gc/maintenance in the fixture repo. The SUT runs
    // a `git rebase` over 50+ loose-object commits; under llvm-cov's slow,
    // heavily-parallel CI run an auto-gc repack can fire mid-rebase and race
    // the revision walk's object reads → "revision walk setup failed: could
    // not read <sha>" (the recurring Coverage-job flake). Read from the common
    // config, so it also covers git invocations inside the worktree.
    git_run(&repo, &["config", "gc.auto", "0"]);
    git_run(&repo, &["config", "maintenance.auto", "false"]);
    // #814 r1: pin per-repo gitconfig so subprocesses spawned by the
    // SUT (which don't inherit our test env vars) still find an
    // identity. Without this, CI runners with no global gitconfig
    // abort `git rebase` at exit 128 "unable to auto-detect email
    // address" — the actual cause of the first CI fail post-#814.
    git_run(&repo, &["config", "user.name", "test"]);
    git_run(&repo, &["config", "user.email", "t@t"]);
    git_run(&repo, &["commit", "--allow-empty", "-m", "main: initial"]);
    let main_sha = String::from_utf8_lossy(&git_run(&repo, &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();
    git_run(
        &repo,
        &["update-ref", "refs/remotes/origin/main", &main_sha],
    );
    let worktree = base.join("wt");
    git_run(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "feature",
            &worktree.display().to_string(),
        ],
    );
    // #814 r1: pin worktree-level gitconfig too. The SUT's
    // `git rebase -i` runs inside the worktree and reads worktree
    // config FIRST — without this we get exit 128 even when the
    // repo dir has user.name set, because the worktree inherits
    // the bare repo `.git/worktrees/wt/config` which is empty by
    // default.
    git_run(&worktree, &["config", "user.name", "test"]);
    git_run(&worktree, &["config", "user.email", "t@t"]);
    (repo, worktree)
}

/// Interleave `n_inits` empty `init` commits with `n_real` commits
/// that actually modify a file, advancing the branch HEAD inside
/// `worktree`. Pattern alternates real / init / real / init ... so
/// the helper exercises the mixed-cleanup rebase path (not the
/// all-empty soft-reset shortcut).
fn create_interleaved_commit_chain(worktree: &std::path::Path, n_inits: usize, n_real: usize) {
    let git_run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(worktree)
            .env("AGEND_GIT_BYPASS", "1")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git ran")
    };
    let total = n_inits + n_real;
    let mut inits_remaining = n_inits;
    let mut reals_remaining = n_real;
    for i in 0..total {
        // Alternate: even index → init, odd → real (when both still available).
        let pick_init = if reals_remaining == 0 {
            true
        } else if inits_remaining == 0 {
            false
        } else {
            i % 2 == 0
        };
        if pick_init {
            git_run(&["commit", "--allow-empty", "-m", "init"]);
            inits_remaining -= 1;
        } else {
            let path = worktree.join(format!("real-{i}.txt"));
            std::fs::write(&path, format!("real commit {i}\n")).expect("write");
            git_run(&["add", &format!("real-{i}.txt")]);
            git_run(&["commit", "-m", &format!("real commit {i}")]);
            reals_remaining -= 1;
        }
    }
}

/// Read the worktree's actual `.git` dir (a worktree's `.git` is a
/// file pointing to `<repo>/.git/worktrees/<name>`).
fn worktree_gitdir(worktree: &std::path::Path) -> std::path::PathBuf {
    let dotgit = worktree.join(".git");
    if dotgit.is_dir() {
        return dotgit;
    }
    let content = std::fs::read_to_string(&dotgit).expect("read .git");
    let gitdir = content
        .lines()
        .find_map(|l| l.strip_prefix("gitdir: "))
        .expect("gitdir prefix");
    std::path::PathBuf::from(gitdir.trim())
}

/// Synthesize a malformed `.git/.../rebase-merge/` dir as if a
/// prior `git rebase -i` failed and `--abort` failed to recover
/// it. This is the exact stale-state that triggered #807's 3
/// consecutive `cleanup_init_commits` failures.
fn pre_poison_stale_rebase_merge(worktree: &std::path::Path) {
    let gitdir = worktree_gitdir(worktree);
    let rebase_merge = gitdir.join("rebase-merge");
    std::fs::create_dir_all(&rebase_merge).expect("create rebase-merge");
    // Minimal content git looks for to refuse a new rebase.
    std::fs::write(rebase_merge.join("head-name"), "refs/heads/feature").expect("write head-name");
    std::fs::write(rebase_merge.join("onto"), "deadbeef").expect("write onto");
    std::fs::write(rebase_merge.join("interactive"), "").expect("write interactive");
}

/// #814 r1 — CI root-cause analysis:
/// `clean_empty_init_commits` shells out to git without setting
/// `GIT_COMMITTER_NAME` / `GIT_COMMITTER_EMAIL`. The local dev
/// machine inherits the developer's global `~/.gitconfig` so
/// rebase succeeds. CI runners have no global gitconfig → rebase
/// aborts with exit 128 ("unable to auto-detect email address").
/// Same root cause across linux/macos/windows in the first CI run.
///
/// Fix: fixture pins per-repo AND per-worktree `user.name` +
/// `user.email` via `git config` so the SUT's subprocess reads
/// them from the worktree's `.git/config` regardless of env vars
/// or global config.
#[test]
fn clean_empty_init_commits_recovers_from_stale_rebase_merge_dir() {
    // #814 RED test: synthesize the exact failure state #807 hit —
    // 32 interleaved empty inits + 3 real commits + a leftover
    // `.git/.../rebase-merge/` dir from a prior failed cleanup.
    // Pre-fix: `git rebase -i` immediately errors with "rebase in
    // progress" → helper returns Err("...status 256"). Post-fix:
    // `clear_stale_rebase_state` removes the stale dir at entry,
    // letting the rebase proceed normally.
    let (_repo, worktree) = setup_repo_and_worktree("recover");
    create_interleaved_commit_chain(&worktree, 32, 3);
    pre_poison_stale_rebase_merge(&worktree);

    let result = super::clean_empty_init_commits(&worktree);
    assert!(
        result.is_ok(),
        "helper must auto-recover from stale rebase-merge dir, got: {result:?}"
    );
    let cleaned = result.unwrap();
    assert_eq!(
        cleaned, 32,
        "all 32 empty inits should be dropped, cleaned: {cleaned}"
    );
    // Post-condition: stale dir is gone (cleared by the helper +
    // by the successful rebase that followed).
    let gitdir = worktree_gitdir(&worktree);
    assert!(
        !gitdir.join("rebase-merge").exists(),
        "rebase-merge dir should be gone post-cleanup"
    );

    std::fs::remove_dir_all(_repo.parent().unwrap()).ok();
}

#[test]
fn clean_empty_init_commits_handles_50_interleaved_inits() {
    // #814 perf coverage: 50 inits + 4 real commits + threshold
    // warn fires (50 > 30). Must complete within a generous bound
    // (30s on slow CI machines) so the helper isn't accidentally
    // unbounded. Validates Cause 2 (30+ ceiling) is NOT a hard
    // ceiling — just a tracing warn signal.
    let (_repo, worktree) = setup_repo_and_worktree("high_count");
    create_interleaved_commit_chain(&worktree, 50, 4);

    let start = std::time::Instant::now();
    let result = super::clean_empty_init_commits(&worktree);
    let elapsed = start.elapsed();

    assert!(
        result.is_ok(),
        "50-init case must succeed (Cause 2 not a hard ceiling), got: {result:?}"
    );
    assert_eq!(result.unwrap(), 50, "all 50 inits should be dropped",);
    assert!(
        elapsed < std::time::Duration::from_secs(30),
        "high-count must complete in < 30s, took {elapsed:?}"
    );

    std::fs::remove_dir_all(_repo.parent().unwrap()).ok();
}

#[test]
fn clean_empty_init_commits_clears_stale_dir_even_when_no_inits_to_drop() {
    // #814 edge case: stale rebase-merge dir survives a prior
    // failed attempt, but the branch has since been fully cleaned
    // (or the operator pushed and there are no inits to drop).
    // The helper should still clear the stale dir defensively so
    // subsequent calls aren't blocked, even when there's no work
    // to do this cycle.
    let (_repo, worktree) = setup_repo_and_worktree("no_inits_stale");
    // No commits added beyond the worktree-creation point — but
    // worktree branched off main so origin/main..HEAD is empty.
    pre_poison_stale_rebase_merge(&worktree);
    let gitdir_before = worktree_gitdir(&worktree);
    assert!(
        gitdir_before.join("rebase-merge").exists(),
        "pre-condition: stale dir exists"
    );

    let result = super::clean_empty_init_commits(&worktree);
    assert!(
        result.is_ok(),
        "no-inits-with-stale-dir case must succeed, got: {result:?}"
    );
    assert_eq!(
        result.unwrap(),
        0,
        "zero inits to drop, but call must not error",
    );
    // Stale dir was cleared at entry even though nothing else ran.
    let gitdir = worktree_gitdir(&worktree);
    assert!(
        !gitdir.join("rebase-merge").exists(),
        "stale rebase-merge dir must be cleared even in no-op case"
    );

    std::fs::remove_dir_all(_repo.parent().unwrap()).ok();
}

// ── #822 cleanup_init_commits heartbeat synonym whitelist ──

/// Commit an empty-diff commit on `worktree`'s HEAD with `subject` as
/// the subject line and optional `body` as the commit body. Pinned
/// per-#814 r1 pattern (per-process author/committer env so CI runners
/// without a global gitconfig can still commit).
fn empty_commit(worktree: &std::path::Path, subject: &str, body: Option<&str>) {
    let mut args: Vec<&str> = vec!["commit", "--allow-empty", "-m", subject];
    if let Some(b) = body {
        args.push("-m");
        args.push(b);
    }
    let status = std::process::Command::new("git")
        .args(&args)
        .current_dir(worktree)
        .env("AGEND_GIT_BYPASS", "1")
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .status()
        .expect("git commit ran");
    assert!(status.success(), "empty_commit `{subject}` failed");
}

/// #822 C1 RED: synthesize an empty-body, empty-diff commit with
/// subject "initial" — the exact #820 stray case. Pre-fix the helper
/// only matches `"init"` so the commit is NOT dropped (returns 0).
/// Post-fix (C2) the whitelist expands to include `"initial"` and
/// the commit IS dropped (returns 1). Asserts the post-fix behavior;
/// fails RED at C1, flips GREEN at C2.
#[test]
fn clean_empty_init_commits_drops_initial_subject_with_empty_body() {
    let (_repo, worktree) = setup_repo_and_worktree("initial_red");
    empty_commit(&worktree, "initial", None);

    let result = super::clean_empty_init_commits(&worktree);
    assert!(
        result.is_ok(),
        "helper must run cleanly on `initial`-named commit, got: {result:?}"
    );
    let cleaned = result.unwrap();
    assert_eq!(
        cleaned, 1,
        "empty-body empty-diff `initial` commit must be dropped \
         (the #820 stray case); cleaned={cleaned}"
    );

    std::fs::remove_dir_all(_repo.parent().unwrap()).ok();
}

/// #822 C3 regression-proof: the body-emptiness gate must guard
/// `initial`-subject commits that carry real commit-body notes. This
/// is the load-bearing FP-class case for v1.5 (the gate also guards
/// future `wip`/`tmp` synonyms). Body present → KEEP, even with
/// empty diff and whitelisted subject.
#[test]
fn clean_empty_init_commits_keeps_initial_subject_with_body_notes() {
    let (_repo, worktree) = setup_repo_and_worktree("initial_body");
    empty_commit(&worktree, "initial", Some("real body notes — do not drop"));

    let result = super::clean_empty_init_commits(&worktree);
    assert!(result.is_ok(), "helper must succeed, got: {result:?}");
    assert_eq!(
        result.unwrap(),
        0,
        "`initial` with non-empty body must be kept (body gate guards FP class)",
    );

    std::fs::remove_dir_all(_repo.parent().unwrap()).ok();
}

/// #822 C3 behavior-change documentation: an `init`-subject empty-
/// diff commit with a non-empty commit body was DROPPED pre-#822
/// (the body was ignored), and is now KEPT (body gate added). This
/// is theoretical-only — zero historical occurrences in 64+ daemon-
/// generated `init` commits, all of which use
/// `commit --allow-empty -m "init"` with no `-m <body>`. This test
/// locks the new behavior so future helper edits can't quietly
/// reintroduce the body-ignore regression.
#[test]
fn clean_empty_init_commits_keeps_init_subject_with_body_notes() {
    let (_repo, worktree) = setup_repo_and_worktree("init_body");
    empty_commit(&worktree, "init", Some("operator-added body — do not drop"));

    let result = super::clean_empty_init_commits(&worktree);
    assert!(result.is_ok(), "helper must succeed, got: {result:?}");
    assert_eq!(
        result.unwrap(),
        0,
        "`init` with non-empty body must be kept post-#822 (behavior change)",
    );

    std::fs::remove_dir_all(_repo.parent().unwrap()).ok();
}

/// #822 C3 regression-proof: the canonical daemon-heartbeat case —
/// `init` subject with empty body AND empty diff — is still DROPPED
/// post-#822. Locks the existing behavior so the body-gate addition
/// can't break the production heartbeat-cleanup path that this
/// helper exists for in the first place.
#[test]
fn clean_empty_init_commits_still_handles_canonical_init_empty() {
    let (_repo, worktree) = setup_repo_and_worktree("init_canonical");
    empty_commit(&worktree, "init", None);

    let result = super::clean_empty_init_commits(&worktree);
    assert!(result.is_ok(), "helper must succeed, got: {result:?}");
    assert_eq!(
        result.unwrap(),
        1,
        "canonical empty `init` heartbeat must still be dropped (existing behavior)",
    );

    std::fs::remove_dir_all(_repo.parent().unwrap()).ok();
}

// ── #833 cleanup_init_commits trailer-whitelist body gate ──

/// #833 C3 regression-proof: the strip respects the
/// trailer-key-followed-by-colon contract. `Agend-Agent-Token: x`
/// must NOT be stripped (it's an operator-extended trailer key,
/// not the daemon's `Agend-Agent`). Locks the partial-prefix
/// guard against future scanner refactors.
#[test]
fn strip_known_trailers_does_not_match_partial_prefix_keys() {
    let body = "Agend-Agent-Token: secret\n\
                Agent-Agent-Plus: other\n\
                Real operator note line";
    let stripped = super::strip_known_trailers(body);
    // None of the lines look like `Agend-Agent:` (with colon directly
    // after the key) — all must survive.
    assert_eq!(
        stripped, body,
        "partial-prefix trailers must not be stripped, got: {stripped:?}"
    );
}

/// #833 C3 regression-proof: a heartbeat-style commit whose body
/// has DAEMON TRAILERS + REAL OPERATOR NOTES must be KEPT. The
/// strip removes only the whitelist lines; the surviving operator
/// content keeps the gate's "is empty" check from returning true.
/// This is the load-bearing safety case — operator-added content
/// stays intact even when daemon trailers are mixed in.
#[test]
fn clean_empty_init_commits_keeps_init_with_real_body_after_trailers() {
    let (_repo, worktree) = setup_repo_and_worktree("833_mixed_body");
    let mixed = "Operator commit notes here.\n\
                 More context paragraphs.\n\
                 \n\
                 Agend-Agent: dev833\n\
                 Agend-Task: t-...";
    empty_commit(&worktree, "init", Some(mixed));

    let result = super::clean_empty_init_commits(&worktree);
    assert!(result.is_ok(), "helper must succeed, got: {result:?}");
    assert_eq!(
        result.unwrap(),
        0,
        "`init` with real operator body must be KEPT even when daemon \
         trailers are present — strip removes only the trailer block",
    );

    std::fs::remove_dir_all(_repo.parent().unwrap()).ok();
}

/// #833 C3 regression-proof: unknown daemon-style trailer keys
/// (e.g., a future `Agend-Sprint:` that hasn't been added to
/// `KNOWN_TRAILER_KEYS` yet) must NOT be stripped. Conservative
/// default — extending the whitelist requires an explicit code
/// change (and ideally the synced-with-hook invariant that
/// lead's post-batch backlog tracks).
#[test]
fn clean_empty_init_commits_keeps_init_with_unknown_trailer_keys() {
    let (_repo, worktree) = setup_repo_and_worktree("833_unknown_trailer");
    // `Agend-Custom` is NOT in KNOWN_TRAILER_KEYS — must survive strip.
    let unknown = "Agend-Custom: something operator added";
    empty_commit(&worktree, "init", Some(unknown));

    let result = super::clean_empty_init_commits(&worktree);
    assert!(result.is_ok(), "helper must succeed, got: {result:?}");
    assert_eq!(
        result.unwrap(),
        0,
        "`init` with non-whitelisted trailer key must be KEPT — conservative default \
         until KNOWN_TRAILER_KEYS is explicitly extended",
    );

    std::fs::remove_dir_all(_repo.parent().unwrap()).ok();
}

/// #833 C1 RED: a heartbeat-style commit with ONLY daemon-injected
/// `Agend-*:` trailers in its body (the actual production state every
/// bound-worktree commit lands in post-hook) must be DROPPED by
/// `cleanup_init_commits`. Pre-#833 the body-emptiness gate (#822)
/// saw the trailer block as non-empty and kept the commit —
/// `repo cleanup_init_commits` was effectively a no-op on real
/// daemon-managed worktrees. The same FP class I surfaced during
/// #827 push prep.
///
/// Asserts the post-fix contract:
/// - 1 commit cleaned (the canonical heartbeat shape)
#[test]
fn clean_empty_init_commits_drops_init_with_only_daemon_trailers() {
    let (_repo, worktree) = setup_repo_and_worktree("833_trailer_only");
    let trailer_block = "Agend-Agent: dev833\n\
                         Agend-Task: t-20260515150751256952-0\n\
                         Agend-Branch: fix/833-cleanup-init-trailer-whitelist\n\
                         Agend-Issued-At: 2026-05-15T18:24:45+00:00";
    empty_commit(&worktree, "init", Some(trailer_block));

    let result = super::clean_empty_init_commits(&worktree);
    assert!(result.is_ok(), "helper must succeed, got: {result:?}");
    assert_eq!(
        result.unwrap(),
        1,
        "`init` with only Agend-* trailers in body must be dropped \
         (the operational case that motivated #833)",
    );

    std::fs::remove_dir_all(_repo.parent().unwrap()).ok();
}

// ── #869 ensure_branch_exists branch-exists path sync tests ────────

/// #869 fix: when `refs/heads/<branch>` already exists locally and
/// `refs/remotes/origin/<branch>` exists at a DIFFERENT SHA, the
/// branch-exists path must refresh the local ref to track the remote
/// before returning. Pre-fix this early-returned without syncing, so
/// the downstream `worktree::create` landed the bound worktree at the
/// stale local SHA (observed 3× in PR-B/PR-C/etc reviewer cycles).
#[test]
fn ensure_branch_exists_syncs_stale_local_to_origin() {
    let home = std::env::temp_dir().join(format!(
        "agend-869-sync-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&home).ok();
    let repo = setup_test_repo(&home, "dev-869");
    let branch = "fix/869-sync-fixture";

    let bypass = |args: &[&str]| -> std::process::Output {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .expect("git spawn")
    };
    // Create the local branch at the initial commit (the "stale" SHA).
    let stale_sha = String::from_utf8(bypass(&["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_string();
    assert!(bypass(&["branch", branch]).status.success());

    // Advance HEAD on main, then point refs/remotes/origin/<branch>
    // at the NEW SHA. This simulates "remote PR head moved forward but
    // local <branch> ref is pinned at the prior cycle's SHA".
    std::fs::write(repo.join("file.txt"), "advance").ok();
    assert!(bypass(&["add", "file.txt"]).status.success());
    assert!(bypass(&[
        "-c",
        "user.name=test",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "advance"
    ])
    .status
    .success());
    let fresh_sha = String::from_utf8(bypass(&["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_string();
    assert_ne!(stale_sha, fresh_sha, "fixture must produce divergent SHAs");
    let remote_ref = format!("refs/remotes/origin/{branch}");
    assert!(bypass(&["update-ref", &remote_ref, &fresh_sha])
        .status
        .success());

    // Sanity: pre-call local ref is still at stale_sha.
    let pre_local =
        String::from_utf8(bypass(&["rev-parse", &format!("refs/heads/{branch}")]).stdout)
            .unwrap()
            .trim()
            .to_string();
    assert_eq!(pre_local, stale_sha);

    // Drive the production function.
    let result = super::ensure_branch_exists(&home, &repo, branch, "origin/main", "dev-869");
    assert!(result.is_ok(), "must succeed: {result:?}");
    let (auto_created, _fetch_attempted) = result.unwrap();
    assert!(
        !auto_created,
        "branch existed pre-call — auto_created must be false"
    );

    // Post-call assertion: local ref now matches origin/<branch> (the
    // PR-head SHA). This is the load-bearing invariant the bug fix
    // restores.
    let post_local =
        String::from_utf8(bypass(&["rev-parse", &format!("refs/heads/{branch}")]).stdout)
            .unwrap()
            .trim()
            .to_string();
    assert_eq!(
        post_local, fresh_sha,
        "#869 contract: local refs/heads/{branch} must be fast-forwarded to refs/remotes/origin/{branch} when both exist"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// #869 edge case: branch exists locally but `refs/remotes/origin/
/// <branch>` does NOT exist (e.g. branch was created locally and never
/// pushed). The function must leave the local ref unchanged — there's
/// no remote ref to sync against — and return `(false, fetched_ok)`
/// without raising an error.
#[test]
fn ensure_branch_exists_leaves_local_untouched_when_no_remote_ref() {
    let home = std::env::temp_dir().join(format!(
        "agend-869-noremote-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&home).ok();
    let repo = setup_test_repo(&home, "dev-869-nr");
    let branch = "fix/869-noremote-fixture";

    let bypass = |args: &[&str]| -> std::process::Output {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .expect("git spawn")
    };
    // Create the local branch; deliberately do NOT populate any
    // refs/remotes/origin/<branch> ref.
    assert!(bypass(&["branch", branch]).status.success());
    let pre_local =
        String::from_utf8(bypass(&["rev-parse", &format!("refs/heads/{branch}")]).stdout)
            .unwrap()
            .trim()
            .to_string();

    let result = super::ensure_branch_exists(&home, &repo, branch, "origin/main", "dev-869-nr");
    assert!(result.is_ok(), "must succeed: {result:?}");
    let (auto_created, _fetch_attempted) = result.unwrap();
    assert!(!auto_created);

    // Post-call: local ref unchanged because no remote ref existed to
    // sync against. (The fetch itself may have spawned and either
    // succeeded with no work to do or failed against the fake remote;
    // the load-bearing invariant is "no silent ref mutation").
    let post_local =
        String::from_utf8(bypass(&["rev-parse", &format!("refs/heads/{branch}")]).stdout)
            .unwrap()
            .trim()
            .to_string();
    assert_eq!(
        pre_local, post_local,
        "#869: local ref must be untouched when origin/<branch> is absent"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// #869 protection: the new sync path must not interfere with the
/// existing "branch doesn't exist locally → create from origin/main"
/// flow. After the fix, dispatching a brand-new branch must still
/// auto-create from origin/main with the existing (auto_created=true,
/// fetch_attempted=false) shape on the fast path.
#[test]
fn ensure_branch_exists_auto_create_from_main_path_unchanged() {
    let home = std::env::temp_dir().join(format!(
        "agend-869-new-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&home).ok();
    let repo = setup_test_repo(&home, "dev-869-new");
    let branch = "fix/869-new-fixture";

    let result = super::ensure_branch_exists(&home, &repo, branch, "origin/main", "dev-869-new");
    assert!(result.is_ok(), "must succeed: {result:?}");
    let (auto_created, fetch_attempted) = result.unwrap();
    assert!(
        auto_created,
        "#869: new branch must still auto-create from origin/main"
    );
    assert!(
        !fetch_attempted,
        "fast path: origin/main is a valid local ref so no fetch needed"
    );

    std::fs::remove_dir_all(&home).ok();
}

// ── #942 canonicalize_repo_slug matrix ──
//
// Single source of truth for repo identity. Covers all 7 divergence
// forms enumerated in `/tmp/dialectic-942-dev-primary.md` §1, plus
// edge cases (empty, malformed, non-GitHub URL).

#[test]
fn canonicalize_repo_slug_collapses_all_known_divergence_forms() {
    let cases: &[(&str, Option<&str>)] = &[
        // bare slug — already canonical
        ("owner/repo", Some("owner/repo")),
        // `.git` suffix
        ("owner/repo.git", Some("owner/repo")),
        // casing
        ("Owner/Repo", Some("owner/repo")),
        ("OWNER/REPO", Some("owner/repo")),
        // whitespace (trim)
        (" owner/repo ", Some("owner/repo")),
        // trailing slash
        ("owner/repo/", Some("owner/repo")),
        // full HTTPS URL
        ("https://github.com/owner/repo", Some("owner/repo")),
        ("https://github.com/owner/repo.git", Some("owner/repo")),
        ("https://github.com/Owner/Repo", Some("owner/repo")),
        // HTTP vs HTTPS
        ("http://github.com/owner/repo", Some("owner/repo")),
        // SSH URL forms
        ("git@github.com:owner/repo.git", Some("owner/repo")),
        ("ssh://git@github.com/owner/repo", Some("owner/repo")),
        // malformed: missing repo part
        ("owner", None),
        // malformed: too many components
        ("owner/repo/extra", None),
        // empty
        ("", None),
        ("   ", None),
        // non-GitHub URL — must NOT canonicalize (returns None;
        // ci_watch only knows GitHub Actions polling)
        ("https://gitlab.com/owner/repo", None),
        ("https://bitbucket.org/owner/repo", None),
    ];
    for (input, expected) in cases {
        let got = super::canonicalize_repo_slug(input);
        assert_eq!(
            got.as_deref(),
            *expected,
            "canonicalize_repo_slug({input:?}) yielded {got:?}, expected {expected:?}"
        );
    }
}

#[test]
fn canonicalize_repo_slug_lowercase_is_load_bearing_for_identity() {
    // GitHub treats org/repo case-insensitively in routing. Two callers
    // supplying different cases must produce same canonical identity
    // for the watch_filename hash to converge.
    let a = super::canonicalize_repo_slug("Owner/Repo").unwrap();
    let b = super::canonicalize_repo_slug("owner/repo").unwrap();
    let c = super::canonicalize_repo_slug("OWNER/REPO").unwrap();
    assert_eq!(a, b);
    assert_eq!(b, c);
    assert_eq!(a, "owner/repo");
}
