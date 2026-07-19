use super::*;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

fn test_ctx(home: &std::path::Path) -> HandlerCtx<'_> {
    // Leak registries for 'static — acceptable in tests.
    let registry: &'static agent::AgentRegistry =
        Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
    let configs: &'static crate::api::ConfigRegistry =
        Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
    let externals: &'static agent::ExternalRegistry =
        Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
    HandlerCtx {
        registry,
        configs,
        externals,
        notifier: None,
        home,
        capability: crate::api::RestartCapability::Unsupported,
        app_restart: None,
        post_flush: crate::api::app_restart::PostFlushSlot::new(),
        shutdown: None,
    }
}

fn tmp_home(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("agend-msg-test-{}-{}", tag, std::process::id()));
    std::fs::create_dir_all(&dir).ok();
    dir
}

#[test]
fn test_send_to_nonexistent_target_returns_error() {
    let home = tmp_home("nonexist");
    // No fleet.yaml → target not in registry or fleet
    let ctx = test_ctx(&home);
    let result = handle_send(
        &json!({"from": "sender", "target": "ghost", "text": "hi"}),
        &ctx,
    );
    assert_eq!(result["ok"], false);
    assert!(
        result["error"].as_str().unwrap_or("").contains("not found"),
        "must return not-found error for nonexistent target: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #bughunt2: a dropped inbox enqueue (disk-low / I/O) must surface as
/// `ok:false`, never the silent `ok:true` for a lost message.
#[test]
fn send_surfaces_enqueue_failure_not_fake_ok() {
    let home = tmp_home("send-enqueue-fail");
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  offline-agent:\n    backend: claude\n",
    )
    .ok();
    // Force the inbox enqueue to fail: `home/inbox` as a FILE makes
    // `create_dir_all(home/inbox)` error inside with_inbox_lock.
    std::fs::write(home.join("inbox"), b"blocker").unwrap();
    let ctx = test_ctx(&home);
    let result = handle_send(
        &json!({"from": "sender", "target": "offline-agent", "text": "hi"}),
        &ctx,
    );
    assert_eq!(
        result["ok"], false,
        "a dropped enqueue must NOT report ok:true: {result}"
    );
    assert!(
        result["error"]
            .as_str()
            .unwrap_or("")
            .contains("not delivered"),
        "must surface the delivery failure: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #2730: a FAILED parented send must NOT settle the sender's parent row. The
/// settle seam is wired only past a successful `route_and_deliver`; a delivery
/// failure early-returns before it. Seed + drain a real delivering parent row in
/// the SENDER's own inbox, force ONLY the target's enqueue to fail (its inbox
/// jsonl is made a directory, so the sender's inbox stays usable), then prove the
/// parent row is still unprocessed (`read_at` unset) — settle did not fire.
#[test]
fn failed_parented_send_does_not_settle_sender_parent() {
    let home = tmp_home("failed-send-no-settle");
    let (sender, target) = ("fsns-worker", "fsns-peer");
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        format!("instances:\n  {target}:\n    backend: claude\n"),
    )
    .unwrap();
    // Seed + drain a real delivering parent row in the SENDER's own inbox.
    let pid = "m-fsns-parent";
    crate::inbox::enqueue(
        &home,
        sender,
        crate::inbox::InboxMessage {
            schema_version: 1,
            id: Some(pid.to_string()),
            from: "codex".to_string(),
            text: "q".to_string(),
            kind: Some("query".to_string()),
            timestamp: chrono::Utc::now().to_rfc3339(),
            ..Default::default()
        },
    )
    .unwrap();
    crate::inbox::drain(&home, sender); // parent: unread → delivering

    // Force ONLY the target's enqueue to fail, without touching the sender's
    // inbox: make the target's RESOLVED inbox path a directory so the append
    // inside route_and_deliver errors. Must be the RESOLVED (not raw-name) path —
    // on Windows inbox_path_resolved migrates name→UUID, so a raw-name-path
    // directory is bypassed and the UUID path succeeds (#2730 r2 Windows failure).
    // Breaking the resolved path makes enqueue hit the id_path-exists branch on
    // BOTH platforms (no symlink/copy migration divergence).
    let target_path = crate::inbox::storage::inbox_path_resolved(&home, target);
    std::fs::create_dir_all(&target_path).unwrap();

    let ctx = test_ctx(&home);
    let resp = handle_send(
        &json!({"from": sender, "target": target, "kind": "report", "parent_id": pid, "text": "answered"}),
        &ctx,
    );
    assert_eq!(
        resp["ok"], false,
        "the send must fail when the target enqueue is broken: {resp}"
    );

    // The sender's parent row must remain unprocessed — settle must NOT fire on
    // the failure path.
    let path = crate::inbox::storage::inbox_path_resolved(&home, sender);
    let body = std::fs::read_to_string(&path).unwrap();
    let row = body
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v.get("id").and_then(|x| x.as_str()) == Some(pid))
        .expect("sender parent row must still exist");
    assert!(
        row.get("read_at").is_none_or(|r| r.is_null()),
        "a FAILED send must not settle the sender parent (read_at must stay unset): {row}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_send_to_fleet_defined_instance_succeeds() {
    let home = tmp_home("fleet-defined");
    // Define instance in fleet.yaml but don't start it
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  offline-agent:\n    backend: claude\n",
    )
    .ok();
    let ctx = test_ctx(&home);
    let result = handle_send(
        &json!({"from": "sender", "target": "offline-agent", "text": "hi"}),
        &ctx,
    );
    assert_eq!(
        result["ok"], true,
        "fleet.yaml-defined instance must be accepted: {result}"
    );
    // Not in registry → inbox_only (not pty)
    assert_eq!(
        result["delivery_mode"].as_str(),
        Some("inbox_only"),
        "inactive target must get inbox_only delivery: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_send_to_active_registry_target_returns_pty() {
    let home = tmp_home("active-pty");
    std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  active-agent:\n    backend: claude\n    id: 0a0a0a0a-0000-4000-8000-000000000001\n  sender:\n    backend: claude\n",
        )
        .ok();
    // Spawn a real agent so it's in the registry
    let registry: &'static agent::AgentRegistry =
        Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
    let spawn_cfg = crate::agent::SpawnConfig {
        name: "active-agent",
        backend: None,
        backend_command: crate::default_shell(),
        args: &[],
        spawn_mode: crate::backend::SpawnMode::Fresh,
        cols: 80,
        rows: 24,
        env: None,
        working_dir: None,
        submit_key: "\r",
        home: Some(&home),
        crash_tx: None,
        shutdown: None,
    };
    crate::agent::spawn_agent(&spawn_cfg, registry).expect("spawn");
    // Override backend_command to "codex" for ACK absorption check
    {
        let mut reg = agent::lock_registry(registry);
        if let Some(h) = reg.values_mut().find(|h| h.name.as_str() == "codex-agent") {
            h.backend_command = "codex".to_string();
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(500));

    let configs: &'static crate::api::ConfigRegistry =
        Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
    let externals: &'static agent::ExternalRegistry =
        Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
    let home_ref: &'static std::path::Path = Box::leak(Box::new(home.clone()));
    let ctx = HandlerCtx {
        registry,
        configs,
        externals,
        notifier: None,
        home: home_ref,
        capability: crate::api::RestartCapability::Unsupported,
        app_restart: None,
        post_flush: crate::api::app_restart::PostFlushSlot::new(),
        shutdown: None,
    };
    let result = handle_send(
        &json!({"from": "sender", "target": "active-agent", "text": "hi"}),
        &ctx,
    );
    assert_eq!(result["ok"], true);
    assert_eq!(
        result["delivery_mode"].as_str(),
        Some("pty"),
        "active agent must get pty delivery: {result}"
    );
    // Cleanup
    let reg = agent::lock_registry(registry);
    if let Some(h) = reg.values().find(|h| h.name.as_str() == "active-agent") {
        let _ = h.child.lock().kill();
    }
    drop(reg);
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_send_to_self_rejected() {
    let home = tmp_home("self-send");
    let ctx = test_ctx(&home);
    let result = handle_send(
        &json!({"from": "agent1", "target": "agent1", "text": "hi"}),
        &ctx,
    );
    assert_eq!(result["ok"], false);
    assert!(result["error"].as_str().unwrap_or("").contains("self"));
    std::fs::remove_dir_all(&home).ok();
}

// --- Sprint 37: team isolation gate tests ---

/// Set up fleet.yaml with given instances and teams. Sprint 54
/// fleet-yaml unification: teams now live in the `teams:` block of
/// fleet.yaml directly (was: separate teams.json runtime store).
/// #1441: deterministic valid UUID from an instance name so a seeded
/// fleet.yaml entry resolves to a stable registry key under the
/// UUID-keyed registry. FNV-1a folded into a version-4/variant-8 layout.
fn det_uuid(name: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("00000000-0000-4000-8000-{:012x}", h & 0xffff_ffff_ffff)
}

fn setup_team_env(home: &std::path::Path, fleet_instances: &[&str], teams: &[(&str, &[&str])]) {
    let mut yaml = String::from("instances:\n");
    for n in fleet_instances {
        yaml.push_str(&format!(
            "  {n}:\n    backend: claude\n    id: {}\n",
            det_uuid(n)
        ));
    }
    if !teams.is_empty() {
        yaml.push_str("teams:\n");
        for (name, members) in teams {
            yaml.push_str(&format!("  {name}:\n    members:\n"));
            for m in members.iter() {
                yaml.push_str(&format!("      - {m}\n"));
            }
            yaml.push_str("    created_at: \"2026-01-01T00:00:00Z\"\n");
        }
    }
    std::fs::write(crate::fleet::fleet_yaml_path(home), yaml).ok();
}

fn audit_log_contains(home: &std::path::Path, kind: &str) -> bool {
    let path = home.join("event-log.jsonl");
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .any(|l| l.contains(kind))
}

#[test]
fn send_same_team_allowed() {
    let home = tmp_home("same-team");
    setup_team_env(&home, &["alice", "bob"], &[("dev2", &["alice", "bob"])]);
    let ctx = test_ctx(&home);
    let result = handle_send(
        &json!({"from": "alice", "target": "bob", "text": "hi"}),
        &ctx,
    );
    assert_eq!(result["ok"], true, "same-team send must succeed: {result}");
    assert!(!audit_log_contains(&home, "send_cross_team_blocked"));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn send_cross_team_blocked() {
    let home = tmp_home("cross-team");
    setup_team_env(
        &home,
        &["alice", "bob"],
        &[("dev2", &["alice"]), ("dev", &["bob"])],
    );
    let ctx = test_ctx(&home);
    let result = handle_send(
        &json!({"from": "alice", "target": "bob", "text": "hi"}),
        &ctx,
    );
    assert_eq!(
        result["ok"], false,
        "cross-team send must be blocked: {result}"
    );
    assert!(
        result["error"]
            .as_str()
            .unwrap_or("")
            .contains("cross-team"),
        "error must mention cross-team: {result}"
    );
    assert!(audit_log_contains(&home, "send_cross_team_blocked"));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn send_to_general_allowed_from_any_team() {
    let home = tmp_home("to-general");
    setup_team_env(&home, &["alice", "general"], &[("dev2", &["alice"])]);
    let ctx = test_ctx(&home);
    let result = handle_send(
        &json!({"from": "alice", "target": "general", "text": "hi"}),
        &ctx,
    );
    assert_eq!(result["ok"], true, "send to general must succeed: {result}");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn send_from_general_to_any_team_allowed() {
    let home = tmp_home("from-general");
    setup_team_env(&home, &["general", "bob"], &[("dev", &["bob"])]);
    let ctx = test_ctx(&home);
    let result = handle_send(
        &json!({"from": "general", "target": "bob", "text": "hi"}),
        &ctx,
    );
    assert_eq!(
        result["ok"], true,
        "send from general must succeed: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn send_self_already_blocked() {
    let home = tmp_home("self-block-team");
    setup_team_env(&home, &["alice"], &[("dev2", &["alice"])]);
    let ctx = test_ctx(&home);
    let result = handle_send(
        &json!({"from": "alice", "target": "alice", "text": "hi"}),
        &ctx,
    );
    assert_eq!(result["ok"], false);
    assert!(
        result["error"].as_str().unwrap_or("").contains("self"),
        "self-send must be caught by existing guard, not team gate"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn send_no_team_to_no_team_allowed() {
    let home = tmp_home("no-team");
    setup_team_env(&home, &["alice", "bob"], &[]);
    let ctx = test_ctx(&home);
    let result = handle_send(
        &json!({"from": "alice", "target": "bob", "text": "hi"}),
        &ctx,
    );
    assert_eq!(
        result["ok"], true,
        "both teamless must be allowed: {result}"
    );
    assert!(!audit_log_contains(&home, "send_cross_team_blocked"));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn send_team_to_no_team_blocked() {
    let home = tmp_home("team-to-none");
    setup_team_env(&home, &["alice", "bob"], &[("dev2", &["alice"])]);
    let ctx = test_ctx(&home);
    let result = handle_send(
        &json!({"from": "alice", "target": "bob", "text": "hi"}),
        &ctx,
    );
    assert_eq!(
        result["ok"], false,
        "team→teamless must be blocked: {result}"
    );
    assert!(audit_log_contains(&home, "send_cross_team_blocked"));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn send_no_team_to_team_blocked() {
    let home = tmp_home("none-to-team");
    setup_team_env(&home, &["alice", "bob"], &[("dev2", &["bob"])]);
    let ctx = test_ctx(&home);
    let result = handle_send(
        &json!({"from": "alice", "target": "bob", "text": "hi"}),
        &ctx,
    );
    assert_eq!(
        result["ok"], false,
        "teamless→team must be blocked: {result}"
    );
    assert!(audit_log_contains(&home, "send_cross_team_blocked"));
    std::fs::remove_dir_all(&home).ok();
}

// --- Sprint 40 T-5: provenance injection invariant at API boundary ---

#[test]
fn provenance_injection_no_active_channel_does_not_panic() {
    // DESIGN §4 Q4 invariant re-pinned at API SEND boundary (moved from
    // MCP comms layer in T-5). When provenance params are present but no
    // active channel exists, handle_send must not panic and must return
    // a successful delivery result (provenance is best-effort).
    let home = tmp_home("prov-no-ch");
    setup_team_env(&home, &["sender", "target"], &[]);
    let ctx = test_ctx(&home);
    let result = handle_send(
        &json!({
            "from": "sender",
            "target": "target",
            "text": "task text",
            "kind": "task",
            "provenance": {"from": "sender", "task": "do the thing"}
        }),
        &ctx,
    );
    // Send succeeds (inbox delivery); provenance silently skipped (no channel).
    assert_eq!(
        result["ok"], true,
        "send with provenance must succeed even without active channel: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// DESIGN §4 Q4 warn-observability invariant: provenance injection
/// failure MUST produce a tracing::warn record, not a silent drop.
/// Re-pinned at API SEND boundary after T-5 moved provenance from
/// MCP comms layer.
#[test]
#[tracing_test::traced_test]
fn provenance_injection_no_active_channel_logs_warn() {
    let home = tmp_home("prov-warn");
    setup_team_env(&home, &["sender", "target"], &[]);
    let ctx = test_ctx(&home);
    let _result = handle_send(
        &json!({
            "from": "sender",
            "target": "target",
            "text": "task text",
            "provenance": {"from": "sender", "task": "do the thing"}
        }),
        &ctx,
    );
    // No active channel → provenance injection fails → warn emitted.
    // The warn text at messaging.rs:185 is "provenance injection failed".
    assert!(
        logs_contain("provenance injection failed"),
        "DESIGN §4 Q4: provenance failure warn must be emitted at API boundary"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #2004 codex P1 negative pin: a kind=query dispatch must NOT emit the
/// record_dispatch failure warn — `record_dispatch` returns None for
/// non-task kinds BY DESIGN (queries never get an idle-nudge sidecar;
/// the kind contract itself is pinned in dispatch_idle). The existing
/// contract test pins "no sidecar"; this pins "no warn" — the first
/// #2004 revision warned on the designed skip and false-alarmed on
/// every ordinary query.
#[test]
#[tracing_test::traced_test]
fn query_dispatch_emits_no_record_failure_warn_2004() {
    let home = tmp_home("query-no-warn");
    setup_team_env(&home, &["sender", "target"], &[]);
    let ctx = test_ctx(&home);
    let _ = handle_send(
        &json!({
            "from": "sender",
            "target": "target",
            "text": "what is the status?",
            "kind": "query",
            "expect_reply_within_secs": 600
        }),
        &ctx,
    );
    assert!(
        !logs_contain("record_dispatch failed"),
        "kind=query: record_dispatch's None is a designed skip, not a failure — no warn"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn provenance_params_passed_through_send() {
    // Verify that provenance field in SEND params is accepted and does
    // not cause errors. The actual channel injection is best-effort;
    // this test pins that the API layer processes the field without panic.
    let home = tmp_home("prov-pass");
    setup_team_env(&home, &["alice", "bob"], &[("dev2", &["alice", "bob"])]);
    let ctx = test_ctx(&home);
    let result = handle_send(
        &json!({
            "from": "alice",
            "target": "bob",
            "text": "delegated task",
            "provenance": {"from": "alice", "task": "build feature X"}
        }),
        &ctx,
    );
    assert_eq!(
        result["ok"], true,
        "send with provenance params must succeed: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}

// --- Sprint 40 T-6: worktree-checkout boundary invariant ---

#[test]
fn send_with_branch_param_does_not_panic() {
    // B2 boundary invariant (safety): branch param in SEND is accepted
    // without panic even when target has no working directory or is not
    // a git repo. Checkout is best-effort.
    let home = tmp_home("branch-safe");
    setup_team_env(&home, &["sender", "target"], &[]);
    let ctx = test_ctx(&home);
    let result = handle_send(
        &json!({
            "from": "sender",
            "target": "target",
            "text": "task with branch",
            "branch": "feat/test-branch"
        }),
        &ctx,
    );
    assert_eq!(
        result["ok"], true,
        "send with branch param must succeed (checkout best-effort): {result}"
    );
    // branch_checked_out absent when target has no working dir
    assert!(
        result.get("branch_checked_out").is_none(),
        "no checkout expected without working dir: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
#[tracing_test::traced_test]
fn send_with_branch_non_git_dir_logs_no_panic() {
    // B2 boundary invariant (order-of-operations): branch checkout
    // happens AFTER delivery, not before. Even if checkout would fail,
    // the send itself succeeds.
    let home = tmp_home("branch-nongit");
    // Create fleet.yaml with working_directory pointing to a non-git dir
    std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            format!(
                "instances:\n  sender:\n    backend: claude\n  target:\n    backend: claude\n    working_directory: {}\n",
                home.join("workspace/target").display()
            ),
        )
        .ok();
    std::fs::create_dir_all(home.join("workspace/target")).ok();
    let ctx = test_ctx(&home);
    let result = handle_send(
        &json!({
            "from": "sender",
            "target": "target",
            "text": "task",
            "branch": "feat/x"
        }),
        &ctx,
    );
    assert_eq!(
        result["ok"], true,
        "send must succeed even when checkout skipped (non-git): {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
#[tracing_test::traced_test]
fn send_with_branch_checkout_failure_logs_warn() {
    // B2 boundary invariant (observability): when checkout fails,
    // tracing::warn must fire. Parallel to DESIGN §4 Q4 pattern.
    // #1834: the checkout target is a REAL source dir OUTSIDE the daemon
    // workspace (a workspace-stub path would now be skipped before checkout,
    // so the warn path could never fire there).
    let home = tmp_home("branch-fail");
    let wd = home.join("src-target");
    std::fs::create_dir_all(&wd).ok();
    // Init a git repo so is_git_repo returns true
    let _ = std::process::Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(&wd)
        .output();
    std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            format!(
                "instances:\n  sender:\n    backend: claude\n  target:\n    backend: claude\n    working_directory: {}\n",
                wd.display()
            ),
        )
        .ok();
    let ctx = test_ctx(&home);
    let result = handle_send(
        &json!({
            "from": "sender",
            "target": "target",
            "text": "task",
            "branch": "invalid..branch"
        }),
        &ctx,
    );
    assert_eq!(
        result["ok"], true,
        "send must succeed even when checkout fails: {result}"
    );
    // Observability pin: warn must fire on checkout failure
    assert!(
        logs_contain("task.branch checkout failed"),
        "B2 observability invariant: warn must fire on checkout failure"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn dispatch_branch_skips_metadata_stub_but_checks_out_real_source_1834() {
    // §3.9 (#1834): `send(kind=task, branch=X)` must NOT check out the task
    // branch on the daemon-managed metadata workspace stub (git-init'd by the
    // Claude backend) — that only accumulates stray branches + misleads the
    // statusline. A REAL source/worktree target (working_directory OUTSIDE
    // <home>/workspace/) still gets the checkout. Drives the real `handle_send`
    // entry. Regression-proof: drop the workspace-stub skip and the
    // no-stray-branch assertion fails.
    fn git(dir: &std::path::Path, args: &[&str]) {
        let _ = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output();
    }
    fn init_repo(dir: &std::path::Path) {
        std::fs::create_dir_all(dir).ok();
        git(dir, &["init", "-b", "main"]);
        git(
            dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ],
        );
    }
    fn branch_exists(dir: &std::path::Path, branch: &str) -> bool {
        std::process::Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", branch])
            .current_dir(dir)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    let home = tmp_home("branch-stub-vs-real");
    // (1) Stub target: working_directory UNDER <home>/workspace/ → skipped.
    let stub_wd = home.join("workspace/stub-agent");
    init_repo(&stub_wd);
    // (2) Real target: working_directory OUTSIDE workspace → checked out.
    let real_wd = home.join("real-src");
    init_repo(&real_wd);

    std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            format!(
                "instances:\n  sender:\n    backend: claude\n  stub-agent:\n    backend: claude\n    working_directory: {}\n  real-agent:\n    backend: claude\n    working_directory: {}\n",
                stub_wd.display(),
                real_wd.display()
            ),
        )
        .ok();
    let ctx = test_ctx(&home);

    // Stub dispatch — no checkout, no stray branch.
    let stub_resp = handle_send(
        &json!({"from":"sender","target":"stub-agent","text":"task","branch":"feat/stub-x"}),
        &ctx,
    );
    assert_eq!(
        stub_resp["ok"], true,
        "send must still succeed: {stub_resp}"
    );
    assert!(
        stub_resp.get("branch_checked_out").is_none(),
        "stub must NOT report a checkout: {stub_resp}"
    );
    assert!(
        !branch_exists(&stub_wd, "feat/stub-x"),
        "#1834: no stray branch may be created on the metadata workspace stub"
    );

    // Real dispatch — branch IS checked out on the real source.
    let real_resp = handle_send(
        &json!({"from":"sender","target":"real-agent","text":"task","branch":"feat/real-x"}),
        &ctx,
    );
    assert_eq!(
        real_resp["branch_checked_out"].as_str(),
        Some("feat/real-x"),
        "real source target must still check out the branch: {real_resp}"
    );
    assert!(
        branch_exists(&real_wd, "feat/real-x"),
        "real source must now have the checked-out branch"
    );

    std::fs::remove_dir_all(&home).ok();
}

// ── Issue #643: cross-team ACK absorption tests ─────────────────

#[test]
fn same_team_codex_update_absorbed() {
    let home = tmp_home("codex-absorbed");
    setup_team_env(
        &home,
        &["codex-agent", "sender"],
        &[("dev", &["codex-agent", "sender"])],
    );
    // Override codex-agent backend to codex in fleet.yaml
    let yaml = std::fs::read_to_string(crate::fleet::fleet_yaml_path(&home)).unwrap();
    let yaml = yaml.replace(
        "  codex-agent:\n    backend: claude",
        "  codex-agent:\n    backend: codex",
    );
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).ok();

    let registry: &'static agent::AgentRegistry =
        Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
    let spawn_cfg = crate::agent::SpawnConfig {
        name: "codex-agent",
        backend: None,
        backend_command: crate::default_shell(),
        args: &[],
        spawn_mode: crate::backend::SpawnMode::Fresh,
        cols: 80,
        rows: 24,
        env: None,
        working_dir: None,
        submit_key: "\r",
        home: Some(&home),
        crash_tx: None,
        shutdown: None,
    };
    crate::agent::spawn_agent(&spawn_cfg, registry).expect("spawn");
    // Override backend_command to "codex" for ACK absorption check
    {
        let mut reg = agent::lock_registry(registry);
        if let Some(h) = reg.values_mut().find(|h| h.name.as_str() == "codex-agent") {
            h.backend_command = "codex".to_string();
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(300));

    let configs: &'static crate::api::ConfigRegistry =
        Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
    let externals: &'static agent::ExternalRegistry =
        Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
    let home_ref: &'static std::path::Path = Box::leak(Box::new(home.clone()));
    let ctx = HandlerCtx {
        registry,
        configs,
        externals,
        notifier: None,
        home: home_ref,
        capability: crate::api::RestartCapability::Unsupported,
        app_restart: None,
        post_flush: crate::api::app_restart::PostFlushSlot::new(),
        shutdown: None,
    };
    let result = handle_send(
        &json!({"from": "sender", "target": "codex-agent", "text": "status update", "kind": "update"}),
        &ctx,
    );
    assert_eq!(result["ok"], true);
    assert_eq!(
        result["delivery_mode"].as_str(),
        Some("inbox_only"),
        "same-team Codex update must be absorbed: {result}"
    );
    // Audit log must record absorption
    assert!(
        audit_log_contains(&home, "ack_absorbed"),
        "ack_absorbed event must be logged"
    );
    let reg = agent::lock_registry(registry);
    if let Some(h) = reg.values().find(|h| h.name.as_str() == "codex-agent") {
        let _ = h.child.lock().kill();
    }
    drop(reg);
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn cross_team_message_not_absorbed() {
    let home = tmp_home("cross-team-no-absorb");
    setup_team_env(
        &home,
        &["codex-agent", "general"],
        &[("team-a", &["general"]), ("team-b", &["codex-agent"])],
    );
    let yaml = std::fs::read_to_string(crate::fleet::fleet_yaml_path(&home)).unwrap();
    let yaml = yaml.replace(
        "  codex-agent:\n    backend: claude",
        "  codex-agent:\n    backend: codex",
    );
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).ok();

    let registry: &'static agent::AgentRegistry =
        Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
    let spawn_cfg = crate::agent::SpawnConfig {
        name: "codex-agent",
        backend: None,
        backend_command: crate::default_shell(),
        args: &[],
        spawn_mode: crate::backend::SpawnMode::Fresh,
        cols: 80,
        rows: 24,
        env: None,
        working_dir: None,
        submit_key: "\r",
        home: Some(&home),
        crash_tx: None,
        shutdown: None,
    };
    crate::agent::spawn_agent(&spawn_cfg, registry).expect("spawn");
    // Override backend_command to "codex" for ACK absorption check
    {
        let mut reg = agent::lock_registry(registry);
        if let Some(h) = reg.values_mut().find(|h| h.name.as_str() == "codex-agent") {
            h.backend_command = "codex".to_string();
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(300));

    let configs: &'static crate::api::ConfigRegistry =
        Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
    let externals: &'static agent::ExternalRegistry =
        Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
    let home_ref: &'static std::path::Path = Box::leak(Box::new(home.clone()));
    let ctx = HandlerCtx {
        registry,
        configs,
        externals,
        notifier: None,
        home: home_ref,
        capability: crate::api::RestartCapability::Unsupported,
        app_restart: None,
        post_flush: crate::api::app_restart::PostFlushSlot::new(),
        shutdown: None,
    };
    // general can send cross-team; codex update should still inject (not absorbed)
    let result = handle_send(
        &json!({"from": "general", "target": "codex-agent", "text": "cross-team update", "kind": "update"}),
        &ctx,
    );
    assert_eq!(
        result["ok"], true,
        "cross-team via general must succeed: {result}"
    );
    assert_eq!(
        result["delivery_mode"].as_str(),
        Some("pty"),
        "cross-team message must NOT be absorbed: {result}"
    );
    let reg = agent::lock_registry(registry);
    if let Some(h) = reg.values().find(|h| h.name.as_str() == "codex-agent") {
        let _ = h.child.lock().kill();
    }
    drop(reg);
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn same_team_codex_update_orchestrator_not_skipped() {
    let home = tmp_home("orch-not-skip");
    // codex-agent is the orchestrator of team-a
    let yaml = "instances:\n  sender:\n    backend: claude\n  codex-agent:\n    backend: codex\n    id: 0c0c0c0c-0000-4000-8000-000000000001\n\
                    teams:\n  team-a:\n    members:\n      - sender\n      - codex-agent\n    orchestrator: codex-agent\n    created_at: \"2026-01-01T00:00:00Z\"\n";
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).ok();

    let registry: &'static agent::AgentRegistry =
        Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
    let spawn_cfg = crate::agent::SpawnConfig {
        name: "codex-agent",
        backend: None,
        backend_command: crate::default_shell(),
        args: &[],
        spawn_mode: crate::backend::SpawnMode::Fresh,
        cols: 80,
        rows: 24,
        env: None,
        working_dir: None,
        submit_key: "\r",
        home: Some(&home),
        crash_tx: None,
        shutdown: None,
    };
    crate::agent::spawn_agent(&spawn_cfg, registry).expect("spawn");
    {
        let mut reg = agent::lock_registry(registry);
        if let Some(h) = reg.values_mut().find(|h| h.name.as_str() == "codex-agent") {
            h.backend_command = "codex".to_string();
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(300));

    let configs: &'static crate::api::ConfigRegistry =
        Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
    let externals: &'static agent::ExternalRegistry =
        Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
    let home_ref: &'static std::path::Path = Box::leak(Box::new(home.clone()));
    let ctx = HandlerCtx {
        registry,
        configs,
        externals,
        notifier: None,
        home: home_ref,
        capability: crate::api::RestartCapability::Unsupported,
        app_restart: None,
        post_flush: crate::api::app_restart::PostFlushSlot::new(),
        shutdown: None,
    };
    let result = handle_send(
        &json!({"from": "sender", "target": "codex-agent", "text": "status update", "kind": "update"}),
        &ctx,
    );
    assert_eq!(result["ok"], true);
    assert_eq!(
        result["delivery_mode"].as_str(),
        Some("pty"),
        "orchestrator must NOT be skipped even for same-team codex update: {result}"
    );
    let reg = agent::lock_registry(registry);
    if let Some(h) = reg.values().find(|h| h.name.as_str() == "codex-agent") {
        let _ = h.child.lock().kill();
    }
    drop(reg);
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn same_team_codex_update_non_orchestrator_skipped() {
    let home = tmp_home("non-orch-skip");
    // codex-agent is NOT the orchestrator (lead is)
    let yaml = "instances:\n  sender:\n    backend: claude\n  codex-agent:\n    backend: codex\n    id: 0c0c0c0c-0000-4000-8000-000000000002\n  lead:\n    backend: claude\n\
                    teams:\n  team-a:\n    members:\n      - sender\n      - codex-agent\n      - lead\n    orchestrator: lead\n    created_at: \"2026-01-01T00:00:00Z\"\n";
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).ok();

    let registry: &'static agent::AgentRegistry =
        Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
    let spawn_cfg = crate::agent::SpawnConfig {
        name: "codex-agent",
        backend: None,
        backend_command: crate::default_shell(),
        args: &[],
        spawn_mode: crate::backend::SpawnMode::Fresh,
        cols: 80,
        rows: 24,
        env: None,
        working_dir: None,
        submit_key: "\r",
        home: Some(&home),
        crash_tx: None,
        shutdown: None,
    };
    crate::agent::spawn_agent(&spawn_cfg, registry).expect("spawn");
    {
        let mut reg = agent::lock_registry(registry);
        if let Some(h) = reg.values_mut().find(|h| h.name.as_str() == "codex-agent") {
            h.backend_command = "codex".to_string();
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(300));

    let configs: &'static crate::api::ConfigRegistry =
        Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
    let externals: &'static agent::ExternalRegistry =
        Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
    let home_ref: &'static std::path::Path = Box::leak(Box::new(home.clone()));
    let ctx = HandlerCtx {
        registry,
        configs,
        externals,
        notifier: None,
        home: home_ref,
        capability: crate::api::RestartCapability::Unsupported,
        app_restart: None,
        post_flush: crate::api::app_restart::PostFlushSlot::new(),
        shutdown: None,
    };
    let result = handle_send(
        &json!({"from": "sender", "target": "codex-agent", "text": "status update", "kind": "update"}),
        &ctx,
    );
    assert_eq!(result["ok"], true);
    assert_eq!(
        result["delivery_mode"].as_str(),
        Some("inbox_only"),
        "non-orchestrator codex must be skipped for same-team update: {result}"
    );
    let reg = agent::lock_registry(registry);
    if let Some(h) = reg.values().find(|h| h.name.as_str() == "codex-agent") {
        let _ = h.child.lock().kill();
    }
    drop(reg);
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn cross_team_codex_update_orchestrator_not_skipped() {
    let home = tmp_home("cross-orch-no-skip");
    // codex-agent is orchestrator, sender is "general" (cross-team bus)
    let yaml = "instances:\n  general:\n    backend: claude\n  codex-agent:\n    backend: codex\n    id: 0c0c0c0c-0000-4000-8000-000000000003\n\
                    teams:\n  team-a:\n    members:\n      - general\n    created_at: \"2026-01-01T00:00:00Z\"\n\
                    \n  team-b:\n    members:\n      - codex-agent\n    orchestrator: codex-agent\n    created_at: \"2026-01-01T00:00:00Z\"\n";
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).ok();

    let registry: &'static agent::AgentRegistry =
        Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
    let spawn_cfg = crate::agent::SpawnConfig {
        name: "codex-agent",
        backend: None,
        backend_command: crate::default_shell(),
        args: &[],
        spawn_mode: crate::backend::SpawnMode::Fresh,
        cols: 80,
        rows: 24,
        env: None,
        working_dir: None,
        submit_key: "\r",
        home: Some(&home),
        crash_tx: None,
        shutdown: None,
    };
    crate::agent::spawn_agent(&spawn_cfg, registry).expect("spawn");
    {
        let mut reg = agent::lock_registry(registry);
        if let Some(h) = reg.values_mut().find(|h| h.name.as_str() == "codex-agent") {
            h.backend_command = "codex".to_string();
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(300));

    let configs: &'static crate::api::ConfigRegistry =
        Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
    let externals: &'static agent::ExternalRegistry =
        Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
    let home_ref: &'static std::path::Path = Box::leak(Box::new(home.clone()));
    let ctx = HandlerCtx {
        registry,
        configs,
        externals,
        notifier: None,
        home: home_ref,
        capability: crate::api::RestartCapability::Unsupported,
        app_restart: None,
        post_flush: crate::api::app_restart::PostFlushSlot::new(),
        shutdown: None,
    };
    let result = handle_send(
        &json!({"from": "general", "target": "codex-agent", "text": "cross-team update", "kind": "update"}),
        &ctx,
    );
    assert_eq!(result["ok"], true);
    assert_eq!(
        result["delivery_mode"].as_str(),
        Some("pty"),
        "cross-team message must NOT be absorbed regardless of orchestrator: {result}"
    );
    let reg = agent::lock_registry(registry);
    if let Some(h) = reg.values().find(|h| h.name.as_str() == "codex-agent") {
        let _ = h.child.lock().kill();
    }
    drop(reg);
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn send_to_team_member_missing_from_registry_returns_team_desync_error() {
    // #785 anchor: target is a team member (per fleet.yaml `teams:`
    // block) but no instance exists (never in registry, never in
    // `instances:` section). Error message must surface the team-
    // desync state with BOTH remediation paths so operators can
    // diagnose without code archaeology.
    //
    // Reviewer C5 fixture pattern: never call create_instance for
    // the target name; team membership set up directly via
    // `teams::create`. No mock plumbing.
    let home = tmp_home("785-desync");
    // Set up a team `dev` with member `ghost-member` — no instance.
    let _ = crate::teams::create(
        &home,
        &json!({
            "name": "dev",
            "members": ["ghost-member"],
            "orchestrator": "ghost-member",
            "repository_path": "/tmp/p785-desync",
        }),
    );

    let ctx = test_ctx(&home);
    let result = handle_send(
        &json!({"from": "sender", "target": "ghost-member", "text": "hi"}),
        &ctx,
    );
    assert_eq!(result["ok"], false);
    let err = result["error"].as_str().unwrap_or("");
    // Content invariants pin the operator-actionable contract
    // (prevent silent wording drift in future PRs).
    assert!(
        err.contains("ghost-member"),
        "error must name the target: {err}"
    );
    assert!(err.contains("dev"), "error must name the team: {err}");
    assert!(
        err.contains("create_instance"),
        "error must surface create_instance remediation path: {err}"
    );
    assert!(
        err.contains("team(action=update"),
        "error must surface team(action=update) cleanup path: {err}"
    );
    // Neutral wording — must NOT claim a specific causal hypothesis.
    assert!(
        !err.contains("likely daemon refresh"),
        "error must use neutral wording (no causal claim): {err}"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── PR1 watchdog hook integration tests (C2 GREEN) ──
//
// These exercise the handle_send → dispatch_idle hook wiring.
// The hook is post-enqueue (auto_release ordering precedent) so
// any failure here doesn't surface to the dispatch primitive.

fn write_fixup_fleet(home: &std::path::Path, members: &[&str]) {
    let list = members
        .iter()
        .map(|m| format!("\"{m}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let orchestrator = members.first().copied().unwrap_or("fixup-lead");
    let yaml = format!(
        "schema_version: 1\n\
             instances:\n\
             {instances}\
             teams:\n  fixup:\n    members: [{list}]\n    orchestrator: {orchestrator}\n",
        instances = members
            .iter()
            .map(|m| format!("  {m}:\n    backend: claude\n"))
            .collect::<String>(),
    );
    std::fs::write(crate::fleet::fleet_yaml_path(home), yaml).unwrap();
}

#[test]
fn hook_kind_report_resolves_pending_by_correlation_id() {
    let home = tmp_home("hook-report-resolves");
    write_fixup_fleet(&home, &["fixup-lead", "fixup-reviewer"]);
    // Seed a pending sidecar (correlation_id = "t-hook").
    let id = crate::daemon::dispatch_idle::record_dispatch(
        &home,
        "fixup-lead",
        "fixup-reviewer",
        Some("t-hook"),
        "task",
        600,
    )
    .expect("seed sidecar");
    let ctx = test_ctx(&home);
    // Reviewer sends report with the matching correlation_id.
    let result = handle_send(
        &json!({
            "from": "fixup-reviewer",
            "target": "fixup-lead",
            "text": "VERIFIED",
            "kind": "report",
            "correlation_id": "t-hook",
        }),
        &ctx,
    );
    assert_eq!(result["ok"], true, "report send must succeed: {result}");
    let pending = crate::daemon::dispatch_idle::list_pending(&home);
    assert!(
        !pending.iter().any(|p| p.dispatch_id == id),
        "kind=report with matching correlation_id must resolve (delete) the sidecar"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn hook_kind_update_does_not_resolve_pending() {
    // Load-bearing contract: BUSY / status updates must NOT
    // suppress the watchdog. Spike challenge #1.
    let home = tmp_home("hook-update-no-resolve");
    write_fixup_fleet(&home, &["fixup-lead", "fixup-reviewer"]);
    let id = crate::daemon::dispatch_idle::record_dispatch(
        &home,
        "fixup-lead",
        "fixup-reviewer",
        Some("t-update"),
        "task",
        600,
    )
    .expect("seed sidecar");
    let ctx = test_ctx(&home);
    let result = handle_send(
        &json!({
            "from": "fixup-reviewer",
            "target": "fixup-lead",
            "text": "BUSY working on the diff",
            "kind": "update",
            "correlation_id": "t-update",
        }),
        &ctx,
    );
    assert_eq!(result["ok"], true);
    let pending = crate::daemon::dispatch_idle::list_pending(&home);
    let entry = pending.iter().find(|p| p.dispatch_id == id).unwrap();
    assert_eq!(
        entry.status,
        crate::daemon::dispatch_idle::DispatchStatus::Pending,
        "kind=update must NOT flip status (watchdog stays armed)"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn hook_fixup_team_dispatch_records_pending_via_default_threshold() {
    // L2 fixup default-threshold injection: sender in fixup team,
    // kind=task, no explicit expect_reply_within_secs → sidecar
    // recorded with the 600s default.
    let home = tmp_home("hook-fixup-default");
    write_fixup_fleet(&home, &["fixup-lead", "fixup-reviewer"]);
    let ctx = test_ctx(&home);
    let result = handle_send(
        &json!({
            "from": "fixup-lead",
            "target": "fixup-reviewer",
            "text": "[task] do the thing",
            "kind": "task",
            "task_id": "t-fixup-default",
        }),
        &ctx,
    );
    assert_eq!(result["ok"], true, "dispatch must succeed: {result}");
    let pending = crate::daemon::dispatch_idle::list_pending(&home);
    let entry = pending
        .iter()
        .find(|p| p.correlation_id.as_deref() == Some("t-fixup-default"))
        .expect("fixup-team dispatch must seed a sidecar via L2 default");
    assert_eq!(entry.dispatcher, "fixup-lead");
    assert_eq!(entry.target, "fixup-reviewer");
    assert_eq!(
        entry.threshold_secs,
        crate::daemon::dispatch_idle::team_nudge::DEFAULT_DISPATCH_THRESHOLD_SECS,
        "L2 must inject the team default threshold (#2031: 1800s)"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// RED: a task dispatch must key delivery tracking by its leaf `task_id`, not
/// by a stale umbrella `correlation_id`; a terminal report then closes the leaf
/// while leaving the legitimate umbrella task open.
#[test]
fn task_dispatch_mismatch_tracks_and_closes_leaf_only() {
    let home = tmp_home("task-correlation-leaf");
    write_fixup_fleet(&home, &["fixup-lead", "fixup-reviewer"]);
    let leaf = crate::tasks::handle(
        &home,
        "fixup-lead",
        &json!({
            "action": "create",
            "title": "leaf task",
            "assignee": "fixup-reviewer",
        }),
    )["id"]
        .as_str()
        .expect("leaf task id")
        .to_string();
    let umbrella = crate::tasks::handle(
        &home,
        "fixup-lead",
        &json!({
            "action": "create",
            "title": "umbrella task",
            "assignee": "fixup-reviewer",
        }),
    )["id"]
        .as_str()
        .expect("umbrella task id")
        .to_string();
    let ctx = test_ctx(&home);

    let dispatched = handle_send(
        &json!({
            "from": "fixup-lead",
            "target": "fixup-reviewer",
            "text": "[delegate_task] leaf",
            "kind": "task",
            "task_id": leaf,
            "correlation_id": umbrella,
            "expect_reply_within_secs": 600,
        }),
        &ctx,
    );
    assert_eq!(
        dispatched["ok"], true,
        "task dispatch must succeed: {dispatched}"
    );
    let delivered = crate::inbox::drain(&home, "fixup-reviewer")
        .into_iter()
        .find(|m| m.text == "[delegate_task] leaf")
        .expect("task message must be delivered to the target inbox");
    assert_eq!(delivered.task_id.as_deref(), Some(leaf.as_str()));
    assert_eq!(
        delivered.correlation_id.as_deref(),
        Some(leaf.as_str()),
        "delivered task message must use task_id as canonical correlation"
    );
    let pending = crate::daemon::dispatch_idle::list_pending(&home);
    assert!(
        pending
            .iter()
            .any(|p| p.correlation_id.as_deref() == Some(leaf.as_str())),
        "dispatch tracking must use the leaf task_id: {pending:?}"
    );
    assert!(
        pending
            .iter()
            .all(|p| p.correlation_id.as_deref() != Some(umbrella.as_str())),
        "stale umbrella correlation must not receive the leaf sidecar: {pending:?}"
    );

    let reported = handle_send(
        &json!({
            "from": "fixup-reviewer",
            "target": "fixup-lead",
            "text": "leaf complete",
            "kind": "report",
            "correlation_id": leaf,
            "terminal": true,
        }),
        &ctx,
    );
    assert_eq!(
        reported["ok"], true,
        "terminal report must deliver: {reported}"
    );
    let listed = crate::tasks::handle(
        &home,
        "fixup-lead",
        &json!({"action": "list", "include_history": true}),
    );
    let tasks = listed["tasks"].as_array().expect("task list");
    let status = |id: &str| {
        tasks
            .iter()
            .find(|task| task["id"].as_str() == Some(id))
            .and_then(|task| task["status"].as_str())
    };
    assert_eq!(
        status(&leaf),
        Some("done"),
        "leaf must auto-close: {tasks:?}"
    );
    assert_eq!(
        status(&umbrella),
        Some("open"),
        "umbrella must remain open: {tasks:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #2099 rework (PR #2108, reviewer-2 catch): close the SECOND ~30min nag
/// channel. A fixup-team dispatch auto-arms dispatch_idle at the 1800s team
/// default; a fire-and-forget dispatch (`no_report_expected=true`) must
/// record NO sidecar, so the watchdog never fires
/// `dispatch_idle_threshold_exceeded` (dispatcher) / `[team-watchdog]`
/// (target). Channel 1 (the DispatchEntry sweep) is pinned separately by
/// `dispatch_tracking::tests::sweep_stuck_skips_no_report_expected_2099`.
///
/// REGRESSION-PROOF: drop the `no_report_expected` short-circuit in
/// `track_dispatch` → the flagged dispatch seeds a sidecar and the first
/// assertion fails.
#[test]
fn no_report_expected_dispatch_records_no_dispatch_idle_sidecar_2099() {
    let home = tmp_home("ff-no-dispatch-idle");
    write_fixup_fleet(&home, &["fixup-lead", "fixup-reviewer"]);
    let ctx = test_ctx(&home);

    // Flagged fire-and-forget kind=task → NO dispatch_idle sidecar.
    let flagged = handle_send(
        &json!({
            "from": "fixup-lead",
            "target": "fixup-reviewer",
            "text": "[delegate_task] fire and forget",
            "kind": "task",
            "task_id": "t-ff",
            "no_report_expected": true,
        }),
        &ctx,
    );
    assert_eq!(
        flagged["ok"], true,
        "flagged dispatch must succeed: {flagged}"
    );
    let pending = crate::daemon::dispatch_idle::list_pending(&home);
    assert!(
            !pending
                .iter()
                .any(|p| p.correlation_id.as_deref() == Some("t-ff")),
            "fire-and-forget dispatch must NOT seed a dispatch_idle sidecar (no ~1800s nag): {pending:?}"
        );

    // Control: an UNflagged kind=task to the same team STILL seeds a sidecar
    // (default unchanged — the 1800s watchdog arms as before).
    let normal = handle_send(
        &json!({
            "from": "fixup-lead",
            "target": "fixup-reviewer",
            "text": "[delegate_task] normal work",
            "kind": "task",
            "task_id": "t-normal",
        }),
        &ctx,
    );
    assert_eq!(
        normal["ok"], true,
        "unflagged dispatch must succeed: {normal}"
    );
    let pending2 = crate::daemon::dispatch_idle::list_pending(&home);
    assert!(
        pending2
            .iter()
            .any(|p| p.correlation_id.as_deref() == Some("t-normal")),
        "unflagged dispatch still seeds a sidecar (default unchanged): {pending2:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn report_clears_sidecar_via_task_id_fallback_1525() {
    // #1525 RED→GREEN: a kind=task dispatch keyed by task_id (no explicit
    // correlation_id) seeds a sidecar via the `correlation_id.or(task_id)`
    // record key. The dispatchee's kind=report carries the id in `task_id`
    // (correlation_id empty) — the clear path must use the SAME fallback,
    // else `mark_resolved`'s exact lookup never runs and the completed
    // dispatch's sidecar stays `pending` → spurious nudge once Idle.
    //
    // REGRESSION-PROOF: revert the clear key to `msg.correlation_id` only →
    // the report's correlation_id is None, mark_resolved is skipped, the
    // sidecar stays `pending`, and the final assertion fails.
    let home = tmp_home("report-clears-1525");
    write_fixup_fleet(&home, &["fixup-lead", "fixup-reviewer"]);
    let ctx = test_ctx(&home);

    // Dispatch: lead → reviewer, kind=task, keyed by task_id only.
    let dispatched = handle_send(
        &json!({
            "from": "fixup-lead",
            "target": "fixup-reviewer",
            "text": "[task] review the thing",
            "kind": "task",
            "task_id": "t-1525-x",
        }),
        &ctx,
    );
    assert_eq!(
        dispatched["ok"], true,
        "dispatch must succeed: {dispatched}"
    );
    let seeded = crate::daemon::dispatch_idle::list_pending(&home);
    assert_eq!(
        seeded
            .iter()
            .find(|p| p.correlation_id.as_deref() == Some("t-1525-x"))
            .map(|p| p.status),
        Some(crate::daemon::dispatch_idle::DispatchStatus::Pending),
        "sidecar must seed pending, keyed by task_id"
    );

    // Verdict: reviewer → lead, kind=report, id ONLY in task_id (no correlation_id).
    let reported = handle_send(
        &json!({
            "from": "fixup-reviewer",
            "target": "fixup-lead",
            "text": "VERIFIED",
            "kind": "report",
            "task_id": "t-1525-x",
        }),
        &ctx,
    );
    assert_eq!(reported["ok"], true, "report must deliver: {reported}");

    // #1525: the report must clear the sidecar. mark_resolved now DELETES the
    // file (rather than flipping to Resolved), so the sidecar must be absent.
    // Pre-fix it stayed `pending` → fired a nudge.
    let after = crate::daemon::dispatch_idle::list_pending(&home);
    assert!(
        after
            .iter()
            .all(|p| p.correlation_id.as_deref() != Some("t-1525-x")),
        "#1525: a report carrying the id in task_id must clear (delete) the \
             sidecar via the correlation_id.or(task_id) symmetry"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn hook_non_fixup_team_dispatch_now_records_via_default_threshold_multiteam() {
    // t-dehardcode-fixup-nudge-multiteam: a NON-fixup team's dispatcher with
    // no explicit threshold now RECORDS a sidecar via the global default (was
    // gated to the fixup team → no sidecar). The teamless (solo) case still
    // records nothing — covered by the team_nudge unit tests.
    let home = tmp_home("hook-non-fixup-records");
    // Distinct team that ISN'T fixup.
    std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "schema_version: 1\n\
             instances:\n  research-lead:\n    backend: claude\n  research-dev:\n    backend: claude\n\
             teams:\n  research:\n    members: [research-lead, research-dev]\n    orchestrator: research-lead\n",
        )
        .unwrap();
    let ctx = test_ctx(&home);
    let result = handle_send(
        &json!({
            "from": "research-lead",
            "target": "research-dev",
            "text": "[task] do the thing",
            "kind": "task",
            "task_id": "t-non-fixup",
        }),
        &ctx,
    );
    assert_eq!(result["ok"], true);
    let pending = crate::daemon::dispatch_idle::list_pending(&home);
    assert!(
        pending
            .iter()
            .any(|p| p.correlation_id.as_deref() == Some("t-non-fixup")),
        "any-team dispatch must now record a sidecar via the default threshold (multi-team)"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn hook_explicit_threshold_overrides_team_default() {
    // Explicit expect_reply_within_secs wins for any team
    // (including non-fixup). Other teams opt in this way.
    let home = tmp_home("hook-explicit-threshold");
    std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "schema_version: 1\n\
             instances:\n  research-lead:\n    backend: claude\n  research-dev:\n    backend: claude\n\
             teams:\n  research:\n    members: [research-lead, research-dev]\n    orchestrator: research-lead\n",
        )
        .unwrap();
    let ctx = test_ctx(&home);
    let result = handle_send(
        &json!({
            "from": "research-lead",
            "target": "research-dev",
            "text": "[task] research thing",
            "kind": "task",
            "task_id": "t-explicit",
            "expect_reply_within_secs": 1200_i64,
        }),
        &ctx,
    );
    assert_eq!(result["ok"], true);
    let pending = crate::daemon::dispatch_idle::list_pending(&home);
    let entry = pending
        .iter()
        .find(|p| p.correlation_id.as_deref() == Some("t-explicit"))
        .expect("explicit-threshold dispatch records sidecar");
    assert_eq!(
        entry.threshold_secs, 1200,
        "explicit threshold must override team default / absent state"
    );
    std::fs::remove_dir_all(&home).ok();
}

// -----------------------------------------------------------------------
// #982 B-narrow — codex ack-absorption override for replies to drained
// blocker dispatches. The empirical bisect found 8 ack_absorbed events
// today (all target=fixup-reviewer codex / from=fixup-lead kind=update|
// report), so the override predicate must distinguish:
//   B1+B2 positive: drained query/task with matching correlation_id
//                   → override absorption, PTY-surface the reply
//   B3    negative: undrained query/task with matching correlation_id
//                   → keep absorption (recipient hasn't read parent)
//   B4    negative: no matching correlation_id in inbox
//                   → keep absorption (no blocking context)
//   B5    negative: correlation_id absent from inbound entirely
//                   → keep absorption (cannot key the lookup)
//   B6    invariant: non-codex backend unaffected by override path
// -----------------------------------------------------------------------

fn make_codex_ctx(
    home: &std::path::Path,
    codex_agent: &str,
    sender: &str,
) -> (
    &'static agent::AgentRegistry,
    HandlerCtx<'static>,
    std::path::PathBuf,
) {
    setup_team_env(
        home,
        &[codex_agent, sender],
        &[("dev", &[codex_agent, sender])],
    );
    // Flip the codex_agent backend in fleet.yaml.
    let yaml = std::fs::read_to_string(crate::fleet::fleet_yaml_path(home)).unwrap();
    let yaml = yaml.replace(
        &format!("  {codex_agent}:\n    backend: claude"),
        &format!("  {codex_agent}:\n    backend: codex"),
    );
    std::fs::write(crate::fleet::fleet_yaml_path(home), yaml).ok();

    let registry: &'static agent::AgentRegistry =
        Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
    let spawn_cfg = crate::agent::SpawnConfig {
        name: codex_agent,
        backend: None,
        backend_command: crate::default_shell(),
        args: &[],
        spawn_mode: crate::backend::SpawnMode::Fresh,
        cols: 80,
        rows: 24,
        env: None,
        working_dir: None,
        submit_key: "\r",
        home: Some(home),
        crash_tx: None,
        shutdown: None,
    };
    crate::agent::spawn_agent(&spawn_cfg, registry).expect("spawn");
    {
        let mut reg = agent::lock_registry(registry);
        if let Some(h) = reg.values_mut().find(|h| h.name.as_str() == codex_agent) {
            h.backend_command = "codex".to_string();
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(150));

    let configs: &'static crate::api::ConfigRegistry =
        Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
    let externals: &'static agent::ExternalRegistry =
        Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
    let home_ref: &'static std::path::Path = Box::leak(Box::new(home.to_path_buf()));
    let ctx = HandlerCtx {
        registry,
        configs,
        externals,
        notifier: None,
        home: home_ref,
        capability: crate::api::RestartCapability::Unsupported,
        app_restart: None,
        post_flush: crate::api::app_restart::PostFlushSlot::new(),
        shutdown: None,
    };
    (registry, ctx, home.to_path_buf())
}

fn seed_drained_blocker(home: &std::path::Path, target: &str, kind: &str, corr: &str) {
    let msg = crate::inbox::InboxMessage {
        schema_version: 0,
        id: Some(format!("m-blocker-{corr}")),
        read_at: Some(chrono::Utc::now().to_rfc3339()),
        delivering_at: None,
        thread_id: None,
        parent_id: None,
        task_id: None,
        force_meta: None,
        correlation_id: Some(corr.to_string()),
        reviewed_head: None,
        report_purpose: Default::default(),
        validated_code_review: None,
        from: "from:fixup-lead".to_string(),
        text: format!("seeded blocker {kind}"),
        kind: Some(kind.to_string()),
        timestamp: chrono::Utc::now().to_rfc3339(),
        channel: None,
        delivery_mode: None,
        attachments: vec![],
        in_reply_to_msg_id: None,
        in_reply_to_excerpt: None,
        reply_target: None,
        superseded_by: None,
        from_id: None,
        broadcast_context: None,
        eta_minutes: None,
        reporting_cadence: None,
        worktree_binding_required: None,
        pr_number: None,
        terminal: None,
        delivery_nonce: None,
        review_assignment: None,
        ci_handoff_episode: None,
        ci_handoff_class: None,
    };
    crate::inbox::enqueue(home, target, msg).expect("seed blocker");
}

fn cleanup_registry(registry: &agent::AgentRegistry, name: &str) {
    let reg = agent::lock_registry(registry);
    if let Some(h) = reg.values().find(|h| h.name.as_str() == name) {
        let _ = h.child.lock().kill();
    }
}

#[test]
fn b1_codex_report_overrides_absorption_when_query_drained() {
    let home = tmp_home("982-b1");
    let (registry, ctx, home_path) = make_codex_ctx(&home, "codex-agent", "sender");
    seed_drained_blocker(&home_path, "codex-agent", "query", "corr-b1");

    let result = handle_send(
        &json!({
            "from": "sender",
            "target": "codex-agent",
            "text": "reply to query",
            "kind": "report",
            "correlation_id": "corr-b1",
        }),
        &ctx,
    );
    assert_eq!(result["ok"], true);
    assert_eq!(
        result["delivery_mode"].as_str(),
        Some("pty"),
        "B-narrow: report to codex must PTY-surface when matching drained query: {result}"
    );
    cleanup_registry(registry, "codex-agent");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn b2_codex_update_overrides_absorption_when_task_drained() {
    let home = tmp_home("982-b2");
    let (registry, ctx, home_path) = make_codex_ctx(&home, "codex-agent", "sender");
    seed_drained_blocker(&home_path, "codex-agent", "task", "corr-b2");

    let result = handle_send(
        &json!({
            "from": "sender",
            "target": "codex-agent",
            "text": "phase-transition update",
            "kind": "update",
            "correlation_id": "corr-b2",
        }),
        &ctx,
    );
    assert_eq!(result["ok"], true);
    assert_eq!(
        result["delivery_mode"].as_str(),
        Some("pty"),
        "B-narrow: update to codex must PTY-surface when matching drained task: {result}"
    );
    cleanup_registry(registry, "codex-agent");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn b3_codex_report_keeps_absorption_when_blocker_undrained() {
    let home = tmp_home("982-b3");
    let (registry, ctx, home_path) = make_codex_ctx(&home, "codex-agent", "sender");
    // Seed an UNDRAINED query.
    let mut msg = crate::inbox::InboxMessage {
        schema_version: 0,
        id: Some("m-undrained".to_string()),
        read_at: None, // ← key: not drained
        delivering_at: None,
        thread_id: None,
        parent_id: None,
        task_id: None,
        force_meta: None,
        correlation_id: Some("corr-b3".to_string()),
        reviewed_head: None,
        report_purpose: Default::default(),
        validated_code_review: None,
        from: "from:fixup-lead".to_string(),
        text: "undrained query".to_string(),
        kind: Some("query".to_string()),
        timestamp: chrono::Utc::now().to_rfc3339(),
        channel: None,
        delivery_mode: None,
        attachments: vec![],
        in_reply_to_msg_id: None,
        in_reply_to_excerpt: None,
        reply_target: None,
        superseded_by: None,
        from_id: None,
        broadcast_context: None,
        eta_minutes: None,
        reporting_cadence: None,
        worktree_binding_required: None,
        pr_number: None,
        terminal: None,
        delivery_nonce: None,
        review_assignment: None,
        ci_handoff_episode: None,
        ci_handoff_class: None,
    };
    msg.read_at = None;
    crate::inbox::enqueue(&home_path, "codex-agent", msg).expect("seed");

    let result = handle_send(
        &json!({
            "from": "sender",
            "target": "codex-agent",
            "text": "premature reply",
            "kind": "report",
            "correlation_id": "corr-b3",
        }),
        &ctx,
    );
    assert_eq!(result["ok"], true);
    assert_eq!(
        result["delivery_mode"].as_str(),
        Some("inbox_only"),
        "B-narrow: undrained blocker leaves codex absorption intact: {result}"
    );
    cleanup_registry(registry, "codex-agent");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn b4_codex_report_keeps_absorption_when_no_correlation_match() {
    let home = tmp_home("982-b4");
    let (registry, ctx, home_path) = make_codex_ctx(&home, "codex-agent", "sender");
    // Seed a drained query with a DIFFERENT correlation id.
    seed_drained_blocker(&home_path, "codex-agent", "query", "corr-OTHER");

    let result = handle_send(
        &json!({
            "from": "sender",
            "target": "codex-agent",
            "text": "stray report",
            "kind": "report",
            "correlation_id": "corr-b4",
        }),
        &ctx,
    );
    assert_eq!(result["ok"], true);
    assert_eq!(
        result["delivery_mode"].as_str(),
        Some("inbox_only"),
        "B-narrow: no correlation match keeps absorption: {result}"
    );
    cleanup_registry(registry, "codex-agent");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn b5_codex_report_keeps_absorption_when_correlation_id_absent() {
    let home = tmp_home("982-b5");
    let (registry, ctx, home_path) = make_codex_ctx(&home, "codex-agent", "sender");
    seed_drained_blocker(&home_path, "codex-agent", "query", "corr-ANY");

    // Inbound omits correlation_id entirely.
    let result = handle_send(
        &json!({
            "from": "sender",
            "target": "codex-agent",
            "text": "manual update",
            "kind": "update",
        }),
        &ctx,
    );
    assert_eq!(result["ok"], true);
    assert_eq!(
        result["delivery_mode"].as_str(),
        Some("inbox_only"),
        "B-narrow: no correlation_id on inbound keeps absorption: {result}"
    );
    cleanup_registry(registry, "codex-agent");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn b6_non_codex_backend_pty_path_unchanged_by_override() {
    // Sanity invariant: non-codex backends always PTY today (no absorption);
    // the override predicate must not redirect them through inbox_only.
    let home = tmp_home("982-b6");
    // Use the default claude-flavored spawn from setup_team_env.
    setup_team_env(
        &home,
        &["claude-agent", "sender"],
        &[("dev", &["claude-agent", "sender"])],
    );
    let registry: &'static agent::AgentRegistry =
        Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
    let spawn_cfg = crate::agent::SpawnConfig {
        name: "claude-agent",
        backend: None,
        backend_command: crate::default_shell(),
        args: &[],
        spawn_mode: crate::backend::SpawnMode::Fresh,
        cols: 80,
        rows: 24,
        env: None,
        working_dir: None,
        submit_key: "\r",
        home: Some(&home),
        crash_tx: None,
        shutdown: None,
    };
    crate::agent::spawn_agent(&spawn_cfg, registry).expect("spawn");
    std::thread::sleep(std::time::Duration::from_millis(150));

    let configs: &'static crate::api::ConfigRegistry =
        Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
    let externals: &'static agent::ExternalRegistry =
        Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
    let home_ref: &'static std::path::Path = Box::leak(Box::new(home.clone()));
    let ctx = HandlerCtx {
        registry,
        configs,
        externals,
        notifier: None,
        home: home_ref,
        capability: crate::api::RestartCapability::Unsupported,
        app_restart: None,
        post_flush: crate::api::app_restart::PostFlushSlot::new(),
        shutdown: None,
    };
    seed_drained_blocker(&home, "claude-agent", "query", "corr-b6");

    let result = handle_send(
        &json!({
            "from": "sender",
            "target": "claude-agent",
            "text": "reply for claude",
            "kind": "report",
            "correlation_id": "corr-b6",
        }),
        &ctx,
    );
    assert_eq!(result["ok"], true);
    assert_eq!(
        result["delivery_mode"].as_str(),
        Some("pty"),
        "non-codex backend always PTY regardless of correlation predicate: {result}"
    );
    cleanup_registry(registry, "claude-agent");
    std::fs::remove_dir_all(&home).ok();
}

// ── #1065 unified routing tests (kind=task → enqueue_with_idle_hint) ──
//
// Before #1065: handle_send used `enqueue` + `compose_aware_send(inject_msg)`
// where inject_msg was the full `[AGEND-MSG] header (use inbox tool)` form
// (or `[from:lead] body` for short messages). Operator-observed pattern:
// ~10% reviewer dispatches via kind=task land but the agent never
// executes — content-size pressure extends codex's typed-inject write
// window past the 50ms pre-submit delay, race-condition on the `\r`.
//
// After #1065: handle_send routes through `enqueue_with_idle_hint`
// (same path as daemon-emitted [ci-ready-for-action] auto-wake which has
// empirically reliable 4/4 fire+execute). Both paths emit the SAME short
// `[AGEND-MSG-PENDING]` hint. Body stays in inbox JSONL (durable).

/// T1 (#1065 RED): structural pin — the SEND delivery path must route
/// through `enqueue_with_idle_hint` (NOT `compose_aware_send`).
/// Post-#2454 the routing logic lives in the neutral service
/// (`agent_ops/messaging.rs`), not in the thin API adapter.
#[test]
fn handle_send_routes_through_enqueue_with_idle_hint() {
    let source = include_str!("../../../agent_ops/messaging.rs");
    let prod_end = source.find("#[cfg(test)]").unwrap_or(source.len());
    let prod_src = &source[..prod_end];
    assert!(
        prod_src.contains("enqueue_with_idle_hint("),
        "#1065 invariant: neutral service must CALL enqueue_with_idle_hint( \
             (same path as daemon auto-wake), not merely mention it"
    );
    assert!(
        !prod_src.contains("compose_aware_send("),
        "#1065 invariant: neutral service must NOT use compose_aware_send \
             for the inject site post-#1065"
    );
}

/// T2 (#1065): kind=task body persists in inbox JSONL regardless of
/// the routing path. Sanity guard: the durable inbox entry must
/// survive the refactor.
#[test]
fn kind_task_body_persisted_in_inbox_jsonl() {
    let home = tmp_home("1065-t2-body");
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  reviewer:\n    backend: claude\n  lead:\n    backend: claude\n",
    )
    .ok();
    let ctx = test_ctx(&home);
    let body = "[delegate_task] long task body".repeat(20);
    let result = handle_send(
        &json!({
            "from": "lead",
            "target": "reviewer",
            "text": body,
            "kind": "task",
        }),
        &ctx,
    );
    assert_eq!(result["ok"], true, "send must succeed: {result}");

    // Read whatever JSONL was written under <home>/inbox/. The path is
    // either name-based or id-based depending on whether fleet.yaml has
    // backfilled an InstanceId — collapse both into one read.
    let inbox_dir = home.join("inbox");
    let mut combined = String::new();
    if let Ok(rd) = std::fs::read_dir(&inbox_dir) {
        for e in rd.flatten() {
            if let Ok(c) = std::fs::read_to_string(e.path()) {
                combined.push_str(&c);
            }
        }
    }
    assert!(
        combined.contains("delegate_task"),
        "kind=task body must persist in inbox JSONL post-#1065: {combined:?}"
    );
    assert!(
        combined.contains("\"kind\":\"task\""),
        "kind=task tag must be preserved in JSONL: {combined:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// T3 (#1065 + #982 preservation): codex same-team kind=update
/// remains ack-absorbed (inbox_only + ack_absorbed event log).
/// The #982 contract must survive the routing refactor.
#[test]
fn kind_update_codex_same_team_remains_ack_absorbed() {
    let home = tmp_home("1065-t3-codex-update");
    let (registry, ctx, home_path) = make_codex_ctx(&home, "codex-rev", "lead");
    let result = handle_send(
        &json!({
            "from": "lead",
            "target": "codex-rev",
            "text": "status update",
            "kind": "update",
        }),
        &ctx,
    );
    assert_eq!(result["ok"], true);
    assert_eq!(
        result["delivery_mode"].as_str(),
        Some("inbox_only"),
        "codex same-team kind=update must remain ack-absorbed (#982): {result}"
    );
    assert!(
        audit_log_contains(&home_path, "ack_absorbed"),
        "ack_absorbed event must be logged"
    );
    cleanup_registry(registry, "codex-rev");
    std::fs::remove_dir_all(&home).ok();
}

/// T4 (#1065 + #612 preservation): codex kind=report from "general"
/// bus to a different-team codex target still injects (delivery_mode=pty).
/// Cross-team unicast is blocked at Rule 3 (line 78+) so the only way
/// to exercise the cross-team-codex-not-absorbed semantics is via the
/// general bus (Rule 2). The #612 invariant must survive the routing
/// refactor — `enqueue_with_idle_hint` must run, NOT ack-absorb.
#[test]
fn kind_report_cross_team_codex_via_general_still_injects() {
    let home = tmp_home("1065-t4-general");
    let yaml = "instances:\n  general:\n    backend: claude\n  \
                    codex-rev:\n    backend: codex\n    id: 0c0c0c0c-0000-4000-8000-000000000004\nteams:\n  \
                    team-a:\n    members:\n      - general\n    \
                    created_at: \"2026-01-01T00:00:00Z\"\n  \
                    team-b:\n    members:\n      - codex-rev\n    \
                    created_at: \"2026-01-01T00:00:00Z\"\n";
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).ok();

    let registry: &'static agent::AgentRegistry =
        Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
    let spawn_cfg = crate::agent::SpawnConfig {
        name: "codex-rev",
        backend: None,
        backend_command: crate::default_shell(),
        args: &[],
        spawn_mode: crate::backend::SpawnMode::Fresh,
        cols: 80,
        rows: 24,
        env: None,
        working_dir: None,
        submit_key: "\r",
        home: Some(&home),
        crash_tx: None,
        shutdown: None,
    };
    crate::agent::spawn_agent(&spawn_cfg, registry).expect("spawn");
    {
        let mut reg = agent::lock_registry(registry);
        if let Some(h) = reg.values_mut().find(|h| h.name.as_str() == "codex-rev") {
            h.backend_command = "codex".to_string();
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(150));

    let configs: &'static crate::api::ConfigRegistry =
        Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
    let externals: &'static agent::ExternalRegistry =
        Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
    let home_ref: &'static std::path::Path = Box::leak(Box::new(home.clone()));
    let ctx = HandlerCtx {
        registry,
        configs,
        externals,
        notifier: None,
        home: home_ref,
        capability: crate::api::RestartCapability::Unsupported,
        app_restart: None,
        post_flush: crate::api::app_restart::PostFlushSlot::new(),
        shutdown: None,
    };
    let result = handle_send(
        &json!({
            "from": "general",
            "target": "codex-rev",
            "text": "cross-team report via general",
            "kind": "report",
        }),
        &ctx,
    );
    assert_eq!(result["ok"], true, "general → cross-team send: {result}");
    assert_eq!(
        result["delivery_mode"].as_str(),
        Some("pty"),
        "cross-team codex kind=report must still inject (#612): {result}"
    );
    assert!(
        !audit_log_contains(&home, "ack_absorbed"),
        "ack_absorbed must NOT be logged for cross-team report"
    );
    cleanup_registry(registry, "codex-rev");
    std::fs::remove_dir_all(&home).ok();
}

/// T5 (#1065): probabilistic race regression — pinned at the unit-test
/// level requires a real codex backend. Kept as documentation that an
/// empirical reproduce protocol exists; runs only under `--ignored`.
/// See /tmp/dialectic-1065-primary-dev.md §6 for the operator-side
/// 10-trial reproduce plan.
#[test]
#[ignore = "requires real codex backend; runs on operator-side empirical protocol"]
fn submit_race_regression_under_long_inject_documented() {
    // Placeholder: pin protocol via doc-comment + ignored marker. The
    // refactor is structurally GREEN per T1; the race regression is
    // observable only through real backend reproduce.
}

/// T6 (#1065 + dev-2 nit): absent target (fleet-defined but not in
/// registry) → inbox_only with no PTY emit. Preserves the original
/// fallback at messaging.rs's `else` branch.
#[test]
fn absent_target_falls_back_to_inbox_only() {
    let home = tmp_home("1065-t6-absent");
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  offline-rev:\n    backend: claude\n  lead:\n    backend: claude\n",
    )
    .ok();
    let ctx = test_ctx(&home);
    let result = handle_send(
        &json!({
            "from": "lead",
            "target": "offline-rev",
            "text": "[delegate_task] do X",
            "kind": "task",
        }),
        &ctx,
    );
    assert_eq!(result["ok"], true);
    assert_eq!(
        result["delivery_mode"].as_str(),
        Some("inbox_only"),
        "absent target must receive inbox_only delivery: {result}"
    );
    // Inbox JSONL still gets the entry — durable path preserved.
    // Read whatever JSONL was written; path may be name- or id-based.
    let inbox_dir = home.join("inbox");
    let mut combined = String::new();
    if let Ok(rd) = std::fs::read_dir(&inbox_dir) {
        for e in rd.flatten() {
            if let Ok(c) = std::fs::read_to_string(e.path()) {
                combined.push_str(&c);
            }
        }
    }
    assert!(
        combined.contains("\"kind\":\"task\""),
        "inbox JSONL must persist the task entry for absent target: {combined:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Build a single-agent registry with `name` present and forced Idle, so
/// `collect_poll_reminders` picks it up (the agent that was absent at send time
/// has now come online). Deterministic: `mk_test_handle` attaches no state-detect
/// listener, so the Idle state can't be raced away. `#[cfg(unix)]`: `mk_test_handle`
/// is `cfg(all(test, unix))`.
#[cfg(unix)]
fn idle_registry(name: &str) -> agent::AgentRegistry {
    let id = crate::types::InstanceId::default();
    let handle = crate::agent::mk_test_handle(name, id);
    handle.core.lock().state.current = crate::state::AgentState::Idle;
    let reg: agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    reg.lock().insert(id, handle);
    reg
}

/// #t-…61487 (r6-required) — routing integration through the REAL send path
/// (`handle_send` → `route_and_deliver`), NOT a bare `enqueue` (the gap that sank
/// v1, [[unit_test_injected_inputs_hide_discovery_path]]). A `report` to an ABSENT
/// target takes `route_and_deliver`'s `!target_in_registry` branch — a bare
/// `enqueue` with NO arrival inject — so the kind-agnostic poll-reminder is its
/// ONLY recovery wake. This pins NO SILENT LOSS: the absent-target report still
/// gets its INITIAL poll-reminder wake. v1's obligation-only count killed this
/// (report → not counted → never woken) → r6 REJECT; the revert to kind-agnostic
/// `unread_count` restores it, and THIS test (driven through the real router, not
/// bare enqueue) guards the regression. The complementary NO-RE-FIRE invariant
/// (reclaim must not re-page for a drained report) is pinned deterministically in
/// `daemon::poll_reminder`'s `reclaim_does_not_rearm_for_non_obligation_report`
/// (a name-based fixture — the real send path backfills a uuid inbox whose
/// name→uuid migration makes a by-name drain/reclaim non-deterministic).
///
/// `#[cfg(unix)]`: `idle_registry` → `mk_test_handle` is `cfg(all(test, unix))`.
#[cfg(unix)]
#[test]
fn report_to_absent_target_still_gets_initial_poll_wake() {
    let home = tmp_home("absent-report-route");
    let target = "offline-rev-route";
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        format!("instances:\n  {target}:\n    backend: claude\n  peer:\n    backend: claude\n"),
    )
    .ok();

    // ── Send a report via the REAL handler; absent target → inbox_only (no inject). ──
    let ctx = test_ctx(&home); // empty registry → target is absent
    let result = handle_send(
        &json!({
            "from": "peer",
            "target": target,
            "text": "VERIFIED",
            "kind": "report",
        }),
        &ctx,
    );
    assert_eq!(result["ok"], true, "send must succeed: {result}");
    assert_eq!(
        result["delivery_mode"].as_str(),
        Some("inbox_only"),
        "absent target → inbox_only (no inject), so the poll-reminder is the only \
             recovery wake: {result}"
    );

    // ── Agent comes online idle: the INITIAL wake MUST fire (no silent loss). ──
    let registry = idle_registry(target);
    crate::daemon::poll_reminder::remove_agent(target); // clear dedup ledger
    let first = crate::daemon::poll_reminder::collect_poll_reminders(&home, &registry);
    assert_eq!(
        first.len(),
        1,
        "absent-target report must still get its INITIAL poll-reminder wake \
             (kind-agnostic unread_count) — the v1 silent-loss regression guard"
    );
    assert!(first[0].1.contains("unread=1"), "got: {}", first[0].1);

    crate::daemon::poll_reminder::remove_agent(target);
    std::fs::remove_dir_all(&home).ok();
}

/// #1268: kind=query must NOT produce a dispatch_idle sidecar.
/// (Replaces #1129 test — query sidecars caused false-positive
/// watchdog nudges on broadcast queries.)
#[test]
fn hook_kind_query_does_not_create_dispatch_sidecar() {
    let home = tmp_home("1268-query-no-sidecar");
    write_fixup_fleet(&home, &["fixup-lead", "fixup-dev"]);
    let ctx = test_ctx(&home);
    let result = handle_send(
        &json!({
            "from": "fixup-lead",
            "target": "fixup-dev",
            "text": "what is the status?",
            "kind": "query",
            "expect_reply_within_secs": 300,
        }),
        &ctx,
    );
    assert_eq!(result["ok"], true, "query must succeed: {result}");
    let pending = crate::daemon::dispatch_idle::list_pending(&home);
    assert!(
        pending.iter().all(|p| p.target != "fixup-dev"),
        "kind=query must not create a dispatch sidecar: {pending:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #1149: send kind=task without task_id auto-creates a task and
/// stamps it on the outbound message + response.
#[test]
fn auto_create_task_on_send_kind_task_without_task_id() {
    let home = tmp_home("1149-auto-create");
    write_fixup_fleet(&home, &["fixup-lead", "fixup-dev"]);
    let ctx = test_ctx(&home);
    let result = handle_send(
        &json!({
            "from": "fixup-lead",
            "target": "fixup-dev",
            "text": "[delegate_task] implement the widget\ndetailed description here",
            "kind": "task",
            "branch": "feat/widget",
        }),
        &ctx,
    );
    assert_eq!(result["ok"], true, "send must succeed: {result}");
    // Response must contain auto-generated task_id.
    let task_id = result["task_id"]
        .as_str()
        .expect("response must include task_id");
    assert!(
        task_id.starts_with("t-"),
        "auto-generated task_id must use t- prefix: {task_id}"
    );
    // Task must exist on the board.
    let tasks = crate::tasks::handle(
        &home,
        "fixup-lead",
        &json!({"action": "list", "include_history": true}),
    );
    let task_list = tasks["tasks"].as_array().expect("tasks array");
    let created = task_list
        .iter()
        .find(|t| t["id"].as_str() == Some(task_id))
        .expect("auto-created task must exist on board");
    assert_eq!(
        created["title"].as_str().unwrap(),
        "[delegate_task] implement the widget"
    );
    assert_eq!(created["branch"].as_str(), Some("feat/widget"));
    assert_eq!(created["assignee"].as_str(), Some("fixup-dev"));
    assert_eq!(created["status"].as_str().unwrap(), "open");
    // Inbox message must carry the task_id.
    let inbox = crate::inbox::drain(&home, "fixup-dev");
    let msg = inbox
        .iter()
        .find(|m| m.kind.as_deref() == Some("task"))
        .expect("task message must be in inbox");
    assert_eq!(
        msg.task_id.as_deref(),
        Some(task_id),
        "outbound message must carry auto-generated task_id"
    );
    assert_eq!(
        msg.correlation_id.as_deref(),
        Some(task_id),
        "auto-created task message must use task_id as canonical correlation"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #1149: send kind=task WITH task_id does NOT auto-create (backward compat).
#[test]
fn no_auto_create_when_task_id_provided() {
    let home = tmp_home("1149-no-auto");
    write_fixup_fleet(&home, &["fixup-lead", "fixup-dev"]);
    let ctx = test_ctx(&home);
    let result = handle_send(
        &json!({
            "from": "fixup-lead",
            "target": "fixup-dev",
            "text": "do the thing",
            "kind": "task",
            "task_id": "t-existing-123",
        }),
        &ctx,
    );
    assert_eq!(result["ok"], true);
    // Response must NOT contain auto-generated task_id.
    assert!(
        result.get("task_id").is_none(),
        "response must NOT include task_id when caller provided one: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── #2079: short-SHA reviewed_head must still flip merge-ready ──
//
// §3.9 + #1493 producer-fed: drive a REAL verdict-report `InboxMessage`
// carrying an ABBREVIATED `reviewed_head` (the #2078 `7e1d422` shape)
// through the REAL ingestion entry (`process_verdicts`), not a synthetic
// `record_verdict` call — a representative fixture is what catches the
// wiring gap. Pre-fix the exact `==` missed the short SHA → silent buffer →
// 24h TTL.

const FULL_HEAD_2079: &str = "7e1d4228bea3cf7fe2d72aab66015297308b48bc";
const SHORT_HEAD_2079: &str = "7e1d422"; // 7-char hex prefix of FULL_HEAD_2079

fn verdict_report_msg(corr: &str, reviewed_head: &str) -> crate::inbox::InboxMessage {
    crate::inbox::InboxMessage::new_system("system:reviewer", "report", "VERIFIED looks good")
        .with_correlation_id(corr.to_string())
        .with_reviewed_head(reviewed_head.to_string())
}

#[test]
fn short_sha_text_without_typed_receipt_is_inert_2760() {
    use crate::daemon::pr_state;
    let home = tmp_home("2079-shortsha-flip");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();

    // CI observed green at the FULL head_sha (single-review gate).
    pr_state::record_ci_result(
        &home,
        "owner/repo",
        "feat/x",
        FULL_HEAD_2079,
        pr_state::CiConclusion::Green,
        vec!["dev".to_string()],
        pr_state::ReviewClass::Single,
    );

    // A legacy raw report carries an abbreviated head and verdict-looking text.
    assert!(!process_verdicts(
        &home,
        &verdict_report_msg("owner/repo@feat/x", SHORT_HEAD_2079),
    ));

    let state = pr_state::load(&home, "owner/repo", "feat/x").expect("state exists");
    assert!(
        !pr_state::is_merge_ready(&state),
        "task66: verdict-looking text and a SHA prefix must have zero merge authority; state={state:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn short_sha_text_without_receipt_never_enters_buffer_2760() {
    use crate::daemon::pr_state;
    let home = tmp_home("2079-shortsha-buffer");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();

    // Verdict-looking legacy text arrives first; it must not enter any buffer.
    assert!(!process_verdicts(
        &home,
        &verdict_report_msg("owner/repo@feat/x", SHORT_HEAD_2079),
    ));

    // Later CI observation cannot upgrade the old raw text into authority.
    pr_state::record_ci_result(
        &home,
        "owner/repo",
        "feat/x",
        FULL_HEAD_2079,
        pr_state::CiConclusion::Green,
        vec!["dev".to_string()],
        pr_state::ReviewClass::Single,
    );

    let state = pr_state::load(&home, "owner/repo", "feat/x").expect("state exists");
    assert!(
        !pr_state::is_merge_ready(&state),
        "task66: a legacy raw report must never replay as typed receipt evidence; state={state:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── #t-127: reviewer-verdict → review-task bridge (ghost-close + sidecar) ──

fn seed_review_task(home: &std::path::Path, task_id: &str, reviewer: &str) {
    use crate::task_events::{InstanceName, TaskEvent, TaskId};
    let emitter = InstanceName::from("test:seed");
    let tid = TaskId(task_id.into());
    crate::task_events::append_batch(
        home,
        &emitter,
        vec![
            TaskEvent::Created {
                task_id: tid.clone(),
                title: "review PR".into(),
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
                by: InstanceName::from(reviewer),
            },
        ],
    )
    .expect("seed review task");
}

fn task_status_of(home: &std::path::Path, task_id: &str) -> Option<crate::task_events::TaskStatus> {
    crate::task_events::replay(home)
        .unwrap_or_default()
        .tasks
        .get(&crate::task_events::TaskId(task_id.into()))
        .map(|r| r.status)
}

fn sidecar_present(home: &std::path::Path, task_id: &str) -> bool {
    crate::daemon::dispatch_idle::list_pending(home)
        .iter()
        .any(|d| d.correlation_id.as_deref() == Some(task_id))
}

fn t127_verdict(verdict_text: &str, task_id: &str, corr: &str) -> crate::inbox::InboxMessage {
    let verdict = if verdict_text.starts_with("VERIFIED") {
        crate::review_receipt::ReviewVerdict::Verified
    } else if verdict_text.starts_with("REJECTED") {
        crate::review_receipt::ReviewVerdict::Rejected
    } else {
        crate::review_receipt::ReviewVerdict::Unverified
    };
    let mut msg = crate::inbox::InboxMessage::new_system("system:reviewer", "report", verdict_text)
        .with_correlation_id(corr.to_string())
        .with_reviewed_head("7e1d4228bea3cf7fe2d72aab66015297308b48bc".to_string());
    msg.report_purpose = crate::review_receipt::ReportPurpose::CodeReview;
    msg.validated_code_review = Some(crate::review_receipt::ValidatedCodeReviewReceipt::for_test(
        crate::review_receipt::ReviewReceiptSummary {
            receipt_id: format!("review-receipt:{task_id}"),
            source_id: format!("m-{task_id}"),
            evidence_digest: "a".repeat(64),
            assignment_id: uuid::Uuid::new_v4(),
            reviewer_instance_id: crate::types::InstanceId::new(),
            reviewer_name: "reviewer".into(),
            repo: "owner/repo".into(),
            pr_number: 1,
            branch: "feat/x".into(),
            task_id: task_id.into(),
            reviewed_head: "7e1d4228bea3cf7fe2d72aab66015297308b48bc".into(),
            review_class: crate::daemon::pr_state::ReviewClass::Single,
            slot: crate::review_receipt::ReviewSlot::Primary,
            verdict,
        },
    ));
    msg
}

fn record_review_dispatch(home: &std::path::Path, dispatcher: &str, reviewer: &str, tid: &str) {
    crate::daemon::dispatch_idle::record_dispatch(
        home,
        dispatcher,
        reviewer,
        Some(tid),
        "task",
        1800,
    )
    .expect("review dispatch sidecar");
}

/// Case (a) dual-1: verdict carries the TASK id (`corr=t-…`). VERIFIED must
/// auto-close the task (terminal synthesized) AND clear the dispatch sidecar.
#[test]
fn verdict_dual1_taskid_verified_closes_task_and_clears_sidecar_t127() {
    let home = tmp_home("t127-dual1");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let reviewer = "fixup-reviewer-6";
    seed_review_task(&home, "t-rev-1", reviewer);
    record_review_dispatch(&home, "fixup-lead", reviewer, "t-rev-1");

    bridge_verdict_to_review_task(
        &home,
        reviewer,
        &t127_verdict("VERIFIED looks good", "t-rev-1", "t-rev-1"),
    );

    assert_eq!(
        task_status_of(&home, "t-rev-1"),
        Some(crate::task_events::TaskStatus::Done),
        "dual-1 VERIFIED (corr=t-…) must auto-close the review task"
    );
    assert!(
        !sidecar_present(&home, "t-rev-1"),
        "the dispatch sidecar must be cleared"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Case (b) dual-2: display correlation may still be `repo@branch`, but the
/// validated receipt carries the exact task id and no reverse lookup is needed.
#[test]
fn verdict_dual2_repobranch_verified_bridges_via_reverse_lookup_t127() {
    let home = tmp_home("t127-dual2");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let reviewer = "fixup-reviewer-4";
    seed_review_task(&home, "t-rev-2", reviewer);
    record_review_dispatch(&home, "fixup-lead", reviewer, "t-rev-2");

    bridge_verdict_to_review_task(
        &home,
        reviewer,
        &t127_verdict("VERIFIED diff clean", "t-rev-2", "owner/repo@feat/x"),
    );

    assert_eq!(
        task_status_of(&home, "t-rev-2"),
        Some(crate::task_events::TaskStatus::Done),
        "dual-2 VERIFIED must bridge through the receipt's exact task id"
    );
    assert!(
        !sidecar_present(&home, "t-rev-2"),
        "sidecar must be cleared via the typed receipt bridge"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// REJECTED → the reviewer responded, so the sidecar clears (no more stuck-ping),
/// but the review task stays OPEN for the re-review cycle (only VERIFIED closes).
#[test]
fn verdict_rejected_clears_sidecar_but_keeps_task_open_t127() {
    let home = tmp_home("t127-rejected");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let reviewer = "fixup-reviewer-2";
    seed_review_task(&home, "t-rev-3", reviewer);
    record_review_dispatch(&home, "fixup-lead", reviewer, "t-rev-3");

    bridge_verdict_to_review_task(
        &home,
        reviewer,
        &t127_verdict("REJECTED found a bug", "t-rev-3", "owner/repo@feat/x"),
    );

    assert!(
        !sidecar_present(&home, "t-rev-3"),
        "any verdict clears the sidecar (reviewer responded → not stuck)"
    );
    assert_eq!(
        task_status_of(&home, "t-rev-3"),
        Some(crate::task_events::TaskStatus::Claimed),
        "REJECTED must NOT close the review task (re-review pending)"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Verdict-looking legacy text cannot trigger the old reporter-name reverse
/// lookup, even when multiple open review dispatches exist.
#[test]
fn legacy_verdict_text_never_reverse_looks_up_review_task_2760() {
    let home = tmp_home("t127-multi");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let reviewer = "fixup-reviewer-3";
    seed_review_task(&home, "t-rev-a", reviewer);
    seed_review_task(&home, "t-rev-b", reviewer);
    record_review_dispatch(&home, "fixup-lead", reviewer, "t-rev-a");
    record_review_dispatch(&home, "fixup-dev", reviewer, "t-rev-b");

    bridge_verdict_to_review_task(
        &home,
        reviewer,
        &crate::inbox::InboxMessage::new_system("system:reviewer", "report", "VERIFIED ok")
            .with_correlation_id("owner/repo@feat/x")
            .with_reviewed_head("7e1d4228bea3cf7fe2d72aab66015297308b48bc"),
    );

    assert_eq!(
        task_status_of(&home, "t-rev-a"),
        Some(crate::task_events::TaskStatus::Claimed),
        "legacy text must NOT close t-rev-a"
    );
    assert_eq!(
        task_status_of(&home, "t-rev-b"),
        Some(crate::task_events::TaskStatus::Claimed),
        "legacy text must NOT close t-rev-b"
    );
    assert!(
        sidecar_present(&home, "t-rev-a") && sidecar_present(&home, "t-rev-b"),
        "legacy text leaves both sidecars unchanged"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// F1 real-entry (spike t-…19288-1): a terminal correlated report driven through
/// the REAL report handler (`track_dispatch`, the fn `handle_send` invokes) must
/// end with the report body in the task's replayed `result`. Complements the
/// helper-level `auto_close.rs` projection test — this covers the messaging entry.
#[test]
fn terminal_report_projects_result_via_track_dispatch() {
    let home = tmp_home("f1-track-dispatch");
    seed_review_task(&home, "t-f1e", "dev-agent"); // Created + Claimed(owner=dev-agent)
    let report = "RESULT: shipped; PR #456 merged.";
    let msg = crate::inbox::InboxMessage {
        from: "dev-agent".into(),
        text: report.into(),
        kind: Some("report".into()),
        correlation_id: Some("t-f1e".into()),
        terminal: Some(true),
        timestamp: "2026-07-12T00:00:00Z".into(),
        ..Default::default()
    };
    // Real report entry (params unused on the report branch).
    track_dispatch(&home, &json!({}), "dev-agent", "lead", &msg);

    assert_eq!(
        task_status_of(&home, "t-f1e"),
        Some(crate::task_events::TaskStatus::Done),
        "precondition: the real report entry auto-closed the task"
    );
    let result = crate::task_events::replay(&home)
        .unwrap()
        .tasks
        .get(&crate::task_events::TaskId("t-f1e".into()))
        .and_then(|r| r.result.clone());
    assert_eq!(
        result.as_deref(),
        Some(report),
        "F1(real): terminal report via track_dispatch must persist `result` (was null)"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #2760 / task66 production-entry RED: a design/analysis verdict is an ordinary
/// report even when its visible text begins with `VERIFIED` and it carries a
/// display-only `reviewed_head`. Drive the exact wrapper emitted by
/// `handle_report_result` through the authoritative API SEND handler and prove it
/// cannot rewrite an already-REJECTED PR state.
#[test]
fn analysis_decision_verified_is_not_a_code_review_receipt_2760() {
    use crate::daemon::pr_state;

    const HEAD: &str = "a4b05610c581f2fca319d619b314b2d326c8d012";
    let home = tmp_home("2760-analysis-no-pr-authority");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    write_fixup_fleet(&home, &["fixup-lead", "analysis-reviewer"]);

    pr_state::record_ci_result(
        &home,
        "owner/repo",
        "fix/rejected",
        HEAD,
        pr_state::CiConclusion::Green,
        vec!["fixup-lead".to_string()],
        pr_state::ReviewClass::Single,
    );
    pr_state::record_verdict(
        &home,
        "t-code-review",
        "code-reviewer",
        Some(HEAD),
        pr_state::VerdictKind::Rejected {
            reason: Some("unsafe"),
        },
    );
    let state_path = pr_state::pr_state_dir(&home)
        .join(pr_state::pr_state_filename("owner/repo", "fix/rejected"));
    let before = std::fs::read(&state_path).expect("seeded rejected PR state");

    let text = crate::mcp::handlers::build_report_text(
        "VERIFIED — the containment design is sound\n\n### Evidence\nran: source audit → complete",
        Some("t-analysis"),
        None,
    );
    let ctx = test_ctx(&home);
    let result = handle_send(
        &json!({
            "from": "analysis-reviewer",
            "target": "fixup-lead",
            "text": text,
            "kind": "report",
            "correlation_id": "t-analysis",
            "reviewed_head": HEAD,
            "report_purpose": "analysis_decision",
            "terminal": true,
        }),
        &ctx,
    );
    assert_eq!(
        result["ok"], true,
        "analysis report must still deliver: {result}"
    );

    let after = std::fs::read(&state_path).expect("PR state still exists");
    assert_eq!(
        after, before,
        "AnalysisDecision is not code-review authority; PR state must be byte-identical"
    );
    assert!(
        !home.join("auto_release_queue").exists(),
        "analysis report must not enqueue code-review auto-release"
    );

    std::fs::remove_dir_all(&home).ok();
}

const RECEIPT_HEAD_2760: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn write_typed_review_fleet(
    home: &std::path::Path,
    reviewer_id: crate::types::InstanceId,
    lead_id: crate::types::InstanceId,
) {
    std::fs::write(
        crate::fleet::fleet_yaml_path(home),
        format!(
            "instances:\n  fixup-lead:\n    backend: claude\n    id: {}\n  typed-reviewer:\n    backend: claude\n    id: {}\nteams:\n  fixup:\n    members: [fixup-lead, typed-reviewer]\n    orchestrator: fixup-lead\n",
            lead_id.full(),
            reviewer_id.full(),
        ),
    )
    .unwrap();
}

fn seed_typed_review_subject(
    home: &std::path::Path,
    assigned_id: crate::types::InstanceId,
) -> crate::daemon::assignment_authority::ActiveAssignment {
    use crate::daemon::pr_state;
    pr_state::record_ci_result(
        home,
        "owner/repo",
        "fix/typed",
        RECEIPT_HEAD_2760,
        pr_state::CiConclusion::Green,
        vec!["fixup-lead".into()],
        pr_state::ReviewClass::Single,
    );
    pr_state::with_pr_state(home, "owner/repo", "fix/typed", |state| {
        state.pr_number = 2769;
    })
    .unwrap();
    let assignment = crate::daemon::assignment_authority::ActiveAssignment::new_pending_typed(
        "owner/repo",
        "fix/typed",
        "typed-reviewer",
        assigned_id,
        2769,
        RECEIPT_HEAD_2760,
        crate::review_receipt::ReviewSlot::Primary,
        "fixup-lead",
        "t-code-review-2760",
        pr_state::ReviewClass::Single,
        crate::mcp::handlers::comms_gates::ReviewAuthor::External("octocat".into()),
        "review exact head",
        None,
        None,
        "2026-07-14T00:00:00Z",
    );
    crate::daemon::assignment_authority::persist(home, &assignment).unwrap();
    assignment
}

fn typed_review_params(assignment_id: uuid::Uuid, verdict: &str, text_token: &str) -> Value {
    json!({
        "from": "typed-reviewer",
        "target": "fixup-lead",
        "text": crate::mcp::handlers::build_report_text(
            &format!("{text_token} — exact review\n\n### Evidence\nran: cargo test → passed"),
            Some("t-code-review-2760"),
            None,
        ),
        "kind": "report",
        "correlation_id": "t-code-review-2760",
        "reviewed_head": "display-only-caller-value",
        "report_purpose": "code_review",
        "code_review": {
            "assignment_id": assignment_id,
            "verdict": verdict,
            "evidence_digest": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        }
    })
}

/// Positive production entry plus A2/exact-once replay: the authenticated API
/// sink assigns the message/source/receipt identities, applies only the exact
/// assignment subject, and a replay of that durable receipt is inert.
#[test]
fn exact_typed_receipt_applies_once_with_server_owned_ids_2760() {
    let home = tmp_home("2760-typed-positive");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let reviewer_id = crate::types::InstanceId::new();
    write_typed_review_fleet(&home, reviewer_id, crate::types::InstanceId::new());
    let assignment = seed_typed_review_subject(&home, reviewer_id);

    let result = handle_send(
        &typed_review_params(assignment.assignment_id, "verified", "VERIFIED"),
        &test_ctx(&home),
    );
    assert_eq!(result["ok"], true, "typed review must deliver: {result}");

    let mut delivered = crate::inbox::drain(&home, "fixup-lead");
    assert_eq!(delivered.len(), 1, "one durable report row");
    let msg = delivered.pop().unwrap();
    let receipt = msg
        .validated_code_review
        .as_ref()
        .expect("API sink attached private validated receipt")
        .summary();
    assert_eq!(msg.id.as_deref(), Some(receipt.source_id.as_str()));
    assert_eq!(
        receipt.receipt_id,
        format!("review-receipt:{}", receipt.source_id),
        "caller cannot choose the exact-once identity"
    );
    assert_eq!(receipt.assignment_id, assignment.assignment_id);
    assert_eq!(receipt.reviewer_instance_id, reviewer_id);
    assert_eq!(receipt.reviewed_head, RECEIPT_HEAD_2760);
    assert_eq!(msg.reviewed_head.as_deref(), Some(RECEIPT_HEAD_2760));

    let state = crate::daemon::pr_state::load(&home, "owner/repo", "fix/typed").unwrap();
    assert_eq!(state.validated_review_receipts.len(), 1);
    assert!(crate::daemon::pr_state::is_merge_ready(&state));
    assert!(home.join("auto_release_queue").exists());

    let state_path = crate::daemon::pr_state::pr_state_dir(&home).join(
        crate::daemon::pr_state::pr_state_filename("owner/repo", "fix/typed"),
    );
    let before_replay = std::fs::read(&state_path).unwrap();
    assert!(
        !process_verdicts(&home, &msg),
        "same source/receipt replays once"
    );
    assert_eq!(std::fs::read(&state_path).unwrap(), before_replay);
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn typed_unverified_is_evidence_block_exempt_but_still_assignment_bound_2760() {
    let home = tmp_home("2760-typed-unverified");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let reviewer_id = crate::types::InstanceId::new();
    write_typed_review_fleet(&home, reviewer_id, crate::types::InstanceId::new());
    let assignment = seed_typed_review_subject(&home, reviewer_id);
    let mut params = typed_review_params(assignment.assignment_id, "unverified", "UNVERIFIED");
    params["text"] = json!(crate::mcp::handlers::build_report_text(
        "UNVERIFIED — claimed but unproven",
        Some("t-code-review-2760"),
        None,
    ));

    let result = handle_send(&params, &test_ctx(&home));
    assert_eq!(
        result["ok"], true,
        "UNVERIFIED is evidence-exempt: {result}"
    );
    let state = crate::daemon::pr_state::load(&home, "owner/repo", "fix/typed").unwrap();
    assert_eq!(state.validated_review_receipts.len(), 1);
    assert_eq!(
        state.validated_review_receipts[0].verdict,
        crate::review_receipt::ReviewVerdict::Unverified
    );
    assert!(!crate::daemon::pr_state::is_merge_ready(&state));
    std::fs::remove_dir_all(&home).ok();
}

/// Missing/foreign assignment, same-name identity ABA, legacy schema, and
/// revoked generation all reject before delivery. These are distinct
/// authorization failures but share the same production API entry and
/// zero-store postcondition.
#[test]
fn forged_foreign_aba_and_revoked_assignments_fail_before_delivery_2760() {
    for case in ["missing", "foreign", "aba", "legacy_schema", "revoked"] {
        let home = tmp_home(&format!("2760-auth-{case}"));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let live_id = crate::types::InstanceId::new();
        write_typed_review_fleet(&home, live_id, crate::types::InstanceId::new());
        let assignment_id = if case == "missing" {
            uuid::Uuid::new_v4()
        } else {
            let assigned_id = if matches!(case, "foreign" | "aba") {
                crate::types::InstanceId::new()
            } else {
                live_id
            };
            let mut assignment = seed_typed_review_subject(&home, assigned_id);
            if case == "foreign" {
                assignment.target = "another-reviewer".into();
                crate::daemon::assignment_authority::revoke(
                    &home,
                    "owner/repo",
                    "fix/typed",
                    "typed-reviewer",
                    "2026-07-14T00:00:01Z",
                )
                .unwrap();
                crate::daemon::assignment_authority::persist(&home, &assignment).unwrap();
            }
            if case == "legacy_schema" {
                assignment.schema_version = 1;
                crate::daemon::assignment_authority::persist(&home, &assignment).unwrap();
            }
            if case == "revoked" {
                crate::daemon::assignment_authority::revoke(
                    &home,
                    "owner/repo",
                    "fix/typed",
                    "typed-reviewer",
                    "2026-07-14T00:00:01Z",
                )
                .unwrap();
            }
            assignment.assignment_id
        };
        let result = handle_send(
            &typed_review_params(assignment_id, "verified", "VERIFIED"),
            &test_ctx(&home),
        );
        assert_eq!(result["ok"], false, "{case} must reject: {result}");
        assert_eq!(
            result["code"], "report_authority_rejected",
            "{case}: {result}"
        );
        assert_eq!(
            crate::inbox::unread_count(&home, "fixup-lead").0,
            0,
            "{case}"
        );
        assert!(!home.join("auto_release_queue").exists(), "{case}");
        std::fs::remove_dir_all(&home).ok();
    }
}

/// The external request contains only assignment_id, explicit enum, and evidence
/// digest. Every caller attempt to assert receipt/subject/slot/source fields is
/// rejected by the deny-unknown typed object before delivery.
#[test]
fn caller_cannot_smuggle_receipt_or_subject_authority_2760() {
    let home = tmp_home("2760-smuggle");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let reviewer_id = crate::types::InstanceId::new();
    write_typed_review_fleet(&home, reviewer_id, crate::types::InstanceId::new());
    let assignment = seed_typed_review_subject(&home, reviewer_id);
    for (field, value) in [
        ("receipt_id", json!("caller-receipt")),
        ("source_id", json!("caller-source")),
        ("repo", json!("other/repo")),
        ("pr_number", json!(1)),
        ("task_id", json!("t-other")),
        ("reviewed_head", json!("b".repeat(40))),
        ("review_class", json!("dual")),
        ("slot", json!("secondary")),
    ] {
        let mut params = typed_review_params(assignment.assignment_id, "verified", "VERIFIED");
        params["code_review"][field] = value;
        let result = handle_send(&params, &test_ctx(&home));
        assert_eq!(
            result["ok"], false,
            "smuggled {field} must reject: {result}"
        );
    }
    assert_eq!(crate::inbox::unread_count(&home, "fixup-lead").0, 0);
    let state = crate::daemon::pr_state::load(&home, "owner/repo", "fix/typed").unwrap();
    assert!(state.validated_review_receipts.is_empty());
    std::fs::remove_dir_all(&home).ok();
}

/// Typed enum/text disagreement is rejected for all three verdicts; receipt
/// fields under a non-code purpose and text-only code reviews fail closed too.
#[test]
fn purpose_and_verdict_shape_mismatches_reject_before_delivery_2760() {
    let home = tmp_home("2760-verdict-mismatch");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let reviewer_id = crate::types::InstanceId::new();
    write_typed_review_fleet(&home, reviewer_id, crate::types::InstanceId::new());
    let assignment = seed_typed_review_subject(&home, reviewer_id);
    for (typed, visible) in [
        ("verified", "REJECTED"),
        ("rejected", "UNVERIFIED"),
        ("unverified", "VERIFIED"),
    ] {
        let result = handle_send(
            &typed_review_params(assignment.assignment_id, typed, visible),
            &test_ctx(&home),
        );
        assert_eq!(result["ok"], false, "{typed}/{visible}: {result}");
    }

    let mut non_code = typed_review_params(assignment.assignment_id, "verified", "VERIFIED");
    non_code["report_purpose"] = json!("analysis_decision");
    assert_eq!(handle_send(&non_code, &test_ctx(&home))["ok"], false);

    let mut no_enum = typed_review_params(assignment.assignment_id, "verified", "VERIFIED");
    no_enum["code_review"]
        .as_object_mut()
        .unwrap()
        .remove("verdict");
    assert_eq!(handle_send(&no_enum, &test_ctx(&home))["ok"], false);
    assert_eq!(crate::inbox::unread_count(&home, "fixup-lead").0, 0);
    std::fs::remove_dir_all(&home).ok();
}

/// Late-validation atomicity: `kind=task` carrying report-only fields
/// (`report_purpose`) must be rejected BEFORE `auto_create_task_if_needed`
/// runs — zero task/inbox side effects.
#[test]
fn report_fields_on_kind_task_rejected_without_leaked_task() {
    let home = tmp_home("prevalidation-atomicity");
    write_fixup_fleet(&home, &["fixup-lead", "fixup-dev"]);
    let ctx = test_ctx(&home);
    let result = handle_send(
        &json!({
            "from": "fixup-lead",
            "target": "fixup-dev",
            "text": "test task with illegal report fields",
            "kind": "task",
            "report_purpose": "code_review",
        }),
        &ctx,
    );
    assert_eq!(result["ok"], false, "must reject: {result}");
    assert_eq!(
        result["code"].as_str(),
        Some("report_authority_rejected"),
        "error code: {result}"
    );
    // The critical invariant: no task must have been created as a side effect.
    let board = crate::task_events::replay(&home).unwrap_or_default();
    assert!(
        board.tasks.is_empty(),
        "rejected send must not leak a task — found {} task(s) on board",
        board.tasks.len()
    );
    // No inbox message delivered either.
    assert_eq!(
        crate::inbox::unread_count(&home, "fixup-dev").0,
        0,
        "rejected send must not deliver an inbox message"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── Architecture-14 item 3 slice 1: reporter-scoped dispatch settlement ──
//
// RED tests proving the current code does NOT scope settlement/refresh by
// reporter identity. Each test sets up a cross-assignee replacement (dispatch
// to agent-A, then to agent-B with the same task correlation) and shows that
// a stale report/update from agent-A incorrectly affects agent-B's state.

/// RED 1: stale prior-assignee report must NOT remove current assignee's
/// dispatch_tracking entry. Exercises the real MCP handle_report_result →
/// mark_completed path (comms.rs:274). mark_completed removes ALL entries
/// matching the correlation_id regardless of reporter vs entry.to.
#[test]
fn red_stale_report_cannot_remove_current_dispatch_tracking() {
    let home = tmp_home("red-dt-stale-report");
    // Fleet needed for handle_report_result's fallback delivery.
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  lead:\n    backend: claude\n  agent-a:\n    backend: claude\n  agent-b:\n    backend: claude\n",
    )
    .unwrap();

    // Dispatch same task to both agent-A and agent-B.
    crate::dispatch_tracking::track_dispatch(
        &home,
        crate::dispatch_tracking::DispatchEntry {
            task_id: Some("t-shared".into()),
            from: "lead".into(),
            to: "agent-a".into(),
            delegated_at: "2026-07-16T00:00:00Z".into(),
            status: "pending".into(),
            ..Default::default()
        },
    );
    crate::dispatch_tracking::track_dispatch(
        &home,
        crate::dispatch_tracking::DispatchEntry {
            task_id: Some("t-shared".into()),
            from: "lead".into(),
            to: "agent-b".into(),
            delegated_at: "2026-07-16T01:00:00Z".into(),
            status: "pending".into(),
            ..Default::default()
        },
    );

    // Stale agent-A sends report through real MCP handle_report_result.
    let sender = crate::identity::Sender::new("agent-a");
    let args = json!({
        "instance": "lead",
        "summary": "done (stale)",
        "correlation_id": "t-shared",
        "request_kind": "report",
    });
    let ctx = crate::mcp::handlers::dispatch::HandlerCtx {
        home: &home,
        args: &args,
        instance_name: sender.as_ref().map_or("", |s| s.as_str()),
        sender: &sender,
        runtime: None,
    };
    crate::mcp::handlers::dispatch::dispatch_send(&ctx);

    // Agent-B's dispatch_tracking entry must survive.
    let store = crate::dispatch_tracking::take_pending_dispatchers_to(&home, "agent-b");
    assert!(
        !store.is_empty(),
        "RED: stale agent-a report must NOT remove agent-b's dispatch_tracking entry"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// RED 2: stale prior-assignee report must NOT delete current assignee's
/// dispatch_idle sidecar. Currently FAILS: mark_resolved matches by
/// correlation_id without checking reporter == sidecar.target.
#[test]
fn red_stale_report_cannot_delete_current_dispatch_idle_sidecar() {
    let home = tmp_home("red-idle-stale-report");
    setup_team_env(
        &home,
        &["lead", "agent-a", "agent-b"],
        &[("team", &["lead", "agent-a", "agent-b"])],
    );

    // Seed sidecar for agent-A dispatch.
    let _id_a = crate::daemon::dispatch_idle::record_dispatch(
        &home,
        "lead",
        "agent-a",
        Some("t-shared"),
        "task",
        600,
    );
    // Seed sidecar for agent-B dispatch (same correlation).
    let id_b = crate::daemon::dispatch_idle::record_dispatch(
        &home,
        "lead",
        "agent-b",
        Some("t-shared"),
        "task",
        600,
    )
    .expect("seed agent-b sidecar");

    // Stale agent-A sends report.
    let ctx = test_ctx(&home);
    let _ = handle_send(
        &json!({
            "from": "agent-a",
            "target": "lead",
            "text": "done (stale)",
            "kind": "report",
            "correlation_id": "t-shared",
        }),
        &ctx,
    );

    // Agent-B's sidecar must survive.
    let pending = crate::daemon::dispatch_idle::list_pending(&home);
    assert!(
        pending.iter().any(|p| p.dispatch_id == id_b),
        "RED: stale agent-a report must NOT delete agent-b's dispatch_idle sidecar"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// RED 3: after restart (re-read from disk), current assignee's watchdog
/// sidecar must still be present when only the stale assignee reported.
#[test]
fn red_restart_preserves_current_watchdog_after_stale_report() {
    let home = tmp_home("red-restart-watchdog");
    setup_team_env(
        &home,
        &["lead", "agent-a", "agent-b"],
        &[("team", &["lead", "agent-a", "agent-b"])],
    );

    let id_b = crate::daemon::dispatch_idle::record_dispatch(
        &home,
        "lead",
        "agent-b",
        Some("t-shared"),
        "task",
        600,
    )
    .expect("seed agent-b sidecar");

    // Stale agent-A report.
    let ctx = test_ctx(&home);
    let _ = handle_send(
        &json!({
            "from": "agent-a",
            "target": "lead",
            "text": "done (stale)",
            "kind": "report",
            "correlation_id": "t-shared",
        }),
        &ctx,
    );

    // Simulate restart: re-read from disk.
    let pending = crate::daemon::dispatch_idle::list_pending(&home);
    assert!(
        pending.iter().any(|p| p.dispatch_id == id_b),
        "RED: after stale report + restart, agent-b's watchdog sidecar must survive on disk"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// RED 4: watchdog must still fire (dispatch_idle_threshold_exceeded) for
/// current assignee after a stale prior-assignee report. Uses an already-
/// expired issued_at fixture and invokes the real scan_and_emit. No sleep.
#[test]
fn red_watchdog_fires_for_current_assignee_after_stale_report() {
    let home = tmp_home("red-watchdog-fires");
    setup_team_env(
        &home,
        &["lead", "agent-a", "agent-b"],
        &[("team", &["lead", "agent-a", "agent-b"])],
    );

    // Seed agent-B sidecar with already-expired issued_at (30s threshold, 2min ago).
    let expired_time = chrono::Utc::now() - chrono::TimeDelta::try_seconds(120).unwrap();
    let dispatch_id = format!("disp-red4-{}", std::process::id());
    let sidecar = crate::daemon::dispatch_idle::PendingDispatch {
        schema_version: 1,
        dispatch_id: dispatch_id.clone(),
        dispatcher: "lead".into(),
        target: "agent-b".into(),
        correlation_id: Some("t-shared".into()),
        expected_kind: "task".into(),
        threshold_secs: 30,
        issued_at: expired_time.to_rfc3339(),
        status: crate::daemon::dispatch_idle::DispatchStatus::Pending,
        ..Default::default()
    };
    let dir = home.join("pending-dispatches");
    std::fs::create_dir_all(&dir).ok();
    let path = crate::daemon::dispatch_idle::pending_path(&home, &dispatch_id);
    std::fs::write(&path, serde_json::to_string_pretty(&sidecar).unwrap()).unwrap();

    // Stale agent-A report (deletes the sidecar via mark_resolved).
    let ctx = test_ctx(&home);
    let _ = handle_send(
        &json!({
            "from": "agent-a",
            "target": "lead",
            "text": "done (stale)",
            "kind": "report",
            "correlation_id": "t-shared",
        }),
        &ctx,
    );

    // Run the real watchdog scan — must detect exceeded for agent-b.
    crate::daemon::dispatch_idle::scan_and_emit(&home);

    // The sidecar must still exist with Exceeded status.
    let pending = crate::daemon::dispatch_idle::list_pending(&home);
    let entry = pending.iter().find(|p| p.dispatch_id == dispatch_id);
    assert!(
        entry.is_some(),
        "RED: agent-b's sidecar must survive stale report so watchdog can fire"
    );
    assert_eq!(
        entry.unwrap().status,
        crate::daemon::dispatch_idle::DispatchStatus::Exceeded,
        "RED: scan_and_emit must flip agent-b's sidecar to Exceeded"
    );
    // dispatch_idle_threshold_exceeded must be durably enqueued to the dispatcher.
    let msgs = crate::inbox::drain(&home, "lead");
    assert!(
        msgs.iter().any(|m| m
            .kind
            .as_deref()
            .is_some_and(|k| k == "dispatch_idle_threshold_exceeded")
            && m.correlation_id.as_deref() == Some("t-shared")),
        "RED: scan_and_emit must enqueue dispatch_idle_threshold_exceeded to lead for t-shared"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// RED 5: stale prior-assignee update/query must NOT refresh current
/// assignee's issued_at. Currently FAILS: refresh_issued_at matches by
/// correlation_id without checking reporter == sidecar.target.
#[test]
fn red_stale_update_cannot_refresh_current_issued_at() {
    let home = tmp_home("red-refresh-stale");
    setup_team_env(
        &home,
        &["lead", "agent-a", "agent-b"],
        &[("team", &["lead", "agent-a", "agent-b"])],
    );

    let id_b = crate::daemon::dispatch_idle::record_dispatch(
        &home,
        "lead",
        "agent-b",
        Some("t-shared"),
        "task",
        600,
    )
    .expect("seed agent-b sidecar");

    // Record original issued_at.
    let before = crate::daemon::dispatch_idle::list_pending(&home);
    let original_issued = before
        .iter()
        .find(|p| p.dispatch_id == id_b)
        .expect("sidecar exists")
        .issued_at
        .clone();

    std::thread::sleep(std::time::Duration::from_millis(50));

    // Stale agent-A sends update with matching correlation.
    let ctx = test_ctx(&home);
    let _ = handle_send(
        &json!({
            "from": "agent-a",
            "target": "lead",
            "text": "BUSY still working",
            "kind": "update",
            "correlation_id": "t-shared",
        }),
        &ctx,
    );

    // Agent-B's issued_at must NOT have been refreshed.
    let after = crate::daemon::dispatch_idle::list_pending(&home);
    let current_issued = after
        .iter()
        .find(|p| p.dispatch_id == id_b)
        .expect("sidecar must still exist")
        .issued_at
        .clone();
    assert_eq!(
        original_issued, current_issued,
        "RED: stale agent-a update must NOT refresh agent-b's issued_at"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// RED 6: validated-review bridge settlement must be reporter-scoped.
/// A verdict from a stale reviewer must NOT resolve the current reviewer's
/// dispatch_idle sidecar. Uses a genuine ValidatedCodeReviewReceipt
/// through the bridge path. The sidecar is keyed by task_id (not
/// repo@branch) so generic report settlement (correlation_id-based)
/// cannot reach it — only the bridge can.
#[test]
fn red_validated_review_bridge_settlement_is_reporter_scoped() {
    let home = tmp_home("red-review-bridge");
    setup_team_env(
        &home,
        &["lead", "reviewer-a", "reviewer-b"],
        &[("team", &["lead", "reviewer-a", "reviewer-b"])],
    );

    // Sidecar for reviewer-B's review dispatch (task-keyed).
    let id_b = crate::daemon::dispatch_idle::record_dispatch(
        &home,
        "lead",
        "reviewer-b",
        Some("t-review-task"),
        "task",
        600,
    )
    .expect("seed reviewer-b sidecar");

    // Construct genuine validated-review receipt from stale reviewer-A.
    // Receipt's task_id matches the sidecar correlation; the bridge
    // resolves mark_resolved via this task_id.
    let receipt = crate::review_receipt::ValidatedCodeReviewReceipt::for_test(
        crate::review_receipt::ReviewReceiptSummary {
            receipt_id: "review-receipt:red6".into(),
            source_id: "m-red6".into(),
            evidence_digest: "a".repeat(64),
            assignment_id: uuid::Uuid::new_v4(),
            reviewer_instance_id: crate::types::InstanceId(uuid::Uuid::new_v4()),
            reviewer_name: "reviewer-a".into(),
            repo: "org/repo".into(),
            pr_number: 42,
            branch: "feat/x".into(),
            task_id: "t-review-task".into(),
            reviewed_head: "b".repeat(40),
            review_class: crate::daemon::pr_state::ReviewClass::Single,
            slot: crate::review_receipt::ReviewSlot::Primary,
            verdict: crate::review_receipt::ReviewVerdict::Verified,
        },
    );

    // Drive through track_dispatch with repo@branch as correlation_id
    // (different from the task_id-keyed sidecar) so the generic
    // mark_resolved at messaging.rs:583 does NOT match the sidecar.
    // Only bridge_verdict_to_review_task (called inside track_dispatch
    // at messaging.rs:611) can reach the task-keyed sidecar.
    let msg = crate::inbox::InboxMessage {
        schema_version: 1,
        id: Some("m-red6-stale".into()),
        from: "reviewer-a".into(),
        text: "VERIFIED — looks good".into(),
        kind: Some("report".into()),
        correlation_id: Some("org/repo@feat/x".into()),
        validated_code_review: Some(receipt),
        timestamp: chrono::Utc::now().to_rfc3339(),
        ..Default::default()
    };
    let params = json!({
        "from": "reviewer-a",
        "target": "lead",
        "kind": "report",
        "correlation_id": "org/repo@feat/x",
    });
    crate::api::handlers::messaging::track_dispatch(&home, &params, "reviewer-a", "lead", &msg);

    // Reviewer-B's sidecar must survive.
    let pending = crate::daemon::dispatch_idle::list_pending(&home);
    assert!(
        pending.iter().any(|p| p.dispatch_id == id_b),
        "RED: stale reviewer-a verdict must NOT resolve reviewer-b's dispatch_idle sidecar"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── Empty-reporter fail-closed regression tests ──

#[test]
fn empty_reporter_mark_completed_leaves_rows_intact() {
    let home = tmp_home("empty-reporter-dt");
    crate::dispatch_tracking::track_dispatch(
        &home,
        crate::dispatch_tracking::DispatchEntry {
            task_id: Some("t-empty".into()),
            from: "lead".into(),
            to: "dev".into(),
            delegated_at: "2026-07-16T00:00:00Z".into(),
            status: "pending".into(),
            ..Default::default()
        },
    );
    crate::dispatch_tracking::mark_completed(&home, Some("t-empty"), "");
    let store = crate::dispatch_tracking::take_pending_dispatchers_to(&home, "dev");
    assert!(
        !store.is_empty(),
        "empty reporter must NOT remove any dispatch_tracking row"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn empty_reporter_mark_resolved_leaves_sidecar_intact() {
    let home = tmp_home("empty-reporter-idle");
    let id = crate::daemon::dispatch_idle::record_dispatch(
        &home,
        "lead",
        "dev",
        Some("t-empty"),
        "task",
        600,
    )
    .expect("seed");
    let resolved = crate::daemon::dispatch_idle::mark_resolved(&home, "t-empty", "");
    assert!(resolved.is_none(), "empty reporter must return None");
    let pending = crate::daemon::dispatch_idle::list_pending(&home);
    assert!(
        pending.iter().any(|p| p.dispatch_id == id),
        "empty reporter must NOT delete the sidecar"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn empty_reporter_refresh_issued_at_leaves_sidecar_unchanged() {
    let home = tmp_home("empty-reporter-refresh");
    let id = crate::daemon::dispatch_idle::record_dispatch(
        &home,
        "lead",
        "dev",
        Some("t-empty"),
        "task",
        600,
    )
    .expect("seed");
    let before = crate::daemon::dispatch_idle::list_pending(&home);
    let original = before
        .iter()
        .find(|p| p.dispatch_id == id)
        .unwrap()
        .issued_at
        .clone();
    std::thread::sleep(std::time::Duration::from_millis(50));
    let refreshed = crate::daemon::dispatch_idle::refresh_issued_at(&home, "t-empty", "");
    assert!(refreshed.is_none(), "empty reporter must return None");
    let after = crate::daemon::dispatch_idle::list_pending(&home);
    let current = after
        .iter()
        .find(|p| p.dispatch_id == id)
        .unwrap()
        .issued_at
        .clone();
    assert_eq!(
        original, current,
        "empty reporter must NOT refresh issued_at"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn flat_review_fields_rejected_before_typed_conversion_2454() {
    let home = tmp_home("2454-flat-smuggle");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let flat_keys = [
        "assignment_id",
        "verdict",
        "evidence_digest",
        "receipt_id",
        "source_id",
        "source_ids",
        "review_receipt",
        "validated_code_review",
    ];
    for kind in ["report", "task", "query", "update"] {
        for key in flat_keys {
            let mut params = json!({
                "from": "alice",
                "target": "bob",
                "text": "hello",
                "kind": kind,
            });
            params[key] = json!("smuggled");
            let result = handle_send(&params, &test_ctx(&home));
            assert_eq!(
                result["ok"], false,
                "kind={kind} with flat {key} must reject: {result}"
            );
            assert!(
                result["error"]
                    .as_str()
                    .unwrap_or("")
                    .contains("inside the typed code_review object"),
                "flat {key} must cite typed object restriction: {result}"
            );
        }
    }
    let valid_nested = json!({
        "from": "alice",
        "target": "bob",
        "text": "hello",
        "kind": "report",
        "code_review": {"assignment_id": "a-1", "verdict": "verified", "evidence_digest": "x"},
    });
    let result = handle_send(&valid_nested, &test_ctx(&home));
    assert!(
        !result
            .get("error")
            .and_then(|e| e.as_str())
            .is_some_and(|s| s.contains("inside the typed code_review object")),
        "nested code_review must NOT be rejected as flat smuggling: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}
