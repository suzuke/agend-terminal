use crate::agent_ops::{save_metadata, save_metadata_batch};
use crate::identity::Sender;
use serde_json::{json, Value};
use std::path::Path;

use super::err_needs_identity;

pub(super) fn handle_set_display_name(home: &Path, args: &Value, instance_name: &str) -> Value {
    let display_name = args["name"].as_str().unwrap_or("");
    // #1604: empty/missing `name` = explicit CLEAR (reset to the default = the
    // agent name; see layout::pane::display_name's `unwrap_or(&agent_name)`),
    // mirroring set_waiting_on. Pre-#1604 it saved `""` → the pane showed a
    // blank name instead of falling back. Clear stores `null` so the reader's
    // Option is None.
    if display_name.is_empty() {
        save_metadata(home, instance_name, "display_name", json!(null));
        return json!({"cleared": true});
    }
    if display_name.len() > 256 {
        return json!({"error": "display_name exceeds 256 character limit"});
    }
    save_metadata(home, instance_name, "display_name", json!(display_name));
    json!({"display_name": display_name})
}

pub(super) fn handle_set_description(home: &Path, args: &Value, instance_name: &str) -> Value {
    let desc = args["description"].as_str().unwrap_or("");
    // #1604: empty/missing `description` = explicit CLEAR (store `null`),
    // consistent with set_display_name + set_waiting_on.
    if desc.is_empty() {
        save_metadata(home, instance_name, "description", json!(null));
        return json!({"cleared": true});
    }
    if desc.len() > 1024 {
        return json!({"error": "description exceeds 1024 character limit"});
    }
    save_metadata(home, instance_name, "description", json!(desc));
    json!({"description": desc})
}

pub(super) fn handle_interrupt(home: &Path, args: &Value) -> Value {
    let target = match args["instance"].as_str() {
        Some(t) => t,
        None => return json!({"error": "missing 'instance'"}),
    };
    crate::validate_name_or_err!(target);
    match crate::api::call(home, &super::interrupt_esc_params(target)) {
        Ok(resp) if resp["ok"].as_bool() == Some(true) => {
            if let Some(reason) = args["reason"].as_str() {
                let header = crate::inbox::format_event_header("interrupt", &[("reason", reason)]);
                crate::inbox::compose_aware_inject(home, target, &header);
            }
            let mut result = json!({"ok": true, "target": target});
            if args["snapshot"].as_bool() == Some(true) {
                if let Ok(snap) = crate::api::call(
                    home,
                    &json!({"method": crate::api::method::PANE_SNAPSHOT, "params": {"name": target, "lines": 40}}),
                ) {
                    if snap["ok"].as_bool() == Some(true) {
                        result["snapshot"] = snap["text"].clone();
                    }
                }
            }
            result
        }
        Ok(resp) => json!({"error": resp["error"].as_str().unwrap_or("inject failed")}),
        Err(e) => {
            json!({"error": format!("interrupt failed — agent '{target}' not reachable (API unavailable: {e})")})
        }
    }
}

pub(super) fn handle_set_waiting_on(
    home: &Path,
    args: &Value,
    instance_name: &str,
    sender: &Option<Sender>,
) -> Value {
    let Some(_) = sender.as_ref() else {
        return err_needs_identity("set_waiting_on");
    };
    let condition = args["condition"].as_str().unwrap_or("");
    if condition.is_empty() {
        crate::daemon::heartbeat_pair::update_with(instance_name, |p| {
            p.waiting_on_since_ms = None;
        });
        save_metadata_batch(
            home,
            instance_name,
            &[
                ("waiting_on", json!(null)),
                ("waiting_on_since", json!(null)),
            ],
        );
        json!({"cleared": true})
    } else {
        let now_ms = crate::daemon::heartbeat_pair::now_ms();
        crate::daemon::heartbeat_pair::update_with(instance_name, |p| {
            p.heartbeat_at_ms = now_ms;
            p.waiting_on_since_ms = Some(now_ms);
        });
        let now = chrono::Utc::now().to_rfc3339();
        save_metadata_batch(
            home,
            instance_name,
            &[
                ("waiting_on", json!(condition)),
                ("waiting_on_since", json!(&now)),
            ],
        );
        json!({"waiting_on": condition, "since": now})
    }
}

