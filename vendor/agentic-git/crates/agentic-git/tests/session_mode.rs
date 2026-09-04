//! Session mode (`agentic-git run`) integration tests — agentic-git issue #1.
//!
//! Covers the issue's own Testing list (1–6) plus the Δ-series additions:
//! Δ1 invocation shapes + direct-CLI error, Δ2 concurrent key race, Δ3 v3
//! accept/reject matrix, Δ4 stale/cross-repo binding errors, hook
//! noninterference. Style matches `shim_phase2.rs`: `CARGO_BIN_EXE`,
//! `env_remove` BOTH the primary AND legacy env names so an ambient legacy
//! fleet env (or this suite's own outer shell) can't leak into a scenario
//! under test.

use std::path::{Path, PathBuf};
use std::process::Command;

fn tempdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "agentic-git-session-{tag}-{}-{}",
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

fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

/// Resolve a REAL git binary, skipping any shim installed under a
/// `.agend-terminal`/`.agentic-git` home. On a clean CI runner the first PATH
/// hit already IS real git; some dev sandboxes carry a legacy fleet shim
/// ahead of it on PATH, which would otherwise self-recurse once its own
/// `*_REAL_GIT` env is stripped for test isolation. Setting
/// `AGENTIC_GIT_REAL_GIT` explicitly to this resolved path (Priority 1 in
/// `resolve_real_git`) makes every test deterministic regardless of PATH
/// contents.
fn resolve_test_real_git() -> PathBuf {
    let path_env = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_env) {
        let candidate = dir.join(if cfg!(windows) { "git.exe" } else { "git" });
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

/// Real git, bypassing the shim twice over (primary + legacy names) — same
/// pattern `cleanup_init_pile_pre_push` uses in `main.rs`.
fn real_git(args: &[&str], cwd: &Path) -> std::process::Output {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("AGENTIC_GIT_BYPASS", "1")
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("run real git")
}

fn init_repo(dir: &Path) {
    assert!(real_git(&["init", "-q", "-b", "main", "."], dir)
        .status
        .success());
    assert!(real_git(&["config", "user.name", "Test"], dir).status.success());
    assert!(real_git(&["config", "user.email", "test@example.com"], dir)
        .status
        .success());
    std::fs::write(dir.join("README.md"), "hello\n").unwrap();
    assert!(real_git(&["add", "."], dir).status.success());
    assert!(real_git(&["commit", "-q", "-m", "init"], dir).status.success());
}

fn git_config_get(dir: &Path, key: &str) -> Option<String> {
    let out = real_git(&["config", "--get", key], dir);
    if !out.status.success() {
        return None;
    }
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

/// Invoke the compiled binary directly (argv[0] basename = "agentic-git") —
/// always CLI mode, never shim mode (Δ1). Every legacy/primary env name is
/// explicitly removed so this outer test-runner's own env can't leak in.
fn run_cli(repo: &Path, home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_agentic-git"))
        .args(args)
        .current_dir(repo)
        .env("AGENTIC_GIT_HOME", home)
        .env("AGENTIC_GIT_REAL_GIT", resolve_test_real_git())
        .env_remove("AGEND_HOME")
        .env_remove("AGENTIC_GIT_AGENT")
        .env_remove("AGEND_INSTANCE_NAME")
        .env_remove("AGENTIC_GIT_SHIM_DEPTH")
        .env_remove("AGEND_GIT_SHIM_DEPTH")
        .env_remove("AGENTIC_GIT_BYPASS")
        .env_remove("AGEND_GIT_BYPASS")
        .env_remove("AGEND_REAL_GIT")
        .output()
        .expect("run agentic-git cli")
}

fn worktree_path(home: &Path, agent: &str, branch: &str) -> PathBuf {
    home.join("worktrees").join(agent).join(branch)
}

// ── Issue Testing list, items 1–6 ───────────────────────────────────────

/// 1. `run --branch t -- sh -c 'git status'` → exit 0, cwd was the worktree.
#[test]
fn test1_run_spawns_in_worktree_with_shim_routing() {
    let root = tempdir("t1");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let home = root.join("home");

    let wt = worktree_path(&home, "agent-t1", "sess/t1");
    let out = run_cli(
        &repo,
        &home,
        &[
            "run",
            "--agent",
            "agent-t1",
            "--branch",
            "sess/t1",
            "--",
            "sh",
            "-c",
            "pwd && git status",
        ],
    );
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let expected_cwd = std::fs::canonicalize(&wt).unwrap_or(wt.clone());
    let printed_cwd = PathBuf::from(stdout.lines().next().unwrap_or_default().trim());
    let printed_cwd = std::fs::canonicalize(&printed_cwd).unwrap_or(printed_cwd);
    assert_eq!(printed_cwd, expected_cwd, "agent cwd must be the worktree");
    assert!(stdout.contains("sess/t1"), "git status must show the session branch: {stdout}");
    cleanup(&root);
}

/// 2. `run … -- sh -c 'git checkout main'` → child sees the deny (exit 1,
///    denied + guidance).
#[test]
fn test2_child_sees_deny_on_cross_branch_checkout() {
    let root = tempdir("t2");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let home = root.join("home");

    let out = run_cli(
        &repo,
        &home,
        &[
            "run",
            "--agent",
            "agent-t2",
            "--branch",
            "sess/t2",
            "--",
            "sh",
            "-c",
            "git checkout main",
        ],
    );
    assert_eq!(out.status.code(), Some(1), "the deny must propagate as the child's exit code");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("denied"), "must contain the shim's deny wording: {stderr}");
    assert!(
        stderr.contains("bypass with one of") || stderr.contains("AGENTIC_GIT_BYPASS"),
        "must include the guidance block: {stderr}"
    );
    cleanup(&root);
}

/// 3. Binding sidecar verifies with `integrity_core::verify`; tampered
///    binding → unbound deny.
#[test]
fn test3_binding_sidecar_verifies_and_tamper_is_unbound_deny() {
    let root = tempdir("t3");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let home = root.join("home");

    let out = run_cli(
        &repo,
        &home,
        &["run", "--agent", "agent-t3", "--branch", "sess/t3", "--", "true"],
    );
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));

    let dir = home.join("runtime").join("agent-t3");
    let content = std::fs::read_to_string(dir.join("binding.json")).expect("read binding");
    let sig = std::fs::read_to_string(dir.join("binding.json.sig")).expect("read sidecar");
    assert!(
        agentic_git_core::integrity_core::verify(&home, content.as_bytes(), &sig).is_ok(),
        "freshly written binding must verify"
    );

    // Tamper without re-signing.
    let tampered = content.replace("sess/t3", "sess/t3-EVIL");
    assert_ne!(tampered, content);
    std::fs::write(dir.join("binding.json"), &tampered).unwrap();
    assert!(
        agentic_git_core::integrity_core::verify(&home, tampered.as_bytes(), &sig).is_err(),
        "tampered binding must fail verify"
    );

    // Live proof: a mutating op through the (git-named) shim symlink is now
    // denied as unbound.
    let bin_git = home.join("bin").join("git");
    let wt = worktree_path(&home, "agent-t3", "sess/t3");
    let out2 = Command::new(&bin_git)
        .args(["commit", "--allow-empty", "-m", "must be denied"])
        .current_dir(&wt)
        .env("AGENTIC_GIT_HOME", &home)
        .env("AGENTIC_GIT_AGENT", "agent-t3")
        .env_remove("AGEND_HOME")
        .env_remove("AGEND_INSTANCE_NAME")
        .env_remove("AGENTIC_GIT_BYPASS")
        .env_remove("AGEND_GIT_BYPASS")
        .env_remove("AGENTIC_GIT_SHIM_DEPTH")
        .env_remove("AGEND_GIT_SHIM_DEPTH")
        .output()
        .expect("run shim via git symlink");
    assert_eq!(out2.status.code(), Some(1));
    let stderr2 = String::from_utf8_lossy(&out2.stderr);
    assert!(stderr2.contains("unbound"), "tampered binding must fail closed to unbound: {stderr2}");
    cleanup(&root);
}

