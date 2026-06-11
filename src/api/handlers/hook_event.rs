//! #hook-state-poc: HOOK_EVENT — lifecycle-hook event ingestion (shadow-mode).
//!
//! Receives one event from a backend hook command (`agend-terminal hook-event
//! --instance <name>`, wired into the per-workspace Claude settings by
//! `mcp_config.rs` under `AGEND_HOOK_STATE_POC=1`). Records it in the
//! [`crate::daemon::hook_shadow`] store and emits the `#hook-shadow`
//! comparison log (hook-derived state vs the live screen-heuristic state) —
//! the agreement data that gates promoting hooks to authoritative.

use super::HandlerCtx;
use crate::agent;
use serde_json::{json, Value};

pub(crate) fn handle_hook_event(params: &Value, ctx: &HandlerCtx) -> Value {
    let name = params["name"].as_str().unwrap_or("");
    if let Err(e) = agent::validate_name(name) {
        return json!({"ok": false, "error": e});
    }
    let hook_event_name = match params["hook_event_name"].as_str() {
        Some(h) if !h.is_empty() => h,
        _ => return json!({"ok": false, "error": "missing 'hook_event_name'"}),
    };
    let notification_type = params["notification_type"].as_str();
    let tool_name = params["tool_name"].as_str();

    let derived =
        crate::daemon::hook_shadow::record_event(name, hook_event_name, notification_type);

    // Shadow comparison: the screen-heuristic state at event receipt. This is
    // the PoC's primary output — production promotion is gated on this
    // agreement data.
    let (screen_state, backend) = {
        let reg = agent::lock_registry(ctx.registry);
        match crate::fleet::resolve_uuid(ctx.home, name).and_then(|id| {
            reg.get(&id)
                .map(|h| (h.core.lock().state.get_state(), h.backend_command.clone()))
        }) {
            Some((s, b)) => (Some(s), Some(b)),
            None => (None, None),
        }
    };
    let agree = match (derived, screen_state) {
        (Some(d), Some(s)) => Some(d == s),
        _ => None,
    };
    // #2016: #1523 promoted hooks to authoritative, so the static "shadow-mode —
    // heuristic still drives" line is no longer true for a promoted backend.
    // Reflect the LIVE disposition (fields unchanged — text only).
    let drive = if backend
        .as_deref()
        .is_some_and(crate::daemon::hook_shadow::is_promoted)
    {
        "authoritative for this backend"
    } else {
        "shadow — heuristic drives"
    };
    tracing::info!(
        tag = "#hook-shadow",
        agent = %name,
        hook_event = hook_event_name,
        notification_type = ?notification_type,
        tool_name = ?tool_name,
        hook_state = ?derived,
        screen_state = ?screen_state,
        agree = ?agree,
        "hook event received ({drive})"
    );
    json!({"ok": true, "derived_state": derived.map(|s| format!("{s:?}"))})
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn test_ctx(home: &std::path::Path) -> HandlerCtx<'_> {
        let registry: &'static crate::agent::AgentRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let configs: &'static crate::api::ConfigRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let externals: &'static crate::agent::ExternalRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        HandlerCtx {
            registry,
            configs,
            externals,
            notifier: None,
            home,
        }
    }

    #[test]
    fn hook_event_records_shadow_and_derives() {
        let home = std::env::temp_dir().join(format!(
            "agend-hookevent-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&home).ok();
        let ctx = test_ctx(&home);
        let resp = handle_hook_event(
            &json!({"name": "hooked", "hook_event_name": "PreToolUse", "tool_name": "Bash"}),
            &ctx,
        );
        assert_eq!(resp["ok"], true, "{resp}");
        let snap = crate::daemon::hook_shadow::snapshot_for("hooked").expect("recorded");
        assert_eq!(snap.last_event, "PreToolUse");
        assert_eq!(snap.derived_state, Some(crate::state::AgentState::ToolUse));

        // Missing event name → honest error, nothing recorded for that call.
        let bad = handle_hook_event(&json!({"name": "hooked"}), &ctx);
        assert_eq!(bad["ok"], false);
        std::fs::remove_dir_all(&home).ok();
    }
}
