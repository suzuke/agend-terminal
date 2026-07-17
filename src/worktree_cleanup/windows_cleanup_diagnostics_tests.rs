//! Regression coverage for diagnostics retained by the worktree-removal caller.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

fn git_in(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("AGEND_GIT_BYPASS", "1")
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_commit_dated(dir: &Path, message: &str, date: &str) {
    let output = Command::new("git")
        .args(["commit", "-m", message])
        .current_dir(dir)
        .env("AGEND_GIT_BYPASS", "1")
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .output()
        .expect("dated git commit");
    assert!(
        output.status.success(),
        "git commit failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn setup_repo() -> PathBuf {
    let repo = std::env::temp_dir().join(format!(
        "agend-wt-diagnostics-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&repo).expect("repo directory");
    git_in(&repo, &["init", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "init").expect("README");
    git_in(&repo, &["add", "."]);
    git_in(&repo, &["commit", "-m", "init"]);
    git_in(&repo, &["checkout", "-b", "feat/done"]);
    std::fs::write(repo.join("feat.txt"), "feature").expect("feature");
    git_in(&repo, &["add", "."]);
    git_commit_dated(&repo, "feature work", "2024-01-01T00:00:00 +0000");
    git_in(&repo, &["checkout", "main"]);
    git_in(&repo, &["worktree", "add", "wt-done", "feat/done"]);
    git_in(&repo, &["merge", "feat/done"]);
    repo
}

fn write_binding(home: &Path, repo: &Path) {
    let runtime = crate::paths::runtime_dir(home).join("other-agent");
    std::fs::create_dir_all(&runtime).expect("runtime directory");
    std::fs::write(
        runtime.join("binding.json"),
        serde_json::to_vec(&serde_json::json!({
            "source_repo": repo.display().to_string()
        }))
        .expect("binding json"),
    )
    .expect("binding");
}

fn hygiene_tasks(home: &Path) -> Vec<(String, serde_json::Value)> {
    crate::task_events::replay(home)
        .map(|state| {
            state
                .tasks
                .values()
                .filter_map(|task| {
                    Some((
                        task.metadata
                            .get(crate::daemon::hygiene_task::ALERT_KEY_META)?
                            .as_str()?
                            .to_string(),
                        task.metadata
                            .get(crate::daemon::hygiene_task::EVIDENCE_META)
                            .cloned()
                            .unwrap_or(serde_json::Value::Null),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn real_git() -> PathBuf {
    let path = std::env::var_os("PATH").expect("PATH");
    let executable = if cfg!(windows) { "git.exe" } else { "git" };
    std::env::split_paths(&path)
        .map(|dir| dir.join(executable))
        .find(|candidate| candidate.is_file())
        .expect("git must be on PATH")
}

#[cfg(unix)]
fn install_failing_git(stub_dir: &Path, real_git: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let script = format!(
        "#!/bin/sh\nif [ \"$1 $2\" = \"worktree remove\" ]; then\n  echo cleanup-diagnostic-sentinel 1>&2\n  exit 23\nfi\nexec '{}' \"$@\"\n",
        real_git.display()
    );
    let path = stub_dir.join("git");
    std::fs::write(&path, script).expect("git stub");
    let mut permissions = std::fs::metadata(&path)
        .expect("git stub metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).expect("git stub permissions");
}

#[cfg(windows)]
fn install_failing_git(stub_dir: &Path, real_git: &Path) {
    let script = format!(
        "@echo off\nif \"%~1 %~2\"==\"worktree remove\" (\n  echo cleanup-diagnostic-sentinel 1>&2\n  exit /b 23\n)\n\"{}\" %*\nexit /b %ERRORLEVEL%\n",
        real_git.display()
    );
    std::fs::write(stub_dir.join("git.cmd"), script).expect("git stub");
}

fn prepend_path(stub_dir: &Path, old_path: &std::ffi::OsString) -> std::ffi::OsString {
    std::env::join_paths(
        std::iter::once(stub_dir.to_path_buf()).chain(std::env::split_paths(old_path)),
    )
    .expect("PATH")
}

#[test]
fn real_sweep_remove_failure_preserves_final_status_and_stderr_2830_red() {
    let _lock = super::tests::ENV_LOCK.lock();
    let repo = setup_repo();
    let home = std::env::temp_dir().join(format!(
        "agend-wt-diagnostics-home-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&home).expect("home directory");
    write_binding(&home, &repo);

    let stub_dir = std::env::temp_dir().join(format!(
        "agend-wt-diagnostics-git-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&stub_dir).expect("stub directory");
    install_failing_git(&stub_dir, &real_git());

    let old_path = std::env::var_os("PATH").expect("PATH");
    let old_cleanup = std::env::var_os("AGEND_WORKTREE_AUTO_CLEANUP");
    std::env::set_var("PATH", prepend_path(&stub_dir, &old_path));
    std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
    let configs = HashMap::from([(String::from("other-agent"), Some(repo.join("other")))]);
    let removed = sweep_from_registry(&home, &configs, &[]);
    std::env::set_var("PATH", old_path);
    match old_cleanup {
        Some(value) => std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", value),
        None => std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP"),
    }

    let tasks = hygiene_tasks(&home);
    let key = format!("residue-remove-failed:{}:feat/done", repo.display());
    let (_, evidence) = tasks
        .iter()
        .find(|(candidate, _)| candidate == &key)
        .unwrap_or_else(|| panic!("expected hygiene task; removed={removed:?}, tasks={tasks:?}"));
    let reason = evidence["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("status 23"),
        "final status was lost: {evidence}"
    );
    assert!(
        reason.contains("cleanup-diagnostic-sentinel"),
        "final stderr was lost: {evidence}"
    );

    std::fs::remove_dir_all(repo).ok();
    std::fs::remove_dir_all(home).ok();
    std::fs::remove_dir_all(stub_dir).ok();
}