/// 4. Commit inside session carries `Agentic-Agent` trailer (hooks wired).
#[test]
fn test4_commit_inside_session_carries_agentic_agent_trailer() {
    let root = tempdir("t4");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let home = root.join("home");

    let out = run_cli(
        &repo,
        &home,
        &[
            "run",
            "--agent",
            "agent-t4",
            "--branch",
            "sess/t4",
            "--",
            "sh",
            "-c",
            "git commit --allow-empty -m committed-in-session",
        ],
    );
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));

    let wt = worktree_path(&home, "agent-t4", "sess/t4");
    let log = real_git(&["log", "-1", "--format=%B"], &wt);
    let msg = String::from_utf8_lossy(&log.stdout);
    assert!(
        msg.contains("Agentic-Agent: agent-t4"),
        "commit must carry the Agentic-Agent trailer: {msg}"
    );
    cleanup(&root);
}

/// 5. Second `run` on the same branch (different identity) fails with git's
///    own worktree error (natural mutual exclusion, no lease machinery).
#[test]
fn test5_second_run_same_branch_different_agent_fails_with_gits_own_error() {
    let root = tempdir("t5");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let home = root.join("home");

    let out1 = run_cli(
        &repo,
        &home,
        &["run", "--agent", "agent-t5a", "--branch", "sess/shared", "--", "true"],
    );
    assert!(out1.status.success(), "stderr={}", String::from_utf8_lossy(&out1.stderr));

    let out2 = run_cli(
        &repo,
        &home,
        &["run", "--agent", "agent-t5b", "--branch", "sess/shared", "--", "true"],
    );
    assert_ne!(out2.status.code(), Some(0), "a second worktree for a checked-out branch must fail");
    let stderr2 = String::from_utf8_lossy(&out2.stderr).to_lowercase();
    assert!(
        stderr2.contains("already"),
        "git's own refusal should be surfaced verbatim: {stderr2}"
    );
    cleanup(&root);
}

