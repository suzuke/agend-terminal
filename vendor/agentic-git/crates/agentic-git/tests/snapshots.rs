//! P2 recovery layer (agentic-git issue #4) — full shim-level acceptance
//! tests (Δf list): spawns the COMPILED shim binary (argv[0] forced to
//! "git", same pattern as `legacy_env_adoption.rs`/`session_mode.rs`), so
//! these exercise the real dispatch path (classify → snapshot hook → exec),
//! not just the pure `snapshot` module functions (see `src/snapshot/tests.rs`
//! for those).

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn tempdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "agentic-git-snapshots-it-{tag}-{}-{}",
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

/// Same convention as `legacy_env_adoption.rs`/`session_mode.rs`: resolve a
/// REAL git, skipping any legacy fleet shim that might be ahead on this dev
/// sandbox's PATH.
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

fn setup_git(real_git: &Path, args: &[&str], cwd: &Path) -> std::process::Output {
    let out = Command::new(real_git)
        .args(args)
        .current_dir(cwd)
        .env("AGENTIC_GIT_BYPASS", "1")
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("run real git for setup");
    assert!(
        out.status.success(),
        "setup git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn init_repo(real_git: &Path, dir: &Path) {
    setup_git(real_git, &["init", "-q", "-b", "main", "."], dir);
    setup_git(real_git, &["config", "user.name", "Test"], dir);
    setup_git(real_git, &["config", "user.email", "t@example.com"], dir);
    std::fs::write(dir.join("README.md"), "hello\n").unwrap();
    setup_git(real_git, &["add", "."], dir);
    setup_git(real_git, &["commit", "-q", "-m", "init"], dir);
}

fn write_binding(home: &Path, agent: &str, branch: &str, worktree: &Path) {
    std::fs::create_dir_all(home).unwrap();
    std::fs::write(home.join(".config-integrity-key"), [7u8; 32]).unwrap();
    let dir = home.join("runtime").join(agent);
    std::fs::create_dir_all(&dir).unwrap();
    let body = serde_json::json!({
        "version": 1,
        "agent": agent,
        "task_id": format!("{agent}-task"),
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

fn worktree_of(root: &Path, real_git: &Path, repo: &Path, branch: &str) -> PathBuf {
    let wt = root.join("wt");
    setup_git(
        real_git,
        &["worktree", "add", wt.to_str().unwrap(), "-b", branch, "main"],
        repo,
    );
    wt
}

/// A handful of pre-existing (unrelated to issue #4) shim code paths spawn
/// a BARE `"git"` resolved via PATH rather than through `resolve_real_git()`
/// (e.g. `push_range_files`, used by the trust-root push denylist that runs
/// before our own push guard). On a dev sandbox that also has some OTHER
/// legacy git-wrapping shim earlier on PATH (common in this kind of
/// fleet-managed environment), that bare spawn can recurse into the WRONG
/// shim entirely. Strip any PATH entry that looks like a git-shim home
/// directory before handing PATH to the child, so every bare `"git"` spawn
/// — ours or pre-existing — resolves to the real binary. Test-harness-only;
/// does not touch production code.
fn sanitized_path(real_git: &Path) -> std::ffi::OsString {
    let path_env = std::env::var_os("PATH").unwrap_or_default();
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(parent) = real_git.parent() {
        dirs.push(parent.to_path_buf());
    }
    for p in std::env::split_paths(&path_env) {
        let s = p.to_string_lossy();
        if s.contains(".agend-terminal") || s.contains(".agentic-git") {
            continue;
        }
        dirs.push(p);
    }
    std::env::join_paths(dirs).unwrap_or(path_env)
}

/// Invoke the shim (argv[0] forced to "git") with every legacy/primary env
/// name explicitly removed except what the caller supplies via `extra_env` —
/// so an ambient ENV in THIS test runner's own shell can never leak in.
fn run_shim(
    cwd: &Path,
    home: &Path,
    agent: &str,
    real_git: &Path,
    extra_env: &[(&str, &str)],
    args: &[&str],
) -> std::process::Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_agentic-git"));
    c.arg0("git")
        .args(args)
        .current_dir(cwd)
        .env("AGENTIC_GIT_HOME", home)
        .env("AGENTIC_GIT_AGENT", agent)
        .env("AGENTIC_GIT_REAL_GIT", real_git)
        .env("PATH", sanitized_path(real_git))
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
        .env_remove("GIT_AUTHOR_DATE")
        .env_remove("GIT_COMMITTER_DATE");
    for (k, v) in extra_env {
        c.env(k, v);
    }
    c.output().expect("run shim")
}

fn snapshot_refs(real_git: &Path, repo: &Path) -> Vec<String> {
    let out = Command::new(real_git)
        .args(["for-each-ref", "--format=%(refname)", "refs/agentic-git/snapshots/"])
        .current_dir(repo)
        .env("AGENTIC_GIT_BYPASS", "1")
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("for-each-ref");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

fn committer_epoch(real_git: &Path, repo: &Path, refname: &str) -> i64 {
    let out = Command::new(real_git)
        .args(["log", "-1", "--format=%ct", refname])
        .current_dir(repo)
        .env("AGENTIC_GIT_BYPASS", "1")
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("log");
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}

// ── 1. THE test: tracked + untracked, `reset --hard`, byte-recoverable ──

#[test]
fn the_test_reset_hard_is_byte_recoverable() {
    let root = tempdir("the-test");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let real_git = resolve_setup_real_git();
    init_repo(&real_git, &repo);
    let wt = worktree_of(&root, &real_git, &repo, "agent/thetest");
    let home = root.join("home");
    write_binding(&home, "agent-tt", "agent/thetest", &wt);

    // Tracked change + untracked file.
    std::fs::write(wt.join("README.md"), "TRACKED CHANGE\n").unwrap();
    std::fs::write(wt.join("untracked.txt"), "UNTRACKED CONTENT\n").unwrap();

    let out = run_shim(
        &repo,
        &home,
        "agent-tt",
        &real_git,
        &[("AGENTIC_GIT_SNAPSHOTS", "1")],
        &["reset", "--hard"],
    );
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));

    let refs = snapshot_refs(&real_git, &repo);
    assert_eq!(refs.len(), 1, "exactly one snapshot ref must exist: {refs:?}");

    // Post-reset: the tracked change is gone (this is what `reset --hard`
    // itself destroys — it never touches untracked files; that's `clean`'s
    // job, covered by the `clean -fd` test). Simulate the untracked file
    // ALSO being lost (the agent's next mistake, or a `clean` in the same
    // breath) so this test proves what the issue's own "THE test" asks for:
    // both categories are byte-recoverable from the ONE ref the snapshot
    // mechanism captured before the destructive op ran.
    assert_eq!(std::fs::read_to_string(wt.join("README.md")).unwrap(), "hello\n");
    std::fs::remove_file(wt.join("untracked.txt")).unwrap();

    // Byte-for-byte recovery via the DOCUMENTED manual path.
    setup_git(&real_git, &["checkout", &refs[0], "--", "."], &wt);
    assert_eq!(
        std::fs::read_to_string(wt.join("README.md")).unwrap(),
        "TRACKED CHANGE\n"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("untracked.txt")).unwrap(),
        "UNTRACKED CONTENT\n"
    );
    cleanup(&root);
}

// ── 2. `clean -fd` variant (untracked dir) ──────────────────────────────

#[test]
fn clean_fd_variant_recovers_untracked_dir() {
    let root = tempdir("clean-fd");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let real_git = resolve_setup_real_git();
    init_repo(&real_git, &repo);
    let wt = worktree_of(&root, &real_git, &repo, "agent/cleanfd");
    let home = root.join("home");
    write_binding(&home, "agent-cf", "agent/cleanfd", &wt);

    std::fs::create_dir_all(wt.join("untracked_dir")).unwrap();
    std::fs::write(wt.join("untracked_dir").join("f.txt"), "in a dir\n").unwrap();

    let out = run_shim(
        &repo,
        &home,
        "agent-cf",
        &real_git,
        &[("AGENTIC_GIT_SNAPSHOTS", "1")],
        &["clean", "-fd"],
    );
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    assert!(!wt.join("untracked_dir").exists(), "clean -fd must have removed the dir");

    let refs = snapshot_refs(&real_git, &repo);
    assert_eq!(refs.len(), 1, "{refs:?}");
    setup_git(&real_git, &["checkout", &refs[0], "--", "."], &wt);
    assert_eq!(
        std::fs::read_to_string(wt.join("untracked_dir").join("f.txt")).unwrap(),
        "in a dir\n"
    );
    cleanup(&root);
}

// ── 3. Skip-when-clean: clean tree → destructive op → NO ref ────────────

#[test]
fn skip_when_clean_creates_no_ref() {
    let root = tempdir("skip-clean");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let real_git = resolve_setup_real_git();
    init_repo(&real_git, &repo);
    let wt = worktree_of(&root, &real_git, &repo, "agent/skipclean");
    let home = root.join("home");
    write_binding(&home, "agent-sc", "agent/skipclean", &wt);

    // wt is freshly checked out — clean.
    let out = run_shim(
        &repo,
        &home,
        "agent-sc",
        &real_git,
        &[("AGENTIC_GIT_SNAPSHOTS", "1")],
        &["reset", "--hard"],
    );
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    assert!(
        snapshot_refs(&real_git, &repo).is_empty(),
        "a clean tree must create NO snapshot ref"
    );
    cleanup(&root);
}

// ── 4. Fail-open: sabotaged snapshot infra must not block the op ───────

#[test]
fn fail_open_sabotaged_snapshot_never_blocks_the_op() {
    let root = tempdir("fail-open");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let real_git = resolve_setup_real_git();
    init_repo(&real_git, &repo);
    let wt = worktree_of(&root, &real_git, &repo, "agent/failopen");
    let home = root.join("home");
    write_binding(&home, "agent-fo", "agent/failopen", &wt);

    std::fs::write(wt.join("README.md"), "about to be reset\n").unwrap();

    // Sabotage: TMPDIR points at a nonexistent directory, so the temp
    // GIT_INDEX_FILE the snapshot mechanism writes to can never be created —
    // `git add -A` into it fails, and `create_snapshot` returns Err.
    let bogus_tmp = root.join("does-not-exist");
    let out = run_shim(
        &repo,
        &home,
        "agent-fo",
        &real_git,
        &[
            ("AGENTIC_GIT_SNAPSHOTS", "1"),
            ("TMPDIR", bogus_tmp.to_str().unwrap()),
        ],
        &["reset", "--hard"],
    );
    assert!(
        out.status.success(),
        "the destructive op must STILL run even though the snapshot failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("pre-op snapshot FAILED"),
        "must print the loud warning: {stderr}"
    );
    assert!(
        snapshot_refs(&real_git, &repo).is_empty(),
        "a failed snapshot must not leave a ref behind"
    );
    let events = std::fs::read_to_string(home.join("fleet_events.jsonl")).unwrap_or_default();
    assert!(
        events.contains("\"snapshot_failed\""),
        "must log a snapshot_failed fleet event: {events}"
    );
    assert!(
        events.contains("\"disposition\":\"warn\""),
        "snapshot_failed must be advisory (warn), not terminal: {events}"
    );
    cleanup(&root);
}

// ── 5. HEAD-less repo, exercised through the full shim path ─────────────

#[test]
fn head_less_repo_snapshot_through_shim() {
    let root = tempdir("headless-shim");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let real_git = resolve_setup_real_git();
    // No commit at all — HEAD is unborn. No worktree machinery needed (a
    // worktree can't be added before the first commit), so bind straight at
    // the repo itself.
    setup_git(&real_git, &["init", "-q", "-b", "main", "."], &repo);
    setup_git(&real_git, &["config", "user.name", "Test"], &repo);
    setup_git(&real_git, &["config", "user.email", "t@example.com"], &repo);
    std::fs::write(repo.join("a.txt"), "not yet committed\n").unwrap();
    let home = root.join("home");
    write_binding(&home, "agent-hl", "main", &repo);

    let out = run_shim(
        &repo,
        &home,
        "agent-hl",
        &real_git,
        &[("AGENTIC_GIT_SNAPSHOTS", "1")],
        &["clean", "-f"],
    );
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    let refs = snapshot_refs(&real_git, &repo);
    assert_eq!(refs.len(), 1, "{refs:?}");
    let parents = Command::new(&real_git)
        .args(["rev-list", "--parents", "-n1", &refs[0]])
        .current_dir(&repo)
        .env("AGENTIC_GIT_BYPASS", "1")
        .output()
        .unwrap();
    let sha_only = Command::new(&real_git)
        .args(["rev-parse", &refs[0]])
        .current_dir(&repo)
        .env("AGENTIC_GIT_BYPASS", "1")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&parents.stdout).trim(),
        String::from_utf8_lossy(&sha_only.stdout).trim(),
        "HEAD-less snapshot must have no parent"
    );
    cleanup(&root);
}

