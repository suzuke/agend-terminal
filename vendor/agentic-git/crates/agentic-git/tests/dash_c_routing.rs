//! Behavioral coverage for explicit `git -C <foreign-repo>` mutation routing.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn real_git_path() -> PathBuf {
    for candidate in [
        "/usr/bin/git",
        "/opt/homebrew/bin/git",
        "/usr/local/bin/git",
    ] {
        if Path::new(candidate).exists() {
            return PathBuf::from(candidate);
        }
    }
    panic!("no real git found");
}

fn real_git(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(real_git_path())
        .args(args)
        .current_dir(dir)
        .env("AGENTIC_GIT_BYPASS", "1")
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("real git runs")
}

fn setup_git(dir: &Path, args: &[&str]) {
    let output = real_git(dir, args);
    assert!(
        output.status.success(),
        "setup git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fixture_home() -> PathBuf {
    let home = PathBuf::from(std::env::var("HOME").expect("HOME set")).join(format!(
        ".agend-dash-c-routing-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join(".config-integrity-key"), [7u8; 32]).unwrap();
    home
}

fn write_signed_binding(home: &Path, worktree: &Path, source_repo: &Path) {
    let dir = home.join("runtime/agent-a");
    std::fs::create_dir_all(&dir).unwrap();
    let body = serde_json::json!({
        "version": 1,
        "agent": "agent-a",
        "task_id": "t-dash-c",
        "branch": "feat/a",
        "issued_at": "2026-09-03T00:00:00Z",
        "worktree": worktree.to_str().unwrap(),
        "source_repo": source_repo.to_str().unwrap(),
    })
    .to_string();
    std::fs::write(dir.join("binding.json"), &body).unwrap();
    let signature = agentic_git_core::integrity_core::sign(home, body.as_bytes());
    std::fs::write(dir.join("binding.json.sig"), signature).unwrap();
}

fn bound_agent_fixture(home: &Path) -> PathBuf {
    let source = home.join("source");
    std::fs::create_dir_all(&source).unwrap();
    setup_git(&source, &["init", "-b", "main"]);
    setup_git(
        &source,
        &[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ],
    );
    let worktree = home.join("worktree");
    setup_git(
        &source,
        &[
            "worktree",
            "add",
            worktree.to_str().unwrap(),
            "-b",
            "feat/a",
        ],
    );
    std::fs::write(
        worktree.join(".agend-managed"),
        format!(
            "agent=agent-a\nbranch=feat/a\nsource_repo={}\nleased_at=2026-09-03T00:00:00+00:00\n",
            source.display()
        ),
    )
    .unwrap();
    write_signed_binding(home, &worktree, &source);
    worktree
}

fn foreign_repo(home: &Path) -> PathBuf {
    let repo = home.join("foreign");
    std::fs::create_dir_all(&repo).unwrap();
    setup_git(&repo, &["init", "-b", "main"]);
    setup_git(
        &repo,
        &[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ],
    );
    repo
}

fn run_shim(cwd: &Path, home: &Path, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agentic-git"));
    command
        .arg0("git")
        .args(args)
        .current_dir(cwd)
        .env("AGENTIC_GIT_HOME", home)
        .env("AGENTIC_GIT_AGENT", "agent-a")
        .env("AGENTIC_GIT_REAL_GIT", real_git_path())
        .env_remove("AGEND_HOME")
        .env_remove("AGEND_INSTANCE_NAME")
        .env_remove("AGEND_REAL_GIT")
        .env_remove("AGENTIC_GIT_BYPASS")
        .env_remove("AGEND_GIT_BYPASS")
        .env_remove("AGENTIC_GIT_SHIM_DEPTH")
        .env_remove("AGEND_GIT_SHIM_DEPTH");
    command.output().expect("shim runs")
}

fn commit_count(repo: &Path) -> String {
    let output = real_git(repo, &["rev-list", "--count", "HEAD"]);
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

#[test]
fn explicit_dash_c_add_and_commit_mutate_foreign_repo_only() {
    let home = fixture_home();
    let worktree = bound_agent_fixture(&home);
    let foreign = foreign_repo(&home);
    let file = foreign.join("foreign-only.txt");
    std::fs::write(&file, "foreign change\n").unwrap();

    let add = run_shim(
        &worktree,
        &home,
        &["-C", foreign.to_str().unwrap(), "add", file.to_str().unwrap()],
    );
    assert!(
        add.status.success(),
        "explicit -C add must target the foreign repo: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let commit = run_shim(
        &worktree,
        &home,
        &[
            "-C",
            foreign.to_str().unwrap(),
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "foreign commit",
        ],
    );
    assert!(
        commit.status.success(),
        "explicit -C commit must target the foreign repo: {}",
        String::from_utf8_lossy(&commit.stderr)
    );

    assert_eq!(commit_count(&foreign), "2", "foreign repo gets the commit");
    assert_eq!(commit_count(&worktree), "1", "bound worktree stays untouched");
    let status = real_git(&worktree, &["status", "--porcelain"]);
    assert!(status.status.success());
    assert!(
        status.stdout.is_empty(),
        "bound worktree must remain clean: {}",
        String::from_utf8_lossy(&status.stdout)
    );

    std::fs::remove_dir_all(home).unwrap();
}
