//! #2158: the "may this re-dispatch REUSE the live worktree?" decision, extracted
//! from `dispatch_auto_bind_lease_with_source_and_chain` so `dispatch_hook/mod.rs`
//! stays under its grandfathered LOC ceiling (tests/file_size_invariant.rs).

use std::path::{Path, PathBuf};

/// The live worktree PATH to REUSE (skipping the destructive lease/reset) iff the
/// agent's `existing` binding is LIVE for the EXACT requested `(source_repo, branch)`:
/// the worktree dir is still on disk AND `binding.source_repo` equals the resolved
/// request `source_repo`. `None` → fall through to the normal lease/reset path. The
/// caller has already matched the branch.
///
/// Why reuse instead of re-lease: re-leasing runs `worktree::create` →
/// `sync_worktree_to_head` (`git reset --hard HEAD` + `clean -fd`) which DESTROYS the
/// worktree's uncommitted work — it wiped an uncommitted PR-B in prod (d-…563566-0).
/// The caller reuses this path verbatim and runs only the NON-destructive metadata
/// tail (`bind_full` refreshes `binding.task_id`/`issued_at`; `auto_watch` refreshes
/// the CI-watch correlation), so the new dispatch's `task_id` reaches its DRIVING
/// consumers (`task_progress` CI push, `auto_release` lease/CAS, `ci_watch`
/// correlation — #2158 r6) WITHOUT touching the tree. A bare early-return would skip
/// that refresh and strand the old task_id (the rejected no-op).
///
/// The `source_repo` check is the #2158 r6 fix: the lease identity is
/// `(source_repo, branch)` but the worktree path (`worktree::worktree_path`) is
/// REPO-INDEPENDENT, so a same-branch re-dispatch carrying a DIFFERENT `source_repo`
/// must NOT reuse — it returns `None` → normal rebind/reset (which records the right
/// source_repo). Fail-closed: a legacy binding missing the `source_repo` field
/// (pre-#2117 P3b) → `None`.
///
/// Gated on binding PRESENCE, not dirtiness: a true worktree REUSE-after-release first
/// RELEASES (clears the binding → `binding::read` → None → this is never reached → the
/// reuse-path reset scrubs #869 ref-advance residue, #2115 preserved).
pub(super) fn live_binding_worktree_to_reuse(
    existing: &serde_json::Value,
    requested_source_repo: &str,
) -> Option<PathBuf> {
    let worktree = existing.get("worktree").and_then(|v| v.as_str())?;
    if !Path::new(worktree).exists() {
        return None;
    }
    let same_source_repo = existing
        .get("source_repo")
        .and_then(|v| v.as_str())
        .is_some_and(|r| r == requested_source_repo);
    same_source_repo.then(|| PathBuf::from(worktree))
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
    fn reuses_when_same_source_repo_and_worktree_live() {
        let b = json!({"worktree": live_dir(), "source_repo": "/repo/a"});
        assert_eq!(
            live_binding_worktree_to_reuse(&b, "/repo/a"),
            Some(PathBuf::from(live_dir()))
        );
    }

    #[test]
    fn no_reuse_when_source_repo_differs_2158_r6() {
        // r6's counter-example: same branch, DIFFERENT source_repo → must NOT reuse.
        let b = json!({"worktree": live_dir(), "source_repo": "/repo/a"});
        assert!(live_binding_worktree_to_reuse(&b, "/repo/b").is_none());
    }

    #[test]
    fn no_reuse_when_legacy_binding_missing_source_repo() {
        // Fail-closed: a pre-#2117-P3b binding without the source_repo field.
        let b = json!({"worktree": live_dir()});
        assert!(live_binding_worktree_to_reuse(&b, "/repo/a").is_none());
    }

    #[test]
    fn no_reuse_when_worktree_gone() {
        let b = json!({"worktree": "/nonexistent/agend-2158-x", "source_repo": "/repo/a"});
        assert!(live_binding_worktree_to_reuse(&b, "/repo/a").is_none());
    }
}