// ── 6. Snapshot push denied — representative spelling matrix ───────────

#[test]
fn snapshot_push_denied_spelling_matrix() {
    let root = tempdir("push-deny");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let real_git = resolve_setup_real_git();
    init_repo(&real_git, &repo);
    let wt = worktree_of(&root, &real_git, &repo, "agent/pushdeny");
    let home = root.join("home");
    write_binding(&home, "agent-pd", "agent/pushdeny", &wt);

    // "origin" = the repo itself (a real remote, fetchable/pushable path).
    setup_git(&real_git, &["remote", "add", "origin", repo.to_str().unwrap()], &wt);
    setup_git(&real_git, &["fetch", "-q", "origin"], &wt);
    // Allow pushing into a non-bare repo's checked-out branch for this test.
    setup_git(&real_git, &["config", "receive.denyCurrentBranch", "updateInstead"], &repo);

    // Create a real snapshot ref by running an ordinary destructive op first.
    std::fs::write(wt.join("README.md"), "dirty for snapshot\n").unwrap();
    let snap_out = run_shim(
        &repo,
        &home,
        "agent-pd",
        &real_git,
        &[("AGENTIC_GIT_SNAPSHOTS", "1")],
        &["reset", "--hard"],
    );
    assert!(snap_out.status.success());
    let refs = snapshot_refs(&real_git, &wt);
    assert_eq!(refs.len(), 1, "{refs:?}");
    let snap_ref = &refs[0];
    let short_ref = snap_ref.strip_prefix("refs/").unwrap();
    setup_git(&real_git, &["branch", "laundered", snap_ref], &wt);

    let cases: &[(&str, String)] = &[
        ("full ref", format!("{snap_ref}:refs/heads/out1")),
        ("abbreviated", format!("{short_ref}:refs/heads/out2")),
        ("wildcard", "refs/agentic-git/snapshots/*:refs/agentic-git/snapshots/*".to_string()),
        ("rev-suffix ^{}", format!("{snap_ref}^{{}}:refs/heads/out3")),
        ("rev-suffix ~0", format!("{snap_ref}~0:refs/heads/out4")),
        ("laundered branch", "laundered:refs/heads/out5".to_string()),
    ];
    for (label, refspec) in cases {
        let out = run_shim(
            &wt,
            &home,
            "agent-pd",
            &real_git,
            &[],
            &["push", "origin", refspec.as_str()],
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.code(),
            Some(1),
            "[{label}] refspec {refspec:?} must be denied; stderr={stderr}"
        );
        // The wildcard-dest case is ALSO caught by the pre-existing,
        // independent protected-ref denylist (any wildcard dest is denied
        // out of caution, regardless of content) — that guard legitimately
        // runs first and denies it for its own reason, so the
        // SNAPSHOT_REF_PUSH tag specifically is only guaranteed on the
        // OTHER spellings here. Both layers correctly deny; only assert the
        // tag where this guard is the one that must fire.
        if *label != "wildcard" {
            assert!(
                stderr.contains("SNAPSHOT_REF_PUSH"),
                "[{label}] must carry the SNAPSHOT_REF_PUSH tag: {stderr}"
            );
        }
    }

    // `--mirror` denied outright. Same defense-in-depth note as `wildcard`
    // above: the pre-existing bulk-push-flag guard (`--all`/`--mirror` both
    // push every local ref, protected ones included) also independently
    // denies this before our own `--mirror` check gets a chance to fire —
    // both layers are correct; only the union (denied) is the contract.
    let mirror = run_shim(&wt, &home, "agent-pd", &real_git, &[], &["push", "--mirror", "origin"]);
    assert_eq!(mirror.status.code(), Some(1));

    // A normal branch push is unaffected.
    let normal = run_shim(
        &wt,
        &home,
        "agent-pd",
        &real_git,
        &[],
        &["push", "origin", "agent/pushdeny:refs/heads/agent/pushdeny"],
    );
    assert!(
        normal.status.success(),
        "a normal branch push must not be denied: stderr={}",
        String::from_utf8_lossy(&normal.stderr)
    );
    cleanup(&root);
}

