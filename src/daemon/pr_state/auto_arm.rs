//! PR-3 (t-ci-ready-pr3-arm-not-armed): daemon-side auto-arm of ci-watches for
//! open PRs that have no armed watch — closing the arm-not-armed footgun (#1782).
//!
//! The footgun: a bypass / non-dispatch push (`AGEND_GIT_BYPASS=1`, raw `git`,
//! IDE, CI-triggered, or a solo agent with no dispatch) leaves NO auto-armed
//! ci-watch. `should_bypass()` short-circuits the agend-git shim *before* it
//! could arm, and no client-side hook survives `--no-verify`. So the only place
//! that reliably observes such a push is server-side: `gh pr list`, which the
//! pr_state scanner already polls. This module piggybacks on that poll — for any
//! OPEN PR with no watch, it arms one so CI completion still produces `[ci-pass]`.
//!
//! Resolution is BINDING-based, NOT gh-author-based: the fleet shares ONE GitHub
//! account, so a PR's gh `author.login` cannot tell which agent authored it
//! (every PR carries the same login → resolving by login mis-notifies, and the
//! `resolve_author` tier-4 fallback is a hard-coded `"fixup-lead"`).
//! [`crate::binding::scan_existing_branch_binding`] maps branch → the agent whose
//! worktree is bound to it (the agent who pushed and is waiting for ci-ready),
//! which is reliable regardless of the shared login. If no agent is bound
//! (released worktree / external PR), we fail LOUD rather than notify the wrong
//! agent.

use std::path::Path;

use super::gh_poll::{GhPrMetadata, GhPrState};

