//! Stale daemon-managed worktree recovery helpers. Originally built to
//! back the standalone `force_release_worktree` MCP tool (Sprint 59 Wave 1
//! PR-5 emergency cherry-pick, closing the architectural defect that drove
//! the Sprint 59 Wave 1 PR-2 BYPASS incident + PR-4 (C)-path stall); #2548
//! PR-2 folded that tool into `release_worktree(force:true)`
//! (`mcp/handlers/worktree.rs`), which is now the sole caller of
//! [`rebase_clean_self`] and the exact-owner metadata arm for that path.
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
#[cfg(test)]
mod gc_legacy;
mod repair;
mod s2;

#[cfg(test)]
pub(crate) use gc::gc_test_seam;
pub(crate) use gc::{prune_exact_git_metadata, ExactMetadataState};
#[cfg(test)]
#[allow(unused_imports)]
pub(crate) use gc_legacy::prune_git_metadata_for_agent;
pub(crate) use repair::attempt_safe_rebind_repair_with_continuation;
pub(crate) use s2::{classify_target, TargetState};
#[cfg(test)]
pub(crate) use s2::{rebase_test_seam, RebaseTestPhase};

/// Outcome of a rebase-clean operation.
#[derive(Debug)]
pub(super) struct RebaseCleanOutcome {
    pub(super) dir_existed: bool,
    pub(super) dir_removed: bool,
    pub(super) binding_outcome: Value,
    pub(super) git_metadata_pruned: usize,
    pub(super) git_metadata_repos: Vec<String>,
}