// ── 7. Ambient-1970 date-skew self-prune regression (Δd) ────────────────

#[test]
fn ambient_1970_date_does_not_self_prune_or_prune_prior_snapshots() {
    let root = tempdir("ambient-1970");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let real_git = resolve_setup_real_git();
    init_repo(&real_git, &repo);
    let wt = worktree_of(&root, &real_git, &repo, "agent/ambient1970");
    let home = root.join("home");
    write_binding(&home, "agent-a70", "agent/ambient1970", &wt);

    let epoch_env: &[(&str, &str)] = &[
        ("AGENTIC_GIT_SNAPSHOTS", "1"),
        ("GIT_AUTHOR_DATE", "1970-01-01T00:00:00"),
        ("GIT_COMMITTER_DATE", "1970-01-01T00:00:00"),
    ];

    // First destructive op under an ambient 1970 date fixture (mirrors our
    // OWN test fixtures elsewhere that set GIT_COMMITTER_DATE — Δd's whole
    // point: this must NOT make the fresh snapshot look pre-expired).
    std::fs::write(wt.join("README.md"), "first dirty\n").unwrap();
    let out1 = run_shim(&repo, &home, "agent-a70", &real_git, epoch_env, &["reset", "--hard"]);
    assert!(out1.status.success(), "stderr={}", String::from_utf8_lossy(&out1.stderr));
    let refs1 = snapshot_refs(&real_git, &repo);
    assert_eq!(refs1.len(), 1, "{refs1:?}");
    let ts1 = committer_epoch(&real_git, &repo, &refs1[0]);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert!(
        (now - ts1).abs() < 300,
        "snapshot committer date must be forced to NOW despite ambient 1970 env, got {ts1}"
    );

    // Second destructive op, SAME ambient 1970 fixture — its own amortized
    // prune pass must NOT delete the FIRST snapshot (which would happen if
    // date-forcing had failed and the first ref's age read as ~56 years).
    std::fs::write(wt.join("README.md"), "second dirty\n").unwrap();
    let out2 = run_shim(&repo, &home, "agent-a70", &real_git, epoch_env, &["reset", "--hard"]);
    assert!(out2.status.success(), "stderr={}", String::from_utf8_lossy(&out2.stderr));
    let refs2 = snapshot_refs(&real_git, &repo);
    assert_eq!(
        refs2.len(),
        2,
        "both snapshots must survive the second invocation's amortized prune: {refs2:?}"
    );
    cleanup(&root);
}

