//! Sparse-pollution real-entry routing — integration tests through the REAL
//! compiled shim binary (argv0=git).
//!
//! Vector (four fleet incidents, pattern `plugins/grafana-mcp`): Claude Code's
//! plugin marketplace sync runs `git sparse-checkout set plugins/<x>` with cwd
//! = the marketplace clone (a FOREIGN repo). Pre-fix the bound-agent shim
//! ChdirPass'd it into the bound worktree — `core.sparseCheckout` plus cone
//! patterns landed in the worktree's `config.worktree` and the agent's working
//! tree was silently emptied while `git status` kept reporting clean. The
//! bare foreign-cwd entry must route to the FOREIGN repo and leave the bound
//! worktree non-sparse; an agent's own bare `sparse-checkout` must keep
//! operating on its bound worktree.
//!
//! The leading-global form `git -C <target> sparse-checkout ...` stays
//! FAIL-CLOSED (unrecognized behind leading globals): `sparse-checkout` is
//! deliberately outside `is_mutating_local`, so a recognized `-C` would
//! survive `strip_target_overrides` and redirect the write to ANY non-foreign
//! target too — a bound agent could sparse the canonical SOURCE repo from its
//! own worktree (#2950 primary review, compiled-shim A/B). Denying the `-C`
//! form closes that while keeping the incident-shape (bare foreign-cwd) fix.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn real_git_path() -> PathBuf {
    for cand in [
        "/usr/bin/git",
        "/opt/homebrew/bin/git",
        "/usr/local/bin/git",
    ] {
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

/// Real-git escape hatch for fixture setup and post-condition inspection —
/// never routes through the shim under test.
fn real_git(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(real_git_path())
        .args(args)
        .current_dir(dir)
        .env("AGENTIC_GIT_BYPASS", "1")
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("git runs")
}

fn setup_git(dir: &Path, args: &[&str]) {
    let out = real_git(dir, args);
    assert!(
        out.status.success(),
        "setup git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn fixture_home(tag: &str) -> PathBuf {
    let home = std::env::var("HOME").expect("HOME set");
    let d = PathBuf::from(home).join(format!(
        ".agend-sparse-routing-{tag}-{}-{}",
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

fn write_signed_binding(
    home: &Path,
    agent: &str,
    branch: &str,
    worktree: &Path,
    source_repo: &Path,
) {
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

/// One bound agent (`agent-a`) on a daemon-managed worktree of `source`.
fn bound_agent_fixture(home: &Path) -> (PathBuf, PathBuf) {
    let src = home.join("source");
    std::fs::create_dir_all(&src).unwrap();
    setup_git(&src, &["init", "-b", "main"]);
    setup_git(
        &src,
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
    );
    let wt = home.join("wt-a");
    setup_git(
        &src,
        &["worktree", "add", wt.to_str().unwrap(), "-b", "feat/a"],
    );
    std::fs::write(
        wt.join(".agend-managed"),
        format!(
            "agent=agent-a\nbranch=feat/a\nsource_repo={}\nleased_at=2026-07-20T00:00:00+00:00\n",
            src.display()
        ),
    )
    .unwrap();
    write_signed_binding(home, "agent-a", "feat/a", &wt, &src);
    (src, wt)
}

/// A third-party repo the agent is NOT bound to, with a cone-shaped subdir —
/// the marketplace-clone stand-in.
fn foreign_repo(home: &Path, tag: &str) -> PathBuf {
    let f = home.join(format!("foreign-{tag}"));
    std::fs::create_dir_all(f.join("plugins/fake-plugin")).unwrap();
    std::fs::write(f.join("plugins/fake-plugin/f.txt"), "x").unwrap();
    std::fs::write(f.join("root.txt"), "y").unwrap();
    setup_git(&f, &["init", "-b", "main"]);
    setup_git(&f, &["add", "-A"]);
    setup_git(
        &f,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-m",
            "init",
        ],
    );
    f
}

fn run_shim(cwd: &Path, home: &Path, agent: &str, args: &[&str]) -> std::process::Output {
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

/// `git sparse-checkout list` via real git: `Ok(list)` when the repo/worktree
/// IS sparse, `Err(stderr)` when it is not (`fatal: this worktree is not
/// sparse`, exit 128).
fn sparse_state(dir: &Path) -> Result<String, String> {
    let out = real_git(dir, &["sparse-checkout", "list"]);
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// The live incident shape: cwd = the foreign repo, bare `sparse-checkout set`.
/// Must apply to the FOREIGN repo and leave the bound worktree non-sparse.
#[test]
fn foreign_cwd_sparse_checkout_routes_to_foreign_repo() {
    let home = fixture_home("bare");
    let (_src, wt) = bound_agent_fixture(&home);
    let foreign = foreign_repo(&home, "bare");

    let out = run_shim(
        &foreign,
        &home,
        "agent-a",
        &["sparse-checkout", "set", "plugins/fake-plugin"],
    );
    assert!(
        out.status.success(),
        "foreign-cwd sparse-checkout must succeed against the foreign repo: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let flist = sparse_state(&foreign).expect("foreign repo must be sparse after the set");
    assert!(
        flist.contains("plugins/fake-plugin"),
        "foreign repo must carry the cone pattern: {flist:?}"
    );
    assert!(
        sparse_state(&wt).is_err(),
        "bound worktree must stay non-sparse (pre-fix pollution wrote \
         core.sparseCheckout into its config.worktree)"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// The leading-global shape: cwd = the agent's own bound worktree, the target
/// travels via `-C <foreign>`. This must stay FAIL-CLOSED (the unknown-global
/// deny): classification rejects `sparse-checkout` behind leading globals
/// before foreign-target post-processing, because the surviving `-C` could
/// carry the write to the caller's target — the same mechanism reaches the
/// canonical source repo (see the negative control below), so the whole `-C`
/// form is denied.
#[test]
fn leading_dash_c_sparse_checkout_stays_fail_closed() {
    let home = fixture_home("dashc");
    let (_src, wt) = bound_agent_fixture(&home);
    let foreign = foreign_repo(&home, "dashc");

    let out = run_shim(
        &wt,
        &home,
        "agent-a",
        &[
            "-C",
            foreign.to_str().unwrap(),
            "sparse-checkout",
            "set",
            "plugins/fake-plugin",
        ],
    );
    assert!(
        !out.status.success(),
        "-C <foreign> sparse-checkout must fail closed (unknown behind leading globals)"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("unrecognized subcommand"),
        "deny must come from the unknown-global guard, not an incidental failure: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        sparse_state(&foreign).is_err(),
        "foreign repo must stay non-sparse — the denied command must not run"
    );
    assert!(
        sparse_state(&wt).is_err(),
        "bound worktree must stay non-sparse"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// Negative control (#2950 primary review blocker): a bound agent in its own
/// worktree must NOT be able to sparse the canonical SOURCE repo via
/// `-C <source-repo>`. With cwd = the bound worktree the foreign-cwd guard
/// never fires (canonical shares the worktree's commondir → non-foreign), so
/// the only thing standing between this argv and a silent write to the shared
/// source repository is the fail-closed unknown-global deny.
#[test]
fn dash_c_canonical_sparse_checkout_denied_source_stays_clean() {
    let home = fixture_home("canon");
    let (src, wt) = bound_agent_fixture(&home);

    let out = run_shim(
        &wt,
        &home,
        "agent-a",
        &[
            "-C",
            src.to_str().unwrap(),
            "sparse-checkout",
            "set",
            "plugins/fake-plugin",
        ],
    );
    assert!(
        !out.status.success(),
        "-C <source-repo> sparse-checkout must be denied: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("unrecognized subcommand"),
        "deny must come from the unknown-global guard, not an incidental failure: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        sparse_state(&src).is_err(),
        "canonical source repo must stay non-sparse — this is the shared-repo \
         harm class the shim exists to prevent"
    );
    assert!(
        sparse_state(&wt).is_err(),
        "bound worktree must stay non-sparse"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// Preserved behavior control: an agent's own bare `sparse-checkout` (cwd =
/// its bound worktree) keeps operating on the bound worktree.
#[test]
fn own_worktree_sparse_checkout_still_targets_bound_worktree() {
    let home = fixture_home("own");
    let (src, wt) = bound_agent_fixture(&home);
    // Give the source repo a cone-shaped subdir so the worktree can go sparse.
    std::fs::create_dir_all(src.join("plugins/fake-plugin")).unwrap();
    std::fs::write(src.join("plugins/fake-plugin/f.txt"), "x").unwrap();
    setup_git(
        &src,
        &["-c", "user.name=t", "-c", "user.email=t@t", "add", "-A"],
    );
    setup_git(
        &src,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-m",
            "cone",
        ],
    );

    let out = run_shim(
        &wt,
        &home,
        "agent-a",
        &["sparse-checkout", "set", "plugins/fake-plugin"],
    );
    assert!(
        out.status.success(),
        "own-worktree sparse-checkout must keep working: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        sparse_state(&wt).is_ok(),
        "the agent's own bound worktree is the intended target here"
    );
    let _ = std::fs::remove_dir_all(&home);
}
