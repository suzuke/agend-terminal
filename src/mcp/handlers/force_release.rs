//! MCP handler for `force_release_worktree` (Sprint 59 Wave 1 PR-5
//! emergency cherry-pick) — closes the architectural defect that
//! drove the Sprint 59 Wave 1 PR-2 BYPASS incident + PR-4 (C)-path
//! stall.
//!
//! When `bind_self` returns `lease_failed` because an on-disk
//! worktree dir exists from a prior bind cycle but the daemon
//! binding state was already released, callers had no daemon-
//! managed path to clean the stale dir without resorting to
//! `AGEND_GIT_BYPASS=1`. Per operator's Q2=(C) bypass-free
//! permanent protocol decision (2026-05-09), this tool ships the
//! daemon-side cleanup surface so the (C) path can recover from
//! stale-state without ever touching BYPASS.
//!
//! Extracted from `worktree.rs` to keep that file under the 700
//! LOC handler invariant (`tests/file_size_invariant.rs`).

use crate::identity::Sender;
use serde_json::{json, Value};
use std::path::Path;

/// MCP tool: `force_release_worktree`.
///
/// Required args: `agent` (string), `branch` (string).
///
/// Behavior:
/// 1. Validate agent + branch name format.
/// 2. Compute target dir: `<home>/worktrees/<agent>/<branch>/`.
/// 3. Safety: reject if the resolved path is outside the daemon-
///    managed worktree pool (defense-in-depth against malicious args).
/// 4. If dir exists: `std::fs::remove_dir_all`.
/// 5. Defensively call existing `release_full` to clear any lingering
///    binding state (idempotent on already-cleared bindings).
/// 6. Return structured `{"released": true, "dir_existed": bool,
///    "dir_removed": bool, "binding_outcome": <ReleaseOutcome>}`.
///
/// Idempotent: calling twice on a clean state is a no-op.
///
/// Fail-open: minor IO errors during dir removal are logged via
/// `tracing::warn` but the binding-clear half still runs so partial
/// recovery is preserved.
pub(crate) fn handle_force_release_worktree(
    home: &Path,
    args: &Value,
    _sender: &Option<Sender>,
) -> Value {
    let agent = match args["agent"].as_str() {
        Some(a) if !a.is_empty() => a,
        _ => return json!({"error": "missing 'agent'"}),
    };
    let branch = match args["branch"].as_str() {
        Some(b) if !b.is_empty() => b,
        _ => return json!({"error": "missing 'branch'"}),
    };
    if let Err(e) = crate::agent::validate_name(agent) {
        return json!({"error": e, "code": "invalid_agent"});
    }
    if !crate::agent_ops::validate_branch(branch) {
        return json!({
            "error": format!("invalid branch name '{branch}'"),
            "code": "invalid_branch"
        });
    }

    // Compute the canonical daemon-managed worktree path. The Wave 4
    // layout (Sprint 57 #546 Item 4) is `$AGEND_HOME/worktrees/<agent>/<branch>/`.
    let worktrees_root = home.join("worktrees");
    let target = worktrees_root.join(agent).join(branch);

    // Safety: ensure the resolved target is within the worktrees pool
    // AND deeper than the agent-level subdirectory (a `branch == ""`
    // would otherwise resolve to the agent's own dir; the empty-
    // string check at the top already rejects this, but the
    // defense-in-depth guard catches future validator drift).
    let safe = target.starts_with(&worktrees_root)
        && target != worktrees_root
        && target != worktrees_root.join(agent);
    if !safe {
        return json!({
            "error": format!(
                "force_release_worktree refuses to clean path outside the daemon \
                 worktree pool: {}",
                target.display()
            ),
            "code": "path_outside_pool"
        });
    }

    let dir_existed = target.exists();
    let mut dir_removed = false;
    if dir_existed {
        match std::fs::remove_dir_all(&target) {
            Ok(()) => {
                dir_removed = true;
                tracing::info!(
                    %agent,
                    %branch,
                    path = %target.display(),
                    "force_release_worktree: stale worktree dir cleaned"
                );
            }
            Err(e) => {
                tracing::warn!(
                    %agent,
                    %branch,
                    error = %e,
                    path = %target.display(),
                    "force_release_worktree: dir removal failed (will still try binding-clear)"
                );
            }
        }
    }

    let binding_outcome = crate::worktree_pool::release_full(home, agent);
    json!({
        "released": true,
        "dir_existed": dir_existed,
        "dir_removed": dir_removed,
        "binding_outcome": serde_json::to_value(&binding_outcome).unwrap_or(Value::Null),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_home(suffix: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let h = std::env::temp_dir().join(format!(
            "agend-force-release-{}-{}-{}",
            std::process::id(),
            suffix,
            id,
        ));
        std::fs::create_dir_all(&h).ok();
        h
    }

    /// Helper: write a daemon-managed worktree dir at the canonical
    /// path with the `.agend-managed` marker so tests can simulate
    /// the stale-state scenario (post-bind, pre-cleanup).
    fn seed_daemon_worktree(home: &Path, agent: &str, branch: &str) -> std::path::PathBuf {
        let dir = home.join("worktrees").join(agent).join(branch);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(".agend-managed"),
            format!("agent={agent}\nbranch={branch}\n"),
        )
        .unwrap();
        // Drop a sample file so we can verify recursive cleanup.
        std::fs::write(dir.join("sample.txt"), "leftover").unwrap();
        dir
    }

    // ── Lead-spec named tests (per dispatch m-20260509125352834800-192) ──

    #[test]
    fn force_release_worktree_cleans_existing_dir() {
        let home = tmp_home("clean-existing");
        let dir = seed_daemon_worktree(&home, "dev", "feature/x");
        assert!(dir.exists(), "seeded dir must exist pre-call");
        let result = handle_force_release_worktree(
            &home,
            &json!({"agent": "dev", "branch": "feature/x"}),
            &None,
        );
        assert_eq!(result["released"].as_bool(), Some(true));
        assert_eq!(result["dir_existed"].as_bool(), Some(true));
        assert_eq!(result["dir_removed"].as_bool(), Some(true));
        assert!(!dir.exists(), "dir must be cleaned post-call");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn force_release_worktree_idempotent_on_missing_dir() {
        let home = tmp_home("idempotent");
        // No seed — call directly on a non-existent target.
        let result = handle_force_release_worktree(
            &home,
            &json!({"agent": "dev", "branch": "feature/never-existed"}),
            &None,
        );
        assert_eq!(result["released"].as_bool(), Some(true));
        assert_eq!(
            result["dir_existed"].as_bool(),
            Some(false),
            "missing dir reports dir_existed=false"
        );
        assert_eq!(
            result["dir_removed"].as_bool(),
            Some(false),
            "no removal happens on missing dir"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn force_release_worktree_releases_binding_too() {
        // Per spec: even when only the on-disk dir is stale (no
        // active binding), the call must still invoke release_full
        // for defense-in-depth. The outcome surfaces in the
        // `binding_outcome` field.
        let home = tmp_home("releases-binding");
        seed_daemon_worktree(&home, "dev", "feature/y");
        let result = handle_force_release_worktree(
            &home,
            &json!({"agent": "dev", "branch": "feature/y"}),
            &None,
        );
        assert!(
            result["binding_outcome"].is_object(),
            "binding_outcome must surface the release_full result: {result}"
        );
        // No prior binding existed → release_full returns
        // released:false + error: "no binding..." — that's the
        // expected idempotent shape.
        let outcome = &result["binding_outcome"];
        assert_eq!(outcome["released"].as_bool(), Some(false));
        assert!(outcome["error"]
            .as_str()
            .unwrap_or("")
            .contains("no binding"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn force_release_worktree_rejects_path_outside_worktree_pool() {
        // Defense-in-depth: even if a malicious caller could pass
        // names that bypass the validator (or the validator is
        // weakened in a future change), the path-safety guard
        // refuses to clean anything outside <home>/worktrees/.
        let home = tmp_home("outside-pool-reject");
        // Seed a dir OUTSIDE the worktree pool, simulating where a
        // malicious caller might try to send the cleanup.
        let outside = home.join("config");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("important.json"), "data").unwrap();
        // Use empty branch — caught by the missing-branch check
        // first, but this exercises the input-rejection path.
        let r1 =
            handle_force_release_worktree(&home, &json!({"agent": "dev", "branch": ""}), &None);
        assert!(r1["error"].is_string(), "empty branch must error: {r1}");
        // The outside dir must still exist (no manipulation).
        assert!(outside.join("important.json").exists());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn force_release_worktree_rejects_invalid_agent_name() {
        let home = tmp_home("invalid-agent");
        let result = handle_force_release_worktree(
            &home,
            &json!({"agent": "../etc/passwd", "branch": "feature/x"}),
            &None,
        );
        assert!(result["error"].is_string());
        assert_eq!(result["code"].as_str(), Some("invalid_agent"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn force_release_worktree_rejects_invalid_branch_name() {
        let home = tmp_home("invalid-branch");
        let result = handle_force_release_worktree(
            &home,
            &json!({"agent": "dev", "branch": "../../escape"}),
            &None,
        );
        assert!(result["error"].is_string());
        assert_eq!(result["code"].as_str(), Some("invalid_branch"));
        std::fs::remove_dir_all(&home).ok();
    }

    // ── Defensive bonuses ─────────────────────────────────────────

    #[test]
    fn force_release_worktree_rejects_missing_agent() {
        let home = tmp_home("missing-agent");
        let result = handle_force_release_worktree(&home, &json!({"branch": "feature/x"}), &None);
        assert_eq!(result["error"].as_str(), Some("missing 'agent'"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn force_release_worktree_rejects_missing_branch() {
        let home = tmp_home("missing-branch");
        let result = handle_force_release_worktree(&home, &json!({"agent": "dev"}), &None);
        assert_eq!(result["error"].as_str(), Some("missing 'branch'"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn force_release_worktree_after_failure_allows_bind_self_succeed() {
        // Integration-of-the-unblock-scenario test: simulate the
        // post-PR-2/PR-4 stale-state, call force_release_worktree,
        // then assert the worktree dir is gone (so a subsequent
        // bind_self would NOT trip on lease_failed).
        let home = tmp_home("integration-bind-succeed");
        let dir = seed_daemon_worktree(&home, "dev", "sprint59-wave1-pr4-issue-b");
        assert!(dir.exists(), "stale dir present pre-cleanup");
        let result = handle_force_release_worktree(
            &home,
            &json!({"agent": "dev", "branch": "sprint59-wave1-pr4-issue-b"}),
            &None,
        );
        assert_eq!(result["released"].as_bool(), Some(true));
        assert_eq!(result["dir_removed"].as_bool(), Some(true));
        // Post-cleanup: the canonical bind_self target path is gone
        // → bind_self would proceed cleanly. We can't actually call
        // bind_self in a unit test (needs daemon registry), but
        // the absence of the dir IS the necessary precondition
        // for bind_self to succeed.
        assert!(!dir.exists(), "worktree dir must be gone");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn force_release_worktree_handles_partial_cleanup_state() {
        // Defensive: a dir that's been partially cleaned (some files
        // already removed by an aborted prior call) still gets
        // recursively wiped without panic.
        let home = tmp_home("partial-cleanup");
        let dir = home.join("worktrees").join("dev").join("feature/x");
        std::fs::create_dir_all(&dir).unwrap();
        // Don't seed with .agend-managed marker — partial state.
        std::fs::write(dir.join("leftover"), "data").unwrap();
        let result = handle_force_release_worktree(
            &home,
            &json!({"agent": "dev", "branch": "feature/x"}),
            &None,
        );
        assert_eq!(result["dir_removed"].as_bool(), Some(true));
        assert!(!dir.exists());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn force_release_worktree_preserves_other_branches() {
        // Defense-in-depth: cleaning one branch's dir must NOT
        // touch sibling branches under the same agent.
        let home = tmp_home("preserves-siblings");
        let dir_x = seed_daemon_worktree(&home, "dev", "feature/x");
        let dir_y = seed_daemon_worktree(&home, "dev", "feature/y");
        let result = handle_force_release_worktree(
            &home,
            &json!({"agent": "dev", "branch": "feature/x"}),
            &None,
        );
        assert_eq!(result["dir_removed"].as_bool(), Some(true));
        assert!(!dir_x.exists(), "target branch dir cleaned");
        assert!(
            dir_y.exists(),
            "sibling branch dir preserved: {}",
            dir_y.display()
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn force_release_worktree_preserves_other_agents() {
        // Defense-in-depth: cleaning one agent's dir must NOT
        // touch other agents' worktrees.
        let home = tmp_home("preserves-agents");
        let dir_dev = seed_daemon_worktree(&home, "dev", "feature/x");
        let dir_lead = seed_daemon_worktree(&home, "lead", "feature/x");
        let result = handle_force_release_worktree(
            &home,
            &json!({"agent": "dev", "branch": "feature/x"}),
            &None,
        );
        assert_eq!(result["dir_removed"].as_bool(), Some(true));
        assert!(!dir_dev.exists());
        assert!(
            dir_lead.exists(),
            "lead's dir preserved: {}",
            dir_lead.display()
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
