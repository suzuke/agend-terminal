#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::path::{Path, PathBuf};
use std::process::Command;

fn tmp_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "agend-wt-lifecycle-r1-{tag}-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn git_in(dir: &Path, args: &[&str]) {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("AGEND_GIT_BYPASS", "1")
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git");
}

fn setup_repo(tag: &str) -> PathBuf {
    let repo = tmp_home(&format!("repo-{tag}"));
    git_in(&repo, &["init", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "init").unwrap();
    git_in(&repo, &["add", "."]);
    git_in(&repo, &["commit", "-m", "init"]);
    repo
}

fn make_old_merged_branch(repo: &Path, branch: &str) {
    git_in(repo, &["checkout", "-b", branch]);
    let file = branch.replace('/', "-");
    std::fs::write(repo.join(format!("{file}.txt")), "feature").unwrap();
    git_in(repo, &["add", "."]);
    Command::new("git")
        .args(["commit", "-m", "feature"])
        .current_dir(repo)
        .env("AGEND_GIT_BYPASS", "1")
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env("GIT_AUTHOR_DATE", "2024-01-01T00:00:00 +0000")
        .env("GIT_COMMITTER_DATE", "2024-01-01T00:00:00 +0000")
        .output()
        .expect("git commit");
    git_in(repo, &["checkout", "main"]);
    git_in(repo, &["merge", "--ff-only", branch]);
}

fn write_source_binding(home: &Path, agent: &str, source_repo: &Path) {
    let dir = crate::paths::runtime_dir(home).join(agent);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("binding.json"),
        serde_json::json!({"source_repo": source_repo.display().to_string()}).to_string(),
    )
    .unwrap();
}

#[test]
fn corrupt_binding_inventory_is_unknown_for_branch_delete_gate() {
    let home = tmp_home("corrupt-binding");
    let repo = setup_repo("corrupt-binding");
    write_source_binding(&home, "valid-agent", &repo);
    let corrupt = crate::paths::runtime_dir(&home).join("corrupt-agent");
    std::fs::create_dir_all(&corrupt).unwrap();
    std::fs::write(corrupt.join("binding.json"), b"not-json").unwrap();

    assert_eq!(
        branch_has_active_binding(&home, &repo, "feat/done"),
        None,
        "corrupt binding evidence must fail closed"
    );

    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn prune_batches_open_pr_inventory_once_per_repo_sweep() {
    let repo = setup_repo("open-pr-batch");
    git_in(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/example/repo.git",
        ],
    );
    make_old_merged_branch(&repo, "feat/old-a");
    make_old_merged_branch(&repo, "feat/old-b");
    let home = tmp_home("open-pr-batch");
    let provider = crate::scm::MockScmProvider::with_pr_list(crate::scm::MockPrList::Prs(0));
    let _provider_guard = crate::scm::set_test_scm_provider(provider.clone());

    let removed = prune_orphaned_branches_with_home(Some(&home), &repo, false);
    assert_eq!(provider.pr_list_calls(), 1, "one bounded open-PR inventory");
    assert_eq!(removed.len(), 2, "both merged branches remain eligible");

    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn failed_open_pr_snapshot_preserves_all_terminal_candidates() {
    let repo = setup_repo("open-pr-failure");
    git_in(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/example/repo.git",
        ],
    );
    make_old_merged_branch(&repo, "feat/old-a");
    make_old_merged_branch(&repo, "feat/old-b");
    let home = tmp_home("open-pr-failure");
    let provider =
        crate::scm::MockScmProvider::with_pr_list(crate::scm::MockPrList::Fail("offline".into()));
    let _provider_guard = crate::scm::set_test_scm_provider(provider.clone());

    let removed = prune_orphaned_branches_with_home(Some(&home), &repo, false);
    assert!(
        removed.is_empty(),
        "unknown open-PR inventory must preserve all branches"
    );
    assert!(crate::git_helpers::git_ok(
        &repo,
        &["show-ref", "--verify", "refs/heads/feat/old-a"]
    ));
    assert!(crate::git_helpers::git_ok(
        &repo,
        &["show-ref", "--verify", "refs/heads/feat/old-b"]
    ));
    assert_eq!(provider.pr_list_calls(), 1);

    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}
