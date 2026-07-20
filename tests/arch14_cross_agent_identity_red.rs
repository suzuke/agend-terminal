#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Arch14 cross-agent git identity — RED (t-20260720064306627171-39872-29,
//! frozen contract d-20260719233444615181-2 clauses 2/3).
//!
//! Through the REAL vendored shim entry (the workspace `agentic-git` bin,
//! invoked under the `git` argv0 so it runs in shim mode): when a BOUND
//! agent's effective read target — the process cwd or a leading `-C` — is a
//! SAME-SOURCE SIBLING worktree managed for ANOTHER agent, the shim today
//! silently ChdirPasses to the caller's own worktree and answers with the
//! WRONG tree's data (a fabricated read). It must instead fail loudly (or
//! structurally refuse) so false target data is impossible. Ordinary
//! scratch-repo reads and write isolation stay unchanged (green guards).

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture_home(tag: &str) -> PathBuf {
    let home = std::env::var("HOME").expect("HOME set");
    let d = PathBuf::from(home).join(format!(
        ".agend-arch14-xid-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

// allow: dead-code-helper — frozen RED contract helper; not shadowing production git_ok semantics (takes &Path not &cwd)
fn git_ok(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn real_git_path() -> String {
    // NEVER `command -v git` here: on a live-fleet machine PATH resolves to
    // the INSTALLED agentic-git shim, and pointing AGENTIC_GIT_REAL_GIT at
    // another shim trips the #1504 recursion guard. System git locations
    // cover macOS + CI runners.
    for cand in [
        "/usr/bin/git",
        "/opt/homebrew/bin/git",
        "/usr/local/bin/git",
    ] {
        if Path::new(cand).exists() {
            return cand.to_string();
        }
    }
    let out = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .expect("resolve git");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Two daemon-managed same-source sibling worktrees for two DIFFERENT agents:
/// four-field markers + HMAC-signed bindings (the exact daemon shape the shim
/// verifies). Returns (source, wt_a, wt_b, shim_git_path).
fn two_agent_fixture(home: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let src = home.join("source");
    std::fs::create_dir_all(&src).unwrap();
    git_ok(&src, &["init", "-b", "main"]);
    git_ok(
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
    let wt_a = home.join("wt-a");
    let wt_b = home.join("wt-b");
    git_ok(
        &src,
        &["worktree", "add", wt_a.to_str().unwrap(), "-b", "feat/a"],
    );
    git_ok(
        &src,
        &["worktree", "add", wt_b.to_str().unwrap(), "-b", "feat/b"],
    );

    for (agent, wt, branch) in [("agent-a", &wt_a, "feat/a"), ("agent-b", &wt_b, "feat/b")] {
        std::fs::write(
            wt.join(".agend-managed"),
            format!(
                "agent={agent}\nbranch={branch}\nsource_repo={}\nleased_at=2026-07-20T00:00:00+00:00\n",
                src.display()
            ),
        )
        .unwrap();
        let rt = home.join("runtime").join(agent);
        std::fs::create_dir_all(&rt).unwrap();
        let body = serde_json::to_vec_pretty(&serde_json::json!({
            "version": 1,
            "agent": agent,
            "task_id": "t-arch14-xid",
            "branch": branch,
            "issued_at": "2026-07-20T00:00:00+00:00",
            "worktree": wt.to_str().unwrap(),
            "source_repo": src.to_str().unwrap(),
        }))
        .unwrap();
        std::fs::write(rt.join("binding.json"), &body).unwrap();
        agentic_git_core::integrity_core::ensure_key(home).expect("key");
        let tag = agentic_git_core::integrity_core::sign_binding(home, &body).expect("sign");
        std::fs::write(rt.join("binding.json.sig"), tag).unwrap();
    }

    // The shim only enters shim mode when argv0 is `git` — symlink the
    // prebuilt workspace `agentic-git` bin under that name.
    let bin_dir = home.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let shim_git = bin_dir.join("git");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_agentic-git"), &shim_git).unwrap();
    (src, wt_a, wt_b, shim_git)
}

fn run_shim(
    shim_git: &Path,
    home: &Path,
    agent: &str,
    cwd: &Path,
    args: &[&str],
) -> std::process::Output {
    Command::new(shim_git)
        .args(args)
        .current_dir(cwd)
        .env("AGENTIC_GIT_HOME", home)
        .env("AGENTIC_GIT_AGENT", agent)
        .env("AGENTIC_GIT_REAL_GIT", real_git_path())
        .output()
        .expect("shim runs")
}

/// RED 1: agent-a reading from INSIDE agent-b's same-source sibling worktree
/// must fail loudly / structurally refuse — never answer with fabricated
/// data from a different tree. Today the shim ChdirPasses to wt-a and
/// reports feat/a with exit 0.
#[test]
fn arch14_sibling_cwd_read_fails_loud_never_fabricates() {
    let home = fixture_home("cwd");
    let (_src, _wt_a, wt_b, shim_git) = two_agent_fixture(&home);

    let out = run_shim(
        &shim_git,
        &home,
        "agent-a",
        &wt_b,
        &["branch", "--show-current"],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !out.status.success(),
        "a read from inside ANOTHER agent's managed worktree must fail loudly / \
         structurally refuse (contract clause 3: nonzero/refusal identity); \
         got exit=0 stdout={stdout:?} (today: fabricated data from the caller's own tree)"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("agent-a") && stderr.contains("agent-b"),
        "the refusal must EXPLICITLY identify the caller (agent-a) and the target \
         owner (agent-b) — a generic error must not pass: stderr={stderr:?}"
    );
    assert!(
        !stdout.contains("feat/a") && !stdout.contains("feat/b"),
        "refusal stdout must carry no branch data: {stdout:?}"
    );
    assert!(wt_b.exists(), "target tree untouched");
    std::fs::remove_dir_all(&home).ok();
}

/// RED 2: the identity boundary through a leading explicit `-C <sibling>`.
/// Empirically today the caller's ChdirPass cwd is overridden by git's own
/// later `-C`, so the read executes INSIDE the other agent's managed tree
/// (exit 0, that tree's real data) — a cross-agent identity crossing with no
/// refusal. The contract requires nonzero/refusal identity for any effective
/// read target that is another agent's managed worktree.
#[test]
fn arch14_sibling_dash_c_read_fails_loud_never_fabricates() {
    let home = fixture_home("dashc");
    let (_src, _wt_a, wt_b, shim_git) = two_agent_fixture(&home);

    let neutral = home.join("neutral");
    std::fs::create_dir_all(&neutral).unwrap();
    let out = run_shim(
        &shim_git,
        &home,
        "agent-a",
        &neutral,
        &[
            "-C",
            wt_b.to_str().unwrap(),
            "rev-parse",
            "--abbrev-ref",
            "HEAD",
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !out.status.success(),
        "an explicit leading -C into ANOTHER agent's managed worktree must fail \
         loudly / structurally refuse (contract clause 3: nonzero/refusal \
         identity); got exit=0 stdout={stdout:?}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("agent-a") && stderr.contains("agent-b"),
        "the -C refusal must EXPLICITLY identify the caller (agent-a) and the \
         target owner (agent-b) — a generic error must not pass: stderr={stderr:?}"
    );
    assert!(
        !stdout.contains("feat/a") && !stdout.contains("feat/b"),
        "refusal stdout must carry no branch data: {stdout:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Green guard: an ordinary scratch repo (NOT daemon-managed — no marker)
/// keeps its existing behavior; the fix must not turn unmanaged reads into
/// refusals.
#[test]
fn arch14_scratch_repo_read_unchanged() {
    let home = fixture_home("scratch");
    let (_src, _wt_a, _wt_b, shim_git) = two_agent_fixture(&home);
    let scratch = home.join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    git_ok(&scratch, &["init", "-b", "scratch-main"]);
    git_ok(
        &scratch,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "--allow-empty",
            "-m",
            "s",
        ],
    );

    let out = run_shim(
        &shim_git,
        &home,
        "agent-a",
        &scratch,
        &["status", "--porcelain"],
    );
    assert!(
        out.status.success(),
        "ordinary scratch-repo read must keep succeeding: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Green guard: write isolation is unchanged — a cross-tree write attempt
/// never mutates the OTHER agent's tree (today it lands in the caller's own
/// tree via ChdirPass; post-fix it may refuse — either way wt-b's HEAD must
/// not move).
#[test]
fn arch14_sibling_write_isolation_unchanged() {
    let home = fixture_home("write");
    let (_src, _wt_a, wt_b, shim_git) = two_agent_fixture(&home);
    let head_before = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&wt_b)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("git");
    let _ = run_shim(
        &shim_git,
        &home,
        "agent-a",
        &wt_b,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "--allow-empty",
            "-m",
            "x",
        ],
    );
    let head_after = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&wt_b)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("git");
    assert_eq!(
        String::from_utf8_lossy(&head_before.stdout),
        String::from_utf8_lossy(&head_after.stdout),
        "the OTHER agent's tree must never move on a cross-tree write attempt"
    );
    std::fs::remove_dir_all(&home).ok();
}
