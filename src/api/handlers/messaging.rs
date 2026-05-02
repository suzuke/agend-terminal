//! Messaging handler: SEND.

use super::HandlerCtx;
use crate::agent;
use serde_json::{json, Value};

pub(crate) fn handle_send(params: &Value, ctx: &HandlerCtx) -> Value {
    // Empty `from` would surface downstream as `[from:] {text}` with no
    // originator — reject at the boundary so misuse is loud rather than
    // silent. The MCP layer already guards this via the `Sender` newtype;
    // this covers direct API callers that bypass the typed path.
    let from = match params["from"].as_str().filter(|s| !s.is_empty()) {
        Some(s) => s,
        None => {
            return json!({
                "ok": false,
                "error": "send requires non-empty 'from' (sender identity)"
            });
        }
    };
    let (target, text) = (
        params["target"].as_str().unwrap_or(""),
        params["text"].as_str().unwrap_or(""),
    );
    if let Err(e) = agent::validate_name(target) {
        return json!({"ok": false, "error": e});
    }
    if from == target {
        return json!({"ok": false, "error": "cannot send to self"});
    }

    // Validate target exists: check runtime registry OR fleet.yaml definitions.
    // Messages to non-existent targets would silently land in an unread inbox file.
    {
        let reg = agent::lock_registry(ctx.registry);
        let in_registry = reg.contains_key(target);
        drop(reg);
        if !in_registry {
            let in_fleet = crate::fleet::FleetConfig::load(&ctx.home.join("fleet.yaml"))
                .ok()
                .map(|c| c.instances.contains_key(target))
                .unwrap_or(false);
            if !in_fleet {
                return json!({"ok": false, "error": format!("target instance '{target}' not found (not in registry or fleet.yaml)")});
            }
        }
    }

    // Sprint 37 team isolation gate — 3 rules, zero escape hatch.
    // Rule 1 (self-send) already rejected above.
    // Rule 2: general bus
    let is_general_bus = from == "general" || target == "general";
    // Rule 3: same-team via Option<Team> equality
    let from_team = crate::teams::find_team_for(ctx.home, from);
    let target_team = crate::teams::find_team_for(ctx.home, target);
    let same_team = match (&from_team, &target_team) {
        (Some(a), Some(b)) => a.name == b.name,
        (None, None) => true,
        _ => false,
    };
    if !is_general_bus && !same_team {
        crate::event_log::log(
            ctx.home,
            "send_cross_team_blocked",
            from,
            &format!(
                "target={target}, sender_team={:?}, target_team={:?}",
                from_team.as_ref().map(|t| &t.name),
                target_team.as_ref().map(|t| &t.name),
            ),
        );
        return json!({
            "ok": false,
            "error": format!(
                "cross-team send blocked: '{from}' (team={:?}) → '{target}' (team={:?}). \
                 Route via general, or use create_instance(team=...) to grow your team.",
                from_team.as_ref().map(|t| &t.name),
                target_team.as_ref().map(|t| &t.name),
            )
        });
    }
    if is_general_bus && !same_team {
        crate::event_log::log(
            ctx.home,
            "send_cross_team_allowed_general",
            from,
            &format!("target={target}"),
        );
    }

    let msg = {
        let mut thread_id = params["thread_id"].as_str().map(String::from);
        let parent_id = params["parent_id"].as_str().map(String::from);

        // Auto-inherit: if parent_id given but thread_id not, inherit from parent
        if thread_id.is_none() {
            if let Some(ref pid) = parent_id {
                if let Some(parent_msg) = crate::inbox::find_message(ctx.home, pid) {
                    thread_id = parent_msg.thread_id.or_else(|| parent_msg.id.clone());
                    // parent becomes thread root
                }
            }
        }

        crate::inbox::InboxMessage {
            schema_version: 0,
            id: None,
            read_at: None,
            thread_id,
            parent_id,
            task_id: params["task_id"].as_str().map(String::from),
            force_meta: params
                .get("force_meta")
                .and_then(|v| serde_json::from_value::<crate::inbox::ForceMeta>(v.clone()).ok()),
            correlation_id: params["correlation_id"].as_str().map(String::from),
            reviewed_head: params["reviewed_head"].as_str().map(String::from),
            // Sprint 46 P1: include instance ID in from field per operator §13.5
            from: {
                let id_suffix = crate::fleet::FleetConfig::load(&ctx.home.join("fleet.yaml"))
                    .ok()
                    .and_then(|c| c.instances.get(from).and_then(|i| i.id.clone()))
                    .and_then(|id| crate::types::InstanceId::parse(&id))
                    .map(|id| format!(" ({})", id.short()))
                    .unwrap_or_default();
                format!("from:{from}{id_suffix}")
            },
            text: text.to_string(),
            kind: params
                .get("kind")
                .and_then(|v| v.as_str())
                .map(String::from),
            timestamp: chrono::Utc::now().to_rfc3339(),
            channel: None,
            delivery_mode: None,
            attachments: vec![],
            in_reply_to_msg_id: None,
            in_reply_to_excerpt: None,
            superseded_by: None,
        }
    };
    let _ = crate::inbox::enqueue(ctx.home, target, msg.clone());

    let inject_msg = if crate::inbox::pointer_only_inject()
        || text.chars().count() > crate::inbox::HEADER_SIZE_THRESHOLD
    {
        format!("{} (use inbox tool)", crate::inbox::format_header(&msg))
    } else {
        let display_text = if text.chars().count() > 200 {
            format!(
                "{}... (use inbox tool)",
                text.chars().take(200).collect::<String>()
            )
        } else {
            text.to_string()
        };
        format!(
            "[from:{from}] {display_text} (Reply using send_to_instance MCP tool, NOT direct text)"
        )
    };

    let reg = agent::lock_registry(ctx.registry);
    let delivery_mode = if reg.contains_key(target) {
        drop(reg);
        crate::inbox::compose_aware_send(ctx.home, target, &inject_msg);
        "pty"
    } else {
        drop(reg);
        "inbox_only"
    };

    // B1 boundary: provenance injection pushed from MCP comms to API SEND.
    // When the caller passes provenance metadata (delegate-task path), inject
    // it into the active channel so operators see task routing in Telegram/Discord.
    if let Some(prov) = params.get("provenance").and_then(|v| v.as_object()) {
        let prov_from = prov
            .get("from")
            .and_then(|v| v.as_str())
            .unwrap_or(from)
            .to_string();
        let prov_task = prov
            .get("task")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if let Some(ch) = crate::channel::active_channel() {
            if let Err(e) = ch.send_from_agent(
                target,
                crate::channel::AgentOutboundOp::InjectProvenance {
                    from: prov_from,
                    task: prov_task,
                },
            ) {
                tracing::warn!(
                    %e, target, from,
                    "provenance injection failed — routing may be broken"
                );
            }
        } else {
            tracing::warn!(
                target,
                from,
                "provenance injection failed — no active channel"
            );
        }
    }

    // B2 boundary: worktree-checkout side-effect pushed from MCP comms to
    // API SEND. When delegate-task carries a branch param, checkout the
    // branch in the target's working directory (Sprint 31 task #52).
    let mut branch_checked_out: Option<&str> = None;
    if let Some(branch) = params["branch"].as_str().filter(|b| !b.is_empty()) {
        let fleet_path = ctx.home.join("fleet.yaml");
        if let Ok(config) = crate::fleet::FleetConfig::load(&fleet_path) {
            if let Some(resolved) = config.resolve_instance(target) {
                if let Some(ref wd) = resolved.working_directory {
                    if crate::worktree::is_git_repo(wd) {
                        match crate::worktree::checkout_branch(wd, branch) {
                            Ok(()) => branch_checked_out = Some(branch),
                            Err(e) => {
                                tracing::warn!(
                                    target_name = target, branch, error = %e,
                                    "task.branch checkout failed"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    let mut resp = json!({"ok": true, "delivery_mode": delivery_mode});
    if let Some(branch) = branch_checked_out {
        resp["branch_checked_out"] = json!(branch);
    }
    resp
}

#[cfg(test)]
mod tests {
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
        }
    }

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("agend-msg-test-{}-{}", tag, std::process::id()));
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

    #[test]
    fn test_send_to_fleet_defined_instance_succeeds() {
        let home = tmp_home("fleet-defined");
        // Define instance in fleet.yaml but don't start it
        std::fs::write(
            home.join("fleet.yaml"),
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
            home.join("fleet.yaml"),
            "instances:\n  active-agent:\n    backend: claude\n  sender:\n    backend: claude\n",
        )
        .ok();
        // Spawn a real agent so it's in the registry
        let registry: &'static agent::AgentRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let spawn_cfg = crate::agent::SpawnConfig {
            name: "active-agent",
            backend_command: crate::default_shell(),
            args: &[],
            spawn_mode: crate::backend::SpawnMode::Fresh,
            cols: 80,
            rows: 24,
            env: None,
            working_dir: None,
            submit_key: "\r",
            home: None,
            crash_tx: None,
            shutdown: None,
        };
        crate::agent::spawn_agent(&spawn_cfg, registry).expect("spawn");
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
        if let Some(h) = reg.get("active-agent") {
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

    /// Set up fleet.yaml with given instances and teams.json with given teams.
    fn setup_team_env(home: &std::path::Path, fleet_instances: &[&str], teams: &[(&str, &[&str])]) {
        let fleet_yaml = fleet_instances
            .iter()
            .map(|n| format!("  {n}:\n    backend: claude"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(
            home.join("fleet.yaml"),
            format!("instances:\n{fleet_yaml}\n"),
        )
        .ok();

        if !teams.is_empty() {
            let team_objs: Vec<serde_json::Value> = teams
                .iter()
                .map(|(name, members)| {
                    json!({
                        "name": name,
                        "members": members,
                        "created_at": "2026-01-01T00:00:00Z"
                    })
                })
                .collect();
            let store = json!({"schema_version": 1, "teams": team_objs});
            std::fs::write(
                home.join("teams.json"),
                serde_json::to_string_pretty(&store).expect("json"),
            )
            .ok();
        }
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
            home.join("fleet.yaml"),
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
        let home = tmp_home("branch-fail");
        let wd = home.join("workspace/target");
        std::fs::create_dir_all(&wd).ok();
        // Init a git repo so is_git_repo returns true
        let _ = std::process::Command::new("git")
            .args(["init"])
            .current_dir(&wd)
            .output();
        std::fs::write(
            home.join("fleet.yaml"),
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
}
