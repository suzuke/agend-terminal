//! Arch14: cross-agent identity boundary — integration tests.
//!
//! Through the REAL compiled shim binary (argv0=git): a bound agent whose
//! effective read target (cwd or leading -C) is another agent's daemon-managed
//! same-source worktree must fail loudly with explicit caller/target identity.
//! Same-agent reads, unmanaged/scratch reads, and write isolation are preserved.

#![cfg(unix)]

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn real_git_path() -> PathBuf {
    for cand in ["/usr/bin/git", "/opt/homebrew/bin/git", "/usr/local/bin/git"] {
        if Path::new(cand).exists() {
            return PathBuf::from(cand);
        }
    }
    let out = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .expect("resolve git");
    PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn sanitized_path(_real_git: &Path) -> std::ffi::OsString {
    let path_env = std::env::var_os("PATH").unwrap_or_default();
    let dirs: Vec<_> = std::env::split_paths(&path_env)
        .filter(|p| {
            let s = p.to_string_lossy();
            !s.contains(".agend-terminal") && !s.contains(".agentic-git")
        })
        .collect();
    std::env::join_paths(dirs).unwrap_or(path_env)
}

fn setup_git(dir: &Path, args: &[&str]) {
    let out = Command::new(real_git_path())
        .args(args)
        .current_dir(dir)
        .env("AGENTIC_GIT_BYPASS", "1")
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "setup git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn fixture_home(tag: &str) -> PathBuf {
    let home = std::env::var("HOME").expect("HOME set");
    let d = PathBuf::from(home).join(format!(
        ".agend-arch14-integ-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join(".config-integrity-key"), [7u8; 32]).unwrap();
    d
}

fn write_signed_binding(home: &Path, agent: &str, branch: &str, worktree: &Path, source_repo: &Path) {
    let dir = home.join("runtime").join(agent);
    std::fs::create_dir_all(&dir).unwrap();
    let body = serde_json::json!({
        "version": 1,
        "agent": agent,
        "task_id": format!("t-{agent}"),
        "branch": branch,
        "issued_at": "2026-07-20T00:00:00Z",
        "worktree": worktree.to_str().unwrap(),
        "source_repo": source_repo.to_str().unwrap(),
    })
    .to_string();
    std::fs::write(dir.join("binding.json"), &body).unwrap();
    let sig = agentic_git_core::integrity_core::sign(home, body.as_bytes());
    std::fs::write(dir.join("binding.json.sig"), sig).unwrap();
}

fn two_agent_fixture(home: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let src = home.join("source");
    std::fs::create_dir_all(&src).unwrap();
    setup_git(&src, &["init", "-b", "main"]);
    setup_git(
        &src,
        &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "--allow-empty", "-m", "init"],
    );
    let wt_a = home.join("wt-a");
    let wt_b = home.join("wt-b");
    setup_git(&src, &["worktree", "add", wt_a.to_str().unwrap(), "-b", "feat/a"]);
    setup_git(&src, &["worktree", "add", wt_b.to_str().unwrap(), "-b", "feat/b"]);

    for (agent, wt, branch) in [("agent-a", &wt_a, "feat/a"), ("agent-b", &wt_b, "feat/b")] {
        std::fs::write(
            wt.join(".agend-managed"),
            format!(
                "agent={agent}\nbranch={branch}\nsource_repo={}\nleased_at=2026-07-20T00:00:00+00:00\n",
                src.display()
            ),
        ).unwrap();
        write_signed_binding(home, agent, branch, wt, &src);
    }

    (src, wt_a, wt_b)
}

fn run_shim(
    cwd: &Path,
    home: &Path,
    agent: &str,
    args: &[&str],
) -> std::process::Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_agentic-git"));
    c.arg0("git")
        .args(args)
        .current_dir(cwd)
        .env("AGENTIC_GIT_HOME", home)
        .env("AGENTIC_GIT_AGENT", agent)
        .env("AGENTIC_GIT_REAL_GIT", real_git_path())
        .env("PATH", sanitized_path(&real_git_path()))
        .env_remove("AGEND_HOME")
        .env_remove("AGEND_INSTANCE_NAME")
        .env_remove("AGEND_REAL_GIT")
        .env_remove("AGENTIC_GIT_BYPASS")
        .env_remove("AGEND_GIT_BYPASS")
        .env_remove("AGENTIC_GIT_BYPASS_AGENT")
        .env_remove("AGEND_GIT_BYPASS_AGENT")
        .env_remove("AGENTIC_GIT_BYPASS_UNTIL")
        .env_remove("AGEND_GIT_BYPASS_UNTIL")
        .env_remove("AGENTIC_GIT_SHIM_DEPTH")
        .env_remove("AGEND_GIT_SHIM_DEPTH")
        .env_remove("AGENTIC_GIT_SNAPSHOTS")
        .env_remove("AGEND_GIT_SNAPSHOTS")
        .env_remove("AGENTIC_GIT_ALLOW_CANONICAL_MUTATE")
        .env_remove("AGEND_GIT_ALLOW_CANONICAL_MUTATE");
    c.output().expect("shim runs")
}

/// agent-a reading from INSIDE agent-b's same-source sibling worktree (cwd)
/// must fail loudly with explicit caller/target identity.
#[test]
fn cross_agent_cwd_read_denied_with_identity() {
    let home = fixture_home("cwd");
    let (_src, _wt_a, wt_b) = two_agent_fixture(&home);

    let out = run_shim(&wt_b, &home, "agent-a", &["branch", "--show-current"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "cross-agent cwd read must fail loudly; got exit=0 stdout={stdout:?}"
    );
    assert!(
        stderr.contains("agent-a") && stderr.contains("agent-b"),
        "stderr must name both caller and target agent: {stderr:?}"
    );
    assert!(
        !stdout.contains("feat/a") && !stdout.contains("feat/b"),
        "stdout must carry no branch data: {stdout:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// agent-a reading from agent-b's worktree via leading -C must fail loudly.
#[test]
fn cross_agent_dash_c_read_denied_with_identity() {
    let home = fixture_home("dashc");
    let (_src, _wt_a, wt_b) = two_agent_fixture(&home);
    let neutral = home.join("neutral");
    std::fs::create_dir_all(&neutral).unwrap();

    let out = run_shim(
        &neutral,
        &home,
        "agent-a",
        &["-C", wt_b.to_str().unwrap(), "rev-parse", "--abbrev-ref", "HEAD"],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "cross-agent -C read must fail loudly; got exit=0 stdout={stdout:?}"
    );
    assert!(
        stderr.contains("agent-a") && stderr.contains("agent-b"),
        "stderr must name both agents: {stderr:?}"
    );
    assert!(
        !stdout.contains("feat/a") && !stdout.contains("feat/b"),
        "stdout must carry no branch data: {stdout:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// agent-a reading from a NESTED subdirectory inside agent-b's worktree (cwd)
/// must also fail loudly — the marker walk-up catches it.
#[test]
fn cross_agent_nested_cwd_read_denied_with_identity() {
    let home = fixture_home("ncwd");
    let (_src, _wt_a, wt_b) = two_agent_fixture(&home);
    let nested = wt_b.join("src").join("deep");
    std::fs::create_dir_all(&nested).unwrap();

    let out = run_shim(&nested, &home, "agent-a", &["branch", "--show-current"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "nested cwd cross-agent read must fail loudly; got exit=0 stdout={stdout:?}"
    );
    assert!(
        stderr.contains("agent-a") && stderr.contains("agent-b"),
        "stderr must name both agents: {stderr:?}"
    );
    assert!(
        !stdout.contains("feat/a") && !stdout.contains("feat/b"),
        "stdout must carry no branch data: {stdout:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// agent-a reading from a nested path in agent-b's worktree via leading -C
/// must also fail loudly.
#[test]
fn cross_agent_nested_dash_c_read_denied_with_identity() {
    let home = fixture_home("ndashc");
    let (_src, _wt_a, wt_b) = two_agent_fixture(&home);
    let nested = wt_b.join("lib").join("sub");
    std::fs::create_dir_all(&nested).unwrap();
    let neutral = home.join("neutral");
    std::fs::create_dir_all(&neutral).unwrap();

    let out = run_shim(
        &neutral,
        &home,
        "agent-a",
        &["-C", nested.to_str().unwrap(), "rev-parse", "--abbrev-ref", "HEAD"],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "nested -C cross-agent read must fail loudly; got exit=0 stdout={stdout:?}"
    );
    assert!(
        stderr.contains("agent-a") && stderr.contains("agent-b"),
        "stderr must name both agents: {stderr:?}"
    );
    assert!(
        !stdout.contains("feat/a") && !stdout.contains("feat/b"),
        "stdout must carry no branch data: {stdout:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Same-agent reads are unchanged — agent-a reading from its own worktree
/// still works normally.
#[test]
fn same_agent_read_passes() {
    let home = fixture_home("same");
    let (_src, wt_a, _wt_b) = two_agent_fixture(&home);

    let out = run_shim(&wt_a, &home, "agent-a", &["branch", "--show-current"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "same-agent read must pass: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(stdout.trim(), "feat/a", "must return own branch");
    std::fs::remove_dir_all(&home).ok();
}

/// Unmanaged/scratch repo reads are unchanged.
#[test]
fn unmanaged_scratch_read_passes() {
    let home = fixture_home("scratch");
    let (_src, _wt_a, _wt_b) = two_agent_fixture(&home);
    let scratch = home.join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    setup_git(&scratch, &["init", "-b", "scratch-main"]);
    setup_git(
        &scratch,
        &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "--allow-empty", "-m", "s"],
    );

    let out = run_shim(&scratch, &home, "agent-a", &["status", "--porcelain"]);
    assert!(
        out.status.success(),
        "unmanaged scratch read must pass: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Write isolation: a cross-agent write attempt must not move the target's HEAD.
#[test]
fn cross_agent_write_isolation() {
    let home = fixture_home("write");
    let (_src, _wt_a, wt_b) = two_agent_fixture(&home);
    let head_before = Command::new(real_git_path())
        .args(["rev-parse", "HEAD"])
        .current_dir(&wt_b)
        .env("AGENTIC_GIT_BYPASS", "1")
        .output()
        .expect("git");
    let _ = run_shim(
        &wt_b,
        &home,
        "agent-a",
        &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "--allow-empty", "-m", "x"],
    );
    let head_after = Command::new(real_git_path())
        .args(["rev-parse", "HEAD"])
        .current_dir(&wt_b)
        .env("AGENTIC_GIT_BYPASS", "1")
        .output()
        .expect("git");
    assert_eq!(
        String::from_utf8_lossy(&head_before.stdout),
        String::from_utf8_lossy(&head_after.stdout),
        "target tree HEAD must not move on cross-agent write"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Symlink alias pointing into agent-b's worktree must be resolved and denied.
#[test]
fn cross_agent_symlink_alias_denied() {
    let home = fixture_home("sym");
    let (_src, _wt_a, wt_b) = two_agent_fixture(&home);
    let alias = home.join("alias-to-b");
    std::os::unix::fs::symlink(&wt_b, &alias).unwrap();

    let out = run_shim(&alias, &home, "agent-a", &["branch", "--show-current"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "symlink alias into sibling must fail; got exit=0 stdout={stdout:?}"
    );
    assert!(
        stderr.contains("agent-a") && stderr.contains("agent-b"),
        "stderr must name both agents: {stderr:?}"
    );
    assert!(
        !stdout.contains("feat/a") && !stdout.contains("feat/b"),
        "stdout must carry no branch data: {stdout:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// >64-level nested path inside sibling worktree must still be denied.
#[test]
fn cross_agent_deep_nested_denied() {
    let home = fixture_home("deep");
    let (_src, _wt_a, wt_b) = two_agent_fixture(&home);
    let mut deep = wt_b.clone();
    for i in 0..70 {
        deep = deep.join(format!("d{i}"));
    }
    std::fs::create_dir_all(&deep).unwrap();

    let out = run_shim(&deep, &home, "agent-a", &["branch", "--show-current"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        ">64-deep nested cross-agent read must fail; got exit=0 stdout={stdout:?}"
    );
    assert!(
        stderr.contains("agent-a") && stderr.contains("agent-b"),
        "stderr must name both agents: {stderr:?}"
    );
    assert!(
        !stdout.contains("feat/a") && !stdout.contains("feat/b"),
        "stdout must carry no branch data: {stdout:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Scratch repo nested UNDER a sibling worktree has a foreign commondir
/// and must pass through — it's not the sibling's data.
#[test]
fn scratch_nested_under_sibling_passes() {
    let home = fixture_home("nscratch");
    let (_src, _wt_a, wt_b) = two_agent_fixture(&home);
    let scratch = wt_b.join("vendor").join("scratch-repo");
    std::fs::create_dir_all(&scratch).unwrap();
    setup_git(&scratch, &["init", "-b", "scratch-main"]);
    setup_git(
        &scratch,
        &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "--allow-empty", "-m", "s"],
    );

    let out = run_shim(&scratch, &home, "agent-a", &["status", "--porcelain"]);
    assert!(
        out.status.success(),
        "scratch repo nested under sibling must pass: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::remove_dir_all(&home).ok();
}
