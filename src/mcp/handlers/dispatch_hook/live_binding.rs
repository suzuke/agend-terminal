//! #2158: the "may this re-dispatch SKIP the lease?" predicate, extracted from
//! `dispatch_auto_bind_lease_with_source_and_chain` so `dispatch_hook/mod.rs`
//! stays under its grandfathered LOC ceiling (tests/file_size_invariant.rs).

use std::path::Path;

/// True iff the agent's `existing` binding is LIVE for the EXACT requested
/// `(source_repo, branch)` pair, so re-leasing would be a destructive no-op and
/// the caller may early-return: the worktree dir is still on disk AND
/// `binding.source_repo` equals the resolved request `source_repo`. The caller
/// has already matched the branch.
///
/// Why the skip exists: it avoids the lease's `worktree::create` →
/// `sync_worktree_to_head` (`git reset --hard HEAD` + `clean -fd`) that DESTROYS
/// the worktree's uncommitted work — it wiped an uncommitted PR-B in prod
/// (d-…563566-0).
///
/// The `source_repo` check is the #2158 r6 fix: the lease identity is
/// `(source_repo, branch)` but the worktree path (`worktree::worktree_path`) is
/// REPO-INDEPENDENT, so a same-branch re-dispatch carrying a DIFFERENT
/// `source_repo` must NOT skip — skipping would strand the stale
/// `binding.source_repo` and bypass ensure_branch_exists / worktree::create /
/// sync / bind_full. Fail-closed: a legacy binding missing the `source_repo`
/// field (pre-#2117 P3b) returns `false` → the normal rebind/reset path runs.
///
/// Gated on binding PRESENCE, not dirtiness: a true worktree REUSE first RELEASES
/// (clears the binding → `binding::read` → None → this is never reached → the
/// reuse-path reset scrubs #869 ref-advance residue, #2115 preserved).
pub(super) fn can_skip_lease_for_live_binding(
    existing: &serde_json::Value,
    requested_source_repo: &str,
) -> bool {
    let worktree_live = existing
        .get("worktree")
        .and_then(|v| v.as_str())
        .is_some_and(|w| Path::new(w).exists());
    let same_source_repo = existing
        .get("source_repo")
        .and_then(|v| v.as_str())
        .is_some_and(|r| r == requested_source_repo);
    worktree_live && same_source_repo
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn live_dir() -> String {
        // temp_dir always exists on disk → satisfies the worktree-live arm.
        std::env::temp_dir().display().to_string()
    }

    #[test]
    fn skips_when_same_source_repo_and_worktree_live() {
        let b = json!({"worktree": live_dir(), "source_repo": "/repo/a"});
        assert!(can_skip_lease_for_live_binding(&b, "/repo/a"));
    }

    #[test]
    fn no_skip_when_source_repo_differs_2158_r6() {
        // r6's counter-example: same branch, DIFFERENT source_repo → must NOT skip.
        let b = json!({"worktree": live_dir(), "source_repo": "/repo/a"});
        assert!(!can_skip_lease_for_live_binding(&b, "/repo/b"));
    }

    #[test]
    fn no_skip_when_legacy_binding_missing_source_repo() {
        // Fail-closed: a pre-#2117-P3b binding without the source_repo field.
        let b = json!({"worktree": live_dir()});
        assert!(!can_skip_lease_for_live_binding(&b, "/repo/a"));
    }

    #[test]
    fn no_skip_when_worktree_gone() {
        let b = json!({"worktree": "/nonexistent/agend-2158-x", "source_repo": "/repo/a"});
        assert!(!can_skip_lease_for_live_binding(&b, "/repo/a"));
    }
}
