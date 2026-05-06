// ── Sprint 53 P0-1: dispatch_auto_bind_lease tests ───────────────
// These call the PRODUCTION function directly (§1.4 compliance).

fn setup_test_repo(home: &std::path::Path, agent: &str) -> std::path::PathBuf {
    let repo = home.join("workspace").join(agent);
    std::fs::create_dir_all(&repo).ok();
    std::process::Command::new("git")
        .args(["init"])
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
    // Write fleet.yaml so resolve_instance works.
    std::fs::write(
        home.join("fleet.yaml"),
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

    let binding_path = home.join("runtime").join("test-agent").join("binding.json");
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
    assert!(err.contains("E4.5"), "error must mention E4.5: {err}");

    // No binding should exist.
    let binding_path = home.join("runtime").join("test-agent").join("binding.json");
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
            home.join("fleet.yaml"),
            format!("instances:\n  agent-a:\n    backend: claude\n    working_directory: {}\n  agent-b:\n    backend: claude\n    working_directory: {}\n",
                home.join("workspace").join("agent-a").display(),
                home.join("workspace").join("agent-b").display()),
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
        err.contains("already leased by 'agent-a'"),
        "error must name leasing agent: {err}"
    );

    // Agent-b binding must NOT exist (Q2: REJECT = no side effects).
    let binding_b = home.join("runtime").join("agent-b").join("binding.json");
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
    let binding = home.join("runtime").join("agent-x").join("binding.json");
    let content = std::fs::read_to_string(&binding).expect("read");
    let v: serde_json::Value = serde_json::from_str(&content).expect("parse");
    assert_eq!(v["task_id"], "T-2", "task_id must update on re-dispatch");

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn bind_file_error_stays_graceful() {
    // Q1: bind file write error → dispatch still succeeds (graceful).
    // Inject error by making runtime/<agent> a regular file (not dir).
    let home = std::env::temp_dir().join(format!("agend-s53-prod-{}-graceful", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    setup_test_repo(&home, "test-agent");

    // Block bind_full by creating runtime/test-agent as a file (not dir).
    let runtime_parent = home.join("runtime");
    std::fs::create_dir_all(&runtime_parent).ok();
    let runtime_agent = runtime_parent.join("test-agent");
    std::fs::write(&runtime_agent, "blocking file").ok();

    // Lease should succeed, bind write should fail gracefully (Q1).
    let result = super::dispatch_auto_bind_lease(&home, "test-agent", "T-1", "feat/graceful", None);
    assert!(
        result.is_ok(),
        "Q1: bind file error stays graceful, dispatch succeeds: {:?}",
        result.err()
    );

    // Worktree was created (lease succeeded).
    let wt = home
        .join("workspace")
        .join("test-agent")
        .join(".worktrees")
        .join("test-agent");
    assert!(wt.exists(), "worktree must be created even if bind fails");

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
        "target_instance": "target-agent",
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

    let repo = home.join("workspace").join("agent-a");
    std::fs::write(
            home.join("fleet.yaml"),
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
        "target_instance": "agent-b",
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

// ── P0-1.6: same agent + different branch must reject ────────────
//
// Pre-fix scenario: agent-x leased feat/A, then operator (or another
// dispatcher) sent a second task with feat/B. worktree::create silently
// reused the existing .worktrees/agent-x dir and echoed feat/B back as
// the lease branch. dispatch_auto_bind_lease saw Ok and proceeded; the
// smoke message landed in agent-x's inbox even though the worktree was
// still on feat/A.
//
// Post-fix: worktree::create runs `git branch --show-current` on the
// existing dir; mismatch returns None → lease fails → dispatch rejects
// with "dispatch rejected: ..." and the message never reaches the inbox.

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
    let binding = home.join("runtime").join("agent-x").join("binding.json");
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
        "target_instance": "agent-x",
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
    let watch_path = home.join("ci-watches").join(&filename);
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
    let binding_path = home.join("runtime").join("test-agent").join("binding.json");
    assert!(
        binding_path.exists(),
        "binding.json must exist (lease succeeded)"
    );

    // But ci-watches dir must be empty / non-existent (no auto-watch fired).
    let ci_dir = home.join("ci-watches");
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
    let watch_path = home.join("ci-watches").join(&filename);
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
    let ci_dir = home.join("ci-watches");
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
        "target_instance": "target-agent",
        "task": "implement feature X",
        "task_id": "T-100",
        "branch": "feat/p02-integration",
        "repo": "owner/repo",
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
    let watch_path = home.join("ci-watches").join(&filename);
    assert!(
        watch_path.exists(),
        "handle_delegate_task end-to-end must create ci-watches entry. \
             Path: {} — this is the Hotfix C non-fire regression check.",
        watch_path.display()
    );

    std::fs::remove_dir_all(&home).ok();
}
