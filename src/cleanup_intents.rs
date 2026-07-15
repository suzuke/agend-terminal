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
}

pub(crate) fn persist_intent(
    home: &Path,
    repo: &str,
    branch: &str,
    expected_head: &str,
    task_id: &str,
    scm_slug: Option<&str>,
) {
    let dir = intents_dir(home);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let intent = CleanupIntent {
        repo: repo.to_string(),
        branch: branch.to_string(),
        expected_head: expected_head.to_string(),
        task_id: task_id.to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        scm_slug: scm_slug.map(String::from),
    };
    let key = intent_key(repo, branch);
    let path = dir.join(format!("{key}.json"));
    if let Ok(body) = serde_json::to_string_pretty(&intent) {
        let _ = crate::store::atomic_write(&path, body.as_bytes());
    }
}

/// Settle a cleanup intent: delete the branch with expected-head CAS.
///
/// `merged` is the authoritative terminal signal — only `true` authorizes
/// deletion. A restart loading pending intents MUST pass `merged=false`
/// (intents survive, never self-authorize). The CI poller passes `true`
/// after confirming PR-merged status.
///
/// Intent file is removed ONLY after successful delete or proven branch
/// absence — transient failures keep the intent durable for retry.
pub(crate) fn settle_intent(
    home: &Path,
    repo: &str,
    branch: &str,
    merged: bool,
) -> Option<(bool, String)> {
    let key = intent_key(repo, branch);
    let path = intents_dir(home).join(format!("{key}.json"));
    let content = std::fs::read_to_string(&path).ok()?;
    let intent: CleanupIntent = serde_json::from_str(&content).ok()?;

    if intent.repo != repo || intent.branch != branch {
        return None;
    }

    if !merged {
        return Some((
            false,
            format!("cleanup intent for '{branch}': not yet merged — intent preserved (pending)"),
        ));
    }

    let source_repo = Path::new(&intent.repo);
    match crate::git_helpers::git_cmd(source_repo, &["rev-parse", branch]) {
        Ok(actual) if actual.trim() == intent.expected_head => {
            let del = crate::git_helpers::git_bypass(source_repo, &["branch", "-D", branch]);
            match del {
                Ok(o) if o.status.success() => {
                    let _ = std::fs::remove_file(&path);
                    Some((true, String::new()))
                }
                Ok(o) => {
                    let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
                    Some((false, format!("git branch -D failed: {stderr}")))
                }
                Err(e) => Some((false, format!("git branch -D failed: {e}"))),
            }
        }
        Ok(actual) => Some((
            false,
            format!(
                "cleanup intent for '{branch}': expected head {} but actual tip is {} \
                 — preserved (fail-closed)",
                intent.expected_head,
                actual.trim()
            ),
        )),
        Err(_) => {
            let _ = std::fs::remove_file(&path);
            Some((
                false,
                "branch no longer exists — intent cleared".to_string(),
            ))
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
) -> Option<(bool, String)> {
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
            return settle_intent(home, &intent.repo, branch, merged);
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

        let repo_str = repo.display().to_string();
        persist_intent(&home, &repo_str, "feat/settle-test", &tip, "t-123", None);
        assert!(has_intent(&home, &repo_str, "feat/settle-test"));

        // merged=true authorizes deletion
        let (deleted, _) =
            settle_intent(&home, &repo_str, "feat/settle-test", true).expect("settle result");
        assert!(deleted, "branch must be deleted on CAS match + merged");
        assert!(!branch_exists(&repo, "feat/settle-test"));
        assert!(!has_intent(&home, &repo_str, "feat/settle-test"));

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn settle_not_merged_preserves_branch_and_intent() {
        let home = tmp_dir("settle-pending");
        let repo = tmp_repo("settle-pending-repo");
        let tip = make_branch(&repo, "feat/pending-test");

        let repo_str = repo.display().to_string();
        persist_intent(&home, &repo_str, "feat/pending-test", &tip, "t-456", None);

        // merged=false → branch AND intent preserved
        let (deleted, reason) =
            settle_intent(&home, &repo_str, "feat/pending-test", false).expect("settle result");
        assert!(!deleted, "not-merged must NOT delete");
        assert!(
            branch_exists(&repo, "feat/pending-test"),
            "branch must survive"
        );
        assert!(
            has_intent(&home, &repo_str, "feat/pending-test"),
            "intent must survive"
        );
        assert!(
            reason.contains("not yet merged"),
            "reason must mention pending: {reason}"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn settle_head_drift_preserves_branch_and_intent() {
        let home = tmp_dir("settle-drift");
        let repo = tmp_repo("settle-drift-repo");
        let _tip = make_branch(&repo, "feat/drift-test");

        let repo_str = repo.display().to_string();
        persist_intent(
            &home,
            &repo_str,
            "feat/drift-test",
            "0000000000000000000000000000000000000000",
            "t-789",
            None,
        );

        // merged=true but head drifted → fail-closed, intent kept for retry
        let (deleted, reason) =
            settle_intent(&home, &repo_str, "feat/drift-test", true).expect("settle result");
        assert!(!deleted, "head-drifted must NOT delete");
        assert!(branch_exists(&repo, "feat/drift-test"));
        assert!(reason.contains("fail-closed"), "reason: {reason}");

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// RED #5: restart before merge keeps branch+intent; merged event settles.
    #[test]
    fn restart_preserves_then_merged_settles() {
        let home = tmp_dir("restart-settle");
        let repo = tmp_repo("restart-settle-repo");
        let tip = make_branch(&repo, "feat/restart-test");

        let repo_str = repo.display().to_string();
        persist_intent(
            &home,
            &repo_str,
            "feat/restart-test",
            &tip,
            "t-restart",
            None,
        );

        // Simulate restart: settle with merged=false (no terminal authority)
        let (deleted, _) =
            settle_intent(&home, &repo_str, "feat/restart-test", false).expect("restart settle");
        assert!(!deleted, "restart must NOT delete");
        assert!(
            branch_exists(&repo, "feat/restart-test"),
            "branch survives restart"
        );
        assert!(
            has_intent(&home, &repo_str, "feat/restart-test"),
            "intent survives restart"
        );

        // Later: PR merges → settle with merged=true
        let (deleted, _) =
            settle_intent(&home, &repo_str, "feat/restart-test", true).expect("merged settle");
        assert!(deleted, "merged event must delete");
        assert!(
            !branch_exists(&repo, "feat/restart-test"),
            "branch gone after merge"
        );
        assert!(
            !has_intent(&home, &repo_str, "feat/restart-test"),
            "intent gone after merge"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn settle_by_slug_matches_scm_identity() {
        let home = tmp_dir("slug-settle");
        let repo = tmp_repo("slug-settle-repo");
        let tip = make_branch(&repo, "feat/slug-test");

        let repo_str = repo.display().to_string();
        persist_intent(
            &home,
            &repo_str,
            "feat/slug-test",
            &tip,
            "t-slug",
            Some("owner/repo"),
        );

        // Settle via SCM slug (as CI poller would)
        let (deleted, _) =
            settle_by_scm_slug(&home, "owner/repo", "feat/slug-test", true).expect("slug settle");
        assert!(deleted, "slug-matched intent must settle");
        assert!(!branch_exists(&repo, "feat/slug-test"));

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn intent_repos_contributes_to_discovery() {
        let home = tmp_dir("intent-repos");
        persist_intent(&home, "/path/to/repo-a", "feat/a", "abc123", "t-1", None);
        persist_intent(&home, "/path/to/repo-b", "feat/b", "def456", "t-2", None);
        persist_intent(&home, "/path/to/repo-a", "feat/c", "ghi789", "t-3", None);

        let repos = intent_repos(&home);
        assert!(repos.contains(&"/path/to/repo-a".to_string()));
        assert!(repos.contains(&"/path/to/repo-b".to_string()));
        assert_eq!(repos.len(), 2, "deduplication: {repos:?}");

        std::fs::remove_dir_all(&home).ok();
    }
}
