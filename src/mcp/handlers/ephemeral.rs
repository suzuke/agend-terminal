//! #1967 Phase-1 (PR1 scaffold): MCP `ephemeral` tool handlers (spawn/list/reap).
//!
//! Thin wrappers over [`crate::ephemeral_tracking`]. PR1 `spawn` launches a FAKE
//! `/bin/sleep` child to exercise the lifecycle + cost guards; real headless
//! backend transport is PR2/PR3. Per-action parameter validation lives here (the
//! shared schema validator only enforces the top-level `action`, mirroring `ci`).

use serde_json::{json, Value};
use std::path::Path;

pub(super) fn handle_spawn(home: &Path, args: &Value, _instance_name: &str) -> Value {
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
    match crate::ephemeral_tracking::spawn_and_track(home, spec) {
        Ok(w) => json!({
            "ok": true,
            "worker_id": w.worker_id,
            "pid": w.pid,
            "workflow_id": w.workflow_id,
            "backend": w.backend,
            "ttl_secs": w.ttl_secs,
            "note": "PR1 scaffold: fake /bin/sleep child — no real backend transport yet",
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
