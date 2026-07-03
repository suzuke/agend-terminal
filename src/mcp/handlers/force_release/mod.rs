//! Stale daemon-managed worktree recovery helpers. Originally built to
//! back the standalone `force_release_worktree` MCP tool (Sprint 59 Wave 1
//! PR-5 emergency cherry-pick, closing the architectural defect that drove
//! the Sprint 59 Wave 1 PR-2 BYPASS incident + PR-4 (C)-path stall); #2548
//! PR-2 folded that tool into `release_worktree(force:true)`
//! (`mcp/handlers/worktree.rs`), which is now the sole caller of
//! [`rebase_clean_self`] + [`prune_git_metadata_for_agent`] for that path.
//! [`attempt_safe_rebind_repair`] remains `bind_self(rebase_mode=true)`'s
//! safe-repair-first helper — unaffected by the #2548 fold-in.
//!
//! When `bind_self` returns `lease_failed` because an on-disk
//! worktree dir exists from a prior bind cycle but the daemon
//! binding state was already released, callers had no daemon-
//! managed path to clean the stale dir without resorting to
//! `AGEND_GIT_BYPASS=1`. Per operator's Q2=(C) bypass-free
//! permanent protocol decision (2026-05-09), this module ships the
//! daemon-side cleanup logic so the (C) path can recover from
//! stale-state without ever touching BYPASS.
//!
//! Extracted from `worktree.rs` to keep that file under the 700
//! LOC handler invariant (`tests/file_size_invariant.rs`).

use serde_json::Value;
use std::path::Path;

mod gc;
mod repair;

pub(crate) use gc::prune_git_metadata_for_agent;
pub(crate) use repair::attempt_safe_rebind_repair;

/// Outcome of a rebase-clean operation.
#[derive(Debug)]
pub(super) struct RebaseCleanOutcome {
    pub(super) dir_existed: bool,
    pub(super) dir_removed: bool,
    pub(super) binding_outcome: Value,
}