// ── 8. Pathspec matrix: `restore --staged` does NOT snapshot ───────────

#[test]
fn restore_staged_does_not_snapshot_but_worktree_restore_does() {
    let root = tempdir("pathspec-matrix");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let real_git = resolve_setup_real_git();
    init_repo(&real_git, &repo);
    let wt = worktree_of(&root, &real_git, &repo, "agent/pathspec");
    let home = root.join("home");
    write_binding(&home, "agent-ps", "agent/pathspec", &wt);

    // Stage a change, then `restore --staged` (index-only — must NOT snapshot).
    std::fs::write(wt.join("README.md"), "staged change\n").unwrap();
    setup_git(&real_git, &["add", "README.md"], &wt);
    let staged = run_shim(
        &repo,
        &home,
        "agent-ps",
        &real_git,
        &[("AGENTIC_GIT_SNAPSHOTS", "1")],
        &["restore", "--staged", "README.md"],
    );
    assert!(staged.status.success(), "stderr={}", String::from_utf8_lossy(&staged.stderr));
    assert!(
        snapshot_refs(&real_git, &repo).is_empty(),
        "`restore --staged` must NOT create a snapshot ref"
    );

    // Now a plain worktree `restore` (overwrites the working tree — must
    // snapshot the still-dirty worktree content first).
    let worktree_restore = run_shim(
        &repo,
        &home,
        "agent-ps",
        &real_git,
        &[("AGENTIC_GIT_SNAPSHOTS", "1")],
        &["restore", "README.md"],
    );
    assert!(
        worktree_restore.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&worktree_restore.stderr)
    );
    let refs = snapshot_refs(&real_git, &repo);
    assert_eq!(refs.len(), 1, "a worktree `restore` must snapshot: {refs:?}");
    cleanup(&root);
}

// ── 9. Mid-merge content roundtrip ──────────────────────────────────────

#[test]
fn mid_merge_conflict_snapshot_preserves_pre_merge_content() {
    let root = tempdir("mid-merge");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let real_git = resolve_setup_real_git();
    init_repo(&real_git, &repo);
    std::fs::write(repo.join("shared.txt"), "base\n").unwrap();
    setup_git(&real_git, &["add", "."], &repo);
    setup_git(&real_git, &["commit", "-q", "-m", "add shared"], &repo);

    setup_git(&real_git, &["checkout", "-q", "-b", "other", "main"], &repo);
    std::fs::write(repo.join("shared.txt"), "other-branch-change\n").unwrap();
    setup_git(&real_git, &["commit", "-qam", "other change"], &repo);
    setup_git(&real_git, &["checkout", "-q", "main"], &repo);

    let wt = worktree_of(&root, &real_git, &repo, "agent/midmerge");
    let home = root.join("home");
    write_binding(&home, "agent-mm", "agent/midmerge", &wt);

    // Pre-merge content on the bound branch (uncommitted!) — this is what a
    // faithful safety net must preserve, since `merge` is in the mid-op
    // mangler set unconditionally.
    std::fs::write(wt.join("shared.txt"), "PRE-MERGE UNCOMMITTED\n").unwrap();

    let merge_out = run_shim(
        &repo,
        &home,
        "agent-mm",
        &real_git,
        &[("AGENTIC_GIT_SNAPSHOTS", "1")],
        &["merge", "other"],
    );
    // A real conflict: merge itself fails (git's own exit code), but the
    // snapshot must have been taken BEFORE it ran.
    assert!(!merge_out.status.success(), "expected a real merge conflict");

    let refs = snapshot_refs(&real_git, &repo);
    assert_eq!(refs.len(), 1, "{refs:?}");
    let show = Command::new(&real_git)
        .args(["show", &format!("{}:shared.txt", refs[0])])
        .current_dir(&wt)
        .env("AGENTIC_GIT_BYPASS", "1")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&show.stdout),
        "PRE-MERGE UNCOMMITTED\n",
        "the snapshot must preserve the PRE-merge content byte-for-byte"
    );
    cleanup(&root);
}

// ── 10. Kill switch off: default OFF in raw shim mode ───────────────────

#[test]
fn kill_switch_default_off_in_raw_shim_no_ref_no_warning() {
    let root = tempdir("kill-switch");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let real_git = resolve_setup_real_git();
    init_repo(&real_git, &repo);
    let wt = worktree_of(&root, &real_git, &repo, "agent/killswitch");
    let home = root.join("home");
    write_binding(&home, "agent-ks", "agent/killswitch", &wt);

    std::fs::write(wt.join("README.md"), "will be lost, no net\n").unwrap();
    // No AGENTIC_GIT_SNAPSHOTS at all — raw shim default.
    let out = run_shim(&repo, &home, "agent-ks", &real_git, &[], &["reset", "--hard"]);
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    assert!(
        snapshot_refs(&real_git, &repo).is_empty(),
        "raw shim mode (no env) must create ZERO refs by default"
    );
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("snapshot"),
        "no snapshot machinery should even be mentioned when the kill switch is off"
    );

    // Explicit `=0` and `=off` also disable (belt and suspenders vs. any
    // future default flip).
    for val in ["0", "off"] {
        std::fs::write(wt.join("README.md"), "will be lost again\n").unwrap();
        let out = run_shim(
            &repo,
            &home,
            "agent-ks",
            &real_git,
            &[("AGENTIC_GIT_SNAPSHOTS", val)],
            &["reset", "--hard"],
        );
        assert!(out.status.success());
        assert!(
            snapshot_refs(&real_git, &repo).is_empty(),
            "AGENTIC_GIT_SNAPSHOTS={val} must disable snapshotting"
        );
    }
    cleanup(&root);
}

// ── 11. Non-destructive ops never create refs ───────────────────────────

