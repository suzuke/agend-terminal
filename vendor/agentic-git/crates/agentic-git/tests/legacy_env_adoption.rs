//! Δ5 / INVARIANT clause 5 (upgraded from "acceptance" to a required CI job):
//! the zero-daemon-change adoption guarantee. A legacy agend-terminal fleet
//! sets ONLY `AGEND_*` env — never any `AGENTIC_GIT_*` name — and must see
//! byte-identical shim behavior. This suite spawns the shim with EVERY
//! `AGENTIC_GIT_*` variable explicitly removed (never merely "not set" —
//! Δ5's own clarification: ambient CI images/caches can leak env) and only
//! legacy names present, then exercises representative route / deny /
//! bypass / snapshot cases (the last extended for issue #4's recovery
//! layer, which carries its own legacy-twin kill switch).
//!
//! The `.github/workflows/ci.yml` `legacy-env-adoption` job additionally
//! count-asserts (via `--nocapture` + a grep on the marker line this test
//! prints) that all `EXPECTED_CASES` actually ran — an accidentally-skipped
//! case (e.g. an early `return` swallowed by a change elsewhere) must not
//! silently pass as green.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// route (bound agent's `status` chdir-passes into the worktree), deny
/// (bound agent's cross-branch checkout is denied), bypass (the SAME deny,
/// but with the LEGACY `AGEND_GIT_BYPASS` name, passes through), snapshot
/// (issue #4: a destructive op on a dirty bound worktree creates a
/// recovery-layer snapshot ref with ONLY the legacy `AGEND_GIT_SNAPSHOTS`
/// name set).
const EXPECTED_CASES: u32 = 4;

fn tempdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "agentic-git-legacy-adopt-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    dir
}

/// Setup-only real-git helper. Not part of the env-purity assertion (that's
/// only about how the SHIM UNDER TEST is invoked, below) — but still
/// resolves past this dev sandbox's own legacy shim on PATH the same way
/// `session_mode.rs` does, so the suite is reproducible outside CI too.
fn resolve_setup_real_git() -> PathBuf {
    let path_env = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_env) {
        let candidate = dir.join("git");
        if candidate.exists() {
            let s = candidate.to_string_lossy();
            if s.contains(".agend-terminal") || s.contains(".agentic-git") {
                continue;
            }
            return candidate;
        }
    }
    panic!("no real (non-shim) git found on PATH");
}

fn setup_git(real_git: &Path, args: &[&str], cwd: &Path) {
    let out = Command::new(real_git)
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run real git for setup");
    assert!(
        out.status.success(),
        "setup git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn write_binding(home: &Path, agent: &str, branch: &str, worktree: &Path) {
    std::fs::create_dir_all(home).unwrap();
    std::fs::write(home.join(".config-integrity-key"), [9u8; 32]).unwrap();
    let dir = home.join("runtime").join(agent);
    std::fs::create_dir_all(&dir).unwrap();
    let body = serde_json::json!({
        "version": 1,
        "agent": agent,
        "task_id": "legacy-adoption-task",
        "branch": branch,
        "issued_at": "2026-01-01T00:00:00Z",
        "worktree": worktree.to_string_lossy(),
        "source_repo": "irrelevant-for-this-suite",
    })
    .to_string();
    std::fs::write(dir.join("binding.json"), &body).unwrap();
    let sig = agentic_git_core::integrity_core::sign(home, body.as_bytes());
    std::fs::write(dir.join("binding.json.sig"), sig).unwrap();
}

/// Invoke the shim with argv[0] forced to "git" (Δ1) and — this is the
/// entire point of the suite — ONLY legacy `AGEND_*` names in its env. Every
/// `AGENTIC_GIT_*` primary name is explicitly `env_remove`d, never merely
/// absent, per Δ5's clarification.
fn run_shim_legacy_only(
    cwd: &Path,
    home: &Path,
    agent: &str,
    real_git: &Path,
    bypass: bool,
    args: &[&str],
) -> std::process::Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_agentic-git"));
    c.arg0("git")
        .args(args)
        .current_dir(cwd)
        .env("AGEND_HOME", home)
        .env("AGEND_INSTANCE_NAME", agent)
        .env("AGEND_REAL_GIT", real_git)
        .env_remove("AGENTIC_GIT_HOME")
        .env_remove("AGENTIC_GIT_AGENT")
        .env_remove("AGENTIC_GIT_REAL_GIT")
        .env_remove("AGENTIC_GIT_BYPASS")
        .env_remove("AGENTIC_GIT_BYPASS_AGENT")
        .env_remove("AGENTIC_GIT_BYPASS_UNTIL")
        .env_remove("AGENTIC_GIT_SHIM_DEPTH")
        .env_remove("AGENTIC_GIT_ALLOW_CANONICAL_MUTATE")
        .env_remove("AGENTIC_GIT_SNAPSHOTS")
        .env_remove("AGEND_GIT_SHIM_DEPTH")
        .env_remove("AGEND_GIT_BYPASS")
        .env_remove("AGEND_GIT_BYPASS_AGENT")
        .env_remove("AGEND_GIT_BYPASS_UNTIL")
        .env_remove("AGEND_GIT_ALLOW_CANONICAL_MUTATE")
        .env_remove("AGEND_GIT_SNAPSHOTS");
    if bypass {
        c.env("AGEND_GIT_BYPASS", "1");
    }
    c.output().expect("run shim (legacy-only env)")
}