/// Sprint 60 W1 PR-1: cleanup helper for the EXPLICIT, operator-callable
/// `force_release_worktree` tool (destructive by design — see its docstring).
///
/// #2496: no longer used by `bind_self(rebase_mode=true)` — that path now
/// tries [`attempt_safe_rebind_repair`] FIRST and fails closed rather than
/// silently landing here (this function's fallthrough to `release_full` is
/// exactly the "as destructive as `release_worktree`" behavior #2496 reported).
///
/// Validates path safety against the daemon worktree pool, removes
/// the stale on-disk dir if present, and clears any lingering binding
/// state via `release_full`. Returns `Err` only on path-safety
/// violation; all other failures are fail-open with tracing::warn so
/// partial recovery is preserved.
///
/// Caller invariant: `agent` and `branch` must be pre-validated by
/// `agent::validate_name` + `agent_ops::validate_branch` respectively.
/// This helper trusts its callers; the path-safety guard below is
/// defense-in-depth, not the primary validator.
pub(super) fn rebase_clean_self(
    home: &Path,
    agent: &str,
    branch: &str,
) -> Result<RebaseCleanOutcome, String> {
    let worktrees_root = home.join("worktrees");
    let target = worktrees_root.join(agent).join(branch);
    let safe = target.starts_with(&worktrees_root)
        && target != worktrees_root
        && target != worktrees_root.join(agent);
    if !safe {
        return Err(format!(
            "refuses to clean path outside the daemon worktree pool: {}",
            target.display()
        ));
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
                    "rebase_clean_self: stale worktree dir cleaned"
                );
            }
            Err(e) => {
                tracing::warn!(
                    %agent,
                    %branch,
                    error = %e,
                    path = %target.display(),
                    "rebase_clean_self: dir removal failed (will still try binding-clear)"
                );
            }
        }
    }

    let binding_outcome = crate::worktree_pool::release_full(home, agent, false);
    // release_full returns early when binding.json is absent (line 244-247
    // in worktree_pool.rs) — skipping clear_bind_in_flight. In the exact
    // stale-state recovery case this tool is for (binding gone, dir present),
    // the in-flight guard would leak and block rebind.
    crate::mcp::handlers::dispatch_hook::clear_bind_in_flight(home, agent);
    Ok(RebaseCleanOutcome {
        dir_existed,
        dir_removed,
        binding_outcome: serde_json::to_value(&binding_outcome).unwrap_or(Value::Null),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;
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

    // ── Sprint 60 W1 PR-1: rebase_clean_self helper tests ──────────────
    //
    // Direct exercise of the shared cleanup helper. handle_bind_self with
    // rebase_mode=true forwards to this helper before the lease attempt;
    // verifying the helper's contract here covers the bind_self call site
    // by construction (the wiring in handle_bind_self is a single
    // `if let Err = rebase_clean_self` branch).

    #[test]
    fn rebase_clean_self_clears_existing_dir_and_invokes_release_full() {
        let home = tmp_home("rebase-clean-existing");
        let dir = seed_daemon_worktree(&home, "dev", "feat/rebase-x");
        assert!(dir.exists());
        let outcome = rebase_clean_self(&home, "dev", "feat/rebase-x")
            .expect("clean state in pool must succeed");
        assert!(outcome.dir_existed);
        assert!(outcome.dir_removed);
        assert!(!dir.exists(), "stale dir must be cleaned");
        assert!(
            outcome.binding_outcome.is_object(),
            "binding_outcome must surface release_full result"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn rebase_clean_self_idempotent_on_clean_state() {
        // No prior bind, no stale dir → helper still runs release_full
        // (idempotent) and reports dir_existed=false.
        let home = tmp_home("rebase-clean-idempotent");
        let outcome = rebase_clean_self(&home, "dev", "feat/never-existed")
            .expect("helper must not error on clean state");
        assert!(!outcome.dir_existed);
        assert!(!outcome.dir_removed);
        // #1465: release_full on missing binding is an idempotent success
        // no-op (released:true, already_released:true, no error).
        let bo = &outcome.binding_outcome;
        assert_eq!(bo["released"].as_bool(), Some(true));
        assert_eq!(bo["already_released"].as_bool(), Some(true));
        assert!(bo["error"].is_null(), "no-op must not error: {bo}");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn rebase_clean_self_rejects_path_outside_worktree_pool() {
        // Defense-in-depth: even if a malicious caller bypassed the
        // outer validators, the helper refuses to clean paths outside
        // <home>/worktrees/. The path-safety check here mirrors
        // force_release_worktree's own guard.
        let home = tmp_home("rebase-outside-pool");
        // An empty branch resolves to <home>/worktrees/dev (the
        // agent-level dir) which the safety check rejects.
        let r = rebase_clean_self(&home, "dev", "");
        assert!(r.is_err(), "empty branch must reject as path-unsafe");
        // A branch with `..` would also escape the pool — but
        // agent_ops::validate_branch already rejects those before this
        // helper is called. The empty-string case is the only path
        // that could slip past upstream validators (e.g. caller passes
        // a JSON null/missing field), so it's the load-bearing guard.
        std::fs::remove_dir_all(&home).ok();
    }

    // ── Sprint 60 W1 PR-1: bind_self handler rebase_mode end-to-end ─────

    #[test]
    fn bind_self_rebase_mode_runs_cleanup_before_lease_attempt() {
        // Seed the stale state that drove the Wave 1 PR-2 BYPASS
        // incident: an on-disk worktree dir + binding lingering from a
        // prior bind cycle. Calling handle_bind_self with
        // rebase_mode=true must clean the dir + binding even though
        // the lease itself will fail (no fleet.yaml + no real git
        // repo in this minimal test fixture).
        //
        // Observable: post-call, the stale dir is gone AND the
        // binding is cleared, regardless of the lease error returned.
        // This proves the rebase_mode wiring runs the cleanup helper
        // before the dispatch_auto_bind_lease call.
        use crate::mcp::handlers::worktree::handle_bind_self;
        let home = tmp_home("bind-rebase-cleanup");
        let dir = seed_daemon_worktree(&home, "dev", "feat/rebase-bind");
        // Seed a binding too so we can verify it's released.
        let runtime = crate::paths::runtime_dir(&home).join("dev");
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::write(
            runtime.join("binding.json"),
            r#"{"agent":"dev","branch":"feat/rebase-bind","worktree":"/stale"}"#,
        )
        .unwrap();
        assert!(dir.exists());

        let _ignored = handle_bind_self(
            &home,
            &json!({"branch": "feat/rebase-bind", "rebase_mode": true}),
            &crate::identity::Sender::new("dev"),
        );
        // Cleanup ran regardless of the downstream lease result.
        assert!(!dir.exists(), "rebase_mode must clean stale dir pre-lease");
        assert!(
            !runtime.join("binding.json").exists(),
            "rebase_mode must clear stale binding pre-lease"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn bind_self_no_rebase_mode_skips_cleanup() {
        // Inverse: without rebase_mode, the cleanup must NOT run —
        // existing behavior is preserved. The pre-existing stale dir
        // remains untouched (dispatch_auto_bind_lease will return
        // its usual lease_failed for the stuck-state scenario).
        use crate::mcp::handlers::worktree::handle_bind_self;
        let home = tmp_home("bind-no-rebase");
        let dir = seed_daemon_worktree(&home, "dev", "feat/no-rebase");
        let _ignored = handle_bind_self(
            &home,
            &json!({"branch": "feat/no-rebase"}),
            &crate::identity::Sender::new("dev"),
        );
        assert!(
            dir.exists(),
            "without rebase_mode, stale dir must be preserved"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
