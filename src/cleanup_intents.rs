use std::path::{Path, PathBuf};

fn intents_dir(home: &Path) -> PathBuf {
    home.join("cleanup-intents")
}

fn intent_key(repo: &str, branch: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    repo.hash(&mut h);
    branch.hash(&mut h);
    format!("{:016x}", h.finish())
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct CleanupIntent {
    pub repo: String,
    pub branch: String,
    pub expected_head: String,
    pub task_id: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scm_slug: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<u64>,
}

pub(crate) fn persist_intent(
    home: &Path,
    repo: &str,
    branch: &str,
    expected_head: &str,
    task_id: &str,
    scm_slug: Option<&str>,
    pr_number: Option<u64>,
) -> Result<(), String> {
    let dir = intents_dir(home);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create cleanup-intents dir: {e}"))?;
    let intent = CleanupIntent {
        repo: repo.to_string(),
        branch: branch.to_string(),
        expected_head: expected_head.to_string(),
        task_id: task_id.to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        scm_slug: scm_slug.map(String::from),
        pr_number,
    };
    let key = intent_key(repo, branch);
    let path = dir.join(format!("{key}.json"));
    let body = serde_json::to_string_pretty(&intent)
        .map_err(|e| format!("serialize cleanup intent: {e}"))?;
    crate::store::atomic_write(&path, body.as_bytes())
        .map_err(|e| format!("write cleanup intent: {e}"))
}

#[derive(Debug)]
#[allow(dead_code)]
pub(crate) enum SettleOutcome {
    Deleted,
    Pending(String),
    HeadDrift(String),
    BranchAbsent,
    GitError(String),
    DeleteFailed(String),
}

/// Settle a cleanup intent: delete the branch with expected-head CAS.
///
/// `merged` is the authoritative terminal signal — only `true` authorizes
/// deletion. `pr_number` provides generation identity — when present on
/// both the event and the intent, they must match to prevent a stale
/// merged event from settling a newer intent.
///
/// Intent file is removed ONLY after successful delete or confirmed branch
/// absence. Git/I/O errors preserve the intent for retry.
pub(crate) fn settle_intent(
    home: &Path,
    repo: &str,
    branch: &str,
    merged: bool,
    event_pr_number: Option<u64>,
) -> Option<SettleOutcome> {
    let key = intent_key(repo, branch);
    let path = intents_dir(home).join(format!("{key}.json"));
    let content = std::fs::read_to_string(&path).ok()?;
    let intent: CleanupIntent = serde_json::from_str(&content).ok()?;

    if intent.repo != repo || intent.branch != branch {
        return None;
    }

    if !merged {
        return Some(SettleOutcome::Pending(format!(
            "cleanup intent for '{branch}': not yet merged — intent preserved"
        )));
    }

    if let (Some(intent_pr), Some(event_pr)) = (intent.pr_number, event_pr_number) {
        if intent_pr != event_pr {
            return Some(SettleOutcome::Pending(format!(
                "cleanup intent for '{branch}': PR generation mismatch \
                 (intent=#{intent_pr}, event=#{event_pr}) — intent preserved"
            )));
        }
    }

    let source_repo = Path::new(&intent.repo);
    if !source_repo.is_dir() {
        return Some(SettleOutcome::GitError(format!(
            "cleanup intent for '{branch}': source repo '{}' not a directory — intent preserved",
            intent.repo
        )));
    }
    match crate::git_helpers::git_cmd(source_repo, &["rev-parse", "--verify", branch]) {
        Ok(actual) if actual.trim() == intent.expected_head => {
            let del = crate::git_helpers::git_bypass(source_repo, &["branch", "-D", branch]);
            match del {
                Ok(o) if o.status.success() => {
                    let _ = std::fs::remove_file(&path);
                    Some(SettleOutcome::Deleted)
                }
                Ok(o) => {
                    let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
                    Some(SettleOutcome::DeleteFailed(format!(
                        "git branch -D failed: {stderr}"
                    )))
                }
                Err(e) => Some(SettleOutcome::DeleteFailed(format!(
                    "git branch -D failed: {e}"
                ))),
            }
        }
        Ok(actual) => Some(SettleOutcome::HeadDrift(format!(
            "cleanup intent for '{branch}': expected head {} but actual tip is {} \
             — preserved (fail-closed)",
            intent.expected_head,
            actual.trim()
        ))),
        Err(e) => {
            let stderr = e.to_string();
            if stderr.contains("not a valid object name")
                || stderr.contains("unknown revision")
                || stderr.contains("bad revision")
            {
                let _ = std::fs::remove_file(&path);
                Some(SettleOutcome::BranchAbsent)
            } else {
                Some(SettleOutcome::GitError(format!(
                    "cleanup intent for '{branch}': git error ({stderr}) — intent preserved for retry"
                )))
            }
        }
    }
}

/// Settle intents matching an SCM slug. The CI poller knows the slug but not
/// the local canonical path; this scans all intents for a slug match.
pub(crate) fn settle_by_scm_slug(
    home: &Path,
    scm_slug: &str,
    branch: &str,
    merged: bool,
    event_pr_number: Option<u64>,
) -> Option<SettleOutcome> {
    let dir = intents_dir(home);
    let entries = std::fs::read_dir(&dir).ok()?;
    for entry in entries.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let content = match std::fs::read_to_string(&p) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let intent: CleanupIntent = match serde_json::from_str(&content) {
            Ok(i) => i,
            Err(_) => continue,
        };
        if intent.branch == branch && intent.scm_slug.as_deref().is_some_and(|s| s == scm_slug) {
            return settle_intent(home, &intent.repo, branch, merged, event_pr_number);
        }
    }
    None
}

