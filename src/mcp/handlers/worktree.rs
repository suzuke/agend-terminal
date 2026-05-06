//! MCP handler: `release_worktree` — Sprint 53 P0-X.
//!
//! Closes the gap left by P0-1's auto-bind/auto-lease: every PR-merge
//! transition leaves a stale `.worktrees/<agent>` plus
//! `runtime/<agent>/binding.json` behind, and the next dispatch trips
//! P0-1.6's actual-HEAD check. This MCP tool releases the daemon-managed
//! worktree + clears the binding so the next dispatch can lease cleanly.
//!
//! Operator-callable + agent-callable. An agent calling this on itself is
//! valid — it's the symmetric counterpart of dispatch's auto-bind.

use crate::identity::Sender;
use serde_json::{json, Value};
use std::path::Path;

/// MCP tool: `release_worktree`.
///
/// Required arg: `agent` (string).
///
/// Returns:
/// - `released`: `true` when the binding was cleared (worktree may still
///   exist if removal was skipped or partially failed — see `error`).
/// - `worktree_removed`: `true` when the worktree directory was actually
///   removed via `git worktree remove --force` (or fallback).
/// - `binding_removed`: `true` when `runtime/<agent>/binding.json` was
///   deleted.
/// - `error`: optional human-readable error. Idempotent second call returns
///   `released: false, error: "no binding for agent X"` per spec.
pub(crate) fn handle_release_worktree(
    home: &Path,
    args: &Value,
    _sender: &Option<Sender>,
) -> Value {
    let agent = match args["agent"].as_str() {
        Some(a) if !a.is_empty() => a,
        _ => return json!({"error": "missing 'agent'"}),
    };
    if let Err(e) = crate::agent::validate_name(agent) {
        return json!({"error": e});
    }
    let outcome = crate::worktree_pool::release_full(home, agent);
    serde_json::to_value(&outcome).unwrap_or_else(|_| json!({"error": "serialize failed"}))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tmp_home(suffix: &str) -> std::path::PathBuf {
        let h = std::env::temp_dir().join(format!(
            "agend-p0x-handler-{}-{}",
            std::process::id(),
            suffix
        ));
        std::fs::create_dir_all(&h).ok();
        h
    }

    #[test]
    fn handler_rejects_missing_agent() {
        let home = tmp_home("no-agent");
        let result = handle_release_worktree(&home, &json!({}), &None);
        assert_eq!(
            result["error"].as_str(),
            Some("missing 'agent'"),
            "missing agent must surface clear error: {result}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn handler_rejects_invalid_agent_name() {
        let home = tmp_home("bad-name");
        // Agent names with `..` are rejected by validate_name.
        let result = handle_release_worktree(&home, &json!({"agent": "../etc/passwd"}), &None);
        assert!(
            result.get("error").is_some(),
            "invalid agent name must error: {result}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn handler_idempotent_no_binding_returns_released_false() {
        // Production-smoke: handler called via the same path the MCP layer
        // uses (`handle_release_worktree`). With no binding, must return
        // released:false and error indicating no binding — not panic.
        let home = tmp_home("idem-no-binding");
        let result = handle_release_worktree(&home, &json!({"agent": "ghost"}), &None);
        assert_eq!(
            result["released"].as_bool(),
            Some(false),
            "missing binding must report released=false: {result}"
        );
        assert!(
            result["error"]
                .as_str()
                .unwrap_or("")
                .contains("no binding"),
            "error must indicate no binding: {result}"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
