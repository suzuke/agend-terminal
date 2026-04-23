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
    let _ = crate::inbox::enqueue(
        ctx.home,
        target,
        crate::inbox::InboxMessage {
            schema_version: 0,
            id: None,
            read_at: None,
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
    if reg.contains_key(target) {
        drop(reg);
        crate::inbox::compose_aware_send(ctx.home, target, &inject_msg);
    }
    json!({"ok": true})
}
