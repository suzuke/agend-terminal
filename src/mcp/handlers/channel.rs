use crate::channel::telegram;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

pub(super) fn handle_reply(home: &Path, args: &Value, instance_name: &str) -> Value {
    let text = args["text"].as_str().unwrap_or("").to_string();
    tracing::info!(from = %instance_name, %text, "reply");
    let fleet_path = home.join("fleet.yaml");
    if !fleet_path.exists() {
        return json!({"error": "No fleet.yaml — cannot send reply"});
    }

    // Sprint 55 P0-A — prefer-chain: sender's HeartbeatPair.reply_to_channel
    // (Sprint 52 router-layer attribution) wins when present + registered;
    // fallback to active_channel() singleton when None. Returns structured
    // error codes so agents can branch on machine-readable signals.
    let snapshot = crate::daemon::heartbeat_pair::snapshot_for(instance_name);
    let ch: Arc<dyn crate::channel::Channel> = match snapshot.reply_to_channel.as_deref() {
        Some(name) => match crate::channel::lookup_channel_by_name(name) {
            Some(ch) => ch,
            None => {
                tracing::warn!(
                    from = %instance_name, channel = %name,
                    "reply_to_channel tagged but not registered — divergence"
                );
                return json!({
                    "error": format!("reply_to_channel '{name}' not registered or offline"),
                    "code": "reply_channel_unavailable"
                });
            }
        },
        None => match crate::channel::active_channel() {
            Some(ch) => ch,
            None => return json!({"error": "no active channel", "code": "no_active_channel"}),
        },
    };
    match ch.send_from_agent(
        instance_name,
        crate::channel::AgentOutboundOp::Reply { text },
    ) {
        Ok(msg) => {
            // Sprint 52: agent replied explicitly — skip mirror for this turn.
            crate::daemon::heartbeat_pair::update_with(instance_name, |p| {
                p.mirror_skip_until_next_turn = true;
            });
            json!({ "message_id": msg.id })
        }
        Err(crate::channel::ChannelError::NotSupported(op)) => {
            tracing::warn!(
                from = %instance_name, channel = %ch.kind(),
                "reply capability unsupported on tagged channel — divergence"
            );
            json!({
                "error": format!("channel '{}' does not support {}", ch.kind(), op),
                "code": "channel_capability_unsupported"
            })
        }
        Err(e) => json!({"error": format!("{e}")}),
    }
}

pub(super) fn handle_react(args: &Value, instance_name: &str) -> Value {
    let emoji = args["emoji"].as_str().unwrap_or("").to_string();
    let message_id = args["message_id"].as_str().map(String::from);
    let Some(ch) = crate::channel::active_channel() else {
        return json!({"error": "no active channel"});
    };
    match ch.send_from_agent(
        instance_name,
        crate::channel::AgentOutboundOp::React {
            emoji: emoji.clone(),
            message_id,
        },
    ) {
        Ok(_) => json!({"emoji": emoji}),
        Err(e) => json!({"error": format!("{e}")}),
    }
}

pub(super) fn handle_download_attachment(home: &Path, args: &Value, instance_name: &str) -> Value {
    let file_id = match args["file_id"].as_str() {
        Some(f) => f,
        None => return json!({"error": "missing 'file_id'"}),
    };
    match telegram::try_download_attachment(home, instance_name, file_id) {
        Ok(path) => json!({"path": path}),
        Err(e) => json!({"error": format!("{e}")}),
    }
}

// Sprint 55 P0-A — handle_reply prefer-chain tests in sibling file.
#[cfg(test)]
#[path = "channel_p0a_tests.rs"]
mod p0a_tests;