/// #4 Δc/Δf: same legacy-only env purity as `run_shim_legacy_only`, PLUS the
/// legacy `AGEND_GIT_SNAPSHOTS=1` kill switch — proving the recovery layer
/// is reachable via the legacy name alone, same zero-daemon-change contract
/// every other var in this suite already follows.
fn run_shim_legacy_only_snapshots_on(
    cwd: &Path,
    home: &Path,
    agent: &str,
    real_git: &Path,
    args: &[&str],
) -> std::process::Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_agentic-git"));
    c.arg0("git")
        .args(args)
        .current_dir(cwd)
        .env("AGEND_HOME", home)
        .env("AGEND_INSTANCE_NAME", agent)
        .env("AGEND_REAL_GIT", real_git)
        .env("AGEND_GIT_SNAPSHOTS", "1")
        .env_remove("AGENTIC_GIT_HOME")
        .env_remove("AGENTIC_GIT_AGENT")
        .env_remove("AGENTIC_GIT_REAL_GIT")
        .env_remove("AGENTIC_GIT_BYPASS")
        .env_remove("AGENTIC_GIT_BYPASS_AGENT")
        .env_remove("AGENTIC_GIT_BYPASS_UNTIL")
        .env_remove("AGENTIC_GIT_SHIM_DEPTH")
        .env_remove("AGENTIC_GIT_ALLOW_CANONICAL_MUTATE")
        .env_remove("AGENTIC_GIT_SNAPSHOTS")
        .env_remove("AGEND_GIT_SHIM_DEPTH")
        .env_remove("AGEND_GIT_BYPASS")
        .env_remove("AGEND_GIT_BYPASS_AGENT")
        .env_remove("AGEND_GIT_BYPASS_UNTIL")
        .env_remove("AGEND_GIT_ALLOW_CANONICAL_MUTATE");
    c.output()
        .expect("run shim (legacy-only env, snapshots on)")
}

