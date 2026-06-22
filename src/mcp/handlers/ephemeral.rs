//! #1967 Phase-1: MCP `ephemeral` tool handlers (spawn/list/reap).
//!
//! Thin wrappers over [`crate::ephemeral_tracking`]. Per-action parameter
//! validation lives here (the shared schema validator only enforces the top-level
//! `action`, mirroring `ci`).

use serde_json::{json, Value};
use std::path::Path;
use std::sync::OnceLock;

/// PR3a FEATURE-OPT-IN GATE: the production `ephemeral spawn` path is OFF by
/// default so the feature is reversible at the daemon boundary. The PR2 safety
/// prerequisites are now LANDED — PR3a spawns the real backend SAFELY via the PTY
/// path ([`crate::agent::spawn_ephemeral_worker`]), which calls the SAME
/// `build_command` a managed agent uses, so the worker inherits git-shim PATH +
/// `AGEND_REAL_GIT` (#1504), #1440 env-isolation, cwd two-pass validation,
/// fleet-env #2106 filtering, and the `AGEND_GIT_BYPASS` strip (#708) IDENTICALLY.
/// The flag therefore now gates the FEATURE opt-in (a default-OFF, reversible
/// rollout of headless ephemeral workers), NOT "the prereqs are missing".
///
/// Default OFF → real-backend spawn is rejected. An operator sets
/// `AGEND_EPHEMERAL_REAL_BACKEND=1` to opt in. The spawn MECHANISM
/// (reserve→spawn→finalize, reap) is exercised regardless via the
/// `ephemeral_tracking` unit tests; this gate only fences the MCP entry point.
fn real_backend_spawn_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("AGEND_EPHEMERAL_REAL_BACKEND").as_deref() == Ok("1"))
}

pub(super) fn handle_spawn(home: &Path, args: &Value, _instance_name: &str) -> Value {
    if !real_backend_spawn_enabled() {
        return json!({"error": "ephemeral spawn: disabled by default (reversible feature opt-in). \
            PR3a spawns the real backend SAFELY via the PTY path (reusing the managed-agent \
            build_command — git-shim PATH + AGEND_REAL_GIT #1504, #1440 env-isolation, cwd \
            validation, fleet-env #2106 filtering all apply), so this flag now gates the FEATURE \
            opt-in, not missing prerequisites. Set AGEND_EPHEMERAL_REAL_BACKEND=1 to enable. See \
            docs/design/1967-ephemeral-phase1.md."});
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
    // PR3b: an optional `prompt` launches the one-shot driver (inject → turn-end →
    // capture → oracle), opencode ONLY (Slice-1; `spawn_and_track` rejects a prompt for
    // any other backend). An optional `model` overrides the worker's default model
    // (provider-prefixed for opencode). A prompt-less spawn is the PR3a lifecycle path.
    let prompt = args["prompt"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_default();
    let driven = !prompt.is_empty();
    let spec = crate::ephemeral_tracking::SpawnSpec {
        workflow_id,
        parent: args["parent"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        backend,
        ttl_secs: args["ttl_secs"].as_u64(),
        token_budget: args["token_budget"].as_u64(),
        prompt,
        model: args["model"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    };
    match crate::ephemeral_tracking::spawn_and_track(home, spec) {
        Ok(w) => json!({
            "ok": true,
            "worker_id": w.worker_id,
            "pid": w.pid,
            "workflow_id": w.workflow_id,
            "backend": w.backend,
            "ttl_secs": w.ttl_secs,
            "driven": driven,
            "note": if driven {
                "PR3b: spawned headlessly via PTY (no pane, no roster); a one-shot driver is \
                 running the prompt ASYNC — poll `ephemeral list` for result_summary/success"
            } else {
                "PR3a: real backend spawned headlessly via PTY (no pane, no roster); no prompt \
                 given, so lifecycle-only (spawn + reap, no driver)"
            },
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
        "max_live": crate::ephemeral_tracking::max_live_workers(),
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

    /// Feature opt-in gate: with the opt-in flag OFF (the default), the production
    /// MCP `ephemeral spawn` must REJECT a real-backend request — it must never
    /// reach `spawn_ephemeral_worker`. (The spawn mechanism itself is exercised
    /// separately in `ephemeral_tracking` unit tests, which bypass this MCP gate.)
    #[test]
    fn handle_spawn_gated_rejects_real_backend_by_default() {
        let home = std::env::temp_dir();
        let args = json!({"action": "spawn", "workflow_id": "wf1", "backend": "claude"});
        let res = handle_spawn(&home, &args, "tester");
        let err = res["error"].as_str().unwrap_or("");
        assert!(
            err.contains("disabled by default") && err.contains("AGEND_EPHEMERAL_REAL_BACKEND"),
            "real-backend ephemeral spawn must be gated off by default: {res}"
        );
        assert_ne!(
            res["ok"].as_bool(),
            Some(true),
            "gated spawn must not report ok"
        );
    }
}