/// 6. Key provisioned 0600 once; second run reuses it (byte-identical).
#[test]
fn test6_key_provisioned_once_0600_and_reused() {
    let root = tempdir("t6");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let home = root.join("home");

    let out1 = run_cli(
        &repo,
        &home,
        &["run", "--agent", "agent-t6a", "--branch", "sess/t6a", "--", "true"],
    );
    assert!(out1.status.success(), "stderr={}", String::from_utf8_lossy(&out1.stderr));

    let key_path = home.join(".config-integrity-key");
    let meta1 = std::fs::metadata(&key_path).expect("key must exist");
    assert_eq!(meta1.len(), 32, "key must be exactly 32 bytes");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            meta1.permissions().mode() & 0o777,
            0o600,
            "key must be 0600"
        );
    }
    let key_bytes1 = std::fs::read(&key_path).unwrap();

    let out2 = run_cli(
        &repo,
        &home,
        &["run", "--agent", "agent-t6b", "--branch", "sess/t6b", "--", "true"],
    );
    assert!(out2.status.success(), "stderr={}", String::from_utf8_lossy(&out2.stderr));
    let key_bytes2 = std::fs::read(&key_path).unwrap();
    assert_eq!(key_bytes1, key_bytes2, "second run must reuse the SAME key, not regenerate");
    cleanup(&root);
}

// ── Δ1: argv[0] invocation shapes + direct-CLI error ────────────────────

/// Bare `git` on PATH (a name lookup, not an absolute path) must still route
/// to shim mode — argv[0] as seen by the child is exactly the string "git"
/// regardless of how PATH resolved it.
#[test]
fn delta1_bare_git_name_on_path_is_shim_mode() {
    let root = tempdir("d1-bare");
    let fake_bin = root.join("fakebin");
    std::fs::create_dir_all(&fake_bin).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_agentic-git"), fake_bin.join("git")).unwrap();
    #[cfg(not(unix))]
    std::fs::copy(env!("CARGO_BIN_EXE_agentic-git"), fake_bin.join("git.exe")).unwrap();

    let real_git_path = resolve_test_real_git();
    let path_env = std::env::join_paths([fake_bin.clone(), real_git_path.parent().unwrap().to_path_buf()]).unwrap();

    let out = Command::new("git")
        .arg("--version")
        .env("PATH", &path_env)
        .env("AGENTIC_GIT_REAL_GIT", &real_git_path)
        .env_remove("AGENTIC_GIT_AGENT")
        .env_remove("AGEND_INSTANCE_NAME")
        .env_remove("AGENTIC_GIT_HOME")
        .env_remove("AGEND_HOME")
        .env_remove("AGENTIC_GIT_BYPASS")
        .env_remove("AGEND_GIT_BYPASS")
        .env_remove("AGEND_REAL_GIT")
        .output()
        .expect("run bare git via PATH");
    // Shim mode with no agent/home passes straight through to real git —
    // `--version` must succeed (CLI mode would instead exit(2), unknown
    // subcommand, since "--version" isn't "version").
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    cleanup(&root);
}