/// Route / deny / bypass, exercised with ONLY legacy `AGEND_*` env set.
/// Prints a machine-checkable marker line the CI job greps for, so a suite
/// that silently ran zero (or fewer than `EXPECTED_CASES`) cases — e.g. an
/// early return introduced by an unrelated future change — fails the job
/// instead of passing as green.
#[test]
fn legacy_only_env_exercises_route_deny_bypass() {
    let root = tempdir("suite");
    let real_git = resolve_setup_real_git();

    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    setup_git(&real_git, &["init", "-q", "-b", "main", "."], &repo);
    setup_git(&real_git, &["config", "user.name", "Test"], &repo);
    setup_git(&real_git, &["config", "user.email", "t@example.com"], &repo);
    std::fs::write(repo.join("README.md"), "hi\n").unwrap();
    setup_git(&real_git, &["add", "."], &repo);
    setup_git(&real_git, &["commit", "-q", "-m", "init"], &repo);

    let worktree = root.join("wt");
    setup_git(
        &real_git,
        &[
            "worktree",
            "add",
            worktree.to_str().unwrap(),
            "-b",
            "agent/legacy",
            "main",
        ],
        &repo,
    );

    let home = root.join("home");
    write_binding(&home, "legacy-agent", "agent/legacy", &worktree);

    let mut cases_run: u32 = 0;

    // ── ROUTE: bound agent's `status` chdir-passes into the worktree ──
    let route = run_shim_legacy_only(&repo, &home, "legacy-agent", &real_git, false, &["status"]);
    assert!(
        route.status.success(),
        "legacy-only route case must pass through cleanly: stderr={}",
        String::from_utf8_lossy(&route.stderr)
    );
    let stdout = String::from_utf8_lossy(&route.stdout);
    assert!(
        stdout.contains("agent/legacy"),
        "status output must reflect the bound worktree's branch, not the canonical repo: {stdout}"
    );
    cases_run += 1;

    // ── DENY: bound agent's cross-branch checkout to `main` is denied ──
    let deny = run_shim_legacy_only(
        &repo,
        &home,
        "legacy-agent",
        &real_git,
        false,
        &["checkout", "main"],
    );
    assert_eq!(deny.status.code(), Some(1), "cross-branch checkout must be denied");
    assert!(
        String::from_utf8_lossy(&deny.stderr).contains("denied"),
        "deny must fire under legacy-only env: {}",
        String::from_utf8_lossy(&deny.stderr)
    );
    cases_run += 1;

    // ── BYPASS: the SAME op, with the LEGACY `AGEND_GIT_BYPASS` name ──
    let bypass = run_shim_legacy_only(
        &repo,
        &home,
        "legacy-agent",
        &real_git,
        true,
        &["checkout", "main"],
    );
    // Assert on the deny's SIGNATURE ("cross-branch — assigned to …"), not the
    // loose word "denied": the #2158 bypass audit dumps process ancestry to
    // stderr, so a parent command containing "denied" would false-trip a
    // substring check. The cross-branch deny message cannot appear when the op
    // is bypassed.
    assert!(
        !String::from_utf8_lossy(&bypass.stderr).contains("cross-branch"),
        "AGEND_GIT_BYPASS=1 (legacy name) must bypass the deny: {}",
        String::from_utf8_lossy(&bypass.stderr)
    );
    cases_run += 1;

    // ── SNAPSHOT: destructive op on a dirty bound worktree creates a
    // recovery-layer snapshot ref, with ONLY the LEGACY `AGEND_GIT_SNAPSHOTS`
    // name set (issue #4 Δc/Δf) ──
    std::fs::write(worktree.join("dirty.txt"), "uncommitted\n").unwrap();
    let snap = run_shim_legacy_only_snapshots_on(
        &repo,
        &home,
        "legacy-agent",
        &real_git,
        &["reset", "--hard"],
    );
    assert!(
        snap.status.success(),
        "the destructive op must still succeed under legacy-only snapshot env: stderr={}",
        String::from_utf8_lossy(&snap.stderr)
    );
    let refs_out = Command::new(&real_git)
        .args(["for-each-ref", "refs/agentic-git/snapshots/"])
        .current_dir(&repo)
        .env("AGENTIC_GIT_BYPASS", "1")
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("list snapshot refs");
    assert!(
        !refs_out.stdout.is_empty(),
        "a snapshot ref must exist when AGEND_GIT_SNAPSHOTS=1 (legacy name only) is set: {}",
        String::from_utf8_lossy(&refs_out.stderr)
    );
    cases_run += 1;

    assert_eq!(
        cases_run, EXPECTED_CASES,
        "an accidentally-skipped case must fail the suite, not pass as green"
    );
    // Machine-checkable marker for the CI job's count-assert (Δ5 clarification).
    println!("LEGACY-ADOPTION-CASES-RUN: {cases_run}");

    let _ = std::fs::remove_dir_all(&root);
}