#[test]
fn non_destructive_ops_never_create_refs_even_when_enabled() {
    let root = tempdir("non-destructive");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let real_git = resolve_setup_real_git();
    init_repo(&real_git, &repo);
    let wt = worktree_of(&root, &real_git, &repo, "agent/nondestructive");
    let home = root.join("home");
    write_binding(&home, "agent-nd", "agent/nondestructive", &wt);

    std::fs::write(wt.join("README.md"), "dirty but never destroyed\n").unwrap();
    for args in [
        vec!["status"],
        vec!["diff"],
        vec!["add", "-A"],
        vec!["commit", "-q", "-m", "safe commit"],
        vec!["log", "-1"],
    ] {
        let out = run_shim(
            &repo,
            &home,
            "agent-nd",
            &real_git,
            &[("AGENTIC_GIT_SNAPSHOTS", "1")],
            &args,
        );
        assert!(
            out.status.success(),
            "{args:?} failed: stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert!(
        snapshot_refs(&real_git, &repo).is_empty(),
        "no non-destructive op may ever create a snapshot ref"
    );
    cleanup(&root);
}

// ── 12. Activation: raw-shim-dormant vs. `run`-session-enabled ─────────

#[test]
fn run_session_enables_snapshots_by_default() {
    let root = tempdir("run-enabled");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let real_git = resolve_setup_real_git();
    init_repo(&real_git, &repo);
    let home = root.join("home");

    let out = Command::new(env!("CARGO_BIN_EXE_agentic-git"))
        .args([
            "run",
            "--agent",
            "run-snap-agent",
            "--branch",
            "sess/snap",
            "--",
            "sh",
            "-c",
            "echo dirty > f.txt && git add -A && git commit -q -m first \
             && echo more >> f.txt && git reset --hard",
        ])
        .current_dir(&repo)
        .env("AGENTIC_GIT_HOME", &home)
        .env("AGENTIC_GIT_REAL_GIT", &real_git)
        .env_remove("AGEND_HOME")
        .env_remove("AGENTIC_GIT_AGENT")
        .env_remove("AGEND_INSTANCE_NAME")
        .env_remove("AGENTIC_GIT_SHIM_DEPTH")
        .env_remove("AGEND_GIT_SHIM_DEPTH")
        .env_remove("AGENTIC_GIT_BYPASS")
        .env_remove("AGEND_GIT_BYPASS")
        .env_remove("AGEND_REAL_GIT")
        .env_remove("AGENTIC_GIT_SNAPSHOTS")
        .env_remove("AGEND_GIT_SNAPSHOTS")
        .output()
        .expect("run agentic-git run");
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("snapshots:"),
        "the session summary must mention the snapshot net: {stderr}"
    );

    let wt = home.join("worktrees").join("run-snap-agent").join("sess/snap");
    let refs = snapshot_refs(&real_git, &wt);
    assert_eq!(
        refs.len(),
        1,
        "a `run` session must get the safety net ON by default: {refs:?}"
    );
    cleanup(&root);
}

#[test]
fn run_session_snapshots_off_respects_explicit_kill_switch() {
    let root = tempdir("run-disabled");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let real_git = resolve_setup_real_git();
    init_repo(&real_git, &repo);
    let home = root.join("home");

    let out = Command::new(env!("CARGO_BIN_EXE_agentic-git"))
        .args([
            "run",
            "--agent",
            "run-snap-off",
            "--branch",
            "sess/snapoff",
            "--",
            "sh",
            "-c",
            "echo dirty > f.txt && git add -A && git commit -q -m first \
             && echo more >> f.txt && git reset --hard",
        ])
        .current_dir(&repo)
        .env("AGENTIC_GIT_HOME", &home)
        .env("AGENTIC_GIT_REAL_GIT", &real_git)
        .env("AGENTIC_GIT_SNAPSHOTS", "0") // explicit user override must win.
        .env_remove("AGEND_HOME")
        .env_remove("AGENTIC_GIT_AGENT")
        .env_remove("AGEND_INSTANCE_NAME")
        .env_remove("AGENTIC_GIT_SHIM_DEPTH")
        .env_remove("AGEND_GIT_SHIM_DEPTH")
        .env_remove("AGENTIC_GIT_BYPASS")
        .env_remove("AGEND_GIT_BYPASS")
        .env_remove("AGEND_REAL_GIT")
        .env_remove("AGEND_GIT_SNAPSHOTS")
        .output()
        .expect("run agentic-git run");
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));

    let wt = home.join("worktrees").join("run-snap-off").join("sess/snapoff");
    assert!(
        snapshot_refs(&real_git, &wt).is_empty(),
        "an explicit AGENTIC_GIT_SNAPSHOTS=0 must still force-disable inside a run session"
    );
    cleanup(&root);
}

// ── impl-review regressions (PR #5 review round 1) ──────────────────────

/// `git switch --discard-changes <own-branch>` discards the worktree even
/// when already on that branch — and `args[1]` being a flag bypasses the
/// shim's `args[1]`-only cross-branch deny, so it REACHES the hook (unlike
/// bare `checkout HEAD <path>`, which the deny matrix rejects as "cross-branch
/// to HEAD" and thus never destroys — verified: it is denied, not run). Real
/// bound-agent run: the pre-op bytes must be recoverable from the snapshot the
/// switch classifier fix now creates.
#[test]
fn switch_discard_changes_is_recoverable_review() {
    let root = tempdir("switch-discard");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let real_git = resolve_setup_real_git();
    init_repo(&real_git, &repo);
    let wt = worktree_of(&root, &real_git, &repo, "agent/swrec");
    let home = root.join("home");
    write_binding(&home, "agent-sw", "agent/swrec", &wt);

    std::fs::write(wt.join("README.md"), "DIRTY EDIT\n").unwrap();
    let out = run_shim(
        &repo,
        &home,
        "agent-sw",
        &real_git,
        &[("AGENTIC_GIT_SNAPSHOTS", "1")],
        &["switch", "--discard-changes", "agent/swrec"],
    );
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    // The op discarded the edit...
    assert_eq!(std::fs::read_to_string(wt.join("README.md")).unwrap(), "hello\n");
    // ...but a snapshot captured it, byte-recoverable.
    let refs = snapshot_refs(&real_git, &repo);
    assert_eq!(refs.len(), 1, "switch --discard-changes must snapshot: {refs:?}");
    setup_git(&real_git, &["checkout", &refs[0], "--", "."], &wt);
    assert_eq!(
        std::fs::read_to_string(wt.join("README.md")).unwrap(),
        "DIRTY EDIT\n"
    );
    cleanup(&root);
}

