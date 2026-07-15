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

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct CleanupIntent {
    pub repo: String,
    pub branch: String,
    pub expected_head: String,
    pub task_id: String,
    pub created_at: String,
}

pub(crate) fn persist_intent(
    home: &Path,
    repo: &str,
    branch: &str,
    expected_head: &str,
    task_id: &str,
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
    };
    let key = intent_key(repo, branch);
    let path = dir.join(format!("{key}.json"));
    if let Ok(body) = serde_json::to_string_pretty(&intent) {
        let _ = crate::store::atomic_write(&path, body.as_bytes());
    }
}

pub(crate) fn settle_intent(home: &Path, repo: &str, branch: &str) -> Option<(bool, String)> {
    let key = intent_key(repo, branch);
    let path = intents_dir(home).join(format!("{key}.json"));
    let content = std::fs::read_to_string(&path).ok()?;
    let intent: CleanupIntent = serde_json::from_str(&content).ok()?;

    if intent.repo != repo || intent.branch != branch {
        return None;
    }

    let source_repo = Path::new(&intent.repo);
    match crate::git_helpers::git_cmd(source_repo, &["rev-parse", branch]) {
        Ok(actual) if actual.trim() == intent.expected_head => {
            let del = crate::git_helpers::git_bypass(source_repo, &["branch", "-D", branch]);
            let _ = std::fs::remove_file(&path);
            match del {
                Ok(o) if o.status.success() => Some((true, String::new())),
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

pub(crate) fn settle_all_intents(home: &Path) -> Vec<(String, bool, String)> {
    let dir = intents_dir(home);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut results = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let intent: CleanupIntent = match serde_json::from_str(&content) {
            Ok(i) => i,
            Err(_) => continue,
        };
        if let Some((deleted, reason)) = settle_intent(home, &intent.repo, &intent.branch) {
            results.push((intent.branch, deleted, reason));
        }
    }
    results
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
            .unwrap()
            .trim()
            .to_string()
    }

    fn branch_exists(repo: &Path, branch: &str) -> bool {
        crate::git_helpers::git_ok(repo, &["rev-parse", "--verify", branch])
    }

    #[test]
    fn persist_and_settle_exact_head_deletes_branch() {
        let home = tmp_dir("settle-ok");
        let repo = tmp_repo("settle-ok-repo");

        git_in(&repo, &["checkout", "-b", "feat/settle-test"]);
        std::fs::write(repo.join("f.txt"), "content").ok();
        git_in(&repo, &["add", "."]);
        git_in(
            &repo,
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
        let tip = branch_tip(&repo, "feat/settle-test");
        git_in(&repo, &["checkout", "main"]);

        let repo_str = repo.display().to_string();
        persist_intent(&home, &repo_str, "feat/settle-test", &tip, "t-123");
        assert!(has_intent(&home, &repo_str, "feat/settle-test"));

        let result = settle_intent(&home, &repo_str, "feat/settle-test");
        assert!(result.is_some());
        let (deleted, _reason) = result.unwrap();
        assert!(deleted, "branch must be deleted on CAS match");
        assert!(
            !branch_exists(&repo, "feat/settle-test"),
            "branch must be gone"
        );
        assert!(
            !has_intent(&home, &repo_str, "feat/settle-test"),
            "intent must be removed"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn settle_head_drift_preserves_branch() {
        let home = tmp_dir("settle-drift");
        let repo = tmp_repo("settle-drift-repo");

        git_in(&repo, &["checkout", "-b", "feat/drift-test"]);
        std::fs::write(repo.join("f.txt"), "content").ok();
        git_in(&repo, &["add", "."]);
        git_in(
            &repo,
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
        git_in(&repo, &["checkout", "main"]);

        let repo_str = repo.display().to_string();
        persist_intent(
            &home,
            &repo_str,
            "feat/drift-test",
            "0000000000000000000000000000000000000000",
            "t-456",
        );

        let result = settle_intent(&home, &repo_str, "feat/drift-test");
        assert!(result.is_some());
        let (deleted, reason) = result.unwrap();
        assert!(!deleted, "head-drifted branch must NOT be deleted");
        assert!(
            branch_exists(&repo, "feat/drift-test"),
            "branch must survive"
        );
        assert!(
            reason.contains("fail-closed"),
            "reason must mention fail-closed: {reason}"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// RED #5: clean feature pre-merge → intent survives restart, settles exact-head
    #[test]
    fn intent_survives_simulated_restart_and_settles() {
        let home = tmp_dir("restart-settle");
        let repo = tmp_repo("restart-settle-repo");

        git_in(&repo, &["checkout", "-b", "feat/restart-test"]);
        std::fs::write(repo.join("f.txt"), "work").ok();
        git_in(&repo, &["add", "."]);
        git_in(
            &repo,
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
        let tip = branch_tip(&repo, "feat/restart-test");
        git_in(&repo, &["checkout", "main"]);

        let repo_str = repo.display().to_string();
        persist_intent(&home, &repo_str, "feat/restart-test", &tip, "t-789");

        // "Restart": re-read from disk (the intent is durable)
        assert!(has_intent(&home, &repo_str, "feat/restart-test"));

        // Intent repos discoverable
        let repos = intent_repos(&home);
        assert!(
            repos.contains(&repo_str),
            "intent repo must be discoverable: {repos:?}"
        );

        // Settle via settle_all (simulates sweep after restart)
        let results = settle_all_intents(&home);
        assert_eq!(results.len(), 1, "one intent settled: {results:?}");
        let (branch, deleted, _) = &results[0];
        assert_eq!(branch, "feat/restart-test");
        assert!(deleted, "branch must be deleted on settlement");
        assert!(!branch_exists(&repo, "feat/restart-test"), "branch gone");
        assert!(
            !has_intent(&home, &repo_str, "feat/restart-test"),
            "intent cleared"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn intent_repos_contributes_to_discovery() {
        let home = tmp_dir("intent-repos");
        persist_intent(&home, "/path/to/repo-a", "feat/a", "abc123", "t-1");
        persist_intent(&home, "/path/to/repo-b", "feat/b", "def456", "t-2");
        persist_intent(&home, "/path/to/repo-a", "feat/c", "ghi789", "t-3");

        let repos = intent_repos(&home);
        assert!(repos.contains(&"/path/to/repo-a".to_string()));
        assert!(repos.contains(&"/path/to/repo-b".to_string()));
        assert_eq!(repos.len(), 2, "deduplication: {repos:?}");

        std::fs::remove_dir_all(&home).ok();
    }
}
