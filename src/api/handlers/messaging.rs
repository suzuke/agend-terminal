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
            from: format!("from:{from}"),
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
    json!({"ok": true, "delivery_mode": delivery_mode})
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
    fn setup_team_env(
        home: &std::path::Path,
        fleet_instances: &[&str],
        teams: &[(& str, &[&str])],
    ) {
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
            std::fs::create_dir_all(home.join("store")).ok();
            std::fs::write(
                home.join("store").join("teams.json"),
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
        let result = handle_send(&json!({"from": "alice", "target": "bob", "text": "hi"}), &ctx);
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
        let result = handle_send(&json!({"from": "alice", "target": "bob", "text": "hi"}), &ctx);
        assert_eq!(result["ok"], false, "cross-team send must be blocked: {result}");
        assert!(
            result["error"].as_str().unwrap_or("").contains("cross-team"),
            "error must mention cross-team: {result}"
        );
        assert!(audit_log_contains(&home, "send_cross_team_blocked"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn send_to_general_allowed_from_any_team() {
        let home = tmp_home("to-general");
        setup_team_env(
            &home,
            &["alice", "general"],
            &[("dev2", &["alice"])],
        );
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
        setup_team_env(
            &home,
            &["general", "bob"],
            &[("dev", &["bob"])],
        );
        let ctx = test_ctx(&home);
        let result = handle_send(
            &json!({"from": "general", "target": "bob", "text": "hi"}),
            &ctx,
        );
        assert_eq!(result["ok"], true, "send from general must succeed: {result}");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn send_self_already_blocked() {
        let home = tmp_home("self-block-team");
        setup_team_env(&home, &["alice"], &[("dev2", &["alice"])]);
        let ctx = test_ctx(&home);
        let result = handle_send(&json!({"from": "alice", "target": "alice", "text": "hi"}), &ctx);
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
        let result = handle_send(&json!({"from": "alice", "target": "bob", "text": "hi"}), &ctx);
        assert_eq!(result["ok"], true, "both teamless must be allowed: {result}");
        assert!(!audit_log_contains(&home, "send_cross_team_blocked"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn send_team_to_no_team_blocked() {
        let home = tmp_home("team-to-none");
        setup_team_env(&home, &["alice", "bob"], &[("dev2", &["alice"])]);
        let ctx = test_ctx(&home);
        let result = handle_send(&json!({"from": "alice", "target": "bob", "text": "hi"}), &ctx);
        assert_eq!(result["ok"], false, "team→teamless must be blocked: {result}");
        assert!(audit_log_contains(&home, "send_cross_team_blocked"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn send_no_team_to_team_blocked() {
        let home = tmp_home("none-to-team");
        setup_team_env(&home, &["alice", "bob"], &[("dev2", &["bob"])]);
        let ctx = test_ctx(&home);
        let result = handle_send(&json!({"from": "alice", "target": "bob", "text": "hi"}), &ctx);
        assert_eq!(result["ok"], false, "teamless→team must be blocked: {result}");
        assert!(audit_log_contains(&home, "send_cross_team_blocked"));
        std::fs::remove_dir_all(&home).ok();
    }
}
