use crate::channel::telegram;
use serde_json::{json, Value};
use std::path::Path;

pub(super) fn handle_reply(home: &Path, args: &Value, instance_name: &str) -> Value {
    let text = args["text"].as_str().unwrap_or("").to_string();
    tracing::info!(from = %instance_name, %text, "reply");
    let fleet_path = home.join("fleet.yaml");
    if !fleet_path.exists() {
        return json!({"error": "No fleet.yaml — cannot send reply"});
    }
    let Some(ch) = crate::channel::active_channel() else {
        return json!({"error": "no active channel"});
    };
    match ch.send_from_agent(
        instance_name,
        crate::channel::AgentOutboundOp::Reply { text },
    ) {
        Ok(msg) => {
            // Sprint 49: mark that agent used reply tool (channel discipline).
            crate::agent_ops::save_metadata(
                home,
                instance_name,
                "reply_tool_called_since_input",
                json!(true),
            );
            json!({ "message_id": msg.id })
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
