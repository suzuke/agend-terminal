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

/// Which agent an auto-arm speaks for, and — only when strictly proven — the
/// task whose authority the watch may carry.
struct AutoArmSubject {
    agent: String,
    /// `Some` ONLY when exactly one signature-valid binding matches this exact
    /// repository AND branch and its `task_id` strictly routes to a board task.
    /// Never fabricated; every other shape leaves this `None`.
    task_id: Option<String>,
}

/// Resolve the auto-arm subject for `repo`/`branch`.
///
/// Two separable questions, deliberately answered by two different rules:
///
/// * WHO is notified keeps the historical branch-only resolution, so a legacy
///   binding written without `source_repo` still receives its `[ci-pass]` —
///   tightening that would silently drop notifications, which is a regression,
///   not a hardening.
/// * WHETHER task-backed authority travels with the watch is strict: exactly
///   one signature-valid binding on this exact repository+branch, whose
///   `task_id` strictly routes. Zero, several, cross-repository, unsigned,
///   tampered, or stale/unroutable all fail closed to notification-only.
fn resolve_auto_arm_subject(home: &Path, repo: &str, branch: &str) -> Option<AutoArmSubject> {
    // Both sides must resolve to the same canonical slug for a repository
    // match to mean anything. A local checkout and its GitHub origin therefore
    // identify the same repository, while an unresolved value cannot carry
    // task authority. A legacy binding without `source_repo` is not an
    // exact-repo candidate.
    let canonical_repo =
        crate::mcp::handlers::dispatch_hook::canonical_repo_slug_for_source(Path::new(repo));
    let exact: Vec<(String, serde_json::Value)> = crate::binding::binding_scan_all(home)
        .into_iter()
        .filter(|(_, v)| {
            if v["branch"].as_str() != Some(branch) {
                return false;
            }
            let Some(source_repo) = v["source_repo"].as_str().map(str::trim) else {
                return false;
            };
            if source_repo.is_empty() {
                return false;
            }
            let Some(canonical_repo) = canonical_repo.as_deref() else {
                return false;
            };
            crate::mcp::handlers::dispatch_hook::canonical_repo_slug_for_source(Path::new(
                source_repo,
            ))
            .as_deref()
                == Some(canonical_repo)
        })
        .collect();

    if let [(agent, binding)] = exact.as_slice() {
        if crate::binding::signature_valid(home, agent) {
            let task_id = binding["task_id"]
                .as_str()
                .filter(|t| !t.is_empty())
                .filter(|t| crate::tasks::load_routed(home, t).is_ok())
                .map(str::to_string);
            return Some(AutoArmSubject {
                agent: agent.clone(),
                task_id,
            });
        }
        tracing::warn!(
            repo = %repo,
            branch = %branch,
            agent = %agent,
            "auto-arm: exact repo+branch binding is not signature-valid — arming \
             notification-only without task authority"
        );
    } else if exact.len() > 1 {
        // Explicit ambiguity: never silently speak for whichever binding the
        // directory happened to yield first.
        tracing::warn!(
            repo = %repo,
            branch = %branch,
            candidates = exact.len(),
            "auto-arm: several bindings claim this exact repo+branch — arming \
             notification-only without task authority"
        );
    }

    crate::binding::scan_existing_branch_binding(home, "", branch, "").map(|agent| AutoArmSubject {
        agent,
        task_id: None,
    })
}

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
        let Some(subject) = resolve_auto_arm_subject(home, repo, branch) else {
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

        let agent = subject.agent;
        let mut args = serde_json::json!({ "repository": repo, "branch": branch });
        // Only an already-PROVEN routed task id is carried. Never fabricated,
        // and `review_class` is still never set here — an inherited class, if
        // the task has one, is resolved by the watch handler from the task
        // itself, not guessed by this module.
        if let Some(task_id) = &subject.task_id {
            args["task_id"] = serde_json::json!(task_id);
        }
        if let Some(orch) = crate::fleet::team_orchestrator_for(home, &agent) {
            if orch != agent {
                args["next_after_ci"] = serde_json::json!(orch);
            }
        }
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
            head_ref_oid: None,
            base_ref_oid: None,
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

    fn watch_json(home: &Path, branch: &str) -> serde_json::Value {
        let path = crate::daemon::ci_watch::ci_watches_dir(home)
            .join(crate::daemon::ci_watch::watch_filename(REPO, branch));
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
    }

    fn write_fleet(home: &Path) {
        std::fs::write(
            crate::fleet::fleet_yaml_path(home),
            "instances:\n  dev-x:\n    backend: claude\n  lead:\n    backend: claude\n\
             teams:\n  t:\n    members: [dev-x, lead]\n    orchestrator: lead\n",
        )
        .unwrap();
    }

    #[test]
    fn auto_arm_team_member_carries_next_after_ci_orchestrator() {
        let home = tmp_home("team-nac");
        write_fleet(&home);
        bind(&home, "dev-x", "feat/x");
        auto_arm_unwatched_open_prs(
            &home,
            REPO,
            &[meta("feat/x", GhPrState::Open, false, false)],
        );
        assert!(watch_exists(&home, "feat/x"), "watch must be armed");
        let watch = watch_json(&home, "feat/x");
        let next: Vec<String> = crate::daemon::ci_watch::watch_state::normalize_next_after_ci(
            watch
                .get("next_after_ci")
                .unwrap_or(&serde_json::Value::Null),
        );
        assert_eq!(
            next,
            vec!["lead"],
            "auto-arm of a non-orchestrator team member must carry \
             next_after_ci=<team orchestrator>; got {next:?}"
        );
        assert!(
            watch.get("task_id").and_then(|v| v.as_str()).is_none(),
            "auto-arm must NOT fabricate a task_id"
        );
        assert!(
            watch.get("review_class").and_then(|v| v.as_str()).is_none(),
            "auto-arm must NOT set review_class (stays Unresolved/fail-closed)"
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

    // ── t-…-21627-10: exact repo+branch task carrier ────────────────────────
    //
    // Authority may be carried ONLY by exactly one signature-valid binding on
    // this exact repository+branch whose task_id strictly routes. Every other
    // shape stays notification-only: the watch is still armed (so `[ci-pass]`
    // is not lost) but carries no task_id and no review_class.

    /// Write a SIGNED binding. Required for any carrier test: an unsigned
    /// binding is fail-closed by design, so a test that forgot the sidecar
    /// would pass for the wrong reason.
    fn bind_signed(home: &Path, agent: &str, branch: &str, repo: &str, task_id: &str) {
        let dir = crate::paths::runtime_dir(home).join(agent);
        std::fs::create_dir_all(&dir).unwrap();
        let payload = serde_json::json!({
            "version": 1,
            "agent": agent,
            "task_id": task_id,
            "branch": branch,
            "worktree": format!("/tmp/wt-{agent}"),
            "source_repo": repo,
            "issued_at": "2026-06-05T00:00:00Z",
        });
        let body = serde_json::to_string_pretty(&payload).unwrap();
        std::fs::write(dir.join("binding.json"), &body).unwrap();
        let tag = agentic_git_core::integrity_core::sign_binding(home, body.as_bytes())
            .expect("sign binding");
        std::fs::write(dir.join("binding.json.sig"), tag).unwrap();
    }

    /// A real routed board task, so `load_routed` genuinely succeeds.
    fn routed_task(home: &Path) -> String {
        let created = crate::tasks::handle(
            home,
            "dev-x",
            &serde_json::json!({"action": "create", "title": "auto-arm carrier"}),
        );
        created["id"].as_str().expect("task id").to_string()
    }

    fn armed_task_id(home: &Path, branch: &str) -> Option<String> {
        watch_json(home, branch)
            .get("task_id")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }

    /// A production-shaped binding source: a local checkout whose origin is
    /// the GitHub slug used by the PR metadata.
    fn real_repo_with_github_origin(root: &Path, name: &str, slug: &str) -> PathBuf {
        let repo = root.join(name);
        std::fs::create_dir_all(&repo).unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .expect("spawn git init");
        assert!(init.status.success(), "git init failed: {:?}", init);
        let origin = format!("https://github.com/{slug}.git");
        let remote = std::process::Command::new("git")
            .args(["remote", "add", "origin", &origin])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .expect("spawn git remote add");
        assert!(
            remote.status.success(),
            "git remote add failed: {:?}",
            remote
        );
        assert_eq!(
            crate::mcp::handlers::dispatch_hook::derive_repo_from_remote_pub(&repo).as_deref(),
            Some(slug),
            "fixture origin must resolve to the expected canonical GitHub slug"
        );
        repo
    }

    /// Control for the whole matrix: the carrier path DOES fire when every
    /// precondition holds. Without this, each negative below could be passing
    /// because the feature never works at all.
    #[test]
    fn exact_signed_binding_with_routed_task_carries_that_task_id() {
        let home = tmp_home("carrier-positive");
        let tid = routed_task(&home);
        bind_signed(&home, "dev-x", "feat/x", REPO, &tid);
        auto_arm_unwatched_open_prs(
            &home,
            REPO,
            &[meta("feat/x", GhPrState::Open, false, false)],
        );
        assert!(watch_exists(&home, "feat/x"), "watch must be armed");
        assert_eq!(
            armed_task_id(&home, "feat/x").as_deref(),
            Some(tid.as_str()),
            "a unique signature-valid exact repo+branch binding with a routed task must carry it"
        );
        assert!(
            watch_json(&home, "feat/x")
                .get("review_class")
                .and_then(|v| v.as_str())
                .is_none(),
            "carrying a task_id must not invent a review_class"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Regression for the real representation emitted by a checkout binding:
    /// the binding stores a local source path while the PR metadata stores a
    /// GitHub owner/repo slug. Both must canonicalize to the same authority.
    #[test]
    fn local_checkout_origin_matching_pr_repo_carries_that_task_id() {
        let home = tmp_home("carrier-local-origin");
        let source_repo = real_repo_with_github_origin(&home, "source-repo", REPO);
        let tid = routed_task(&home);
        bind_signed(
            &home,
            "dev-x",
            "feat/x",
            &source_repo.display().to_string(),
            &tid,
        );
        auto_arm_unwatched_open_prs(
            &home,
            REPO,
            &[meta("feat/x", GhPrState::Open, false, false)],
        );
        assert_eq!(
            armed_task_id(&home, "feat/x").as_deref(),
            Some(tid.as_str()),
            "a local source path with a matching origin must carry the routed task"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Cross-repository collision: same branch name, DIFFERENT repository. The
    /// old branch-only scan would select this binding and speak for it.
    #[test]
    fn cross_repo_same_branch_binding_carries_no_task_id() {
        let home = tmp_home("carrier-crossrepo");
        let tid = routed_task(&home);
        bind_signed(&home, "dev-other", "feat/x", "other/repo", &tid);
        auto_arm_unwatched_open_prs(
            &home,
            REPO,
            &[meta("feat/x", GhPrState::Open, false, false)],
        );
        assert!(
            watch_exists(&home, "feat/x"),
            "notification-only arm still happens"
        );
        assert_eq!(
            armed_task_id(&home, "feat/x"),
            None,
            "a binding from another repository must never carry task authority here"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Two signature-valid bindings on the SAME exact repo+branch: ambiguous,
    /// so no authority may be carried (first-match-wins is exactly the bug).
    #[test]
    fn ambiguous_exact_bindings_carry_no_task_id() {
        let home = tmp_home("carrier-ambiguous");
        let tid = routed_task(&home);
        bind_signed(&home, "dev-a", "feat/x", REPO, &tid);
        bind_signed(&home, "dev-b", "feat/x", REPO, &tid);
        auto_arm_unwatched_open_prs(
            &home,
            REPO,
            &[meta("feat/x", GhPrState::Open, false, false)],
        );
        assert!(
            watch_exists(&home, "feat/x"),
            "notification-only arm still happens"
        );
        assert_eq!(
            armed_task_id(&home, "feat/x"),
            None,
            "ambiguous candidates must fail closed, not pick the first one"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Signature sidecar tampered after signing → not signature-valid.
    #[test]
    fn invalid_signature_binding_carries_no_task_id() {
        let home = tmp_home("carrier-badsig");
        let tid = routed_task(&home);
        bind_signed(&home, "dev-x", "feat/x", REPO, &tid);
        let dir = crate::paths::runtime_dir(&home).join("dev-x");
        std::fs::write(dir.join("binding.json.sig"), "deadbeef").unwrap();
        auto_arm_unwatched_open_prs(
            &home,
            REPO,
            &[meta("feat/x", GhPrState::Open, false, false)],
        );
        assert!(
            watch_exists(&home, "feat/x"),
            "notification-only arm still happens"
        );
        assert_eq!(
            armed_task_id(&home, "feat/x"),
            None,
            "an invalid signature must fail closed on authority"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// The binding names a task that routes to no board (stale / deleted).
    #[test]
    fn stale_unroutable_task_id_is_not_carried() {
        let home = tmp_home("carrier-stale");
        bind_signed(&home, "dev-x", "feat/x", REPO, "t-does-not-exist");
        auto_arm_unwatched_open_prs(
            &home,
            REPO,
            &[meta("feat/x", GhPrState::Open, false, false)],
        );
        assert!(
            watch_exists(&home, "feat/x"),
            "notification-only arm still happens"
        );
        assert_eq!(
            armed_task_id(&home, "feat/x"),
            None,
            "an unroutable task_id must never be carried"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
