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
            from: format!("from:{from}"),
            text: text.to_string(),
            kind: params
                .get("kind")
                .and_then(|v| v.as_str())
                .map(String::from),
            timestamp: chrono::Utc::now().to_rfc3339(),
        }
    };
    let _ = crate::inbox::enqueue(ctx.home, target, msg.clone());

    let inject_msg = if text.chars().count() > crate::inbox::HEADER_SIZE_THRESHOLD {
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
    if reg.contains_key(target) {
        drop(reg);
        crate::inbox::compose_aware_send(ctx.home, target, &inject_msg);
    }
    json!({"ok": true})
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

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
}