pub(crate) fn intent_repos(home: &Path) -> Vec<String> {
    let dir = intents_dir(home);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut repos = std::collections::HashSet::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(intent) = serde_json::from_str::<CleanupIntent>(&content) {
                repos.insert(intent.repo);
            }
        }
    }
    repos.into_iter().collect()
}

#[allow(dead_code)]
pub(crate) fn has_intent(home: &Path, repo: &str, branch: &str) -> bool {
    let key = intent_key(repo, branch);
    intents_dir(home).join(format!("{key}.json")).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-intent-test-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn tmp_repo(tag: &str) -> PathBuf {
        let dir = tmp_dir(tag);
        std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&dir)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .ok();
        std::process::Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ])
            .current_dir(&dir)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .ok();
        dir
    }

    fn git_in(wt: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(wt)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn branch_tip(repo: &Path, branch: &str) -> String {
        crate::git_helpers::git_cmd(repo, &["rev-parse", branch])
            .expect("rev-parse")
            .trim()
            .to_string()
    }

    fn branch_exists(repo: &Path, branch: &str) -> bool {
        crate::git_helpers::git_ok(repo, &["rev-parse", "--verify", branch])
    }

    fn make_branch(repo: &Path, name: &str) -> String {
        git_in(repo, &["checkout", "-b", name]);
        std::fs::write(repo.join("f.txt"), "content").ok();
        git_in(repo, &["add", "."]);
        git_in(
            repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-m",
                "work",
            ],
        );
        let tip = branch_tip(repo, name);
        git_in(repo, &["checkout", "main"]);
        tip
    }

    #[test]
    fn settle_merged_exact_head_deletes_branch() {
        let home = tmp_dir("settle-ok");
        let repo = tmp_repo("settle-ok-repo");
        let tip = make_branch(&repo, "feat/settle-test");
        let rs = repo.display().to_string();
        persist_intent(
            &home,
            &rs,
            "feat/settle-test",
            &tip,
            "t-123",
            None,
            Some(42),
        )
        .expect("persist");

        let outcome = settle_intent(&home, &rs, "feat/settle-test", true, Some(42));
        assert!(matches!(outcome, Some(SettleOutcome::Deleted)));
        assert!(!branch_exists(&repo, "feat/settle-test"));
        assert!(!has_intent(&home, &rs, "feat/settle-test"));

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn settle_not_merged_preserves_branch_and_intent() {
        let home = tmp_dir("settle-pending");
        let repo = tmp_repo("settle-pending-repo");
        let tip = make_branch(&repo, "feat/pending-test");
        let rs = repo.display().to_string();
        persist_intent(&home, &rs, "feat/pending-test", &tip, "t-456", None, None)
            .expect("persist");

        let outcome = settle_intent(&home, &rs, "feat/pending-test", false, None);
        assert!(matches!(outcome, Some(SettleOutcome::Pending(_))));
        assert!(branch_exists(&repo, "feat/pending-test"));
        assert!(has_intent(&home, &rs, "feat/pending-test"));

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn settle_head_drift_preserves_branch_and_intent() {
        let home = tmp_dir("settle-drift");
        let repo = tmp_repo("settle-drift-repo");
        let _tip = make_branch(&repo, "feat/drift-test");
        let rs = repo.display().to_string();
        persist_intent(
            &home,
            &rs,
            "feat/drift-test",
            "0000000000000000000000000000000000000000",
            "t-789",
            None,
            None,
        )
        .expect("persist");

        let outcome = settle_intent(&home, &rs, "feat/drift-test", true, None);
        assert!(matches!(outcome, Some(SettleOutcome::HeadDrift(_))));
        assert!(branch_exists(&repo, "feat/drift-test"));

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn restart_preserves_then_merged_settles() {
        let home = tmp_dir("restart-settle");
        let repo = tmp_repo("restart-settle-repo");
        let tip = make_branch(&repo, "feat/restart-test");
        let rs = repo.display().to_string();
        persist_intent(
            &home,
            &rs,
            "feat/restart-test",
            &tip,
            "t-restart",
            None,
            Some(10),
        )
        .expect("persist");

        // Restart: merged=false → preserved
        let outcome = settle_intent(&home, &rs, "feat/restart-test", false, None);
        assert!(matches!(outcome, Some(SettleOutcome::Pending(_))));
        assert!(branch_exists(&repo, "feat/restart-test"));
        assert!(has_intent(&home, &rs, "feat/restart-test"));

        // PR merges with matching generation
        let outcome = settle_intent(&home, &rs, "feat/restart-test", true, Some(10));
        assert!(matches!(outcome, Some(SettleOutcome::Deleted)));
        assert!(!branch_exists(&repo, "feat/restart-test"));

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn old_generation_event_does_not_settle_newer_intent() {
        let home = tmp_dir("gen-mismatch");
        let repo = tmp_repo("gen-mismatch-repo");
        let tip = make_branch(&repo, "feat/gen-test");
        let rs = repo.display().to_string();
        // Intent for PR #20 (newer)
        persist_intent(&home, &rs, "feat/gen-test", &tip, "t-gen", None, Some(20))
            .expect("persist");

        // Old PR #10 merged event → generation mismatch → preserved
        let outcome = settle_intent(&home, &rs, "feat/gen-test", true, Some(10));
        assert!(matches!(outcome, Some(SettleOutcome::Pending(_))));
        assert!(branch_exists(&repo, "feat/gen-test"));
        assert!(has_intent(&home, &rs, "feat/gen-test"));

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn transient_git_error_preserves_intent_for_retry() {
        let home = tmp_dir("git-error");
        // Non-existent repo path → git error, not branch absence
        persist_intent(
            &home,
            "/nonexistent/repo/path",
            "feat/error-test",
            "abc123",
            "t-err",
            None,
            None,
        )
        .expect("persist");

        let outcome = settle_intent(
            &home,
            "/nonexistent/repo/path",
            "feat/error-test",
            true,
            None,
        );
        assert!(
            matches!(outcome, Some(SettleOutcome::GitError(_))),
            "git error must NOT clear intent: {outcome:?}"
        );
        assert!(
            has_intent(&home, "/nonexistent/repo/path", "feat/error-test"),
            "intent must survive git error"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn persist_failure_is_surfaced() {
        // Persist to an unwritable path
        let result = persist_intent(
            Path::new("/nonexistent/root/agend-test"),
            "/repo",
            "feat/x",
            "abc",
            "t-1",
            None,
            None,
        );
        assert!(result.is_err(), "persist to unwritable path must fail");
    }

    #[test]
    fn settle_by_slug_matches_scm_identity() {
        let home = tmp_dir("slug-settle");
        let repo = tmp_repo("slug-settle-repo");
        let tip = make_branch(&repo, "feat/slug-test");
        let rs = repo.display().to_string();
        persist_intent(
            &home,
            &rs,
            "feat/slug-test",
            &tip,
            "t-slug",
            Some("owner/repo"),
            Some(5),
        )
        .expect("persist");

        let outcome = settle_by_scm_slug(&home, "owner/repo", "feat/slug-test", true, Some(5));
        assert!(matches!(outcome, Some(SettleOutcome::Deleted)));
        assert!(!branch_exists(&repo, "feat/slug-test"));

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn intent_repos_contributes_to_discovery() {
        let home = tmp_dir("intent-repos");
        persist_intent(
            &home,
            "/path/to/repo-a",
            "feat/a",
            "abc123",
            "t-1",
            None,
            None,
        )
        .expect("persist");
        persist_intent(
            &home,
            "/path/to/repo-b",
            "feat/b",
            "def456",
            "t-2",
            None,
            None,
        )
        .expect("persist");
        persist_intent(
            &home,
            "/path/to/repo-a",
            "feat/c",
            "ghi789",
            "t-3",
            None,
            None,
        )
        .expect("persist");

        let repos = intent_repos(&home);
        assert!(repos.contains(&"/path/to/repo-a".to_string()));
        assert!(repos.contains(&"/path/to/repo-b".to_string()));
        assert_eq!(repos.len(), 2, "deduplication: {repos:?}");

        std::fs::remove_dir_all(&home).ok();
    }
}