/// Absolute-path invocation of `<home>/bin/git` (exactly what `run` step 7
/// wires up) — must be shim mode.
#[test]
fn delta1_absolute_path_bin_git_is_shim_mode() {
    let root = tempdir("d1-abs");
    let bin = root.join("home").join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let bin_git = bin.join(if cfg!(windows) { "git.exe" } else { "git" });
    #[cfg(unix)]
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_agentic-git"), &bin_git).unwrap();
    #[cfg(not(unix))]
    std::fs::copy(env!("CARGO_BIN_EXE_agentic-git"), &bin_git).unwrap();

    let out = Command::new(&bin_git)
        .arg("--version")
        .env("AGENTIC_GIT_REAL_GIT", resolve_test_real_git())
        .env_remove("AGENTIC_GIT_AGENT")
        .env_remove("AGEND_INSTANCE_NAME")
        .env_remove("AGENTIC_GIT_HOME")
        .env_remove("AGEND_HOME")
        .env_remove("AGENTIC_GIT_BYPASS")
        .env_remove("AGEND_GIT_BYPASS")
        .env_remove("AGEND_REAL_GIT")
        .output()
        .expect("run absolute bin/git");
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    cleanup(&root);
}

/// CLI mode with an unknown subcommand → exit(2) with usage + the shim hint
/// (issue's own worked example: `agentic-git status`).
#[test]
fn delta1_cli_unknown_subcommand_exits_2_with_usage_hint() {
    let out = Command::new(env!("CARGO_BIN_EXE_agentic-git"))
        .arg("status")
        .env_remove("AGENTIC_GIT_AGENT")
        .env_remove("AGEND_INSTANCE_NAME")
        .env_remove("AGENTIC_GIT_HOME")
        .env_remove("AGEND_HOME")
        .output()
        .expect("run agentic-git status");
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown subcommand"), "{stderr}");
    assert!(stderr.contains("bin/git"), "must hint at the shim invocation path: {stderr}");
}

/// No subcommand at all is likewise a hard CLI error, not a silent shim.
#[test]
fn delta1_cli_no_subcommand_exits_2() {
    let out = Command::new(env!("CARGO_BIN_EXE_agentic-git"))
        .env_remove("AGENTIC_GIT_AGENT")
        .env_remove("AGEND_INSTANCE_NAME")
        .env_remove("AGENTIC_GIT_HOME")
        .env_remove("AGEND_HOME")
        .output()
        .expect("run agentic-git with no args");
    assert_eq!(out.status.code(), Some(2));
}

// ── Δ2: concurrent first-run key race ───────────────────────────────────

/// Two racing first-`run`s against the SAME home: both must succeed, the key
/// must end up exactly 32 bytes, and BOTH resulting bindings must verify
/// against the ONE surviving key (no partial-write, no split-brain key).
#[test]
fn delta2_concurrent_first_runs_race_key_both_bindings_verify() {
    let root = tempdir("d2-race");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let home = root.join("home");

    let (repo1, home1) = (repo.clone(), home.clone());
    let (repo2, home2) = (repo.clone(), home.clone());
    let t1 = std::thread::spawn(move || {
        run_cli(
            &repo1,
            &home1,
            &["run", "--agent", "race-a", "--branch", "sess/race-a", "--", "true"],
        )
    });
    let t2 = std::thread::spawn(move || {
        run_cli(
            &repo2,
            &home2,
            &["run", "--agent", "race-b", "--branch", "sess/race-b", "--", "true"],
        )
    });
    let out1 = t1.join().expect("thread 1");
    let out2 = t2.join().expect("thread 2");
    assert!(out1.status.success(), "stderr={}", String::from_utf8_lossy(&out1.stderr));
    assert!(out2.status.success(), "stderr={}", String::from_utf8_lossy(&out2.stderr));

    let key_path = home.join(".config-integrity-key");
    let meta = std::fs::metadata(&key_path).expect("key must exist after the race");
    assert_eq!(meta.len(), 32);

    for agent in ["race-a", "race-b"] {
        let dir = home.join("runtime").join(agent);
        let content = std::fs::read_to_string(dir.join("binding.json")).unwrap();
        let sig = std::fs::read_to_string(dir.join("binding.json.sig")).unwrap();
        assert!(
            agentic_git_core::integrity_core::verify(&home, content.as_bytes(), &sig).is_ok(),
            "{agent}'s binding must verify against the one surviving key"
        );
    }
    cleanup(&root);
}

