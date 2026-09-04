//! #34: hermetic real-entry integration tests for the fail-closed submodule
//! policy. Drives the COMPILED shim binary (argv[0] forced to "git") through
//! actual file:// submodule fixtures. Covers: target preservation, recognized
//! read flags, bound/unbound own/foreign routing, depth-0 helper deny vs
//! depth>0 helper passthrough, unknown-op deny, bypass audit behaviour.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn tempdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "agentic-git-submod-{tag}-{}-{}",
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

fn resolve_real_git() -> PathBuf {
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

fn setup_git(real_git: &Path, args: &[&str], cwd: &Path) -> std::process::Output {
    let out = Command::new(real_git)
        .args(["-c", "protocol.file.allow=always"])
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
        .args(["-c", "protocol.file.allow=always"])
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

/// Create a parent repo with a file:// submodule already added and committed.
fn setup_parent_with_submodule(root: &Path, real_git: &Path) -> (PathBuf, PathBuf) {
    let sub_repo = root.join("sub-upstream");
    std::fs::create_dir_all(&sub_repo).unwrap();
    init_repo(real_git, &sub_repo);
    std::fs::write(sub_repo.join("lib.txt"), "submodule content\n").unwrap();
    setup_git(real_git, &["add", "."], &sub_repo);
    setup_git(real_git, &["commit", "-q", "-m", "sub-content"], &sub_repo);

    let parent = root.join("parent");
    std::fs::create_dir_all(&parent).unwrap();
    init_repo(real_git, &parent);
    let sub_url = format!("file://{}", sub_repo.display());
    setup_git(
        real_git,
        &["submodule", "add", &sub_url, "vendor/sub"],
        &parent,
    );
    setup_git(
        real_git,
        &["commit", "-q", "-m", "add submodule"],
        &parent,
    );
    (parent, sub_repo)
}

// ── Unbound routing ────────────────────────────────────────────────────

#[test]
fn unbound_submodule_status_passes_through() {
    let root = tempdir("unbound-read");
    let real_git = resolve_real_git();
    let (parent, _sub) = setup_parent_with_submodule(&root, &real_git);
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();

    let out = run_shim(&parent, &home, "test-agent", &real_git, &[], &["submodule", "status"]);
    assert!(
        out.status.success() || out.status.code() == Some(0),
        "unbound submodule status must pass through; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    cleanup(&root);
}

#[test]
fn unbound_submodule_update_denied() {
    let root = tempdir("unbound-write");
    let real_git = resolve_real_git();
    let (parent, _sub) = setup_parent_with_submodule(&root, &real_git);
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();

    let out = run_shim(&parent, &home, "test-agent", &real_git, &[], &["submodule", "update"]);
    assert!(
        !out.status.success(),
        "unbound submodule update must be denied"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unbound") || stderr.contains("Deny"),
        "deny message must mention unbound; stderr={stderr}"
    );
    cleanup(&root);
}

#[test]
fn unbound_submodule_quiet_status_passes_through() {
    let root = tempdir("unbound-quiet-read");
    let real_git = resolve_real_git();
    let (parent, _sub) = setup_parent_with_submodule(&root, &real_git);
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();

    let out = run_shim(
        &parent,
        &home,
        "test-agent",
        &real_git,
        &[],
        &["submodule", "--quiet", "status"],
    );
    assert!(
        out.status.success(),
        "unbound submodule --quiet status must pass through; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    cleanup(&root);
}

#[test]
fn unbound_submodule_quiet_update_denied() {
    let root = tempdir("unbound-quiet-write");
    let real_git = resolve_real_git();
    let (parent, _sub) = setup_parent_with_submodule(&root, &real_git);
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();

    let out = run_shim(
        &parent,
        &home,
        "test-agent",
        &real_git,
        &[],
        &["submodule", "--quiet", "update"],
    );
    assert!(
        !out.status.success(),
        "unbound submodule --quiet update must be denied"
    );
    cleanup(&root);
}

#[test]
fn unbound_submodule_unknown_op_denied() {
    let root = tempdir("unbound-unknown");
    let real_git = resolve_real_git();
    let (parent, _sub) = setup_parent_with_submodule(&root, &real_git);
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();

    let out = run_shim(
        &parent,
        &home,
        "test-agent",
        &real_git,
        &[],
        &["submodule", "futureop"],
    );
    assert!(
        !out.status.success(),
        "unbound submodule with unknown op must be denied (fail-closed)"
    );
    cleanup(&root);
}

// ── Bound routing ──────────────────────────────────────────────────────

#[test]
fn bound_submodule_status_routes_to_worktree() {
    let root = tempdir("bound-read");
    let real_git = resolve_real_git();
    let (parent, _sub) = setup_parent_with_submodule(&root, &real_git);
    let home = root.join("home");
    let wt = worktree_of(&root, &real_git, &parent, "fix/test");
    write_binding(&home, "test-agent", "fix/test", &wt);

    let out = run_shim(&wt, &home, "test-agent", &real_git, &[], &["submodule", "status"]);
    assert!(
        out.status.success(),
        "bound submodule status must succeed via ChdirPass; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    cleanup(&root);
}

#[test]
fn bound_submodule_update_routes_to_worktree() {
    let root = tempdir("bound-write");
    let real_git = resolve_real_git();
    let (parent, _sub) = setup_parent_with_submodule(&root, &real_git);
    let home = root.join("home");
    let wt = worktree_of(&root, &real_git, &parent, "fix/test");
    write_binding(&home, "test-agent", "fix/test", &wt);

    let out = run_shim(
        &wt,
        &home,
        "test-agent",
        &real_git,
        &[],
        &["submodule", "update", "--init"],
    );
    assert!(
        out.status.success(),
        "bound submodule update --init must succeed via ChdirPass; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wt.join("vendor/sub/lib.txt").exists(),
        "submodule content must be checked out in the worktree"
    );
    cleanup(&root);
}

// ── Helper depth boundary ──────────────────────────────────────────────

#[test]
fn submodule_helper_depth0_denied() {
    let root = tempdir("helper-d0");
    let real_git = resolve_real_git();
    let (parent, _sub) = setup_parent_with_submodule(&root, &real_git);
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();

    let out = run_shim(
        &parent,
        &home,
        "test-agent",
        &real_git,
        &[],
        &["submodule--helper", "update"],
    );
    assert!(
        !out.status.success(),
        "direct submodule--helper at depth 0 must be denied"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("submodule--helper") || stderr.contains("not allowed"),
        "deny message must mention submodule--helper; stderr={stderr}"
    );
    cleanup(&root);
}

#[test]
fn submodule_helper_depth1_passes_through() {
    let root = tempdir("helper-d1");
    let real_git = resolve_real_git();
    let (parent, _sub) = setup_parent_with_submodule(&root, &real_git);
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();

    let out = run_shim(
        &parent,
        &home,
        "test-agent",
        &real_git,
        &[("AGENTIC_GIT_SHIM_DEPTH", "1")],
        &["submodule--helper", "status"],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    // Discriminating: old behavior (no depth boundary) would deny with
    // "submodule--helper invocation is not allowed". Passthrough must NOT
    // produce the shim deny message regardless of exit code (the helper
    // invoked directly may exit non-zero for other reasons).
    assert!(
        !stderr.contains("not allowed"),
        "depth>0 submodule--helper must pass through, not be denied; stderr={stderr}"
    );
    assert!(
        !stderr.contains("Deny"),
        "depth>0 submodule--helper must not hit classify deny; stderr={stderr}"
    );
    cleanup(&root);
}

// ── Target preservation (reads preserve -C; writes strip) ──────────────

#[test]
fn bound_submodule_read_preserves_target_override() {
    let root = tempdir("target-read");
    let real_git = resolve_real_git();
    let (parent, _sub) = setup_parent_with_submodule(&root, &real_git);
    let home = root.join("home");
    let wt = worktree_of(&root, &real_git, &parent, "fix/test");
    write_binding(&home, "test-agent", "fix/test", &wt);

    let other_dir = root.join("other");
    std::fs::create_dir_all(&other_dir).unwrap();
    init_repo(&real_git, &other_dir);

    let out = run_shim(
        &wt,
        &home,
        "test-agent",
        &real_git,
        &[],
        &["-C", other_dir.to_str().unwrap(), "submodule", "status"],
    );
    assert!(
        out.status.success(),
        "submodule read with -C must preserve the target; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Discriminating: other_dir has NO submodules → stdout must be empty.
    // If -C were stripped (old behavior), the shim would run in the worktree
    // which HAS a submodule, producing non-empty stdout.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("vendor/sub"),
        "stdout must NOT show worktree's submodule — -C should redirect to other_dir; stdout={stdout}"
    );
    cleanup(&root);
}

// ── Bypass audit ───────────────────────────────────────────────────────

#[test]
fn bypass_submodule_write_emits_audit() {
    let root = tempdir("bypass-audit");
    let real_git = resolve_real_git();
    let (parent, _sub) = setup_parent_with_submodule(&root, &real_git);
    let home = root.join("home");
    std::fs::create_dir_all(home.join("runtime").join("test-agent")).unwrap();

    let out = run_shim(
        &parent,
        &home,
        "test-agent",
        &real_git,
        &[("AGENTIC_GIT_BYPASS", "1")],
        &["submodule", "update", "--init"],
    );
    // With bypass, the command should succeed (passthrough).
    assert!(
        out.status.success(),
        "bypassed submodule update must succeed; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Discriminating: old behavior (missing audit) would not create the
    // events file. The shim writes to $AGENTIC_GIT_HOME/fleet_events.jsonl.
    let events_path = home.join("fleet_events.jsonl");
    assert!(
        events_path.exists(),
        "fleet_events.jsonl must exist after bypass write; path={events_path:?}"
    );
    let content = std::fs::read_to_string(&events_path).unwrap();
    assert!(
        content.contains("submodule"),
        "bypass audit event must mention submodule; events={content}"
    );
    cleanup(&root);
}

#[test]
fn bypass_submodule_read_no_audit() {
    let root = tempdir("bypass-no-audit");
    let real_git = resolve_real_git();
    let (parent, _sub) = setup_parent_with_submodule(&root, &real_git);
    let home = root.join("home");
    std::fs::create_dir_all(home.join("runtime").join("test-agent")).unwrap();

    let out = run_shim(
        &parent,
        &home,
        "test-agent",
        &real_git,
        &[("AGENTIC_GIT_BYPASS", "1")],
        &["submodule", "status"],
    );
    assert!(
        out.status.success(),
        "bypassed submodule status must succeed; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Read operations should NOT emit a bypass audit event.
    let events_path = home.join("fleet_events.jsonl");
    if events_path.exists() {
        let content = std::fs::read_to_string(&events_path).unwrap_or_default();
        let has_submodule_audit = content
            .lines()
            .any(|l| l.contains("\"git_event\"") && l.contains("submodule"));
        assert!(
            !has_submodule_audit,
            "bypass submodule READ must NOT emit audit; events={content}"
        );
    }
    cleanup(&root);
}

// ── Foreign-cwd routing ───────────────────────────────────────────────

#[test]
fn foreign_cwd_submodule_read_routes_to_worktree() {
    let root = tempdir("foreign-read");
    let real_git = resolve_real_git();
    let (parent, _sub) = setup_parent_with_submodule(&root, &real_git);
    let home = root.join("home");
    let wt = worktree_of(&root, &real_git, &parent, "fix/test");
    write_binding(&home, "test-agent", "fix/test", &wt);

    // Create a foreign repo (different git object store).
    let foreign = root.join("foreign");
    std::fs::create_dir_all(&foreign).unwrap();
    init_repo(&real_git, &foreign);

    // Run from foreign cwd — read must route to bound worktree (ChdirPass).
    // The worktree has a submodule at vendor/sub; the foreign repo does not.
    let out = run_shim(
        &foreign,
        &home,
        "test-agent",
        &real_git,
        &[],
        &["submodule", "status"],
    );
    assert!(
        out.status.success(),
        "foreign-cwd submodule read must route to worktree (ChdirPass); stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Discriminating: if the shim ran in the foreign repo (old Passthrough
    // behavior), stdout would be empty (no submodules there). ChdirPass to
    // the bound worktree must show the worktree's registered submodule.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("vendor/sub"),
        "foreign-cwd read must show bound worktree's submodule (proves ChdirPass); stdout={stdout}"
    );
    cleanup(&root);
}

#[test]
fn foreign_cwd_submodule_write_denied() {
    let root = tempdir("foreign-write");
    let real_git = resolve_real_git();
    let (parent, _sub) = setup_parent_with_submodule(&root, &real_git);
    let home = root.join("home");
    let wt = worktree_of(&root, &real_git, &parent, "fix/test");
    write_binding(&home, "test-agent", "fix/test", &wt);

    // Create a foreign repo (different git object store).
    let foreign = root.join("foreign");
    std::fs::create_dir_all(&foreign).unwrap();
    init_repo(&real_git, &foreign);

    // Run from foreign cwd — write must be denied (not passthrough).
    let out = run_shim(
        &foreign,
        &home,
        "test-agent",
        &real_git,
        &[],
        &["submodule", "update"],
    );
    assert!(
        !out.status.success(),
        "foreign-cwd submodule write must be denied"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("denied") || stderr.contains("foreign"),
        "deny message must mention foreign/denied; stderr={stderr}"
    );
    cleanup(&root);
}

// ── Snapshot on submodule write ─────────────────────────────────────────

#[test]
fn bound_submodule_write_creates_snapshot() {
    let root = tempdir("snap-write");
    let real_git = resolve_real_git();
    let (parent, _sub) = setup_parent_with_submodule(&root, &real_git);
    let home = root.join("home");
    let wt = worktree_of(&root, &real_git, &parent, "fix/test");
    write_binding(&home, "test-agent", "fix/test", &wt);

    // Dirty the worktree so the snapshot layer has content to capture.
    // Leave a file staged (not committed) — a clean tree skips snapshotting.
    std::fs::write(wt.join("dirty.txt"), "dirty\n").unwrap();
    setup_git(&real_git, &["add", "dirty.txt"], &wt);

    let out = run_shim(
        &wt,
        &home,
        "test-agent",
        &real_git,
        &[("AGENTIC_GIT_SNAPSHOTS", "1")],
        &["submodule", "update", "--init"],
    );
    assert!(
        out.status.success(),
        "bound submodule update must succeed; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Snapshots are stored in the COMMON .git (shared across worktrees).
    // Check for a snapshot ref containing 'submodule' in the parent repo.
    let refs_out = Command::new(&real_git)
        .args([
            "-c",
            "protocol.file.allow=always",
            "for-each-ref",
            "--format=%(refname)",
            "refs/agentic-git/snapshots/",
        ])
        .current_dir(&wt)
        .env("AGENTIC_GIT_BYPASS", "1")
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("for-each-ref");
    let refs = String::from_utf8_lossy(&refs_out.stdout);
    let has_submod_snap = refs.lines().any(|l| l.contains("submodule"));
    assert!(
        has_submod_snap,
        "submodule write should create a snapshot ref containing 'submodule'; refs={}",
        refs
    );
    cleanup(&root);
}
