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
    // #1441: registry is UUID-keyed; resolve name via fleet.yaml to detect a
    // managed-name collision.
    if crate::fleet::resolve_uuid(ctx.home, name).is_some_and(|id| reg.contains_key(&id)) {
        return json!({"ok": false, "error": format!("agent '{name}' already exists (managed)")});
    }
    // CR-2026-06-14 (lock order): release the registry lock BEFORE taking the
    // external lock — this was the ONLY site nesting external INSIDE registry
    // (and it even held registry across the `resolve_uuid` disk read). Holding
    // both established an undocumented registry→external order; a future
    // external-then-registry holder would deadlock against it. The managed-name
    // collision check above is the only thing that needs `reg`, so drop it here.
    drop(reg);
    let mut ext = agent::lock_external(ctx.externals);
    if ext.contains_key(name) {
        return json!({"ok": false, "error": format!("agent '{name}' already exists (external)")});
    }
    let backend = params["backend"].as_str().unwrap_or("unknown");
    // #1891: `pid` is required for liveness tracking — a missing / null /
    // non-integer / 0 pid must be REJECTED, not defaulted to 0. On Unix
    // `is_pid_alive(0)` is `kill(0, 0)` which targets the caller's whole process
    // group → always reports "alive", so a 0 pid would make the external_liveness
    // sweep treat the entry as permanently live → an unreapable zombie
    // registration that squats the name forever.
    let pid = match params["pid"].as_u64() {
        Some(p) if p > 0 && p <= u64::from(u32::MAX) => p as u32,
        _ => {
            return json!({
                "ok": false,
                "error": "missing or invalid 'pid' (required: a positive integer process id)"
            })
        }
    };
    ext.insert(
        name.to_string(),
        agent::ExternalAgentHandle {
            backend_command: backend.to_string(),
            pid,
        },
    );
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::api::handlers::HandlerCtx;
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    fn test_ctx() -> (HandlerCtx<'static>, std::path::PathBuf) {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let home =
            std::env::temp_dir().join(format!("agend-ext-test-{}-{}", std::process::id(), id));
        std::fs::create_dir_all(&home).ok();
        let registry: &'static agent::AgentRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let configs: &'static crate::api::ConfigRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let externals: &'static agent::ExternalRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let home_ref: &'static std::path::Path = Box::leak(home.clone().into_boxed_path());
        let ctx = HandlerCtx {
            registry,
            configs,
            externals,
            notifier: None,
            home: home_ref,
        };
        (ctx, home)
    }

    #[test]
    fn register_external_missing_pid_rejected_1891() {
        // #1891: a missing pid must be REJECTED, not defaulted to 0 (which would
        // be an unreapable zombie). No entry may be inserted.
        let (ctx, home) = test_ctx();
        let resp = handle_register_external(&json!({"name": "ext-a", "backend": "claude"}), &ctx);
        assert_eq!(resp["ok"], json!(false), "missing pid must be rejected");
        assert!(resp["error"].as_str().unwrap().contains("pid"));
        assert!(
            agent::lock_external(ctx.externals).is_empty(),
            "no zombie entry inserted on rejection"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn register_external_zero_pid_rejected_1891() {
        let (ctx, home) = test_ctx();
        let resp = handle_register_external(
            &json!({"name": "ext-b", "backend": "claude", "pid": 0}),
            &ctx,
        );
        assert_eq!(resp["ok"], json!(false), "pid 0 must be rejected");
        assert!(agent::lock_external(ctx.externals).is_empty());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn register_external_valid_pid_registers_1891() {
        let (ctx, home) = test_ctx();
        let resp = handle_register_external(
            &json!({"name": "ext-c", "backend": "claude", "pid": 4242}),
            &ctx,
        );
        assert_eq!(resp["ok"], json!(true), "valid pid must register");
        let ext = agent::lock_external(ctx.externals);
        assert_eq!(
            ext.get("ext-c").map(|h| h.pid),
            Some(4242),
            "entry tracked with the real pid"
        );
        drop(ext);
        std::fs::remove_dir_all(&home).ok();
    }
}
