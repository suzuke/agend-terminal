//! External agent handlers: REGISTER_EXTERNAL, DEREGISTER_EXTERNAL.

use super::HandlerCtx;
use crate::agent;
use serde_json::{json, Value};

pub(crate) fn handle_register_external(params: &Value, ctx: &HandlerCtx) -> Value {
    register_external_with_seam(params, ctx, None)
}

/// `before_managed_recheck_seam`: test-only injection point fired WHILE the
/// external lock is held but BEFORE the managed re-check + insert — used to
/// deterministically reproduce a concurrent managed spawn landing in the
/// register-external window (t-65). `None` in production.
fn register_external_with_seam(
    params: &Value,
    ctx: &HandlerCtx,
    before_managed_recheck_seam: Option<&dyn Fn()>,
) -> Value {
    let name = match params["name"].as_str() {
        Some(n) => n,
        None => return json!({"ok": false, "error": "missing name"}),
    };
    if let Err(e) = agent::validate_name(name) {
        return json!({"ok": false, "error": e});
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
    // t-65 (TOCTOU) + #2197 (lock order): take the external lock FIRST, then the
    // registry lock (external→registry — the safe order). #2197 removed every
    // registry→external nesting (audited across all `lock_external` sites: each
    // drops the registry first or never holds it), so external→registry has no
    // AB-BA partner. Holding BOTH across the managed re-check + insert makes
    // "no managed `name`" + "insert external `name`" atomic w.r.t. a concurrent
    // managed spawn (which serializes on the registry lock to insert) — closing
    // the check→insert TOCTOU. The registry lock is NEVER taken before the
    // external lock here, so this is NOT the registry→external nesting #2197
    // removed.
    let mut ext = agent::lock_external(ctx.externals);
    if ext.contains_key(name) {
        return json!({"ok": false, "error": format!("agent '{name}' already exists (external)")});
    }
    if let Some(seam) = before_managed_recheck_seam {
        seam();
    }
    {
        let reg = agent::lock_registry(ctx.registry);
        if crate::fleet::resolve_uuid(ctx.home, name).is_some_and(|id| reg.contains_key(&id)) {
            return json!({"ok": false, "error": format!("agent '{name}' already exists (managed)")});
        }
        ext.insert(
            name.to_string(),
            agent::ExternalAgentHandle {
                backend_command: backend.to_string(),
                pid,
            },
        );
    }
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

    /// t-65: a managed agent created in the register-external window — while the
    /// external lock is held, before the managed re-check + insert — must NOT
    /// produce a managed/external name collision. The fix re-checks the managed
    /// registry under the external lock (external→registry) before inserting;
    /// pre-fix (insert without that re-check) the external entry was written
    /// despite the managed twin. Deterministic via the
    /// `before_managed_recheck_seam` hook (no flaky thread timing). `unix`-gated:
    /// `mk_test_handle` spawns a `true` PTY.
    #[cfg(unix)]
    #[test]
    fn register_external_rejects_managed_created_in_toctou_window_t65() {
        const ID: &str = "a1a1a1a1-0000-4000-8000-000000000165";
        let (ctx, home) = test_ctx();
        let name = "twin";

        // Seam fires in the window (external lock held, before the managed
        // re-check + insert): land a concurrent managed `twin` in fleet.yaml +
        // registry so the re-check sees it.
        let inject = || {
            let id = crate::types::InstanceId::parse(ID).unwrap();
            std::fs::write(
                crate::fleet::fleet_yaml_path(ctx.home),
                format!("instances:\n  {name}:\n    id: {ID}\n"),
            )
            .expect("write fleet.yaml");
            ctx.registry
                .lock()
                .insert(id, agent::mk_test_handle(name, id));
        };

        let resp = super::register_external_with_seam(
            &json!({"name": name, "backend": "claude", "pid": 4242}),
            &ctx,
            Some(&inject),
        );

        // Post-fix: external registration must be REJECTED — the managed twin
        // appeared in the window, so registering it as external would collide.
        assert_eq!(
            resp["ok"],
            json!(false),
            "must reject — managed twin appeared in the window: {resp:?}"
        );
        assert!(
            resp["error"].as_str().unwrap().contains("managed"),
            "rejection must cite the managed collision: {resp:?}"
        );
        // No external `twin` entry — no managed/external name collision.
        assert!(
            !agent::lock_external(ctx.externals).contains_key(name),
            "no external entry may be inserted for a name now held by a managed agent"
        );

        // cleanup: kill the throwaway managed child + remove temp home.
        for h in ctx.registry.lock().values() {
            let _ = h.child.lock().kill();
        }
        std::fs::remove_dir_all(&home).ok();
    }
}