/// Guarded cleanup helper for the explicit force release tool. It delegates
/// every destructive step to the S2 transaction, which resolves one canonical
/// owner, acquires `L(repo,branch) -> A -> B`, and fails closed on opaque or
/// mismatched state.
///
/// Caller invariant: `agent` and `branch` must be pre-validated by
/// `agent::validate_name` + `agent_ops::validate_branch` respectively.
/// This helper trusts its callers; the path-safety guard below is
/// defense-in-depth, not the primary validator.
pub(super) fn rebase_clean_self(
    home: &Path,
    agent: &str,
    branch: &str,
    explicit_repo: Option<&Path>,
    sender: Option<&str>,
) -> Result<RebaseCleanOutcome, String> {
    if branch.is_empty() {
        return Err("refuses to clean path outside the daemon worktree pool".to_string());
    }
    let result = s2::force_release(home, agent, branch, explicit_repo, sender)?;
    if let Some(error) = result.outcome.error.as_ref() {
        return Err(error.clone());
    }
    Ok(RebaseCleanOutcome {
        dir_existed: result.dir_existed,
        dir_removed: result.dir_removed,
        binding_outcome: serde_json::to_value(&result.outcome).unwrap_or(Value::Null),
        git_metadata_pruned: result.git_metadata_pruned,
        git_metadata_repos: result.git_metadata_repos,
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
        let source_repo = home.join("source-repo");
        std::fs::create_dir_all(&source_repo).unwrap();
        std::fs::write(
            dir.join(".agend-managed"),
            format!(
                "agent={agent}\nbranch={branch}\nsource_repo={}\n",
                source_repo.display()
            ),
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
        let outcome = rebase_clean_self(&home, "dev", "feat/rebase-x", None, None)
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
        let source = home.join("source-repo");
        std::fs::create_dir_all(&source).unwrap();
        let outcome = rebase_clean_self(&home, "dev", "feat/never-existed", Some(&source), None)
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
        let r = rebase_clean_self(&home, "dev", "", None, None);
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
    fn bind_self_rebase_mode_refuses_mismatched_binding() {
        // Seed the stale state that drove the Wave 1 PR-2 BYPASS
        // incident: an on-disk worktree dir + binding lingering from a
        // prior bind cycle. Calling handle_bind_self with
        // S2: a binding whose recorded worktree identity is stale must not
        // authorize removal of a different on-disk target. Rebase mode fails
        // closed and leaves both pieces of evidence intact.
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
        assert!(dir.exists(), "mismatched binding must preserve stale dir");
        assert!(
            runtime.join("binding.json").exists(),
            "mismatched binding must preserve binding evidence"
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

    // ── #2158-adjacent: force_release (force:true) dirty-WIP preservation ──

    fn git_bypassed(dir: &Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .expect("git")
    }

    #[test]
    fn rebase_clean_self_preserves_dirty_wip_before_removal() {
        // A REAL linked worktree at the daemon pool path, dirtied with untracked
        // WIP, then force_release'd. force_release is destructive-by-design, but
        // the WIP must still be recoverable via refs/agend/recovery/<branch>/<ts>.
        let home = tmp_home("force-release-dirty");
        let repo = home.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        assert!(git_bypassed(&repo, &["init", "-b", "main"])
            .status
            .success());
        assert!(git_bypassed(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ],
        )
        .status
        .success());

        let agent = "dev-fr-dirty";
        let branch = "feat/fr-dirty";
        let target = home.join("worktrees").join(agent).join(branch);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        let add = git_bypassed(
            &repo,
            &["worktree", "add", "-b", branch, target.to_str().unwrap()],
        );
        assert!(
            add.status.success(),
            "worktree add failed: {}",
            String::from_utf8_lossy(&add.stderr)
        );
        std::fs::write(
            target.join(crate::worktree_pool::MANAGED_MARKER),
            format!(
                "agent={agent}\nbranch={branch}\nsource_repo={}\n",
                repo.display()
            ),
        )
        .unwrap();
        std::fs::write(target.join("fr-wip.txt"), b"force-release WIP").unwrap();

        let outcome =
            rebase_clean_self(&home, agent, branch, None, None).expect("rebase_clean_self ok");
        assert!(
            outcome.dir_existed && outcome.dir_removed,
            "dirty worktree removed by force_release: {outcome:?}"
        );
        assert!(!target.exists(), "worktree dir gone");

        // The WIP survives in a recovery ref.
        let refs = git_bypassed(
            &repo,
            &[
                "for-each-ref",
                "--format=%(refname)",
                &format!("refs/agend/recovery/{branch}/"),
            ],
        );
        let ref_list = String::from_utf8_lossy(&refs.stdout);
        let ref_name = ref_list
            .lines()
            .find(|l| !l.is_empty())
            .expect("a recovery ref exists after dirty force_release");
        let tree = git_bypassed(&repo, &["ls-tree", "-r", "--name-only", ref_name]);
        assert!(
            String::from_utf8_lossy(&tree.stdout).contains("fr-wip.txt"),
            "untracked WIP captured on the force_release path"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// reviewer4 #2672 (fail-OPEN regression) for the force:true path: a dirty
    /// worktree whose WIP cannot be snapshotted (contended `index.lock`) must be
    /// FAIL-CLOSED — `rebase_clean_self` returns Err and leaves the worktree in
    /// place, NOT a silent destructive `remove_dir_all` that evaporates the WIP.
    #[test]
    fn rebase_clean_self_refuses_when_dirty_wip_unpreservable() {
        let home = tmp_home("force-release-blocked");
        let repo = home.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        assert!(git_bypassed(&repo, &["init", "-b", "main"])
            .status
            .success());
        assert!(git_bypassed(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ],
        )
        .status
        .success());
        let agent = "dev-fr-blk";
        let branch = "feat/fr-blk";
        let target = home.join("worktrees").join(agent).join(branch);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        assert!(git_bypassed(
            &repo,
            &["worktree", "add", "-b", branch, target.to_str().unwrap()],
        )
        .status
        .success());
        std::fs::write(
            target.join(crate::worktree_pool::MANAGED_MARKER),
            format!(
                "agent={agent}\nbranch={branch}\nsource_repo={}\n",
                repo.display()
            ),
        )
        .unwrap();
        std::fs::write(target.join("fr-wip.txt"), b"must not vanish").unwrap();
        // Jam the worktree's index so `git add -A` fails during preservation.
        let gitlink = std::fs::read_to_string(target.join(".git")).unwrap();
        let gitdir = gitlink.strip_prefix("gitdir:").unwrap().trim();
        std::fs::write(Path::new(gitdir).join("index.lock"), b"").unwrap();

        let result = rebase_clean_self(&home, agent, branch, None, None);
        assert!(
            result.is_err(),
            "force_release must FAIL-CLOSED (Err) when dirty WIP can't be preserved: {result:?}"
        );
        assert!(
            result.unwrap_err().contains("could not be preserved"),
            "error must name the refusal"
        );
        assert!(
            target.exists(),
            "worktree must NOT be removed (fail-closed)"
        );
        assert!(
            target.join("fr-wip.txt").exists(),
            "untracked WIP must survive in place"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