/// For every OPEN, non-draft, same-repo PR in `prs` that has no armed ci-watch,
/// arm one (subscriber = the agent bound to the branch). Idempotent: an existing
/// watch is left untouched (a repeated push to the same branch does NOT re-arm).
/// Best-effort; never blocks the scanner.
pub fn auto_arm_unwatched_open_prs(home: &Path, repo: &str, prs: &[GhPrMetadata]) {
    for meta in prs {
        // Only open, non-draft, same-repo PRs. Drafts/cross-repo forks and
        // merged/closed PRs are intentionally skipped (a fork head_ref is not a
        // base-repo branch; a terminal PR needs no further CI notification).
        if meta.state != GhPrState::Open || meta.is_draft || meta.is_cross_repository {
            continue;
        }
        let branch = meta.head_ref.as_str();

        // Idempotent: skip if a watch already exists (natural dedup — repeated
        // pushes to the same branch do not re-arm, and an explicitly-armed watch
        // with its own subscribers/next_after_ci is never clobbered).
        let watch_path = crate::daemon::ci_watch::ci_watches_dir(home)
            .join(crate::daemon::ci_watch::watch_filename(repo, branch));
        if watch_path.exists() {
            continue;
        }

        // Binding-based resolution (shared-account-proof): which agent is bound
        // to this branch? That is the agent who pushed and is waiting.
        // #2117 P3b: branch-only scan (source_repo="") — route CI-pass to whoever
        // is bound to this branch; repo precision unnecessary for this lookup.
        let Some(agent) = crate::binding::scan_existing_branch_binding(home, "", branch, "") else {
            // Fail LOUD — never silently drop. We cannot reliably route a
            // `[ci-pass]` for an open PR with no bound agent (released worktree /
            // external PR); notifying the wrong agent is worse than a loud log.
            tracing::warn!(
                repo = %repo,
                branch = %branch,
                pr = meta.number,
                "PR-3: open PR has no armed ci-watch AND no bound agent — cannot \
                 auto-arm (arm manually via `ci action=watch`, or rebind the branch)"
            );
            continue;
        };

        // Arm with the bound agent as the sole subscriber; next_after_ci unset →
        // on CI pass the agent receives the informational `[ci-pass]` (PR-1 #1796
        // fallback). The actionable `[ci-ready-for-action]` chain still requires
        // an explicit next_after_ci (review handoff stays explicit, PR-2 #1797).
        let args = serde_json::json!({ "repository": repo, "branch": branch });
        let resp = crate::mcp::handlers::ci::handle_watch_ci(home, &args, &agent);
        if let Some(err) = resp.get("error").and_then(|e| e.as_str()) {
            tracing::warn!(
                repo = %repo,
                branch = %branch,
                agent = %agent,
                error = %err,
                "PR-3: auto-arm handle_watch_ci failed"
            );
        } else {
            tracing::info!(
                repo = %repo,
                branch = %branch,
                agent = %agent,
                pr = meta.number,
                "PR-3: auto-armed ci-watch for previously-unwatched open PR"
            );
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    const REPO: &str = "owner/repo";

    fn tmp_home(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-pr3-autoarm-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Bind `agent` to `branch` (writes the binding.json `scan_existing_branch_binding` reads).
    fn bind(home: &Path, agent: &str, branch: &str) {
        let dir = crate::paths::runtime_dir(home).join(agent);
        std::fs::create_dir_all(&dir).unwrap();
        let payload = serde_json::json!({
            "version": 1,
            "agent": agent,
            "task_id": "t-test",
            "branch": branch,
            "worktree": format!("/tmp/wt-{agent}"),
            "source_repo": REPO,
            "issued_at": "2026-06-05T00:00:00Z",
        });
        std::fs::write(
            dir.join("binding.json"),
            serde_json::to_string_pretty(&payload).unwrap(),
        )
        .unwrap();
    }

    fn meta(branch: &str, state: GhPrState, draft: bool, cross: bool) -> GhPrMetadata {
        GhPrMetadata {
            number: 42,
            author_login: "suzuke".to_string(),
            head_ref: branch.to_string(),
            is_cross_repository: cross,
            is_draft: draft,
            state,
            merged_at: None,
        }
    }

    fn watch_exists(home: &Path, branch: &str) -> bool {
        crate::daemon::ci_watch::ci_watches_dir(home)
            .join(crate::daemon::ci_watch::watch_filename(REPO, branch))
            .exists()
    }

    fn watch_subscribers(home: &Path, branch: &str) -> Vec<String> {
        let path = crate::daemon::ci_watch::ci_watches_dir(home)
            .join(crate::daemon::ci_watch::watch_filename(REPO, branch));
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        v["subscribers"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|s| s["instance"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn unwatched_open_pr_with_bound_agent_gets_armed() {
        let home = tmp_home("armed");
        bind(&home, "dev-x", "feat/x");
        auto_arm_unwatched_open_prs(
            &home,
            REPO,
            &[meta("feat/x", GhPrState::Open, false, false)],
        );
        assert!(
            watch_exists(&home, "feat/x"),
            "open PR's watch must be armed"
        );
        assert!(
            watch_subscribers(&home, "feat/x").contains(&"dev-x".to_string()),
            "the BOUND agent (not the gh author login) must be the subscriber"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn already_armed_open_pr_is_noop() {
        let home = tmp_home("noop");
        bind(&home, "dev-x", "feat/x");
        // Pre-arm with a DIFFERENT subscriber (e.g. an explicit `ci action=watch`).
        crate::mcp::handlers::ci::handle_watch_ci(
            &home,
            &serde_json::json!({"repository": REPO, "branch": "feat/x"}),
            "other-agent",
        );
        auto_arm_unwatched_open_prs(
            &home,
            REPO,
            &[meta("feat/x", GhPrState::Open, false, false)],
        );
        let subs = watch_subscribers(&home, "feat/x");
        assert!(
            subs.contains(&"other-agent".to_string()) && !subs.contains(&"dev-x".to_string()),
            "an existing watch must be left untouched (no re-arm / no subscriber churn): {subs:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn merged_draft_crossrepo_are_skipped() {
        let home = tmp_home("skip");
        bind(&home, "dev-x", "feat/merged");
        bind(&home, "dev-y", "feat/draft");
        bind(&home, "dev-z", "feat/fork");
        auto_arm_unwatched_open_prs(
            &home,
            REPO,
            &[
                meta("feat/merged", GhPrState::Merged, false, false),
                meta("feat/draft", GhPrState::Open, true, false),
                meta("feat/fork", GhPrState::Open, false, true),
            ],
        );
        assert!(
            !watch_exists(&home, "feat/merged"),
            "merged PR must not arm"
        );
        assert!(!watch_exists(&home, "feat/draft"), "draft PR must not arm");
        assert!(
            !watch_exists(&home, "feat/fork"),
            "cross-repo PR must not arm"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1991: an explicit `ci unwatch` leaves a TOMBSTONE (empty-subscriber
    /// watch file with `auto_arm_optout`) precisely so this sweep does NOT
    /// re-arm it. Pre-#1991 unwatch DELETED the file, the next pr_state scan
    /// re-armed the open PR, and the just-unwatched agent was re-subscribed
    /// ~60s later (the #1991 storm's unstoppable-from-agent-side half).
    #[test]
    fn unwatch_tombstone_is_not_rearmed_1991() {
        let home = tmp_home("tombstone");
        bind(&home, "dev-x", "feat/x");
        // Arm, then explicitly unwatch to a tombstone.
        crate::mcp::handlers::ci::handle_watch_ci(
            &home,
            &serde_json::json!({"repository": REPO, "branch": "feat/x"}),
            "dev-x",
        );
        crate::mcp::handlers::ci::handle_unwatch_ci(
            &home,
            &serde_json::json!({"repository": REPO, "branch": "feat/x", "instance": "dev-x"}),
            "dev-x",
        );
        assert!(
            watch_exists(&home, "feat/x"),
            "precondition: unwatch leaves a tombstone file"
        );
        // The PR is still open and the agent still bound — the exact shape
        // that pre-#1991 re-armed every scan.
        auto_arm_unwatched_open_prs(
            &home,
            REPO,
            &[meta("feat/x", GhPrState::Open, false, false)],
        );
        assert!(
            watch_subscribers(&home, "feat/x").is_empty(),
            "auto-arm must respect the unwatch tombstone (no re-subscribe)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn open_pr_no_bound_agent_fails_loud_no_arm() {
        let home = tmp_home("failloud");
        // No binding for feat/orphan → cannot resolve an agent → fail loud, no arm.
        auto_arm_unwatched_open_prs(
            &home,
            REPO,
            &[meta("feat/orphan", GhPrState::Open, false, false)],
        );
        assert!(
            !watch_exists(&home, "feat/orphan"),
            "with no bound agent, must NOT arm (fail-loud, not mis-notify)"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