// ── Δ3 v3: agent-name accept/reject matrix (CLI-level) ──────────────────

#[test]
fn delta3_run_rejects_invalid_agent_names_exit_2() {
    let root = tempdir("d3-bad");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let home = root.join("home");

    for (i, bad) in ["CON", "con", "Nul.log", "Agent", "agent.", "com1", "a/b", "../x", ".hidden"]
        .iter()
        .enumerate()
    {
        let branch = format!("sess/bad{i}");
        let out = run_cli(
            &repo,
            &home,
            &["run", "--agent", bad, "--branch", &branch, "--", "true"],
        );
        assert_eq!(
            out.status.code(),
            Some(2),
            "agent {bad:?} must be rejected; stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    cleanup(&root);
}

#[test]
fn delta3_run_accepts_valid_agent_name() {
    let root = tempdir("d3-good");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let home = root.join("home");

    let out = run_cli(
        &repo,
        &home,
        &["run", "--agent", "valid-agent.1", "--branch", "sess/good", "--", "true"],
    );
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    cleanup(&root);
}

// ── Δ4: stale / cross-repo binding hard errors ──────────────────────────

/// Partial cleanup: worktree manually removed, binding left behind → hard
/// error naming the stale `runtime/<agent>` path, never a silent rebind.
#[test]
fn delta4_stale_binding_after_manual_worktree_removal_is_hard_error() {
    let root = tempdir("d4-stale");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let home = root.join("home");

    let out1 = run_cli(
        &repo,
        &home,
        &["run", "--agent", "agent-stale", "--branch", "sess/stale", "--", "true"],
    );
    assert!(out1.status.success(), "stderr={}", String::from_utf8_lossy(&out1.stderr));

    let wt = worktree_path(&home, "agent-stale", "sess/stale");
    std::fs::remove_dir_all(&wt).expect("simulate manual worktree removal");

    let out2 = run_cli(
        &repo,
        &home,
        &["run", "--agent", "agent-stale", "--branch", "sess/stale", "--", "true"],
    );
    assert_ne!(out2.status.code(), Some(0));
    let stderr2 = String::from_utf8_lossy(&out2.stderr);
    assert!(
        stderr2.contains("runtime") && stderr2.contains("agent-stale"),
        "must name the stale runtime/<agent> path: {stderr2}"
    );
    cleanup(&root);
}

/// Cross-repo stale binding: same agent name + same branch string reused
/// against a DIFFERENT repo → hard error (never silently rebind across
/// repos).
#[test]
fn delta4_cross_repo_stale_binding_is_hard_error() {
    let root = tempdir("d4-cross");
    let repo_a = root.join("repo-a");
    std::fs::create_dir_all(&repo_a).unwrap();
    init_repo(&repo_a);
    let repo_b = root.join("repo-b");
    std::fs::create_dir_all(&repo_b).unwrap();
    init_repo(&repo_b);
    let home = root.join("home");

    let out1 = run_cli(
        &repo_a,
        &home,
        &["run", "--agent", "agent-cross", "--branch", "sess/cross", "--", "true"],
    );
    assert!(out1.status.success(), "stderr={}", String::from_utf8_lossy(&out1.stderr));

    let out2 = run_cli(
        &repo_b,
        &home,
        &["run", "--agent", "agent-cross", "--branch", "sess/cross", "--", "true"],
    );
    assert_ne!(out2.status.code(), Some(0), "must not silently rebind across repos");
    let stderr2 = String::from_utf8_lossy(&out2.stderr);
    assert!(
        stderr2.contains("source_repo"),
        "must name the source_repo mismatch: {stderr2}"
    );
    cleanup(&root);
}

/// Same agent + same branch + same repo, worktree intact → REUSE (no error,
/// no re-provisioning).
#[test]
fn delta4_matching_rerun_reuses_without_error() {
    let root = tempdir("d4-reuse");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let home = root.join("home");

    let out1 = run_cli(
        &repo,
        &home,
        &["run", "--agent", "agent-reuse", "--branch", "sess/reuse", "--", "true"],
    );
    assert!(out1.status.success(), "stderr={}", String::from_utf8_lossy(&out1.stderr));

    let out2 = run_cli(
        &repo,
        &home,
        &["run", "--agent", "agent-reuse", "--branch", "sess/reuse", "--", "true"],
    );
    assert!(
        out2.status.success(),
        "identical re-run must reuse cleanly: stderr={}",
        String::from_utf8_lossy(&out2.stderr)
    );
    cleanup(&root);
}

// ── Hook noninterference ────────────────────────────────────────────────

/// The source repo's OWN checkout must keep its own hooks — only
/// `extensions.worktreeConfig` (repo-wide) may land in the shared config;
/// `core.hooksPath` must be `--worktree`-scoped to the session's worktree
/// only.
#[test]
fn hooks_are_worktree_scoped_source_checkout_keeps_its_own_hooks() {
    let root = tempdir("hooks");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let home = root.join("home");

    let out = run_cli(
        &repo,
        &home,
        &["run", "--agent", "hook-agent", "--branch", "sess/hooks", "--", "true"],
    );
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));

    assert!(
        git_config_get(&repo, "core.hooksPath").is_none(),
        "the source repo's own checkout must NOT get core.hooksPath"
    );
    assert_eq!(
        git_config_get(&repo, "extensions.worktreeConfig").as_deref(),
        Some("true")
    );

    let wt = worktree_path(&home, "hook-agent", "sess/hooks");
    assert!(
        git_config_get(&wt, "core.hooksPath").is_some(),
        "the SESSION worktree must get core.hooksPath"
    );
    cleanup(&root);
}

