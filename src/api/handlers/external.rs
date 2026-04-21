//! External agent handlers: REGISTER_EXTERNAL, DEREGISTER_EXTERNAL.

use super::HandlerCtx;
use crate::agent;
use serde_json::{json, Value};

pub(crate) fn handle_register_external(params: &Value, ctx: &HandlerCtx) -> Value {
    let name = match params["name"].as_str() {
        Some(n) => n,
        None => return json!({"ok": false, "error": "missing name"}),
    };
    if let Err(e) = agent::validate_name(name) {
        return json!({"ok": false, "error": e});
    }
    let reg = agent::lock_registry(ctx.registry);
    if reg.contains_key(name) {
        return json!({"ok": false, "error": format!("agent '{name}' already exists (managed)")});
    }
    let mut ext = agent::lock_external(ctx.externals);
    if ext.contains_key(name) {
        return json!({"ok": false, "error": format!("agent '{name}' already exists (external)")});
    }
    let backend = params["backend"].as_str().unwrap_or("unknown");
    let pid = params["pid"].as_u64().unwrap_or(0) as u32;
    ext.insert(
        name.to_string(),
        agent::ExternalAgentHandle {
            backend_command: backend.to_string(),
            pid,
        },
    );
    drop(reg);
    drop(ext);
    crate::event_log::log(
        ctx.home,
        "connect",
        name,
        &format!("external agent registered (pid={pid}, backend={backend})"),
    );
    tracing::info!(agent = name, pid, backend, "external agent registered");
    json!({"ok": true})
}

pub(crate) fn handle_deregister_external(params: &Value, ctx: &HandlerCtx) -> Value {
    let name = params["name"].as_str().unwrap_or("");
    if let Err(e) = agent::validate_name(name) {
        return json!({"ok": false, "error": e});
    }
    let mut ext = agent::lock_external(ctx.externals);
    if ext.remove(name).is_some() {
        drop(ext);
        crate::event_log::log(ctx.home, "disconnect", name, "external agent deregistered");
        tracing::info!(agent = name, "external agent deregistered");
        json!({"ok": true})
    } else {
        json!({"ok": false, "error": format!("external agent '{name}' not found")})
    }
}
