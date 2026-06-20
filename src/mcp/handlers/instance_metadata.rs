use crate::agent_ops::{save_metadata, save_metadata_batch};
use crate::identity::Sender;
use serde_json::{json, Value};
use std::path::Path;

use super::err_needs_identity;

/// #2050 simplify PR-B (⑩): shared body for the `set_display_name` /
/// `set_description` metadata setters (byte-identical to the former inline code).
///
/// #1604 semantics: an empty `value` = explicit CLEAR — store `null` (so the
/// reader's `Option` is `None`, falling back to the default) and return
/// `{"cleared": true}`. Over `max_len` (BYTES — `str::len`, unchanged) →
/// `{"error": "<attr> exceeds <max_len> character limit"}`. Otherwise persist and
/// echo `{"<attr>": value}`. Exactly one `save_metadata` write executes per call.
fn set_string_attr(
    home: &Path,
    instance_name: &str,
    value: &str,
    attr: &str,
    max_len: usize,
) -> Value {
    if value.is_empty() {
        save_metadata(home, instance_name, attr, json!(null));
        return json!({"cleared": true});
    }
    if value.len() > max_len {
        return json!({"error": format!("{attr} exceeds {max_len} character limit")});
    }
    save_metadata(home, instance_name, attr, json!(value));
    json!({ attr: value })
}

pub(super) fn handle_set_display_name(home: &Path, args: &Value, instance_name: &str) -> Value {
    let display_name = args["name"].as_str().unwrap_or("");
    set_string_attr(home, instance_name, display_name, "display_name", 256)
}

pub(super) fn handle_set_description(home: &Path, args: &Value, instance_name: &str) -> Value {
    let desc = args["description"].as_str().unwrap_or("");
    set_string_attr(home, instance_name, desc, "description", 1024)
}

pub(super) fn handle_interrupt(home: &Path, args: &Value) -> Value {
    let target = match super::require_instance(args) {
        Ok(t) => t,
        Err(e) => return e,
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
    let target = match super::require_instance(args) {
        Ok(t) => t,
        Err(e) => return e,
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
    let instance = match super::require_instance(args) {
        Ok(n) => n,
        Err(e) => return e,
    };
    // CR-2026-06-14: validate the instance name at the MCP boundary before the
    // CLEAR_BLOCKED_REASON RPC, mirroring the sibling handlers in this file
    // (handle_interrupt / handle_pane_snapshot). Without it a malformed name
    // (`../evil`) was forwarded straight to the daemon.
    crate::validate_name_or_err!(instance);
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
