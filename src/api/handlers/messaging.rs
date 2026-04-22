//! Messaging handler: SEND.

use super::HandlerCtx;
use crate::agent;
use serde_json::{json, Value};

pub(crate) fn handle_send(params: &Value, ctx: &HandlerCtx) -> Value {
    let (from, target, text) = (
        params["from"].as_str().unwrap_or("unknown"),
        params["target"].as_str().unwrap_or(""),
        params["text"].as_str().unwrap_or(""),
    );
    if let Err(e) = agent::validate_name(target) {
        return json!({"ok": false, "error": e});
    }
    if from == target {
        return json!({"ok": false, "error": "cannot send to self"});
    }
    let _ = crate::inbox::enqueue(
        ctx.home,
        target,
        crate::inbox::InboxMessage {
            from: format!("from:{from}"),
            text: text.to_string(),
            kind: params
                .get("kind")
                .and_then(|v| v.as_str())
                .map(String::from),
            timestamp: chrono::Utc::now().to_rfc3339(),
        },
    );
    let display_text = if text.chars().count() > 200 {
        format!(
            "{}... (use inbox tool)",
            text.chars().take(200).collect::<String>()
        )
    } else {
        text.to_string()
    };
    let inject_msg = format!(
        "[from:{from}] {display_text} (Reply using send_to_instance MCP tool, NOT direct text)"
    );

    let reg = agent::lock_registry(ctx.registry);
    if let Some(handle) = reg.get(target) {
        let _ = agent::inject_to_agent(handle, inject_msg.as_bytes());
    }
    json!({"ok": true})
}