/// Deviation #4 fix: a solo user with only `AGENTIC_GIT_SNAPSHOTS=1` and NO
/// agent/home context still gets the net (who=`noagent`) via the non-agent
/// early-exit path; and with snapshots NOT enabled that same raw path creates
/// zero refs (default-off boundary preserved).
#[test]
fn solo_opt_in_noagent_snapshots_review() {
    let root = tempdir("solo-optin");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let real_git = resolve_setup_real_git();
    init_repo(&real_git, &repo); // plain repo, no remote → not canonical-rooted

    let dirty = || std::fs::write(repo.join("README.md"), "SOLO DIRTY\n").unwrap();
    let run_noagent = |snap: Option<&str>| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_agentic-git"));
        c.arg0("git")
            .args(["reset", "--hard"])
            .current_dir(&repo)
            .env("AGENTIC_GIT_REAL_GIT", &real_git)
            .env("PATH", sanitized_path(&real_git))
            .env_remove("AGENTIC_GIT_HOME")
            .env_remove("AGEND_HOME")
            .env_remove("AGENTIC_GIT_AGENT")
            .env_remove("AGEND_INSTANCE_NAME")
            .env_remove("AGENTIC_GIT_BYPASS")
            .env_remove("AGEND_GIT_BYPASS")
            .env_remove("AGENTIC_GIT_SHIM_DEPTH")
            .env_remove("AGEND_GIT_SHIM_DEPTH")
            .env_remove("AGENTIC_GIT_SNAPSHOTS")
            .env_remove("AGEND_GIT_SNAPSHOTS")
            .env_remove("GIT_AUTHOR_DATE")
            .env_remove("GIT_COMMITTER_DATE");
        if let Some(v) = snap {
            c.env("AGENTIC_GIT_SNAPSHOTS", v);
        }
        c.output().expect("run shim noagent")
    };

    // (a) opted in, no agent → snapshot with who=noagent.
    dirty();
    let out = run_noagent(Some("1"));
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    let refs = snapshot_refs(&real_git, &repo);
    assert_eq!(refs.len(), 1, "solo opt-in must snapshot with no agent: {refs:?}");
    assert!(refs[0].contains("/noagent/"), "who must be noagent: {}", refs[0]);

    // (b) default off, no agent → zero refs.
    setup_git(&real_git, &["update-ref", "-d", &refs[0]], &repo);
    dirty();
    assert!(run_noagent(None).status.success());
    assert!(
        snapshot_refs(&real_git, &repo).is_empty(),
        "default-off must create no ref"
    );
    cleanup(&root);
}

/// Impl-review round 2 (fugu): `git checkout --pathspec-from-file=<f>` is a
/// REACHABLE worktree discard (`args[1]` is a flag → dodges the args[1]-only
/// cross-branch deny → runs → restores the listed paths, discarding edits),
/// non-interactive, and the flag-enumerating classifier missed it. The
/// fail-safe checkout rule now snapshots it; the discarded bytes recover.
#[test]
fn checkout_pathspec_from_file_is_recoverable_review2() {
    let root = tempdir("checkout-psff");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let real_git = resolve_setup_real_git();
    init_repo(&real_git, &repo);
    let wt = worktree_of(&root, &real_git, &repo, "agent/psff");
    let home = root.join("home");
    write_binding(&home, "agent-ps", "agent/psff", &wt);

    // A pathspec file listing README.md, and a dirty README.md to be discarded.
    std::fs::write(wt.join("ps.txt"), "README.md\n").unwrap();
    std::fs::write(wt.join("README.md"), "DIRTY EDIT\n").unwrap();
    let out = run_shim(
        &repo,
        &home,
        "agent-ps",
        &real_git,
        &[("AGENTIC_GIT_SNAPSHOTS", "1")],
        &["checkout", "--pathspec-from-file=ps.txt"],
    );
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    // The op discarded the edit (README reverted to committed content)...
    assert_eq!(std::fs::read_to_string(wt.join("README.md")).unwrap(), "hello\n");
    // ...but a snapshot captured it, byte-recoverable.
    let refs = snapshot_refs(&real_git, &repo);
    assert_eq!(refs.len(), 1, "--pathspec-from-file must snapshot: {refs:?}");
    setup_git(&real_git, &["checkout", &refs[0], "--", "README.md"], &wt);
    assert_eq!(
        std::fs::read_to_string(wt.join("README.md")).unwrap(),
        "DIRTY EDIT\n"
    );
    cleanup(&root);
}

// ════════════════════════════════════════════════════════════════════════
// `snapshots restore` — the one-command recovery CLI (issue #4 P2 follow-up)
// Drives the SHIPPED binary via CLI dispatch (argv[0] = binary name, NOT the
// shim's forced "git"). Snapshots are created through the real shim path
// (`run_shim` + a destructive op), so these are end-to-end.
// ════════════════════════════════════════════════════════════════════════

/// Invoke the CLI surface (NOT the shim) — `snapshots restore` etc. Strips
/// ambient bypass/real-git twins like `run_shim`, but resolves the real git
/// via `AGENTIC_GIT_REAL_GIT` so restore's own plumbing never re-enters a shim.
fn run_cli(cwd: &Path, real_git: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_agentic-git"))
        .args(args)
        .current_dir(cwd)
        .env("AGENTIC_GIT_REAL_GIT", real_git)
        .env("PATH", sanitized_path(real_git))
        .env_remove("AGEND_REAL_GIT")
        .env_remove("AGENTIC_GIT_BYPASS")
        .env_remove("AGEND_GIT_BYPASS")
        .output()
        .expect("run cli")
}

/// One dirty tracked change + reset --hard → exactly one snapshot. Returns the
/// worktree so the caller can drive `snapshots restore` in it.
fn dirty_reset_snapshot(root: &Path, real_git: &Path, repo: &Path, agent: &str, branch: &str, home: &Path, readme: &str) -> PathBuf {
    let wt = worktree_of(root, real_git, repo, branch);
    write_binding(home, agent, branch, &wt);
    std::fs::write(wt.join("README.md"), readme).unwrap();
    let out = run_shim(repo, home, agent, real_git, &[("AGENTIC_GIT_SNAPSHOTS", "1")], &["reset", "--hard"]);
    assert!(out.status.success(), "seed reset --hard: {}", String::from_utf8_lossy(&out.stderr));
    wt
}

