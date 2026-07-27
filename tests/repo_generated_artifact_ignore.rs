//! Repository-generated branch-cleanup artifacts must not pollute `git status`.

use std::path::Path;
use std::process::Command;

const ROOT_GITIGNORE: &str = include_str!("../.gitignore");

fn git_check_ignored(repo: &Path, path: &str) -> bool {
    // allow: raw-git-subprocess — hermetic temp-repo ignore probe with bypass env
    Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .env("AGENTIC_GIT_BYPASS", "1")
        .args(["check-ignore", "-q", "--no-index", "--", path])
        .current_dir(repo)
        .status()
        .expect("git check-ignore must run")
        .success()
}

#[test]
fn branch_cleanup_log_is_ignored_without_hiding_regular_logs() {
    let repo = std::env::temp_dir().join(format!("agend-3096-ignore-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(&repo).expect("temporary Git repository directory must be created");
    std::fs::write(repo.join(".gitignore"), ROOT_GITIGNORE)
        .expect("temporary Git repository .gitignore must be written");
    // allow: raw-git-subprocess — hermetic temp-repo setup with bypass env
    let init = Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .env("AGENTIC_GIT_BYPASS", "1")
        .args(["init", "-q", "-b", "main"])
        .current_dir(&repo)
        .status()
        .expect("temporary Git repository must initialize");
    assert!(init.success(), "temporary Git repository must initialize");

    let generated = format!(
        ".agend-terminal-branch-cleanup-{}.log",
        chrono::Utc::now().format("%Y-%m-%d")
    );
    std::fs::write(repo.join(&generated), "branch cleanup audit\n")
        .expect("generated branch cleanup artifact must be written");
    std::fs::write(repo.join("operator.log"), "operator log\n")
        .expect("ordinary operator log must be written");

    assert!(
        git_check_ignored(&repo, &generated),
        "producer-realistic branch cleanup artifact must be ignored: {generated}"
    );
    assert!(
        !git_check_ignored(&repo, "operator.log"),
        "ordinary .log files must remain visible to git status"
    );
    let _ = std::fs::remove_dir_all(&repo);
}
