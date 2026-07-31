//! Real-entry regression for #3142: a bound agent in a non-repository cwd must
//! not have read-only git commands silently redirected to its bound worktree.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn real_git() -> PathBuf {
    for candidate in ["/usr/bin/git", "/opt/homebrew/bin/git", "/usr/local/bin/git"] {
        let path = PathBuf::from(candidate);
        if path.exists() {
            return path;
        }
    }
    panic!("no real git found");
}

fn tempdir(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "agentic-git-nonrepo-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn run_real_git(real_git: &Path, cwd: &Path, args: &[&str]) -> Output {
    let output = Command::new(real_git)
        .args(args)
        .current_dir(cwd)
        .env("AGENTIC_GIT_BYPASS", "1")
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "real git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn write_binding(home: &Path, agent: &str, branch: &str, worktree: &Path, source: &Path) {
    let runtime = home.join("runtime").join(agent);
    std::fs::create_dir_all(&runtime).unwrap();
    std::fs::write(home.join(".config-integrity-key"), [7u8; 32]).unwrap();
    let body = serde_json::json!({
        "version": 1,
        "agent": agent,
        "task_id": format!("{agent}-task"),
        "branch": branch,
        "issued_at": "2026-01-01T00:00:00Z",
        "worktree": worktree.to_string_lossy(),
        "source_repo": source.to_string_lossy(),
    })
    .to_string();
    std::fs::write(runtime.join("binding.json"), &body).unwrap();
    let signature = agentic_git_core::integrity_core::sign(home, body.as_bytes());
    std::fs::write(runtime.join("binding.json.sig"), signature).unwrap();
}

fn run_shim(cwd: &Path, home: &Path, agent: &str, args: &[&str], real_git: &Path) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agentic-git"));
    command
        .arg0("git")
        .args(args)
        .current_dir(cwd)
        .env("AGENTIC_GIT_HOME", home)
        .env("AGENTIC_GIT_AGENT", agent)
        .env("AGENTIC_GIT_REAL_GIT", real_git)
        .env("PATH", real_git.parent().unwrap())
        .env_remove("AGEND_HOME")
        .env_remove("AGEND_INSTANCE_NAME")
        .env_remove("AGEND_REAL_GIT")
        .env_remove("AGENTIC_GIT_BYPASS")
        .env_remove("AGEND_GIT_BYPASS")
        .env_remove("AGENTIC_GIT_BYPASS_AGENT")
        .env_remove("AGEND_GIT_BYPASS_AGENT")
        .env_remove("AGENTIC_GIT_BYPASS_UNTIL")
        .env_remove("AGEND_GIT_BYPASS_UNTIL")
        .env_remove("AGENTIC_GIT_SHIM_DEPTH");
    command.output().unwrap()
}

#[test]
fn bound_agent_nonrepo_cwd_keeps_read_only_git_at_real_cwd() {
    let root = tempdir("red");
    let home = root.join("agend-home");
    let source = root.join("source");
    let worktree = root.join("bound-worktree");
    let nonrepo = root.join("archive");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir_all(&nonrepo).unwrap();
    let real_git = real_git();

    run_real_git(&real_git, &source, &["init", "-q", "-b", "main"]);
    run_real_git(&real_git, &source, &["config", "user.name", "test"]);
    run_real_git(&real_git, &source, &["config", "user.email", "test@example.com"]);
    std::fs::write(source.join("README.md"), "fixture\n").unwrap();
    run_real_git(&real_git, &source, &["add", "."]);
    run_real_git(&real_git, &source, &["commit", "-q", "-m", "init"]);
    run_real_git(
        &real_git,
        &source,
        &["worktree", "add", "-q", "-b", "agent/3142", worktree.to_str().unwrap()],
    );
    write_binding(&home, "agent-3142", "agent/3142", &worktree, &source);

    let rev_parse = run_shim(
        &nonrepo,
        &home,
        "agent-3142",
        &["rev-parse", "--show-toplevel"],
        &real_git,
    );
    assert!(
        !rev_parse.status.success(),
        "non-repo rev-parse must fail in the caller cwd, not report the bound worktree: {}",
        String::from_utf8_lossy(&rev_parse.stdout)
    );
    assert!(!String::from_utf8_lossy(&rev_parse.stdout).contains(worktree.to_str().unwrap()));

    let status = run_shim(
        &nonrepo,
        &home,
        "agent-3142",
        &["status", "--porcelain"],
        &real_git,
    );
    assert!(
        !status.status.success(),
        "non-repo status must fail in the caller cwd, not silently use the bound worktree"
    );
    assert!(!String::from_utf8_lossy(&status.stdout).contains(worktree.to_str().unwrap()));

    std::fs::remove_dir_all(root).unwrap();
}