// ── R1. core: recovers tracked + untracked, lands UNSTAGED, no new ref ───
#[test]
fn restore_cli_recovers_tracked_and_untracked_unstaged() {
    let root = tempdir("restore-core");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let real_git = resolve_setup_real_git();
    init_repo(&real_git, &repo);
    let wt = worktree_of(&root, &real_git, &repo, "agent/rcore");
    let home = root.join("home");
    write_binding(&home, "agent-rc", "agent/rcore", &wt);

    std::fs::write(wt.join("README.md"), "TRACKED CHANGE\n").unwrap();
    std::fs::write(wt.join("untracked.txt"), "UNTRACKED CONTENT\n").unwrap();
    let out = run_shim(&repo, &home, "agent-rc", &real_git, &[("AGENTIC_GIT_SNAPSHOTS", "1")], &["reset", "--hard"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(snapshot_refs(&real_git, &repo).len(), 1);

    // Both look lost: reset reverted the tracked file; drop the untracked one.
    assert_eq!(std::fs::read_to_string(wt.join("README.md")).unwrap(), "hello\n");
    std::fs::remove_file(wt.join("untracked.txt")).unwrap();

    // ONE command, no ref (exactly one snapshot), no bypass.
    let r = run_cli(&wt, &real_git, &["snapshots", "restore"]);
    assert!(r.status.success(), "restore failed: {}", String::from_utf8_lossy(&r.stderr));

    assert_eq!(std::fs::read_to_string(wt.join("README.md")).unwrap(), "TRACKED CHANGE\n");
    assert_eq!(std::fs::read_to_string(wt.join("untracked.txt")).unwrap(), "UNTRACKED CONTENT\n");
    // Tree was clean at restore time → no pre-restore snapshot.
    assert_eq!(snapshot_refs(&real_git, &repo).len(), 1, "clean tree → no new ref");

    // Recovery is UNSTAGED — nothing in the index.
    let status = setup_git(&real_git, &["status", "--porcelain"], &wt);
    let s = String::from_utf8_lossy(&status.stdout);
    assert!(s.contains(" M README.md"), "README unstaged-modified: {s:?}");
    assert!(s.contains("?? untracked.txt"), "recovered untracked stays untracked: {s:?}");
    for line in s.lines() {
        assert!(
            !matches!(line.as_bytes().first(), Some(b'M' | b'A' | b'D' | b'R' | b'C')),
            "no path may be staged: {line:?}"
        );
    }
    cleanup(&root);
}

// ── R2. `--staged` opts back into the classic staged restore ─────────────
#[test]
fn restore_cli_staged_flag_leaves_index_staged() {
    let root = tempdir("restore-staged");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let real_git = resolve_setup_real_git();
    init_repo(&real_git, &repo);
    let home = root.join("home");
    let wt = dirty_reset_snapshot(&root, &real_git, &repo, "agent-rs", "agent/rstaged", &home, "STAGED WANTED\n");

    let r = run_cli(&wt, &real_git, &["snapshots", "restore", "--staged"]);
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    assert_eq!(std::fs::read_to_string(wt.join("README.md")).unwrap(), "STAGED WANTED\n");

    let status = setup_git(&real_git, &["status", "--porcelain"], &wt);
    let s = String::from_utf8_lossy(&status.stdout);
    assert!(s.contains("M  README.md"), "--staged must leave README staged: {s:?}");
    cleanup(&root);
}

// ── R3. several snapshots + no ref/--yes → refuse; --yes → newest ────────
#[test]
fn restore_cli_refuses_to_guess_then_yes_takes_newest() {
    let root = tempdir("restore-ambig");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let real_git = resolve_setup_real_git();
    init_repo(&real_git, &repo);
    let wt = worktree_of(&root, &real_git, &repo, "agent/rambig");
    let home = root.join("home");
    write_binding(&home, "agent-ra", "agent/rambig", &wt);

    // Two snapshots: V1 then V2 (V2 newest).
    for v in ["V1\n", "V2\n"] {
        std::fs::write(wt.join("README.md"), v).unwrap();
        let out = run_shim(&repo, &home, "agent-ra", &real_git, &[("AGENTIC_GIT_SNAPSHOTS", "1")], &["reset", "--hard"]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    assert_eq!(snapshot_refs(&real_git, &repo).len(), 2);

    // No ref, no --yes → refuse to guess (exit 2).
    let refuse = run_cli(&wt, &real_git, &["snapshots", "restore"]);
    assert_eq!(refuse.status.code(), Some(2), "must refuse: {}", String::from_utf8_lossy(&refuse.stderr));
    assert!(String::from_utf8_lossy(&refuse.stderr).contains("refusing to guess"));

    // --yes → the NEWEST (V2), regardless of same-second ordering.
    let yes = run_cli(&wt, &real_git, &["snapshots", "restore", "--yes"]);
    assert!(yes.status.success(), "{}", String::from_utf8_lossy(&yes.stderr));
    assert_eq!(std::fs::read_to_string(wt.join("README.md")).unwrap(), "V2\n");
    cleanup(&root);
}

// ── R4. a non-snapshot ref is refused (never restores a branch/tag) ──────
#[test]
fn restore_cli_rejects_non_snapshot_ref() {
    let root = tempdir("restore-badref");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let real_git = resolve_setup_real_git();
    init_repo(&real_git, &repo);

    let r = run_cli(&repo, &real_git, &["snapshots", "restore", "refs/heads/main"]);
    assert_eq!(r.status.code(), Some(2), "bad ref must exit 2: {}", String::from_utf8_lossy(&r.stderr));
    assert!(String::from_utf8_lossy(&r.stderr).contains("not an agentic-git snapshot ref"));
    cleanup(&root);
}

// ── R5. no snapshots at all → clean exit 1, actionable message ───────────
#[test]
fn restore_cli_no_snapshots_exit_1() {
    let root = tempdir("restore-none");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let real_git = resolve_setup_real_git();
    init_repo(&real_git, &repo);

    let r = run_cli(&repo, &real_git, &["snapshots", "restore"]);
    assert_eq!(r.status.code(), Some(1), "no snapshots → exit 1: {}", String::from_utf8_lossy(&r.stderr));
    assert!(String::from_utf8_lossy(&r.stderr).contains("no snapshots to restore from"));
    cleanup(&root);
}

// ── R6. non-destructive + undo round-trip via the pre-restore snapshot ───
#[test]
fn restore_cli_is_nondestructive_and_undoable() {
    let root = tempdir("restore-undo");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let real_git = resolve_setup_real_git();
    init_repo(&real_git, &repo);
    let wt = worktree_of(&root, &real_git, &repo, "agent/rundo");
    let home = root.join("home");
    write_binding(&home, "agent-ru", "agent/rundo", &wt);

    // Snapshot S1 captures README="OLD WORK" before reset reverts it.
    std::fs::write(wt.join("README.md"), "OLD WORK\n").unwrap();
    let out = run_shim(&repo, &home, "agent-ru", &real_git, &[("AGENTIC_GIT_SNAPSHOTS", "1")], &["reset", "--hard"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let s1: Vec<String> = snapshot_refs(&real_git, &repo);
    assert_eq!(s1.len(), 1);

    // Fresh, DIFFERENT current work (dirty tree at restore time).
    std::fs::write(wt.join("README.md"), "CURRENT\n").unwrap();
    std::fs::write(wt.join("newfile.txt"), "NEW CURRENT\n").unwrap();

    // Restore S1 (only snapshot). Dirty tree → a pre-restore snapshot is taken.
    let r = run_cli(&wt, &real_git, &["snapshots", "restore"]);
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    // Tracked recovered to S1; the file created AFTER S1 is left untouched.
    assert_eq!(std::fs::read_to_string(wt.join("README.md")).unwrap(), "OLD WORK\n");
    assert_eq!(std::fs::read_to_string(wt.join("newfile.txt")).unwrap(), "NEW CURRENT\n", "non-destructive: newer file survives");
    let after: Vec<String> = snapshot_refs(&real_git, &repo);
    assert_eq!(after.len(), 2, "dirty tree → one pre-restore snapshot added: {after:?}");

    // Undo: restore the pre-restore snapshot (the one that isn't S1).
    let undo_ref = after.iter().find(|r| !s1.contains(r)).expect("pre-restore ref").clone();
    let u = run_cli(&wt, &real_git, &["snapshots", "restore", &undo_ref]);
    assert!(u.status.success(), "undo: {}", String::from_utf8_lossy(&u.stderr));
    assert_eq!(std::fs::read_to_string(wt.join("README.md")).unwrap(), "CURRENT\n", "undo brings current work back");
    cleanup(&root);
}

// ── R7. large snapshot (many long paths) — no ARG_MAX / E2BIG (fugu #10) ──
// A `clean -fd`-style loss of a big generated/vendored tree must be fully
// recoverable. Passing every path as argv (the pre-fix approach) blew ARG_MAX
// and restored ZERO files; restore now feeds pathspecs via stdin.
#[test]
fn restore_cli_handles_many_long_paths_without_argv_limit() {
    let root = tempdir("restore-bulk");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let real_git = resolve_setup_real_git();
    init_repo(&real_git, &repo);
    let wt = worktree_of(&root, &real_git, &repo, "agent/rbulk");
    let home = root.join("home");
    write_binding(&home, "agent-rbk", "agent/rbulk", &wt);

    let n = 5000usize;
    let pad = "x".repeat(230);
    // Total pathspec bytes must exceed a typical ARG_MAX (~1 MB on macOS).
    assert!(n * (pad.len() + 15) > 1_100_000, "test must exceed ARG_MAX to be meaningful");
    let bulk = wt.join("gen");
    std::fs::create_dir_all(&bulk).unwrap();
    for i in 0..n {
        std::fs::write(bulk.join(format!("f{i:05}_{pad}.txt")), b"gen\n").unwrap();
    }

    let out = run_shim(&repo, &home, "agent-rbk", &real_git, &[("AGENTIC_GIT_SNAPSHOTS", "1")], &["reset", "--hard"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(snapshot_refs(&real_git, &repo).len(), 1);

    // Lose the whole generated tree, then recover it in ONE command.
    std::fs::remove_dir_all(&bulk).unwrap();
    let r = run_cli(&wt, &real_git, &["snapshots", "restore"]);
    assert!(r.status.success(), "bulk restore failed: {}", String::from_utf8_lossy(&r.stderr));
    let restored = std::fs::read_dir(&bulk).map(|d| d.count()).unwrap_or(0);
    assert_eq!(restored, n, "every file in the large snapshot must be recovered");
    cleanup(&root);
}

// ── R8. a filename that LOOKS like pathspec magic is recovered literally ──
// `:(glob)literal.txt` is a perfectly legal filename; without literal
// pathspecs git parses it as a magic expression and restore cannot recover
// the file it snapshotted (fugu PR #10 re-review). This file is unix-only
// (see the `std::os::unix` import), so the `:` name is always valid here.
#[test]
fn restore_cli_recovers_pathspec_magic_filename() {
    let root = tempdir("restore-magic");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let real_git = resolve_setup_real_git();
    init_repo(&real_git, &repo);
    let wt = worktree_of(&root, &real_git, &repo, "agent/rmagic");
    let home = root.join("home");
    write_binding(&home, "agent-rm", "agent/rmagic", &wt);

    let magic = ":(glob)literal.txt";
    std::fs::write(wt.join("README.md"), "trigger\n").unwrap(); // dirty → snapshot
    std::fs::write(wt.join(magic), "MAGIC CONTENT\n").unwrap();
    let out = run_shim(&repo, &home, "agent-rm", &real_git, &[("AGENTIC_GIT_SNAPSHOTS", "1")], &["reset", "--hard"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(snapshot_refs(&real_git, &repo).len(), 1);

    std::fs::remove_file(wt.join(magic)).unwrap();
    assert!(!wt.join(magic).exists());

    let r = run_cli(&wt, &real_git, &["snapshots", "restore"]);
    assert!(r.status.success(), "magic-name restore failed: {}", String::from_utf8_lossy(&r.stderr));
    assert_eq!(
        std::fs::read_to_string(wt.join(magic)).unwrap(),
        "MAGIC CONTENT\n",
        "a file named like pathspec magic must still be recovered literally"
    );
    cleanup(&root);
}
