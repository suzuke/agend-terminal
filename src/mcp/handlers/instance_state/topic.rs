use serde_json::{json, Value};
use std::path::Path;

/// #991 Phase 2: retrofit a Telegram topic for a deferred/auto-without-topic
/// instance. See bind_topic_for_instance for the core logic and
/// BindTopicOutcome variants for the exact result shapes below.
pub(crate) fn handle_bind_topic(home: &Path, args: &Value) -> Value {
    let name = match crate::mcp::handlers::require_instance(args) {
        Ok(n) => n,
        Err(e) => return e,
    };
    crate::validate_name_or_err!(name);
    if let Some(channel) = args["channel"].as_str() {
        if channel != "telegram" {
            return json!({
                "error": format!("bind_topic: channel '{channel}' not yet supported (only 'telegram')"),
                "code": "channel_not_supported"
            });
        }
    }
    use crate::channel::telegram::BindTopicOutcome;
    match crate::channel::telegram::bind_topic_for_instance(home, name) {
        BindTopicOutcome::Bound(tid) => json!({"bound": true, "topic_id": tid}),
        BindTopicOutcome::AlreadyBound(tid) => {
            json!({"bound": true, "topic_id": tid, "already_bound": true})
        }
        BindTopicOutcome::NotEligible { reason } => {
            json!({"error": reason, "code": "not_eligible"})
        }
        BindTopicOutcome::InstanceNotFound => {
            json!({"error": format!("instance '{name}' not found"), "code": "instance_not_found"})
        }
        BindTopicOutcome::ChannelUnavailable => json!({
            "error": "telegram channel not ready yet — retry in a few seconds",
            "code": "channel_unavailable"
        }),
        BindTopicOutcome::ApiError(e) => json!({"error": e, "code": "api_error"}),
    }
}