pub(super) fn handle_move_pane(home: &Path, args: &Value) -> Value {
    // MCP arg is `instance`; the daemon RPC contract names the field `agent`.
    let instance = args["instance"].as_str().unwrap_or("");
    let params = json!({
        "agent": instance,
        "target_tab": args["target_tab"],
        "split_dir": args["split_dir"],
    });
    match crate::api::call(
        home,
        &json!({"method": crate::api::method::MOVE_PANE, "params": params}),
    ) {
        Ok(resp) if resp["ok"].as_bool() == Some(true) => {
            json!({"ok": true, "instance": instance, "target_tab": args["target_tab"]})
        }
        Ok(resp) => json!({"error": resp["error"].as_str().unwrap_or("move_pane failed")}),
        Err(e) => json!({"error": format!("move_pane: {e}")}),
    }
}

pub(super) fn handle_pane_snapshot(home: &Path, args: &Value) -> Value {
    let target = match args["instance"].as_str() {
        Some(t) => t,
        None => return json!({"error": "missing 'instance'"}),
    };
    crate::validate_name_or_err!(target);
    let lines_u64 = args["lines"].as_u64().unwrap_or(100);
    // M1: explicit bounds check before u64→usize cast (32-bit safety)
    if lines_u64 > 10000 {
        return json!({"error": "lines must be <= 10000 (scrolling_history limit)"});
    }
    let lines = lines_u64 as usize;
    match crate::api::call(
        home,
        &json!({"method": crate::api::method::PANE_SNAPSHOT, "params": {"name": target, "lines": lines}}),
    ) {
        Ok(resp) if resp["ok"].as_bool() == Some(true) => {
            json!({"ok": true, "text": resp["text"]})
        }
        Ok(resp) => json!({"error": resp["error"].as_str().unwrap_or("pane_snapshot failed")}),
        Err(e) => json!({"error": format!("pane_snapshot: {e}")}),
    }
}

pub(super) fn handle_report_health(
    home: &Path,
    args: &Value,
    instance_name: &str,
    sender: &Option<Sender>,
) -> Value {
    let Some(_) = sender.as_ref() else {
        return err_needs_identity("report_health");
    };
    let reason = match args["reason"].as_str() {
        Some(r) => r,
        None => return json!({"error": "missing 'reason'"}),
    };
    match crate::api::call(
        home,
        &json!({
            "method": crate::api::method::SET_BLOCKED_REASON,
            "params": {
                "name": instance_name,
                "reason": reason,
                "retry_after_secs": args.get("retry_after_secs"),
                // #1933: forward the operator-readable note (was dropped here — the
                // schema advertised it but no mechanism consumed it).
                "note": args.get("note")
            }
        }),
    ) {
        Ok(resp) if resp["ok"].as_bool() == Some(true) => {
            json!({
                "status": "reason_set",
                "reason": reason,
                "current_state": resp["current_state"]
            })
        }
        Ok(resp) => {
            json!({"error": resp["error"].as_str().unwrap_or("set_blocked_reason failed")})
        }
        Err(e) => json!({"error": format!("{e}")}),
    }
}

pub(super) fn handle_clear_blocked_reason(home: &Path, args: &Value) -> Value {
    let instance = match args["instance"].as_str() {
        Some(n) => n,
        None => return json!({"error": "missing 'instance'"}),
    };
    let mut params = json!({"name": instance});
    if let Some(r) = args["reason"].as_str() {
        params["reason"] = json!(r);
    }
    match crate::api::call(
        home,
        &json!({
            "method": crate::api::method::CLEAR_BLOCKED_REASON,
            "params": params
        }),
    ) {
        Ok(resp) if resp["ok"].as_bool() == Some(true) => {
            json!({
                "status": "cleared",
                "instance": instance,
                "was": resp["was"]
            })
        }
        Ok(resp) => {
            json!({"error": resp["error"].as_str().unwrap_or("clear_blocked_reason failed")})
        }
        Err(e) => json!({"error": format!("{e}")}),
    }
}

// --- Private helpers (moved from mod.rs) ---
