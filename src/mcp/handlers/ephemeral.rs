//! #1967 Phase-1: MCP `ephemeral` tool handlers (spawn/list/reap).
//!
//! Thin wrappers over [`crate::ephemeral_tracking`]. Per-action parameter
//! validation lives here (the shared schema validator only enforces the top-level
//! `action`, mirroring `ci`).

use serde_json::{json, Value};
use std::path::Path;
use std::sync::OnceLock;

/// PR2 SAFETY GATE (r4 MEDIUM): the production `ephemeral spawn` path must NOT
/// launch a real headless backend until PR3 lands the deferred safety
/// prerequisites — git-shim PATH shadowing + `AGEND_REAL_GIT` (#1504), #1440
/// env-isolation, cwd validation/provisioning, fleet-env #2106 filtering. Without
/// them a real backend would ESCAPE the git-shim gate and inherit the daemon's
/// full env at startup, and "no protocol → no work" does NOT stop a backend's
/// STARTUP behavior (git/env reads, opencode autoupdate). An injected agent could
/// call `ephemeral spawn` to exploit that, so the gate is enforced in CODE here,
/// not merely documented.
///
/// Default OFF → real-backend spawn is rejected (safe against the injected-agent
/// threat; the agent cannot set the daemon's env). An operator may set
/// `AGEND_EPHEMERAL_REAL_BACKEND=1` to opt in for dev/testing, EXPLICITLY
/// accepting that the PR3 safety prerequisites are not yet applied — do NOT enable
/// in production until PR3. The spawn MECHANISM (reserve→spawn→finalize, reap) is
/// fully exercised regardless via the stub transport in `ephemeral_tracking`
/// unit tests; this gate only fences the MCP entry point.
fn real_backend_spawn_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("AGEND_EPHEMERAL_REAL_BACKEND").as_deref() == Ok("1"))
}

pub(super) fn handle_spawn(home: &Path, args: &Value, _instance_name: &str) -> Value {
    if !real_backend_spawn_enabled() {
        return json!({"error": "ephemeral spawn: disabled in PR2 (mechanism-only). Launching a real \
            headless backend requires PR3 safety prerequisites not yet landed — git-shim PATH + \
            AGEND_REAL_GIT (#1504), #1440 env-isolation, cwd validation, fleet-env #2106 filtering. \
            Until then a real backend would escape the git-shim gate / inherit the daemon env. See \
            docs/design/1967-ephemeral-phase1.md. (Operators may set AGEND_EPHEMERAL_REAL_BACKEND=1 \
            to opt in for dev/testing, accepting the missing safety.)"});
    }
    let workflow_id = match args["workflow_id"].as_str().filter(|s| !s.is_empty()) {
        Some(s) => s.to_string(),
        None => {
            return json!({"error": "ephemeral spawn: missing required parameter 'workflow_id'"})
        }
    };
    let backend = match args["backend"].as_str().filter(|s| !s.is_empty()) {
        Some(s) => s.to_string(),
        None => return json!({"error": "ephemeral spawn: missing required parameter 'backend'"}),
    };
    let spec = crate::ephemeral_tracking::SpawnSpec {
        workflow_id,
        parent: args["parent"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        backend,
        ttl_secs: args["ttl_secs"].as_u64(),
        token_budget: args["token_budget"].as_u64(),
    };
    match crate::ephemeral_tracking::spawn_and_track(home, spec, &crate::headless::StdioTransport) {
        Ok(w) => json!({
            "ok": true,
            "worker_id": w.worker_id,
            "pid": w.pid,
            "workflow_id": w.workflow_id,
            "backend": w.backend,
            "ttl_secs": w.ttl_secs,
            "note": "PR2: real headless process (no-PTY, piped stdio); no protocol driving it yet (PR3 ACP)",
        }),
        Err(e) => json!({"error": e.to_string()}),
    }
}

pub(super) fn handle_list(home: &Path, args: &Value) -> Value {
    let wf = args["workflow_id"].as_str().filter(|s| !s.is_empty());
    let workers = crate::ephemeral_tracking::list(home, wf);
    let count = workers.len();
    json!({
        "workers": workers,
        "count": count,
        "max_live": crate::ephemeral_tracking::MAX_LIVE_WORKERS,
    })
}

pub(super) fn handle_reap(home: &Path, args: &Value) -> Value {
    if let Some(id) = args["worker_id"].as_str().filter(|s| !s.is_empty()) {
        return match crate::ephemeral_tracking::reap_one(home, id) {
            Some(w) => json!({"ok": true, "reaped": [w.worker_id]}),
            None => json!({"error": format!("ephemeral reap: no worker with id '{id}'")}),
        };
    }
    if let Some(wf) = args["workflow_id"].as_str().filter(|s| !s.is_empty()) {
        let ids: Vec<String> = crate::ephemeral_tracking::reap_workflow(home, wf)
            .into_iter()
            .map(|w| w.worker_id)
            .collect();
        let count = ids.len();
        return json!({"ok": true, "reaped": ids, "count": count});
    }
    if args["all_stale"].as_bool() == Some(true) {
        let ids: Vec<String> = crate::ephemeral_tracking::reap_sweep(home)
            .into_iter()
            .map(|w| w.worker_id)
            .collect();
        let count = ids.len();
        return json!({"ok": true, "reaped": ids, "count": count});
    }
    json!({"error": "ephemeral reap: specify worker_id, workflow_id, or all_stale=true"})
}

#[cfg(test)]
mod tests {
    use super::*;

    /// r4 MEDIUM safety gate: with the opt-in flag OFF (the default), the
    /// production MCP `ephemeral spawn` must REJECT a real-backend request — it
    /// must never reach `which::which` / spawn a real backend that would escape
    /// the git-shim gate before PR3's safety prerequisites land. (The spawn
    /// mechanism itself is exercised separately via the stub transport in
    /// `ephemeral_tracking` unit tests, which bypass this MCP entry gate.)
    #[test]
    fn handle_spawn_gated_rejects_real_backend_by_default() {
        let home = std::env::temp_dir();
        let args = json!({"action": "spawn", "workflow_id": "wf1", "backend": "claude"});
        let res = handle_spawn(&home, &args, "tester");
        let err = res["error"].as_str().unwrap_or("");
        assert!(
            err.contains("mechanism-only") && err.contains("PR3"),
            "real-backend ephemeral spawn must be gated off by default: {res}"
        );
        assert_ne!(
            res["ok"].as_bool(),
            Some(true),
            "gated spawn must not report ok"
        );
    }
}