// ── Δ2 stress: many concurrent first-runs must all provision ────────────
/// A harder version of the Δ2 race: many first-`run`s hammer the SAME repo +
/// home at once. Provisioning is not concurrency-safe on its own — git's
/// `worktree add` AND the `config --worktree` hooks wiring both read the shared
/// worktree list and fatal on a sibling's half-written admin metadata
/// (`failed to read .git/worktrees/.../commondir`); the per-repo provisioning
/// lock serializes them. All must succeed, the key stays exactly 32 bytes, and
/// every binding must verify against the one surviving key.
#[test]
fn concurrent_worktree_provision_stress_all_succeed() {
    const N: usize = 12;
    let root = tempdir("wt-stress");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let home = root.join("home");

    let handles: Vec<_> = (0..N)
        .map(|i| {
            let repo = repo.clone();
            let home = home.clone();
            std::thread::spawn(move || {
                let agent = format!("stress-{i}");
                let branch = format!("sess/stress-{i}");
                let out = run_cli(
                    &repo,
                    &home,
                    &["run", "--agent", agent.as_str(), "--branch", branch.as_str(), "--", "true"],
                );
                (agent, out)
            })
        })
        .collect();
    for h in handles {
        let (agent, out) = h.join().expect("thread");
        assert!(
            out.status.success(),
            "{agent} failed to provision under concurrency: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let key = std::fs::metadata(home.join(".config-integrity-key")).expect("key must exist");
    assert_eq!(key.len(), 32, "one 32-byte key survives the race");
    for i in 0..N {
        let agent = format!("stress-{i}");
        let dir = home.join("runtime").join(&agent);
        let content = std::fs::read_to_string(dir.join("binding.json")).unwrap();
        let sig = std::fs::read_to_string(dir.join("binding.json.sig")).unwrap();
        assert!(
            agentic_git_core::integrity_core::verify(&home, content.as_bytes(), &sig).is_ok(),
            "{agent}'s binding must verify against the surviving key"
        );
    }
    cleanup(&root);
}
