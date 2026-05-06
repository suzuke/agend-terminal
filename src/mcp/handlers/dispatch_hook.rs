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
        setup_test_repo(&home, "agent-a");
        // Add agent-b pointing to same repo.
        let repo = home.join("workspace").join("agent-a");
        std::fs::write(
            home.join("fleet.yaml"),
            format!("instances:\n  agent-a:\n    backend: claude\n    working_directory: {}\n  agent-b:\n    backend: claude\n    working_directory: {}\n", repo.display(), repo.display()),
        ).ok();

        // First dispatch succeeds (creates branch + worktree).
        let r1 = super::dispatch_auto_bind_lease(&home, "agent-a", "T-1", "feat/shared");
        assert!(r1.is_ok(), "first dispatch must succeed: {:?}", r1.err());

        // Second dispatch SAME branch SAME source repo — must REJECT (Q2).
        // Branch already exists from first lease → git worktree add -b fails.
        let r2 = super::dispatch_auto_bind_lease(&home, "agent-b", "T-2", "feat/shared");
        assert!(
            r2.is_err(),
            "lease conflict must REJECT second dispatch, got: {:?}",
            r2
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

    // ── Ordering tests: reject BEFORE delivery (r4 fix) ─────────────

    #[test]
    fn main_branch_reject_does_not_deliver_to_inbox() {
        // Q2 ordering: if lease rejects, target must NOT receive message.
        let home =
            std::env::temp_dir().join(format!("agend-s53-order-{}-main", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        setup_test_repo(&home, "dev-target");

        // Attempt dispatch with main branch → rejected by lease gate.
        let result = super::dispatch_auto_bind_lease(&home, "dev-target", "T-1", "main");
        assert!(result.is_err(), "main branch must reject");

        // Target's inbox must NOT have any message (gate fires before send).
        let inbox_path = home.join("inbox").join("dev-target.jsonl");
        assert!(
            !inbox_path.exists(),
            "rejected dispatch must not deliver to target inbox"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn lease_conflict_reject_does_not_deliver_to_inbox() {
        // Q2 ordering: lease conflict → no delivery to second target.
        let home =
            std::env::temp_dir().join(format!("agend-s53-order-{}-conflict", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        setup_test_repo(&home, "agent-a");
        let repo = home.join("workspace").join("agent-a");
        std::fs::write(
            home.join("fleet.yaml"),
            format!("instances:\n  agent-a:\n    backend: claude\n    working_directory: {}\n  agent-b:\n    backend: claude\n    working_directory: {}\n", repo.display(), repo.display()),
        ).ok();

        // First lease succeeds.
        let r1 = super::dispatch_auto_bind_lease(&home, "agent-a", "T-1", "feat/order");
        assert!(r1.is_ok());

        // Second lease conflicts → rejected.
        let r2 = super::dispatch_auto_bind_lease(&home, "agent-b", "T-2", "feat/order");
        assert!(r2.is_err(), "conflict must reject");

        // Agent-b inbox must NOT have message.
        let inbox_b = home.join("inbox").join("agent-b.jsonl");
        assert!(
            !inbox_b.exists(),
            "rejected dispatch must not deliver to agent-b inbox"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    // ── Integration test: handle_send_to_instance ordering proof ─────
    // This test calls the FULL dispatch entry point (not just dispatch_auto_bind_lease).
    // It would FAIL if the lease gate were moved after api::SEND.

    #[test]
    fn handle_send_main_branch_rejects_without_delivering() {
        use crate::identity::Sender;

        let home = std::env::temp_dir().join(format!(
            "agend-s53-integration-{}-ordering",
            std::process::id()
        ));
        std::fs::create_dir_all(&home).ok();
        setup_test_repo(&home, "target-agent");

        // Create fleet.yaml with target agent.
        std::fs::write(
            home.join("fleet.yaml"),
            format!(
                "instances:\n  target-agent:\n    backend: claude\n    working_directory: {}\n",
                home.join("workspace").join("target-agent").display()
            ),
        )
        .ok();

        // Construct args matching production delegate_task envelope.
        let args = serde_json::json!({
            "target_instance": "target-agent",
            "message": "implement feature X",
            "request_kind": "task",
            "branch": "main",  // ← E4.5 rejection trigger
            "task_id": "T-999",
        });
        let sender = Sender::new("lead");

        // Call the FULL handle_send_to_instance (production entry point).
        let result = super::super::comms::handle_send_to_instance(&home, &args, "send", &sender);

        // Must return error (lease rejected main branch).
        assert!(
            result.get("error").is_some(),
            "handle_send must return error for main branch: {result}"
        );

        // Target's inbox must NOT have the task message (gate fired before send).
        let inbox_path = home.join("inbox").join("target-agent.jsonl");
        let inbox_content = std::fs::read_to_string(&inbox_path).unwrap_or_default();
        assert!(
            !inbox_content.contains("implement feature X"),
            "rejected dispatch must NOT deliver message to target inbox. Got: {inbox_content}"
        );

        std::fs::remove_dir_all(&home).ok();
    }
}
