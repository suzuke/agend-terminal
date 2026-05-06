//! Sprint 53 P0-1: dispatch hook — auto-bind + lease on delegate_task.
//!
//! Extracted from comms.rs to stay under 700 LOC file size invariant.

/// Sprint 53 P0-1: auto-bind + lease worktree on delegate_task dispatch.
///
/// Production call path: app::run_app / daemon::run → MCP tool call →
/// handle_send → is_task_kind → dispatch_auto_bind_lease.
///
/// Failure recovery per operator Q1+Q2+§3.3:
/// - Bind file write error: log warn, dispatch proceeds (Q1 graceful)
/// - Lease conflict: REJECT dispatch with explicit error (Q2)
/// - Lease creation fails: REJECT dispatch with explicit error (§3.3)
/// - Main branch rejected: REJECT dispatch (E4.5)
pub(crate) fn dispatch_auto_bind_lease(
    home: &std::path::Path,
    target: &str,
    task_id: &str,
    branch: &str,
) -> Result<(), String> {
    // Resolve source repo from target agent's working directory.
    let source_repo = crate::fleet::FleetConfig::load(&home.join("fleet.yaml"))
        .ok()
        .and_then(|f| f.resolve_instance(target))
        .and_then(|r| r.working_directory)
        .unwrap_or_else(|| home.join("workspace").join(target));

    // P0-1.5: central lease registry check — reject if another agent holds this branch.
    if let Some(other) = crate::binding::scan_existing_branch_binding(home, branch, target) {
        return Err(format!(
            "branch '{branch}' already leased by '{other}' — release first or use a different branch"
        ));
    }

    // Attempt lease (creates worktree + tags as daemon-managed).
    // Lease errors REJECT the dispatch (Q2 + §3.3).
    let lease = crate::worktree_pool::lease(home, &source_repo, target, branch)?;

    // Bind with worktree path. Bind file write error stays graceful (Q1).
    crate::binding::bind_full(home, target, task_id, branch, &lease.path);
    tracing::info!(
        %target, %branch, path = %lease.path.display(),
        "dispatch auto-bind + lease OK"
    );
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
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
        let result = super::dispatch_auto_bind_lease(&home, "test-agent", "T-100", "feat/test");
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
        let result = super::dispatch_auto_bind_lease(&home, "test-agent", "T-1", "main");
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
        let home =
            std::env::temp_dir().join(format!("agend-s53-prod-{}-conflict", std::process::id()));
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
        let r1 = super::dispatch_auto_bind_lease(&home, "agent-a", "T-1", "feat/shared");
        assert!(r1.is_ok(), "first dispatch must succeed: {:?}", r1.err());

        // Second dispatch SAME branch DIFFERENT agent → REJECT via central registry.
        let r2 = super::dispatch_auto_bind_lease(&home, "agent-b", "T-2", "feat/shared");
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
        let r1 = super::dispatch_auto_bind_lease(&home, "agent-x", "T-1", "feat/test");
        assert!(r1.is_ok(), "first dispatch must succeed: {:?}", r1.err());

        // Same agent re-dispatch same branch with different task_id → idempotent.
        let r2 = super::dispatch_auto_bind_lease(&home, "agent-x", "T-2", "feat/test");
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
        let home =
            std::env::temp_dir().join(format!("agend-s53-prod-{}-graceful", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        setup_test_repo(&home, "test-agent");

        // Block bind_full by creating runtime/test-agent as a file (not dir).
        let runtime_parent = home.join("runtime");
        std::fs::create_dir_all(&runtime_parent).ok();
        let runtime_agent = runtime_parent.join("test-agent");
        std::fs::write(&runtime_agent, "blocking file").ok();

        // Lease should succeed, bind write should fail gracefully (Q1).
        let result = super::dispatch_auto_bind_lease(&home, "test-agent", "T-1", "feat/graceful");
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
            format!("instances:\n  agent-a:\n    backend: claude\n    working_directory: {}\n  agent-b:\n    backend: claude\n    working_directory: {}\n", repo.display(), repo.display()),
        ).ok();

        // First lease seeds the worktree pool with feat/end2end for agent-a.
        let r1 = super::dispatch_auto_bind_lease(&home, "agent-a", "T-1", "feat/end2end");
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
        let r1 = super::dispatch_auto_bind_lease(&home, "agent-x", "T-1", "feat/A");
        assert!(r1.is_ok(), "first lease must succeed: {r1:?}");

        // Second dispatch with a DIFFERENT branch must reject.
        let r2 = super::dispatch_auto_bind_lease(&home, "agent-x", "T-2", "feat/B");
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
        let r1 = super::dispatch_auto_bind_lease(&home, "agent-x", "T-1", "feat/A");
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
}
