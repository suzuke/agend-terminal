use super::*;

fn tmp_home(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-pool-test-{}-{}-{}",
        std::process::id(),
        tag,
        id
    ));
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// #2234 Phase 0: build a daemon-managed git WORKTREE at the per-agent
/// workspace path (`<home>/workspace/<agent>`), mirroring the cure-(B)
/// world where the workspace dir IS the bound worktree (its `.git` a gitlink
/// FILE). Returns the worktree path.
fn managed_workspace_worktree(home: &Path, repo: &Path, agent: &str, branch: &str) -> PathBuf {
    let wt = crate::paths::workspace_dir(home).join(agent);
    std::fs::create_dir_all(wt.parent().expect("workspace parent")).ok();
    let out = std::process::Command::new("git")
        .args(["worktree", "add", "-b", branch, &wt.display().to_string()])
        .current_dir(repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("git worktree add");
    assert!(
        out.status.success(),
        "git worktree add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Daemon-managed marker (as lease() writes).
    std::fs::write(wt.join(MANAGED_MARKER), "").ok();
    assert!(
        wt.join(".git").is_file(),
        "worktree .git must be a gitlink file"
    );
    wt
}

/// `git worktree list --porcelain` for `repo` — used to assert no orphan
/// registration survives a teardown.
fn worktree_list(repo: &Path) -> String {
    let out = std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("git worktree list");
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Number of registered worktrees (one `worktree ` line each). Counting +
/// the `prunable` marker is path-format-independent — a path-STRING
/// `.contains(wt.display())` is Windows-fragile (git lists forward slashes,
/// `Path::display` emits backslashes), which is unrelated to the orphan
/// property under test.
fn worktree_entry_count(repo: &Path) -> usize {
    worktree_list(repo)
        .lines()
        .filter(|l| l.starts_with("worktree "))
        .count()
}

/// #2234 Phase 0 (RED→GREEN): tearing down a per-agent workspace that is a
/// daemon-managed worktree must route through `git worktree remove` (clearing
/// the canonical registration) — NOT a bare `remove_dir_all`, which deletes
/// the dir but leaves an ORPHAN worktree entry in `<canonical>/.git/worktrees/`.
#[test]
fn cleanup_working_dir_managed_worktree_removes_via_git_no_orphan() {
    let home = tmp_home("p0-wt-noorphan");
    let repo = tmp_repo("p0-wt-noorphan-repo");
    let wt = managed_workspace_worktree(&home, &repo, "devw", "feat/p0");
    assert_eq!(
        worktree_entry_count(&repo),
        2,
        "baseline: main + the agent worktree are registered"
    );

    crate::agent_ops::cleanup_working_dir(&home, "devw", &wt);

    assert!(!wt.exists(), "worktree dir must be removed");
    let after = worktree_list(&repo);
    assert_eq!(
        after.lines().filter(|l| l.starts_with("worktree ")).count(),
        1,
        "only main may remain — a bare remove_dir_all would leave the entry: {after}"
    );
    assert!(
        !after.contains("prunable"),
        "no ORPHAN (prunable) worktree registration may survive: {after}"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// Find the single reconcile-backup dir for `agent` (epoch suffix varies).
fn backup_dir_for(home: &Path, agent: &str) -> Option<PathBuf> {
    std::fs::read_dir(home.join("reconcile-backups"))
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&format!("{agent}-")))
        })
}

/// #2234 Phase 0: a worktree with UNCOMMITTED work is backed up WHOLE before
/// the git removal — never silently destroyed.
#[test]
fn cleanup_working_dir_dirty_worktree_backs_up_before_remove() {
    let home = tmp_home("p0-wt-dirty");
    let repo = tmp_repo("p0-wt-dirty-repo");
    let wt = managed_workspace_worktree(&home, &repo, "devd", "feat/p0d");
    std::fs::write(wt.join("WIP.txt"), "unsaved work").unwrap();

    crate::agent_ops::cleanup_working_dir(&home, "devd", &wt);

    assert!(!wt.exists(), "worktree removed");
    let backup = backup_dir_for(&home, "devd").expect("backup dir created");
    assert_eq!(
        std::fs::read_to_string(backup.join("WIP.txt")).unwrap(),
        "unsaved work",
        "uncommitted work must be preserved in the backup"
    );
    assert!(
        !backup.join(".git").exists() && !backup.join("target").exists(),
        "backup excludes the gitlink + regenerable target/"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// #2234 Phase 0: a worktree with a local commit not on any remote
/// (committed-orphan) is backed up before removal — the has_uncommitted
/// guard alone would miss it.
#[test]
fn cleanup_working_dir_committed_orphan_backs_up_before_remove() {
    let home = tmp_home("p0-wt-orphan");
    let repo = tmp_repo("p0-wt-orphan-repo");
    let wt = managed_workspace_worktree(&home, &repo, "devo", "feat/p0o");
    // A remote exists but nothing is pushed → HEAD's commits are unreachable
    // from remotes = committed-orphan. Tree itself is clean.
    std::process::Command::new("git")
        .args(["remote", "add", "origin", &repo.display().to_string()])
        .current_dir(&wt)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("git remote add");

    crate::agent_ops::cleanup_working_dir(&home, "devo", &wt);

    assert!(!wt.exists(), "worktree removed");
    assert!(
        backup_dir_for(&home, "devo").is_some(),
        "committed-orphan worktree must be backed up before removal"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// #2234 Phase 0 (r6/lead dialectic #1): a gitlink worktree MISSING the
/// `.agend-managed` marker (e.g. interrupted reconcile) still routes through
/// `git worktree remove` — the marker is NEVER a veto into the
/// orphan-leaving remove_dir_all path.
#[test]
fn teardown_marker_missing_still_removes_via_git_no_orphan() {
    let home = tmp_home("p0-wt-nomarker");
    let repo = tmp_repo("p0-wt-nomarker-repo");
    let wt = managed_workspace_worktree(&home, &repo, "devn", "feat/p0n");
    std::fs::remove_file(wt.join(MANAGED_MARKER)).unwrap();
    assert!(!is_daemon_managed(&wt));

    let handled = teardown_workspace_worktree(&home, "devn", &wt);

    assert!(
        handled,
        "gitlink present → must take the worktree path even sans marker"
    );
    assert!(!wt.exists(), "worktree removed");
    let after = worktree_list(&repo);
    assert_eq!(
        after.lines().filter(|l| l.starts_with("worktree ")).count(),
        1,
        "only main may remain (marker-less worktree still git-removed): {after}"
    );
    assert!(
        !after.contains("prunable"),
        "no orphan (prunable) registration may survive: {after}"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// #2234 Phase 0: a pre-(B) STANDALONE clone (`.git` is a DIRECTORY) is NOT
/// a worktree → `teardown_workspace_worktree` declines (returns false) and
/// `cleanup_working_dir` falls back to the byte-identical remove_dir_all.
#[test]
fn teardown_standalone_clone_declines_byte_identical() {
    let home = tmp_home("p0-standalone");
    let ws = crate::paths::workspace_dir(&home).join("devs");
    std::fs::create_dir_all(&ws).unwrap();
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(&ws)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("git init");
    assert!(
        ws.join(".git").is_dir(),
        ".git must be a directory (standalone)"
    );

    // Helper declines (not a worktree).
    assert!(!teardown_workspace_worktree(&home, "devs", &ws));
    // Public path still removes the whole dir (byte-identical pre-(B)).
    crate::agent_ops::cleanup_working_dir(&home, "devs", &ws);
    assert!(!ws.exists(), "standalone workspace dir removed as before");
    assert!(
        backup_dir_for(&home, "devs").is_none(),
        "no backup for standalone"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── #2234 cure-(B) Phase 1: reconcile_workspace_to_worktree ──────────────

/// (i) empty workspace → a real daemon-managed gitlink worktree.
#[test]
fn reconcile_empty_workspace_creates_gitlink_worktree() {
    let home = tmp_home("p1-empty");
    let repo = tmp_repo("p1-empty-repo");
    let ws = crate::paths::workspace_dir(&home).join("devx");

    let got =
        reconcile_workspace_to_worktree(&home, "devx", &ws, &repo, None).expect("reconcile empty");

    assert_eq!(got, ws);
    assert!(ws.join(".git").is_file(), "real gitlink FILE (r6 #4)");
    assert!(is_daemon_managed(&ws), ".agend-managed marker written");
    assert!(
        worktree_common_dir_matches(&ws, &repo),
        "worktree rooted at the canonical source_repo"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// (ii) standalone clone (`.git` is a DIR) → backup WHOLE dir, then convert
/// to a gitlink worktree. Work is preserved in the backup.
#[test]
fn reconcile_standalone_clone_backs_up_then_converts() {
    let home = tmp_home("p1-standalone");
    let repo = tmp_repo("p1-standalone-repo");
    let ws = crate::paths::workspace_dir(&home).join("devy");
    std::fs::create_dir_all(&ws).unwrap();
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(&ws)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("git init standalone");
    std::fs::write(ws.join("work.txt"), "wip").unwrap();
    assert!(
        ws.join(".git").is_dir(),
        "precondition: standalone .git dir"
    );

    reconcile_workspace_to_worktree(&home, "devy", &ws, &repo, None).expect("reconcile");

    assert!(ws.join(".git").is_file(), "converted to gitlink worktree");
    assert!(is_daemon_managed(&ws));
    let backup = backup_dir_for(&home, "devy").expect("standalone work backed up");
    assert_eq!(
        std::fs::read_to_string(backup.join("work.txt")).unwrap(),
        "wip",
        "pre-existing work preserved in backup"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// (ii) fail-closed: if the whole-dir backup fails, reconcile ABORTS and
/// leaves the standalone UNTOUCHED — never destroy work without a backup.
#[test]
fn reconcile_backup_failure_aborts_fail_closed() {
    let home = tmp_home("p1-backupfail");
    let repo = tmp_repo("p1-backupfail-repo");
    let ws = crate::paths::workspace_dir(&home).join("devz");
    std::fs::create_dir_all(&ws).unwrap();
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(&ws)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("git init standalone");
    std::fs::write(ws.join("work.txt"), "precious").unwrap();
    // Force backup_worktree_dir's create_dir_all to fail: make the
    // reconcile-backups parent a FILE.
    std::fs::create_dir_all(&home).ok();
    std::fs::write(home.join("reconcile-backups"), "blocker").unwrap();

    let err = reconcile_workspace_to_worktree(&home, "devz", &ws, &repo, None)
        .expect_err("must abort when backup fails");
    assert!(
        err.contains("backup"),
        "error names the backup failure: {err}"
    );

    // Standalone left fully intact — no work lost, not converted.
    assert!(
        ws.join(".git").is_dir(),
        "standalone still present (untouched)"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("work.txt")).unwrap(),
        "precious",
        "work must NOT be destroyed on backup failure"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// (iii) already a worktree of this repo → idempotent NO-OP (no second
/// backup, gitlink unchanged).
#[test]
fn reconcile_already_worktree_is_idempotent_noop() {
    let home = tmp_home("p1-idem");
    let repo = tmp_repo("p1-idem-repo");
    let ws = crate::paths::workspace_dir(&home).join("devi");

    reconcile_workspace_to_worktree(&home, "devi", &ws, &repo, None).expect("first");
    assert!(ws.join(".git").is_file());
    assert!(
        backup_dir_for(&home, "devi").is_none(),
        "no backup for fresh provision"
    );

    let again = reconcile_workspace_to_worktree(&home, "devi", &ws, &repo, None)
        .expect("second reconcile is a no-op");

    assert_eq!(again, ws);
    assert!(ws.join(".git").is_file(), "still a gitlink worktree");
    assert!(
        backup_dir_for(&home, "devi").is_none(),
        "idempotent no-op must NOT back up again"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// #1919 (Method B, the e2e that backs flipping the flag ON): reconcile keeps
/// the cwd PATH stable, so the PRODUCTION claude-session locator
/// (`backend::claude_session::has_resumable` + `encode_project_dir`) still
/// finds the agent's resumable session after a standalone→worktree convert —
/// `claude --continue` is not orphaned.
#[test]
fn reconcile_preserves_claude_session_key_1919() {
    use crate::backend::claude_session::{encode_project_dir, has_resumable};
    let home = tmp_home("p1-1919");
    let repo = tmp_repo("p1-1919-repo");
    let ws = crate::paths::workspace_dir(&home).join("dev9");
    std::fs::create_dir_all(&ws).unwrap();
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(&ws)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("git init standalone");
    std::fs::write(ws.join("src.rs"), "fn main() {}").unwrap();

    // A fake claude session under an injectable projects root, keyed exactly
    // as the production locator computes it from the cwd.
    let proj_root = home.join("fake-claude-projects");
    let key_before = encode_project_dir(&dunce::canonicalize(&ws).unwrap());
    let proj_dir = proj_root.join(&key_before);
    std::fs::create_dir_all(&proj_dir).unwrap();
    std::fs::write(
        proj_dir.join("sess.jsonl"),
        "{\"type\":\"user\",\"message\":\"hi\"}\n",
    )
    .unwrap();
    assert!(
        has_resumable(&ws, &proj_root),
        "baseline: session is resumable before reconcile"
    );

    reconcile_workspace_to_worktree(&home, "dev9", &ws, &repo, None).expect("reconcile");

    let key_after = encode_project_dir(&dunce::canonicalize(&ws).unwrap());
    assert_eq!(
        key_before, key_after,
        "cwd PATH stable across reconcile → claude session key unchanged"
    );
    assert!(
        has_resumable(&ws, &proj_root),
        "#1919: reconcile preserves the resumable session — claude --continue not orphaned"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

// ── #2234 cure-(B) Phase 1c: release_stale_branch_holders + in-place checkout ──

/// A clean legacy holder is released without a backup.
#[test]
fn release_stale_holder_clean_removes_no_backup() {
    let home = tmp_home("p1c-clean");
    let repo = tmp_repo("p1c-clean-repo");
    let l = lease(&home, &repo, "deva", "feat/clean").expect("lease legacy holder");
    assert!(l.path.exists());

    release_one_stale_holder(&home, "deva", &repo, "feat/clean", &l.path).expect("release");

    assert!(!l.path.exists(), "clean legacy holder removed via git");
    assert!(backup_dir_for(&home, "deva").is_none(), "clean → no backup");
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// A legacy holder with work-at-risk (uncommitted) is backed up before
/// removal — never silently destroyed (the shim-redirected-commit hazard).
#[test]
fn release_stale_holder_work_at_risk_backs_up() {
    let home = tmp_home("p1c-risk");
    let repo = tmp_repo("p1c-risk-repo");
    let l = lease(&home, &repo, "devb", "feat/risk").expect("lease");
    std::fs::write(l.path.join("WIP.txt"), "unsaved").unwrap();
    assert!(worktree_has_work_at_risk(&l.path));

    release_one_stale_holder(&home, "devb", &repo, "feat/risk", &l.path).expect("release");

    assert!(!l.path.exists(), "holder removed after backup");
    let backup = backup_dir_for(&home, "devb").expect("work-at-risk backed up");
    assert_eq!(
        std::fs::read_to_string(backup.join("WIP.txt")).unwrap(),
        "unsaved"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// Fail-closed: if a work-at-risk holder's backup fails, release ABORTS and
/// leaves the holder untouched (never --force without a durable backup).
#[test]
fn release_stale_holder_backup_fail_aborts() {
    let home = tmp_home("p1c-bkfail");
    let repo = tmp_repo("p1c-bkfail-repo");
    let l = lease(&home, &repo, "devc", "feat/bk").expect("lease");
    std::fs::write(l.path.join("WIP.txt"), "precious").unwrap();
    // Block backup: make the reconcile-backups parent a FILE.
    std::fs::create_dir_all(&home).ok();
    std::fs::write(home.join("reconcile-backups"), "blocker").unwrap();

    let err = release_one_stale_holder(&home, "devc", &repo, "feat/bk", &l.path)
        .expect_err("must abort on backup failure");
    assert!(err.contains("backup"), "names the backup failure: {err}");
    assert!(l.path.exists(), "holder untouched");
    assert_eq!(
        std::fs::read_to_string(l.path.join("WIP.txt")).unwrap(),
        "precious",
        "work not destroyed on backup failure"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// The confluence end-to-end: a legacy `worktrees/<agent>/<branch>` holds the
/// branch, so the workspace worktree CANNOT check it out in place — until
/// `release_stale_branch_holders` frees it.
#[test]
fn release_stale_branch_holders_frees_branch_for_in_place_checkout() {
    let home = tmp_home("p1c-free");
    let repo = tmp_repo("p1c-free-repo");
    // (B) workspace worktree (detached holding).
    let ws = crate::paths::workspace_dir(&home).join("devf");
    reconcile_workspace_to_worktree(&home, "devf", &ws, &repo, None).expect("provision ws");
    // Legacy holder of feat/coexist (checks the branch out THERE).
    let l = lease(&home, &repo, "devf", "feat/coexist").expect("lease legacy");
    assert!(l.path.exists());
    assert!(
        checkout_workspace_branch(&ws, "feat/coexist").is_err(),
        "branch already checked out at the legacy holder → in-place checkout blocked"
    );

    release_stale_branch_holders(&home, "devf", &repo, "feat/coexist", &ws).expect("free");

    assert!(!l.path.exists(), "legacy holder released");
    checkout_workspace_branch(&ws, "feat/coexist")
        .expect("branch is now free → in-place checkout succeeds");
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// In-place checkout + holding-detach rollback round-trip.
#[test]
fn checkout_workspace_branch_and_detach_rollback() {
    let home = tmp_home("p1c-checkout");
    let repo = tmp_repo("p1c-checkout-repo");
    let ws = crate::paths::workspace_dir(&home).join("devg");
    reconcile_workspace_to_worktree(&home, "devg", &ws, &repo, None).expect("provision");

    // Make a branch to land on.
    std::process::Command::new("git")
        .args(["branch", "feat/land"])
        .current_dir(&repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("git branch");

    checkout_workspace_branch(&ws, "feat/land").expect("in-place checkout");
    let cur = crate::git_helpers::git_cmd(&ws, &["branch", "--show-current"]).unwrap();
    assert_eq!(cur, "feat/land", "workspace now on the branch");

    detach_workspace_to_holding(&ws).expect("rollback to holding");
    let detached = crate::git_helpers::git_cmd(&ws, &["branch", "--show-current"]).unwrap();
    assert!(
        detached.is_empty(),
        "rollback → detached holding (no branch)"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// #2234 Phase 1c: the production flag decision (`workspace_as_worktree_from_env`)
/// — pure over (flag, allowlist) inputs, no process-global env (so no leak).
#[test]
fn workspace_as_worktree_from_env_flag_and_allowlist() {
    // Off by default / unset / wrong value.
    assert!(!workspace_as_worktree_from_env(None, None, "a"));
    assert!(!workspace_as_worktree_from_env(Some("0"), None, "a"));
    assert!(!workspace_as_worktree_from_env(Some("yes"), None, "a"));
    // On for all agents when set and no allowlist.
    assert!(workspace_as_worktree_from_env(Some("1"), None, "a"));
    assert!(workspace_as_worktree_from_env(Some("true"), Some(""), "a"));
    // Allowlist scopes to listed agents only.
    assert!(workspace_as_worktree_from_env(Some("1"), Some("a,b"), "a"));
    assert!(workspace_as_worktree_from_env(
        Some("1"),
        Some(" a , b "),
        "b"
    ));
    assert!(!workspace_as_worktree_from_env(Some("1"), Some("a,b"), "c"));
}

/// #2234 Phase 1c: the thread-local test seam overrides the env decision for
/// the current thread only, and the RAII guard restores on drop.
#[test]
fn workspace_worktree_test_seam_is_thread_scoped_and_restores() {
    assert!(!workspace_as_worktree_enabled("z"), "default off");
    {
        let _g = workspace_worktree_test_seam::force(true);
        assert!(
            workspace_as_worktree_enabled("z"),
            "forced on for this thread"
        );
    }
    assert!(
        !workspace_as_worktree_enabled("z"),
        "guard drop restores the env-default (off)"
    );
}

// ── #2234 rollback primitive: reverse_reconcile ─────────────────────────

/// Helper: convert /workspace into a (B) worktree on `branch` with one
/// committed-but-unpushed commit (simulates post-conversion in-place work).
fn converted_workspace_with_commit(home: &Path, repo: &Path, agent: &str, branch: &str) -> PathBuf {
    let ws = crate::paths::workspace_dir(home).join(agent);
    reconcile_workspace_to_worktree(home, agent, &ws, repo, None).expect("reconcile");
    // Create the branch in canonical, check it out in the workspace worktree,
    // commit there (commit lands in canonical's object store + refs/heads).
    std::process::Command::new("git")
        .args(["branch", branch])
        .current_dir(repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("git branch");
    checkout_workspace_branch(&ws, branch).expect("checkout");
    std::fs::write(ws.join("work.rs"), "fn main() {}").unwrap();
    for args in [
        vec!["add", "work.rs"],
        vec![
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-m",
            "unpushed C1",
        ],
    ] {
        std::process::Command::new("git")
            .args(&args)
            .current_dir(&ws)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .expect("git commit");
    }
    ws
}

fn rev_parse(dir: &Path, rev: &str) -> Option<String> {
    crate::git_helpers::git_cmd(dir, &["rev-parse", rev]).ok()
}

/// #2234 (毀-work core): a committed-but-unpushed commit on the converted
/// workspace's branch is preserved BY CONSTRUCTION across reverse_reconcile —
/// it lives in canonical, and a subsequent OFF lease of the branch recovers it.
#[test]
fn reverse_reconcile_preserves_committed_work_via_canonical() {
    let home = tmp_home("rr-commit");
    let repo = tmp_repo("rr-commit-repo");
    let ws = converted_workspace_with_commit(&home, &repo, "deva", "feat/rr");
    let c1 = rev_parse(&ws, "HEAD").expect("ws HEAD");

    reverse_reconcile(&home, "deva").expect("reverse_reconcile");

    // Workspace is no longer a (B) worktree (restored to a standalone).
    assert!(
        !ws.join(".git").is_file(),
        "workspace reverted from gitlink worktree to standalone"
    );
    // The commit + branch ref SURVIVE in canonical (not lost).
    assert_eq!(
        rev_parse(&repo, "feat/rr").as_deref(),
        Some(c1.as_str()),
        "committed work preserved in canonical (not via reconcile-backups)"
    );
    // An OFF-style lease of the branch recovers the commit's tree.
    let l = lease(&home, &repo, "deva", "feat/rr").expect("re-lease recovers branch");
    assert_eq!(
        std::fs::read_to_string(l.path.join("work.rs")).unwrap(),
        "fn main() {}",
        "re-leased worktree has the recovered committed work"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// Uncommitted/untracked work IS at risk → backed up before the revert.
#[test]
fn reverse_reconcile_backs_up_uncommitted_work() {
    let home = tmp_home("rr-uncommitted");
    let repo = tmp_repo("rr-uncommitted-repo");
    let ws = crate::paths::workspace_dir(&home).join("devb");
    reconcile_workspace_to_worktree(&home, "devb", &ws, &repo, None).expect("reconcile");
    std::fs::write(ws.join("WIP.txt"), "unsaved").unwrap();

    reverse_reconcile(&home, "devb").expect("reverse_reconcile");

    let backup = backup_dir_for(&home, "devb").expect("uncommitted work backed up");
    assert_eq!(
        std::fs::read_to_string(backup.join("WIP.txt")).unwrap(),
        "unsaved"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// Fail-closed: if the uncommitted-work backup fails, the revert ABORTS and
/// leaves the (B) worktree untouched (never destroy work without a backup).
#[test]
fn reverse_reconcile_backup_fail_aborts() {
    let home = tmp_home("rr-bkfail");
    let repo = tmp_repo("rr-bkfail-repo");
    let ws = crate::paths::workspace_dir(&home).join("devc");
    reconcile_workspace_to_worktree(&home, "devc", &ws, &repo, None).expect("reconcile");
    std::fs::write(ws.join("WIP.txt"), "precious").unwrap();
    std::fs::create_dir_all(&home).ok();
    std::fs::write(home.join("reconcile-backups"), "blocker").unwrap();

    let err = reverse_reconcile(&home, "devc").expect_err("must abort on backup failure");
    assert!(err.contains("backup"), "names backup failure: {err}");
    assert!(
        ws.join(".git").is_file(),
        "still a (B) worktree (untouched on abort)"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("WIP.txt")).unwrap(),
        "precious",
        "work not destroyed on backup failure"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// "Can turn off" proof: after reverse_reconcile, the workspace worktree is
/// gone from canonical's registry (no "already checked out" block for a future
/// OFF lease) and the (B) binding is cleared. Also no-op on an unconverted dir.
#[test]
fn reverse_reconcile_clears_registration_and_is_noop_when_unconverted() {
    let home = tmp_home("rr-offready");
    let repo = tmp_repo("rr-offready-repo");
    // No-op on an absent/unconverted workspace.
    reverse_reconcile(&home, "devd").expect("no-op on unconverted");

    let ws = converted_workspace_with_commit(&home, &repo, "devd", "feat/off");
    crate::binding::bind_full(&home, "devd", "T-1", "feat/off", &ws, &repo, false).ok();
    let wt_listed = |repo: &Path| {
        crate::git_helpers::git_cmd(repo, &["worktree", "list", "--porcelain"]).unwrap_or_default()
    };
    // Precondition: workspace is registered as a 2nd worktree (canonical +
    // workspace). Count rather than substring-match the path — git porcelain
    // emits a canonicalized path form (drive-case / separator) that need not
    // equal ws.display() on Windows.
    let before = wt_listed(&repo);
    assert_eq!(
        before
            .lines()
            .filter(|l| l.starts_with("worktree "))
            .count(),
        2,
        "workspace registered before reverse_reconcile: {before}"
    );

    reverse_reconcile(&home, "devd").expect("reverse_reconcile");

    let after = wt_listed(&repo);
    assert_eq!(
        after.lines().filter(|l| l.starts_with("worktree ")).count(),
        1,
        "only canonical remains — workspace worktree deregistered (OFF lease won't conflict): {after}"
    );
    assert!(
        !after.contains("prunable"),
        "no orphan registration: {after}"
    );
    assert!(
        crate::binding::read(&home, "devd").is_none(),
        "(B) binding cleared"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

fn tmp_repo(tag: &str) -> PathBuf {
    let dir = tmp_home(tag);
    // #1463: scratch-repo git must bypass the agend-git shim, else an
    // agent-run suite (AGEND_INSTANCE_NAME set) ChdirPass-redirects the
    // commit into the bound worktree (init-pile pollution).
    std::process::Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(&dir)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .ok();
    std::process::Command::new("git")
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=t@t",
            "commit",
            "--allow-empty",
            "-m",
            "init",
        ])
        .current_dir(&dir)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .ok();
    dir
}

/// Lease + bind — finding D+H: `lease` no longer writes binding.json (the
/// authoritative caller binds AFTER leasing). Tests that exercise `release`/
/// `release_full` need a binding present, so this helper simulates dispatch's
/// pre-build bind (the production `bind_full` that now solely owns binding).
fn lease_bound(home: &Path, repo: &Path, agent: &str, branch: &str) -> WorktreeLease {
    let l = lease(home, repo, agent, branch).expect("lease");
    crate::binding::bind_full(home, agent, "", branch, &l.path, repo, false)
        .expect("bind_full (simulates the authoritative caller)");
    l
}

#[test]
fn lease_main_branch_rejected() {
    let home = tmp_home("main-reject");
    let repo = tmp_repo("main-reject-repo");
    let result = lease(&home, &repo, "agent-1", "main");
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        LeaseError::ProtectedBranch(_)
    ));
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// Finding D+H load-bearing: `lease` returns DISTINCT typed variants — a
/// protected ref is `ProtectedBranch`, a `worktree::create` failure is
/// `CreateFailed`. Reverse-mutation guard: collapsing both arms to one
/// variant (or back to a `String`) breaks the dispatch-boundary match.
#[test]
fn lease_returns_typed_protected_and_propagates_errors() {
    let home = tmp_home("typed-err");
    let repo = tmp_repo("typed-err-repo");

    // E4.5 protected ref → ProtectedBranch (message preserves "E4.5").
    match lease(&home, &repo, "agent-t", "main") {
        Err(LeaseError::ProtectedBranch(m)) => assert!(m.contains("E4.5"), "msg: {m}"),
        other => panic!("expected ProtectedBranch, got {other:?}"),
    }

    // A non-protected but invalid branch name (`..` fails validate_branch)
    // makes `worktree::create` return None → CreateFailed (NOT ProtectedBranch).
    match lease(&home, &repo, "agent-t", "feat..bad") {
        Err(LeaseError::CreateFailed(m)) => {
            assert!(m.contains("feat..bad"), "msg names the target: {m}")
        }
        other => panic!("expected CreateFailed, got {other:?}"),
    }

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn lease_creates_daemon_tagged_worktree() {
    let home = tmp_home("lease-tag");
    let repo = tmp_repo("lease-tag-repo");
    let result = lease(&home, &repo, "agent-2", "feat/test");
    assert!(result.is_ok());
    let l = result.expect("lease");
    assert!(l.path.exists());
    assert!(is_daemon_managed(&l.path));
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn release_marks_candidate_no_delete() {
    let home = tmp_home("release");
    let repo = tmp_repo("release-repo");
    let l = lease(&home, &repo, "agent-3", "feat/release").expect("lease");
    release(&home, &l);
    // Worktree still exists (no delete in Phase 3).
    assert!(l.path.exists());
    // Binding cleared.
    assert!(crate::binding::read(&home, "agent-3").is_none());
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn release_idempotent() {
    let home = tmp_home("release-idem");
    let repo = tmp_repo("release-idem-repo");
    let l = lease(&home, &repo, "agent-4", "feat/idem").expect("lease");
    release(&home, &l);
    release(&home, &l); // second release — no panic
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

// ── Sprint 53 P0-X: release_full (hard release) tests ───────────────
//
// These call the production function `release_full`, which in turn is
// the body of the `release_worktree` MCP tool. The MCP layer test in
// `src/mcp/handlers/worktree.rs` covers the handler contract; here we
// focus on the filesystem semantics.
//
// Regression-proof: comment out the `git worktree remove` block in
// `release_full` and `p0x_release_full_happy_path_removes_worktree_and_binding`
// FAILS (`worktree_removed` stays false; `l.path.exists()` stays true).
// Restore → PASS. See commit message §regression-proof.

#[test]
fn p0x_release_full_happy_path_removes_worktree_and_binding() {
    let home = tmp_home("p0x-happy");
    let repo = tmp_repo("p0x-happy-repo");
    let l = lease_bound(&home, &repo, "agent-h", "feat/happy");
    // Pre-condition: lease created both binding + worktree.
    assert!(l.path.exists(), "pre: worktree must exist");
    assert!(crate::binding::read(&home, "agent-h").is_some());
    assert!(is_daemon_managed(&l.path));

    let outcome = release_full(&home, "agent-h", false);

    assert!(outcome.released, "happy path must report released");
    assert!(outcome.worktree_removed, "worktree must be removed");
    assert!(outcome.binding_removed, "binding must be removed");
    assert!(outcome.error.is_none(), "no error: {:?}", outcome.error);
    assert!(!l.path.exists(), "worktree dir must be gone post-release");
    assert!(crate::binding::read(&home, "agent-h").is_none());

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// §3.9 regression (#t-21 HIGH #1): `release_full(dry_run=true)` must be
/// observation-only — the worktree directory AND binding.json must survive.
/// Pre-fix, `remove_worktree` + `clear_binding_state` ran unconditionally,
/// so a dry run actually destroyed both. Regression-proof: revert the
/// `if dry_run` guard in `release_full` and this FAILS (`l.path` gone,
/// binding cleared).
#[test]
fn dry_run_release_preserves_worktree_and_binding_t21() {
    let home = tmp_home("t21-dry-run");
    let repo = tmp_repo("t21-dry-run-repo");
    let l = lease_bound(&home, &repo, "agent-dry", "feat/keep");
    assert!(l.path.exists(), "pre: worktree must exist");
    assert!(crate::binding::read(&home, "agent-dry").is_some());

    let outcome = release_full(&home, "agent-dry", true);

    // Observation-success, nothing actually removed.
    assert!(outcome.released, "dry-run reports observation success");
    assert!(
        !outcome.worktree_removed,
        "dry-run must NOT remove worktree"
    );
    assert!(!outcome.binding_removed, "dry-run must NOT clear binding");
    assert!(outcome.error.is_none(), "no error: {:?}", outcome.error);
    // The destructive effects are previewed, not performed.
    assert!(
        outcome.dry_run_preview.as_deref().is_some_and(
            |p| p.contains("would remove worktree") && p.contains("would clear binding")
        ),
        "dry-run must preview both effects: {:?}",
        outcome.dry_run_preview
    );
    // The actual on-disk state is untouched.
    assert!(
        l.path.exists(),
        "worktree dir MUST survive a dry-run release"
    );
    assert!(
        crate::binding::read(&home, "agent-dry").is_some(),
        "binding.json MUST survive a dry-run release"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn p0x_release_full_idempotent_second_call_noop() {
    // #1465: release is idempotent. The first call tears down; the
    // second (no binding left) is a SUCCESS no-op — `released:true,
    // already_released:true`, no error — NOT the pre-#1465 `released:
    // false + "no binding"` error (that encoded the bug this fixes).
    let home = tmp_home("p0x-idem");
    let repo = tmp_repo("p0x-idem-repo");
    lease_bound(&home, &repo, "agent-i", "feat/idem");
    let r1 = release_full(&home, "agent-i", false);
    assert!(r1.released, "first call must release");
    assert!(
        !r1.already_released,
        "first call is a real teardown, not a no-op"
    );
    let r2 = release_full(&home, "agent-i", false);
    assert!(r2.released, "second call must be idempotent success");
    assert!(
        r2.already_released,
        "second call must flag already_released"
    );
    assert!(
        r2.error.is_none(),
        "idempotent no-op must NOT error: {:?}",
        r2.error
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn p0x_release_full_missing_binding_graceful() {
    // #1465: releasing an agent that never had a binding is a success
    // no-op (release target state already reached), not an error.
    let home = tmp_home("p0x-missing-binding");
    let outcome = release_full(&home, "ghost-agent", false);
    assert!(
        outcome.released,
        "missing binding must be idempotent success"
    );
    assert!(outcome.already_released, "must flag already_released");
    assert!(
        outcome.error.is_none(),
        "no-op must not error: {:?}",
        outcome.error
    );
    // Nothing was actually torn down — no worktree/binding removal.
    assert!(!outcome.worktree_removed);
    assert!(!outcome.binding_removed);
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn p0x_release_full_missing_worktree_path_clears_binding_anyway() {
    // Binding exists but the worktree directory was deleted out from
    // under us (manual cleanup, daemon restart races, etc.). Spec:
    // "still remove binding (partial cleanup ok)".
    let home = tmp_home("p0x-missing-wt");
    let repo = tmp_repo("p0x-missing-wt-repo");
    let l = lease_bound(&home, &repo, "agent-mw", "feat/mw");
    // Manually remove the worktree dir behind the daemon's back, but
    // leave the binding pointing at the now-stale path.
    std::fs::remove_dir_all(&l.path).ok();
    assert!(!l.path.exists(), "pre: worktree must be gone");
    assert!(crate::binding::read(&home, "agent-mw").is_some());

    let outcome = release_full(&home, "agent-mw", false);
    assert!(outcome.released, "must still release: {:?}", outcome);
    assert!(outcome.binding_removed, "binding must be cleared");
    assert!(
        !outcome.worktree_removed,
        "worktree wasn't removed by us (it was already gone)"
    );
    assert!(crate::binding::read(&home, "agent-mw").is_none());

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn p0x_release_full_unmanaged_worktree_skipped_safely() {
    // R14 safety: if the binding points at a worktree that lacks the
    // .agend-managed marker (operator-created, not daemon-leased), the
    // release MUST NOT remove the worktree. #1879 (WT-LEAK-2): the stale
    // binding IS cleared, though — leaving it leaked the binding and blocked
    // a same-agent re-bind. The worktree (operator data) survives for
    // investigation; the daemon's binding to it does not.
    let home = tmp_home("p0x-unmanaged");
    let unmanaged_wt = tmp_home("p0x-unmanaged-wt-target");
    // Hand-craft a binding pointing at an unmanaged path.
    std::fs::create_dir_all(crate::paths::runtime_dir(&home).join("agent-u")).ok();
    let binding = serde_json::json!({
        "version": 1,
        "agent": "agent-u",
        "task_id": "T-1",
        "branch": "feat/manual",
        "issued_at": chrono::Utc::now().to_rfc3339(),
        "worktree": unmanaged_wt.display().to_string(),
    });
    std::fs::write(
        crate::paths::runtime_dir(&home)
            .join("agent-u")
            .join("binding.json"),
        serde_json::to_string_pretty(&binding).unwrap(),
    )
    .unwrap();
    // Sanity: no marker.
    assert!(!is_daemon_managed(&unmanaged_wt));

    let outcome = release_full(&home, "agent-u", false);
    assert!(
        !outcome.released,
        "unmanaged worktree must NOT be released: {:?}",
        outcome
    );
    assert!(
        outcome.binding_removed,
        "#1879 WT-LEAK-2: the stale binding must be CLEARED even when the unmanaged worktree removal is refused"
    );
    assert!(
        outcome
            .error
            .as_deref()
            .unwrap_or("")
            .contains(".agend-managed"),
        "error must explain the marker check: {:?}",
        outcome.error
    );
    assert!(unmanaged_wt.exists(), "operator-created dir must survive");
    assert!(
        crate::binding::read(&home, "agent-u").is_none(),
        "#1879 WT-LEAK-2: the binding must be cleared (no leak / re-bind block)"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&unmanaged_wt).ok();
}

/// Helper: assert `git worktree list --porcelain` from `repo` does NOT
/// emit any `prunable` line (registry leak indicator).
fn assert_no_prunable_registry(repo: &Path, scenario: &str) {
    let output = std::process::Command::new("git")
        .current_dir(repo)
        .args(["worktree", "list", "--porcelain"])
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("git worktree list");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        !stdout.contains("prunable"),
        "[{scenario}] git worktree registry must be clean — found `prunable` entry. Output:\n{stdout}"
    );
}

#[test]
fn p0x_release_full_clears_git_worktree_registry() {
    // r1 reviewer (PR #470): the prior IMPL didn't pass `.current_dir(source_repo)`
    // and the `remove_dir_all` fallback didn't `git worktree prune`, so
    // `git worktree list --porcelain` kept emitting `prunable` entries
    // that would block re-lease (registry vs filesystem skew).
    //
    // Scenario A: happy path — `release_full` invokes `git worktree
    // remove --force` from the owning repo's cwd. Registry must be clean.
    let home = tmp_home("p0x-registry-happy");
    let repo = tmp_repo("p0x-registry-happy-repo");
    let _l = lease_bound(&home, &repo, "agent-r", "feat/registry");

    let outcome = release_full(&home, "agent-r", false);
    assert!(outcome.released);
    assert!(outcome.worktree_removed);
    assert_no_prunable_registry(&repo, "happy-path");

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn p0x_release_full_prunes_registry_after_external_dir_removal() {
    // Reviewer's exact failure mode: the worktree dir gets removed
    // externally (daemon crash mid-op, manual `rm`), so when `release_full`
    // runs the dir is already gone but the git registry still lists the
    // path as `prunable`. Without the explicit `git worktree prune` call
    // in the missing-path branch, the next lease re-attempt fails because
    // the registry sees the path as still claimed.
    //
    // This is the load-bearing regression-proof for the r1 fix:
    // commenting out the `git worktree prune` block in `release_full`'s
    // missing-path branch makes this test FAIL on the post-release
    // assertion. Restore → PASS.
    let home = tmp_home("p0x-registry-prune");
    let repo = tmp_repo("p0x-registry-prune-repo");
    let l = lease_bound(&home, &repo, "agent-rm", "feat/prune");

    // Simulate the leak: yank the worktree dir behind git's back.
    std::fs::remove_dir_all(&l.path).ok();
    assert!(!l.path.exists(), "test setup: dir must be gone");

    // Pre-condition sanity: registry MUST list the now-missing entry as
    // `prunable` before release_full runs. If git's behavior changes and
    // this assertion no longer holds, the test setup is no longer
    // exercising the bug — flag it via panic in the assertion.
    let pre_output = std::process::Command::new("git")
        .current_dir(&repo)
        .args(["worktree", "list", "--porcelain"])
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("git worktree list pre");
    let pre_stdout = String::from_utf8_lossy(&pre_output.stdout).to_string();
    assert!(
        pre_stdout.contains("prunable"),
        "test setup invariant: dir-removed worktree must show as prunable pre-release. Output:\n{pre_stdout}"
    );

    let outcome = release_full(&home, "agent-rm", false);
    assert!(outcome.released);
    assert!(outcome.binding_removed);

    // Post-condition: prune must have run, registry is clean.
    assert_no_prunable_registry(&repo, "post-external-rm");

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn p0x_release_full_via_handle_release_worktree_end_to_end() {
    // Production smoke (§5): exercise the full MCP path —
    // `handle_release_worktree(home, args, sender)` — the same function
    // the daemon dispatches `release_worktree` calls into. Asserts that
    // a leased agent + worktree gets fully cleaned up via the MCP layer.
    let home = tmp_home("p0x-prod-smoke");
    let repo = tmp_repo("p0x-prod-smoke-repo");
    let l = lease_bound(&home, &repo, "agent-prod", "feat/prod");
    assert!(l.path.exists());

    let result = crate::mcp::handlers::worktree_test_release(
        &home,
        &serde_json::json!({"instance": "agent-prod"}),
    );
    assert_eq!(result["released"].as_bool(), Some(true), "{result}");
    assert_eq!(result["worktree_removed"].as_bool(), Some(true), "{result}");
    assert_eq!(result["binding_removed"].as_bool(), Some(true), "{result}");
    assert!(!l.path.exists(), "worktree must be removed by MCP path");
    assert!(crate::binding::read(&home, "agent-prod").is_none());

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn is_daemon_managed_excludes_human_worktrees() {
    let dir = tmp_home("human-wt");
    // No marker → not managed.
    assert!(!is_daemon_managed(&dir));
    // Add marker → managed.
    std::fs::write(dir.join(MANAGED_MARKER), "test").ok();
    assert!(is_daemon_managed(&dir));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn pin_unpin_idempotent() {
    let dir = tmp_home("pin");
    pin(&dir);
    assert!(is_pinned(&dir));
    pin(&dir); // idempotent
    assert!(is_pinned(&dir));
    unpin(&dir);
    assert!(!is_pinned(&dir));
    unpin(&dir); // idempotent
    assert!(!is_pinned(&dir));
    std::fs::remove_dir_all(&dir).ok();
}

// ── Phase 4 GC tests ────────────────────────────────────────────

fn make_gc_candidate(home: &Path, agent: &str) -> PathBuf {
    let wt = home
        .join("workspace")
        .join("repo")
        .join(".worktrees")
        .join(agent);
    std::fs::create_dir_all(&wt).ok();
    // Daemon-managed marker with old timestamp (past grace).
    let old_ts = (chrono::Utc::now() - chrono::Duration::hours(48)).to_rfc3339();
    std::fs::write(
        wt.join(MANAGED_MARKER),
        format!("agent={agent}\nleased_at={old_ts}\nreleased_at={old_ts}\n"),
    )
    .ok();
    wt
}

#[test]
fn gc_candidates_includes_only_daemon_tagged() {
    let home = tmp_home("gc-tagged");
    let wt = home
        .join("workspace")
        .join("repo")
        .join(".worktrees")
        .join("human");
    std::fs::create_dir_all(&wt).ok();
    // No .agend-managed marker → not a candidate.
    let candidates = gc_candidates(&home);
    assert!(
        candidates.is_empty(),
        "human worktree must not be candidate"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #2234 Phase 2 gc-safety (r6 #2269 observation): a MARKER-LESS
/// `workspace/<agent>` gitlink worktree — a real worktree (`.git` is a gitlink
/// FILE) but missing the `.agend-managed` marker (e.g. an interrupted
/// reconcile) — is enumerated by the new (B) `workspace_gitlink_worktrees`
/// scan (gitlink-alone gate), so it DOES reach `evaluate_candidate`; it MUST
/// NOT become a GC candidate because the `is_daemon_managed` marker-gate
/// rejects it. The sibling `gc_candidates_includes_only_daemon_tagged` only
/// covers a NESTED `.worktrees/human` dir that the marker-walk collect stage
/// filters BEFORE evaluate, so it never exercises this workspace-gitlink path.
#[test]
fn gc_candidates_excludes_marker_less_workspace_gitlink_2234() {
    let home = tmp_home("gc-markerless-ws");
    let repo = tmp_repo("gc-markerless-ws-repo");
    // Same fixture as `managed_workspace_worktree`, minus the marker write.
    let ws = managed_workspace_worktree(&home, &repo, "deve", "feat/markerless");
    std::fs::remove_file(ws.join(MANAGED_MARKER)).expect("drop marker");
    assert!(
        ws.join(".git").is_file(),
        "fixture is a real gitlink worktree"
    );
    // Precondition: the (B) scan DOES enumerate it (so it reaches evaluate).
    assert!(
        fs_managed_worktrees(&home).iter().any(|p| p == &ws),
        "marker-less workspace gitlink must be enumerated (reaches evaluate_candidate)"
    );
    // Property under test: the marker-gate keeps it OUT of the candidate set.
    let candidates = gc_candidates(&home);
    assert!(
        candidates.iter().all(|c| c.path != ws),
        "marker-less workspace gitlink must NOT be a GC candidate: {candidates:?}"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn gc_candidates_excludes_pinned() {
    let home = tmp_home("gc-pinned");
    let wt = make_gc_candidate(&home, "pinned-agent");
    pin(&wt);
    let candidates = gc_candidates(&home);
    assert!(candidates.is_empty(), "pinned must not be candidate");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn gc_candidates_respects_grace_ttl() {
    let home = tmp_home("gc-grace");
    let wt = home
        .join("workspace")
        .join("repo")
        .join(".worktrees")
        .join("fresh");
    std::fs::create_dir_all(&wt).ok();
    // Recent timestamp (within grace).
    let recent = chrono::Utc::now().to_rfc3339();
    std::fs::write(
        wt.join(MANAGED_MARKER),
        format!("agent=fresh\nleased_at={recent}\nreleased_at={recent}\n"),
    )
    .ok();
    let candidates = gc_candidates(&home);
    assert!(
        candidates.is_empty(),
        "fresh worktree within grace must not be candidate"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// §3.9 #1870 (H1): a worktree whose `.agend-managed` `released_at=` is
/// MALFORMED (e.g. a partial-write / crash-truncated marker) must NOT be
/// reclaimed — the grace window protects just-released WIP, so a parse
/// failure fails conservative (skip GC). A valid PAST-grace `released_at`
/// still yields a candidate (behavior unchanged). Regression-proof: revert
/// the fix and the malformed worktree falls through to a CleanRelease
/// candidate, so `bad-ts` appears.
#[test]
fn gc_candidates_skips_malformed_released_at_1870() {
    let home = tmp_home("gc-malformed-ts");
    // Malformed released_at + a RECENT lease → must be kept. #1870 stopped the
    // immediate grace-bypass reclaim; #1882 (WT-LEAK-1) then routes a corrupt
    // marker to the force-reclaim backstop — but its leased_at age-cap still
    // protects a recently-leased (possibly still-in-use) worktree. So a recent
    // `leased_at` here stays NOT a candidate (an ABANDONED corrupt marker IS
    // reclaimed — see force_reclaim_corrupt_marker_* tests).
    let recent = chrono::Utc::now().to_rfc3339();
    let bad = home
        .join("workspace")
        .join("repo")
        .join(".worktrees")
        .join("bad-ts");
    std::fs::create_dir_all(&bad).ok();
    std::fs::write(
        bad.join(MANAGED_MARKER),
        format!("agent=bad-ts\nleased_at={recent}\nreleased_at=not-a-timestamp\n"),
    )
    .ok();
    // Valid past-grace released_at → still a candidate (unchanged).
    make_gc_candidate(&home, "good-ts");

    let agents: Vec<String> = gc_candidates(&home).into_iter().map(|c| c.agent).collect();
    assert!(
        !agents.iter().any(|a| a == "bad-ts"),
        "#1870/#1882: a malformed released_at on a RECENT lease must NOT be reclaimed (age-cap protects it), got: {agents:?}"
    );
    assert!(
        agents.iter().any(|a| a == "good-ts"),
        "#1870: a valid past-grace released_at must STILL yield a candidate (unchanged), got: {agents:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn gc_candidates_excludes_active_binding() {
    let home = tmp_home("gc-active");
    make_gc_candidate(&home, "active-agent");
    // Create active binding.
    crate::binding::bind(&home, "active-agent", "T-1", "feat");
    let candidates = gc_candidates(&home);
    assert!(candidates.is_empty(), "active binding must exclude from GC");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn dry_run_no_actual_delete() {
    let home = tmp_home("gc-dry");
    let wt = make_gc_candidate(&home, "dry-agent");
    gc_dry_run(&home);
    assert!(wt.exists(), "dry-run must NOT delete");
    std::fs::remove_dir_all(&home).ok();
}

// ------------------------------------------------------------------
// Sprint 57 Wave 2 Track B (#546 Item 2) — release_worktree must
// unsubscribe the released agent from EVERY ci-watch they appear
// on, not just the binding-branch entry.
// ------------------------------------------------------------------

/// Helper: write a synthetic ci-watch JSON listing the given
/// subscribers on `(repo, branch)`. Returns the watch path.
fn write_ci_watch(
    home: &std::path::Path,
    repo: &str,
    branch: &str,
    subscribers: &[&str],
) -> PathBuf {
    write_ci_watch_with_extras(home, repo, branch, subscribers, None, None)
}

/// #931: variant that also stores `next_after_ci` (workflow chain) and
/// `last_notified_head_sha` (polling state). Used by the decouple-fix
/// tests to assert release_full preserves these fields.
fn write_ci_watch_with_extras(
    home: &std::path::Path,
    repo: &str,
    branch: &str,
    subscribers: &[&str],
    next_after_ci: Option<&str>,
    last_notified_head_sha: Option<&str>,
) -> PathBuf {
    let ci_dir = crate::daemon::ci_watch::ci_watches_dir(home);
    std::fs::create_dir_all(&ci_dir).ok();
    let filename = crate::daemon::ci_watch::watch_filename(repo, branch);
    let path = ci_dir.join(&filename);
    let subs: Vec<serde_json::Value> = subscribers
        .iter()
        .map(|s| serde_json::json!({"instance": *s}))
        .collect();
    let mut watch = serde_json::json!({
        "repo": repo,
        "branch": branch,
        "interval_secs": 60,
        "subscribers": subs,
        "instance": subscribers.first().copied().unwrap_or(""),
        "last_run_id": 12345_u64,
        "head_sha": "deadbeefcafe",
        "last_polled_at": chrono::Utc::now().timestamp_millis(),
        "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
        "last_terminal_seen_at": null,
    });
    if let Some(n) = next_after_ci {
        watch["next_after_ci"] = serde_json::json!(n);
    }
    if let Some(sha) = last_notified_head_sha {
        watch["last_notified_head_sha"] = serde_json::json!(sha);
    }
    std::fs::write(&path, serde_json::to_string_pretty(&watch).unwrap()).ok();
    path
}

/// #931 helper: read a watch field as string (returns None if absent or
/// not a string). Used by decouple tests to assert state preservation.
fn read_ci_watch_field(path: &std::path::Path, field: &str) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&content).ok()?;
    v.get(field)?.as_str().map(String::from)
}

/// Read a ci-watch JSON's subscriber `instance` strings. Returns
/// empty Vec if file missing or parse fails — `assert` on the
/// caller handles the missing-file case as appropriate.
fn read_ci_watch_subscribers(path: &std::path::Path) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(watch) = serde_json::from_str::<serde_json::Value>(&content) else {
        return Vec::new();
    };
    crate::daemon::ci_watch::parse_subscribers(&watch)
}

#[test]
fn release_worktree_unsubscribes_all_agent_ci_watches() {
    // #931 INVERTED (was Sprint 57 Wave 2 Track B #546 Item 2 pin).
    //
    // Pre-#931: release_full unconditionally swept the released agent
    // out of EVERY ci-watch they appeared on (binding-branch + ad-hoc).
    // That cleanup cascaded to watch-file deletion when the released
    // agent was the sole subscriber, destroying `next_after_ci`
    // chains and polling state — 4-in-a-row PR stalls
    // (#920/#925/#928/#929) traced to this exact path.
    //
    // Post-#931 (Direction A.1): release_full no longer mutates any
    // ci-watch on the agent's behalf. Subscriptions persist across
    // release per operator intent in issue #931:
    //   "Subscription persists across bind handoff unless explicitly
    //    `unwatch`ed."
    //
    // Hygiene is delegated to:
    //   - 72h absolute TTL (`expires_at`)
    //   - 72h inactivity TTL (`last_terminal_seen_at`)
    //   - PR-terminal auto-clear (poller's `check_pr_terminal`)
    //   - Explicit `ci action=unwatch` (operator-callable)
    //
    // This test now PINS the new persist-across-release behavior so
    // a regression that re-introduces the broad sweep is caught
    // immediately. Rollback criteria documented in PR #931 body.
    let home = tmp_home("931-persist-multi");
    let repo = tmp_repo("931-persist-multi-repo");
    let l = lease_bound(&home, &repo, "dev", "feat-track-x");
    assert!(l.path.exists(), "pre: worktree must exist");

    let auto_watch = write_ci_watch(&home, "owner/repo", "feat-track-x", &["dev", "lead"]);
    let main_watch = write_ci_watch(&home, "owner/repo", "main", &["dev", "lead"]);
    let bystander = write_ci_watch(&home, "owner/repo", "feat-bystander", &["lead"]);

    let outcome = release_full(&home, "dev", false);

    assert!(outcome.released, "release must succeed");
    assert!(outcome.binding_removed, "binding must be cleared");

    // Auto-watch (binding-branch): dev MUST STILL be subscribed.
    let auto_subs = read_ci_watch_subscribers(&auto_watch);
    assert!(
        auto_subs.contains(&"dev".to_string()),
        "#931: dev must persist on binding-branch watch — got {auto_subs:?}"
    );
    assert!(
        auto_subs.contains(&"lead".to_string()),
        "lead untouched on binding-branch watch — got {auto_subs:?}"
    );

    // Ad-hoc cross-branch watch on main: dev MUST STILL be subscribed.
    let main_subs = read_ci_watch_subscribers(&main_watch);
    assert!(
        main_subs.contains(&"dev".to_string()),
        "#931: dev must persist on ad-hoc main watch — got {main_subs:?}"
    );

    // Bystander: untouched (dev never subscribed).
    assert!(bystander.exists(), "bystander watch must survive untouched");

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn release_worktree_deletes_watch_when_last_subscriber_unsubscribes() {
    // #931 INVERTED (was P0-X bonus delete-on-empty pin).
    //
    // Pre-#931: when the released agent was the sole subscriber,
    // release_full deleted the watch file entirely — losing
    // `next_after_ci`, `last_notified_head_sha`, polling state.
    // Post-#931: file persists across release. Cleanup via TTL
    // and PR-terminal paths only.
    let home = tmp_home("931-persist-sole");
    let repo = tmp_repo("931-persist-sole-repo");
    let _l = lease(&home, &repo, "dev", "feat-x").expect("lease");

    let solo_watch = write_ci_watch(&home, "owner/repo", "main", &["dev"]);

    release_full(&home, "dev", false);

    assert!(
        solo_watch.exists(),
        "#931: sole-subscriber watch must persist across release (TTL handles cleanup)"
    );
    // Subs should still contain dev — pure persistence.
    let subs = read_ci_watch_subscribers(&solo_watch);
    assert!(
        subs.contains(&"dev".to_string()),
        "#931: dev persists in subs across release — got {subs:?}"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

// ── #931: ci_watch decouple from worktree release lifecycle ─────────
//
// Issue: 4-in-a-row PR stalls overnight (#920/#925/#928/#929) traced
// to `release_full` calling `unsubscribe_all_ci_watches_for_agent`,
// which removed the released agent from every ci-watch (binding-branch
// and ad-hoc), cascading to watch-file deletion on sole-subscriber.
// The cascade destroyed `next_after_ci` chains + polling state, so
// reviewer/dev never received post-CI handoff notifications.
//
// Direction A.1 (operator-approved 2026-05-19): decouple subscription
// from worktree binding entirely. Hygiene via 72h TTL + PR-terminal
// auto-clear + explicit unwatch only.
//
// RED→GREEN regression-proof anchors: each test below documents the
// pre-fix failure signature; if the call at the historic
// `unsubscribe_all_ci_watches_for_agent` site is re-introduced, these
// tests immediately fail.

#[test]
fn release_does_not_delete_ci_watch_when_agent_was_sole_subscriber_931() {
    // Anchor: pre-#931 release_full ran `remove_file(&path)` when subs
    // became empty (`unsubscribe_all_ci_watches_for_agent`,
    // `worktree_pool.rs:464-468`). The watch file gone → poller skipped
    // → `next_after_ci` target never injected. Post-#931 the file
    // persists with full state.
    let home = tmp_home("931-sole-persist");
    let repo = tmp_repo("931-sole-persist-repo");
    let _l = lease(&home, &repo, "dev", "feat/931-sole").expect("lease");

    let watch_path = write_ci_watch_with_extras(
        &home,
        "owner/repo",
        "feat/931-sole",
        &["dev"],
        Some("reviewer"),
        Some("cafe1234"),
    );
    assert!(watch_path.exists(), "pre: watch exists");

    release_full(&home, "dev", false);

    assert!(
        watch_path.exists(),
        "#931 GREEN: sole-subscriber watch file MUST persist across release"
    );
    assert_eq!(
        read_ci_watch_field(&watch_path, "next_after_ci"),
        Some("reviewer".to_string()),
        "#931 GREEN: next_after_ci chain MUST survive release"
    );
    assert_eq!(
        read_ci_watch_field(&watch_path, "last_notified_head_sha"),
        Some("cafe1234".to_string()),
        "#931 GREEN: polling state MUST survive release"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn release_does_not_remove_agent_from_multi_subscriber_watch_931() {
    // Anchor: pre-#931, retain(|s| s != agent) shrank subscriber lists
    // on EVERY watch the released agent appeared on, including non-
    // binding-branch ad-hoc watches (e.g. agent watching `main` to
    // follow upstream during closeout). Post-#931, no subscriber list
    // is mutated on release — operator's stated direction is full
    // persistence.
    let home = tmp_home("931-multi-persist");
    let repo = tmp_repo("931-multi-persist-repo");
    let _l = lease(&home, &repo, "dev", "feat/binding").expect("lease");

    let binding_watch = write_ci_watch(&home, "owner/repo", "feat/binding", &["dev", "reviewer"]);
    let other_watch = write_ci_watch(&home, "owner/repo", "feat/other", &["dev"]);

    release_full(&home, "dev", false);

    // Binding branch watch: dev preserved alongside reviewer.
    let binding_subs = read_ci_watch_subscribers(&binding_watch);
    assert!(
        binding_subs.contains(&"dev".to_string()),
        "#931 GREEN: dev preserved on binding-branch watch — got {binding_subs:?}"
    );
    assert!(
        binding_subs.contains(&"reviewer".to_string()),
        "co-subscriber preserved — got {binding_subs:?}"
    );

    // Non-binding branch watch: dev preserved untouched.
    let other_subs = read_ci_watch_subscribers(&other_watch);
    assert!(
        other_subs.contains(&"dev".to_string()),
        "#931 GREEN: dev preserved on non-binding-branch ad-hoc watch — got {other_subs:?}"
    );
    assert!(
        other_watch.exists(),
        "non-binding-branch watch file preserved"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn release_under_rebase_mode_preserves_subscription_931() {
    // #931 Fix 1 corollary: when `bind_self(rebase_mode=true)` triggers
    // `rebase_clean_self` (force_release/mod.rs:187), the underlying
    // `release_full` call MUST preserve the ci-watch so that the
    // immediately-following `dispatch_auto_bind_lease` re-arms the
    // existing watch via append-idempotent handle_watch_ci — keeping
    // any prior `next_after_ci` chain intact.
    //
    // Pre-#931: rebase_clean_self → release_full → file deleted
    // (sole-sub case) → re-dispatch creates fresh watch missing
    // next_after_ci → reviewer never gets [ci-ready-for-action].
    //
    // Post-#931: file persists across the rebase round-trip; the
    // re-dispatch sees the same watch JSON and appends; chain intact.
    //
    // This test exercises the release-half of the rebase cycle
    // directly (calling release_full is what rebase_clean_self does
    // internally). The full bind_self(rebase_mode=true) round-trip
    // is covered by the dispatch_hook test for next_after_ci wiring
    // (test 6) — those two together pin both halves.
    let home = tmp_home("931-rebase");
    let repo = tmp_repo("931-rebase-repo");
    let _l = lease(&home, &repo, "dev", "feat/rebase-cycle").expect("lease");

    let watch_path = write_ci_watch_with_extras(
        &home,
        "owner/repo",
        "feat/rebase-cycle",
        &["dev"],
        Some("reviewer"),
        Some("beefcafe"),
    );

    // Release (the rebase_clean_self path's release_full invocation).
    release_full(&home, "dev", false);

    // File persists with next_after_ci + state intact across release.
    assert!(
        watch_path.exists(),
        "#931 GREEN: rebase-path release_full must preserve watch file"
    );
    assert_eq!(
        read_ci_watch_field(&watch_path, "next_after_ci"),
        Some("reviewer".to_string()),
        "#931 GREEN: next_after_ci chain survives rebase-path release"
    );
    assert_eq!(
        read_ci_watch_field(&watch_path, "last_notified_head_sha"),
        Some("beefcafe".to_string()),
        "#931 GREEN: polling state survives rebase-path release"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn explicit_unwatch_wins_over_concurrent_release_931() {
    // #931 race invariant (§3.20 SOP 1 deterministic): when an operator
    // explicitly unsubscribes an agent (`ci action=unwatch`) AND a
    // concurrent release_full fires, the explicit-unwatch's destructive
    // intent (drop agent from subs; remove watch if sole) MUST be the
    // surviving outcome regardless of arrival order.
    //
    // Post-#931 Fix 1 the race is degenerate by construction:
    // release_full is a no-op against ci-watch state, so the explicit
    // unwatch alone decides the outcome. This test pins that property
    // so a future regression that re-introduces release-side mutation
    // (or worse, race-with-unwatch double-write) is caught.
    let home = tmp_home("931-unwatch-vs-release");
    let repo = tmp_repo("931-unwatch-vs-release-repo");
    let _l = lease(&home, &repo, "dev", "feat/unwatch-race").expect("lease");

    let watch_path = write_ci_watch(&home, "owner/repo", "feat/unwatch-race", &["dev"]);

    // Order 1: release then explicit unwatch via direct file mutation
    // (mirrors what `handle_unwatch_ci`'s last-subscriber path does:
    // remove the watch file). Deterministic — no sleep, no threads.
    release_full(&home, "dev", false);
    assert!(
        watch_path.exists(),
        "release_full is no-op for ci-watch post-#931"
    );

    // Simulate explicit unwatch: agent's removal cascades to file
    // deletion (sole-subscriber path of handle_unwatch_ci).
    let _ = std::fs::remove_file(&watch_path);

    assert!(
        !watch_path.exists(),
        "#931: explicit unwatch wins → watch file gone after both ops"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn poll_tick_vs_subscriber_mutation_preserves_single_delivery_931() {
    // #931 race invariant (§3.20 SOP 1 deterministic): a poll cycle
    // reading the watch file MUST see a consistent subscriber list
    // even if release_full or handle_watch_ci (subscribe) interleaves.
    //
    // Post-#931 Fix 1, release_full does not mutate ci-watch state →
    // the only mutating writer on this file is `handle_watch_ci`
    // (append) and `handle_unwatch_ci` (shrink/delete). All use
    // `crate::store::atomic_write` so a half-written file is never
    // observed by a concurrent reader (atomicity == temp-file +
    // rename invariant).
    //
    // Determinism: this test does NOT spawn threads. Instead it
    // exercises the read-modify-write contract sequentially and
    // asserts the file's parseability + subscriber stability invariant
    // at each step. SOP 1 pattern — no sleeps, no joins.
    let home = tmp_home("931-poll-mut-race");
    let repo = tmp_repo("931-poll-mut-race-repo");
    let _l = lease(&home, &repo, "dev", "feat/poll-mut").expect("lease");

    let watch_path = write_ci_watch(&home, "owner/repo", "feat/poll-mut", &["dev", "reviewer"]);

    // Snapshot 1: pre-release reading must observe both subscribers
    // and be a fully-parseable JSON (atomic-write invariant).
    let snap1 = read_ci_watch_subscribers(&watch_path);
    assert_eq!(snap1.len(), 2, "pre-release snapshot: 2 subscribers");

    // Release fires — must not corrupt file or strip subscribers.
    release_full(&home, "dev", false);

    // Snapshot 2: post-release reading STILL parses + STILL has both.
    let snap2 = read_ci_watch_subscribers(&watch_path);
    assert_eq!(
        snap1, snap2,
        "#931: release_full preserves subscriber list (poll reader sees stable state)"
    );

    // File still atomically parseable (no partial write).
    let content = std::fs::read_to_string(&watch_path).expect("readable");
    let parsed: serde_json::Value = serde_json::from_str(&content).expect("parseable JSON");
    assert_eq!(parsed["branch"].as_str(), Some("feat/poll-mut"));

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn released_agent_still_receives_ci_pass_inject_931() {
    // #931 MANDATORY INTEGRATION TEST.
    //
    // End-to-end: after release_full, the agent's subscription on
    // the binding branch's ci-watch MUST persist so that a subsequent
    // CI-pass poll cycle still enqueues `[ci-pass]` to their inbox.
    // Pre-#931 this was impossible because release_full stripped the
    // agent and (in sole-subscriber case) deleted the file entirely.
    //
    // Note on harness: this test exercises the SUBSCRIPTION half of
    // the integration (release → subs preserved → file ready to be
    // polled), not the full HTTP→provider→enqueue chain (that's
    // already covered by `mock_success_run_updates_watch_state` and
    // others in poller.rs#tests, which use the in-process MockCiProvider).
    // The decouple fix is purely about subscriber-state preservation
    // across release; the poll path is unchanged.
    //
    // Specifically: we assert that immediately after release_full,
    // (a) the watch file exists, (b) the released agent is still in
    // subscribers, (c) the next_after_ci chain is intact, (d) the
    // poll-state fields haven't been clobbered. If all four hold,
    // the next ci_check_repo invocation by the daemon's tick loop
    // will fan out [ci-pass] to the agent verbatim — same code path
    // as the unchanged poller tests verify.
    let home = tmp_home("931-integration-still-receives");
    let repo = tmp_repo("931-integration-still-receives-repo");
    let _l = lease(&home, &repo, "dev", "feat/integration").expect("lease");

    // Pre-state: ci-watch armed with dev as sole subscriber + chain.
    let watch_path = write_ci_watch_with_extras(
        &home,
        "owner/repo",
        "feat/integration",
        &["dev"],
        Some("reviewer"),
        Some("cafefeed"),
    );

    // The operator's pattern: dev pushes PR + releases worktree
    // (frees for next task), expects CI-pass notification later.
    release_full(&home, "dev", false);

    // INTEGRATION ASSERTIONS — all four conditions for the poll
    // pipeline to fan out [ci-pass] to dev's inbox:
    assert!(
        watch_path.exists(),
        "#931 GREEN: (a) watch file present after release"
    );

    let subs = read_ci_watch_subscribers(&watch_path);
    assert!(
        subs.contains(&"dev".to_string()),
        "#931 GREEN: (b) dev still in subscribers — got {subs:?}"
    );

    assert_eq!(
        read_ci_watch_field(&watch_path, "next_after_ci"),
        Some("reviewer".to_string()),
        "#931 GREEN: (c) next_after_ci chain intact"
    );

    // Polling state: last_notified_head_sha preserved (so dedup +
    // rerun detection both keep working).
    assert_eq!(
        read_ci_watch_field(&watch_path, "last_notified_head_sha"),
        Some("cafefeed".to_string()),
        "#931 GREEN: (d) polling state preserved"
    );

    // Pre-#931, all four would fail in the sole-subscriber case
    // because the watch file was deleted entirely. The fact that the
    // existing poller test `mock_success_run_updates_watch_state`
    // demonstrates the [ci-pass] enqueue path works given a valid
    // watch file completes the end-to-end argument.

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn release_dry_run_does_not_mutate_subscribers_931() {
    // Defensive: dry_run=true is contract-defined as observation-only.
    // Pre-#931, even dry_run paths through release_full would invoke
    // the unsubscribe sweep (no dry_run gate around it). Post-#931
    // there's nothing to gate — but the test pins the invariant in
    // case future code re-introduces mutation on this path.
    let home = tmp_home("931-dry-run");
    let repo = tmp_repo("931-dry-run-repo");
    let _l = lease(&home, &repo, "dev", "feat/dry").expect("lease");

    let watch_path = write_ci_watch_with_extras(
        &home,
        "owner/repo",
        "feat/dry",
        &["dev", "reviewer"],
        Some("next-agent"),
        None,
    );
    let subs_before = read_ci_watch_subscribers(&watch_path);

    let outcome = release_full(&home, "dev", true);
    // dry_run skips actual git/binding teardown semantics elsewhere;
    // we only assert ci-watch state is identical pre/post.

    let subs_after = read_ci_watch_subscribers(&watch_path);
    assert_eq!(
        subs_before, subs_after,
        "#931: dry_run must not mutate subscriber list — before {subs_before:?} after {subs_after:?} outcome {outcome:?}"
    );
    assert_eq!(
        read_ci_watch_field(&watch_path, "next_after_ci"),
        Some("next-agent".to_string()),
        "#931: dry_run must preserve next_after_ci"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

// ── Issue #611: branch cleanup tests ────────────────────────────────

#[test]
fn release_full_deletes_merged_branch() {
    let home = tmp_home("611-merged");
    let repo = tmp_repo("611-merged-repo");
    // Lease creates the branch + worktree.
    let l = lease_bound(&home, &repo, "agent-611m", "feat/merged");
    // Add a commit on the feature branch via the worktree.
    std::process::Command::new("git")
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=t@t",
            "commit",
            "--allow-empty",
            "-m",
            "feat",
        ])
        .current_dir(&l.path)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .unwrap();
    // Merge feat/merged into main from the source repo (without checking it out).
    std::process::Command::new("git")
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=t@t",
            "merge",
            "feat/merged",
            "--no-ff",
            "-m",
            "merge",
        ])
        .current_dir(&repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .unwrap();

    let outcome = release_full(&home, "agent-611m", false);

    assert!(outcome.released);
    assert!(
        outcome.branch_deleted,
        "merged branch must be deleted: {:?}",
        outcome
    );
    assert!(outcome.branch_cleanup_skipped_reason.is_none());
    // Verify branch is actually gone from the repo.
    let branch_exists = std::process::Command::new("git")
        .args(["rev-parse", "--verify", "feat/merged"])
        .current_dir(&repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(
        !branch_exists,
        "branch must not exist in repo after cleanup"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn release_full_preserves_unmerged_branch() {
    let home = tmp_home("611-unmerged");
    let repo = tmp_repo("611-unmerged-repo");
    // Lease creates the branch + worktree.
    let l = lease_bound(&home, &repo, "agent-611u", "feat/unmerged");
    // Add a commit on the feature branch (not merged into main).
    std::process::Command::new("git")
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=t@t",
            "commit",
            "--allow-empty",
            "-m",
            "wip",
        ])
        .current_dir(&l.path)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .unwrap();

    let outcome = release_full(&home, "agent-611u", false);

    assert!(outcome.released);
    assert!(
        !outcome.branch_deleted,
        "unmerged branch must NOT be deleted"
    );
    // #P3: the fixture repo has NO github remote, so PR-merge detection is
    // Unknown → the split fail-closed keep reason (the pre-#P3 blanket "not
    // merged into main…" text was misleading — it implied a definitive verdict
    // when the check could not actually run). Either way the branch is KEPT.
    assert_eq!(
        outcome.branch_cleanup_skipped_reason.as_deref(),
        Some(
            "branch 'feat/unmerged': PR-merge detection unavailable (no github \
             remote, or gh/scm error) — kept, fail-closed; retried next sweep"
        )
    );
    // Verify branch still exists.
    let branch_exists = std::process::Command::new("git")
        .args(["rev-parse", "--verify", "feat/unmerged"])
        .current_dir(&repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(branch_exists, "unmerged branch must still exist in repo");

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn release_full_absent_worktree_merged_branch_cleaned_up() {
    let home = tmp_home("1249-absent-merged");
    let repo = tmp_repo("1249-absent-merged-repo");
    let l = lease_bound(&home, &repo, "agent-1249m", "feat/absent-merged");
    std::process::Command::new("git")
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=t@t",
            "commit",
            "--allow-empty",
            "-m",
            "feat",
        ])
        .current_dir(&l.path)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=t@t",
            "merge",
            "feat/absent-merged",
            "--no-ff",
            "-m",
            "merge",
        ])
        .current_dir(&repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .unwrap();
    // Remove worktree directory to simulate absent-worktree scenario.
    std::fs::remove_dir_all(&l.path).unwrap();
    std::process::Command::new("git")
        .args(["worktree", "prune"])
        .current_dir(&repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .unwrap();

    let outcome = release_full(&home, "agent-1249m", false);

    assert!(outcome.released);
    assert!(
        outcome.branch_deleted,
        "merged branch must be deleted even when worktree absent: {outcome:?}"
    );
    assert!(outcome.branch_cleanup_skipped_reason.is_none());
    let branch_exists = std::process::Command::new("git")
        .args(["rev-parse", "--verify", "feat/absent-merged"])
        .current_dir(&repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(!branch_exists, "branch must not exist after cleanup");
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn release_full_absent_worktree_unmerged_branch_preserved() {
    let home = tmp_home("1249-absent-unmerged");
    let repo = tmp_repo("1249-absent-unmerged-repo");
    let l = lease_bound(&home, &repo, "agent-1249u", "feat/absent-unmerged");
    std::process::Command::new("git")
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=t@t",
            "commit",
            "--allow-empty",
            "-m",
            "wip",
        ])
        .current_dir(&l.path)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .unwrap();
    // Remove worktree directory without merging.
    std::fs::remove_dir_all(&l.path).unwrap();
    std::process::Command::new("git")
        .args(["worktree", "prune"])
        .current_dir(&repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .unwrap();

    let outcome = release_full(&home, "agent-1249u", false);

    assert!(outcome.released);
    assert!(
        !outcome.branch_deleted,
        "unmerged branch must NOT be deleted"
    );
    // #P3: the fixture repo has NO github remote, so PR-merge detection is
    // Unknown → the split fail-closed keep reason (the pre-#P3 blanket "not
    // merged into main…" text was misleading — it implied a definitive
    // not-merged verdict when the check couldn't actually run). In production a
    // bound repo has a github remote, so a genuinely-unmerged branch there gets
    // the "is not merged into 'main'…" reason instead; either way it is KEPT.
    assert_eq!(
        outcome.branch_cleanup_skipped_reason.as_deref(),
        Some(
            "branch 'feat/absent-unmerged': PR-merge detection unavailable \
             (no github remote, or gh/scm error) — kept, fail-closed; retried next sweep"
        )
    );
    let branch_exists = std::process::Command::new("git")
        .args(["rev-parse", "--verify", "feat/absent-unmerged"])
        .current_dir(&repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(branch_exists, "unmerged branch must still exist");
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn release_full_dry_run_does_not_delete_branch() {
    let home = tmp_home("611-dryrun");
    let repo = tmp_repo("611-dryrun-repo");
    // Lease creates the branch + worktree.
    let l = lease_bound(&home, &repo, "agent-611d", "feat/dryrun");
    // Add a commit and merge into main.
    std::process::Command::new("git")
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=t@t",
            "commit",
            "--allow-empty",
            "-m",
            "feat",
        ])
        .current_dir(&l.path)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=t@t",
            "merge",
            "feat/dryrun",
            "--no-ff",
            "-m",
            "merge",
        ])
        .current_dir(&repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .unwrap();

    let outcome = release_full(&home, "agent-611d", true);

    assert!(outcome.released);
    assert!(!outcome.branch_deleted, "dry-run must NOT delete branch");
    assert_eq!(
        outcome.branch_cleanup_skipped_reason.as_deref(),
        Some("dry-run: would delete branch 'feat/dryrun'")
    );
    // Verify branch still exists.
    let branch_exists = std::process::Command::new("git")
        .args(["rev-parse", "--verify", "feat/dryrun"])
        .current_dir(&repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(branch_exists, "branch must survive dry-run");

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// §3.9 (#t-7, #1824 follow-up): a dry-run `release_full` must be
/// observation-only — it must NOT run the ref-mutating `git fetch --prune`
/// inside `cleanup_merged_branch`. Proven by planting a STALE
/// remote-tracking ref (`refs/remotes/origin/ghost`, absent on the real
/// origin) that a `fetch --prune` WOULD remove, then asserting it survives a
/// dry-run. Regression-proof: un-gate the fetch and `ghost` is pruned →
/// the ref set differs.
#[test]
fn dry_run_release_does_not_mutate_remote_tracking_refs_t7() {
    fn git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .expect("git")
    }
    fn refs_remotes(dir: &std::path::Path) -> String {
        String::from_utf8_lossy(&git(dir, &["for-each-ref", "refs/remotes"]).stdout).to_string()
    }

    let home = tmp_home("t7-dryrun-refs");
    // A real upstream + a clone (so the clone has an `origin` remote +
    // refs/remotes/origin/*). `release_full` operates on the clone.
    let origin = tmp_repo("t7-origin");
    let source = tmp_home("t7-source");
    git(
        std::path::Path::new("/"),
        &[
            "clone",
            &origin.display().to_string(),
            &source.display().to_string(),
        ],
    );
    // Plant a stale remote-tracking ref that `fetch --prune` would remove.
    let head = String::from_utf8_lossy(&git(&source, &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();
    git(&source, &["update-ref", "refs/remotes/origin/ghost", &head]);

    // Lease a worktree in the clone (binds source_repo=source); merge state
    // is irrelevant — the fetch runs BEFORE the merge check.
    let _l = lease_bound(&home, &source, "agent-t7", "feat/t7");

    let before = refs_remotes(&source);
    assert!(
        before.contains("refs/remotes/origin/ghost"),
        "pre-cond: stale ghost ref planted: {before}"
    );

    let outcome = release_full(&home, "agent-t7", true); // dry-run
    assert!(outcome.released, "dry-run reports observation success");

    let after = refs_remotes(&source);
    assert_eq!(
        before, after,
        "dry-run must NOT mutate remote-tracking refs (no fetch --prune)"
    );
    assert!(
        after.contains("refs/remotes/origin/ghost"),
        "the prune-target stale ref must survive a dry-run: {after}"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&origin).ok();
    std::fs::remove_dir_all(&source).ok();
}

#[test]
fn release_full_does_not_delete_unrelated_branch() {
    let home = tmp_home("unrelated-branch");
    let repo = tmp_repo("unrelated-branch-repo");
    // Create an unrelated user branch with its own commit
    std::process::Command::new("git")
        .args(["checkout", "-b", "user/my-feature"])
        .current_dir(&repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=t@t",
            "commit",
            "--allow-empty",
            "-m",
            "user work",
        ])
        .current_dir(&repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["checkout", "main"])
        .current_dir(&repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .unwrap();
    // Lease a different branch
    let _l = lease_bound(&home, &repo, "agent-x", "feat/daemon-task");
    let outcome = release_full(&home, "agent-x", false);
    assert!(outcome.released);
    // Unrelated branch must still exist
    let branch_exists = std::process::Command::new("git")
        .args(["rev-parse", "--verify", "user/my-feature"])
        .current_dir(&repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(
        branch_exists,
        "unrelated user branch must NOT be deleted by release_worktree"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn gc_new_layout_active_binding_not_candidate() {
    let home = tmp_home("gc-new-active");
    // Create new-layout worktree with active binding
    let wt = home.join("worktrees").join("dev-1").join("feat-branch");
    std::fs::create_dir_all(&wt).unwrap();
    let old = (chrono::Utc::now() - chrono::Duration::hours(100)).to_rfc3339();
    std::fs::write(
        wt.join(MANAGED_MARKER),
        format!("agent=dev-1\nbranch=feat-branch\nleased_at={old}\nreleased_at={old}\n"),
    )
    .unwrap();
    // Create active binding for dev-1
    let rt = crate::paths::runtime_dir(&home).join("dev-1");
    std::fs::create_dir_all(&rt).unwrap();
    std::fs::write(
        rt.join("binding.json"),
        r#"{"worktree":"/tmp/x","branch":"feat-branch"}"#,
    )
    .unwrap();

    let candidates = gc_candidates(&home);
    assert!(
        candidates.is_empty(),
        "new-layout worktree with active binding must not be GC candidate"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn gc_new_layout_released_past_grace_is_candidate() {
    let home = tmp_home("gc-new-released");
    let wt = home.join("worktrees").join("dev-2").join("old-branch");
    std::fs::create_dir_all(&wt).unwrap();
    let old = (chrono::Utc::now() - chrono::Duration::hours(100)).to_rfc3339();
    std::fs::write(
        wt.join(MANAGED_MARKER),
        format!("agent=dev-2\nbranch=old-branch\nleased_at={old}\nreleased_at={old}\n"),
    )
    .unwrap();
    // No binding for dev-2

    let candidates = gc_candidates(&home);
    assert_eq!(
        candidates.len(),
        1,
        "new-layout released worktree past grace should be GC candidate"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── #807 Item 2 — ReleaseOutcome serialization shape ──

#[test]
fn test_release_outcome_success_omits_error_key() {
    // #807 Item 2 RED: pre-fix `ReleaseOutcome` always serializes
    // `error: None` → `"error": null`, which client renderers
    // (Claude Code, etc.) interpret as an `<error>` envelope on
    // what is actually a successful release. Fix: add
    // `#[serde(skip_serializing_if = "Option::is_none")]` so the
    // `error` key is absent on success.
    let outcome = ReleaseOutcome {
        released: true,
        worktree_removed: true,
        binding_removed: true,
        branch_deleted: true,
        ..Default::default()
    };
    let json = serde_json::to_value(&outcome).expect("serialize");
    let obj = json.as_object().expect("object shape");
    assert!(
        !obj.contains_key("error"),
        "success response must NOT carry `error` key (#807 cosmetic fix), got keys: {:?}",
        obj.keys().collect::<Vec<_>>()
    );
    assert!(
        !obj.contains_key("branch_cleanup_skipped_reason"),
        "success response must NOT carry `branch_cleanup_skipped_reason` when None, got keys: {:?}",
        obj.keys().collect::<Vec<_>>()
    );
}

#[test]
fn test_release_outcome_real_failure_emits_error_key() {
    // #807 Item 2 contract guarantee: actual failures STILL emit
    // the `error` field. Only the `None`-on-success case is
    // omitted — `skip_serializing_if` only drops `None`, never
    // `Some`.
    let outcome = ReleaseOutcome {
        released: false,
        error: Some("test failure".to_string()),
        ..Default::default()
    };
    let json = serde_json::to_value(&outcome).expect("serialize");
    let obj = json.as_object().expect("object shape");
    assert!(
        obj.contains_key("error"),
        "real failure must surface `error` key, got keys: {:?}",
        obj.keys().collect::<Vec<_>>()
    );
    assert_eq!(
        obj["error"], "test failure",
        "error message must round-trip unchanged"
    );
}

// ── gc_run tests ──────────────────────────────────────────────

#[test]
fn gc_run_returns_empty_when_no_candidates() {
    let home = tmp_home("gc-run-empty");
    let results = gc_run(&home);
    assert!(results.is_empty());
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn gc_stale_ci_watch_locks_removes_old_locks() {
    let home = tmp_home("gc-locks");
    let ci_dir = home.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).unwrap();

    // Create a lock file with an old mtime (> 7 days ago)
    let stale_lock = ci_dir.join("pr-123.lock");
    std::fs::write(&stale_lock, "locked").unwrap();
    // Set mtime to 8 days ago
    let eight_days_ago =
        std::time::SystemTime::now() - std::time::Duration::from_secs(8 * 24 * 3600);
    let f = std::fs::File::options()
        .write(true)
        .open(&stale_lock)
        .unwrap();
    f.set_modified(eight_days_ago).unwrap();

    // Create a recent lock file (should NOT be removed)
    let recent_lock = ci_dir.join("pr-456.lock");
    std::fs::write(&recent_lock, "locked").unwrap();

    // Create a non-lock file (should NOT be removed)
    let json_file = ci_dir.join("pr-789.json");
    std::fs::write(&json_file, "{}").unwrap();

    let removed = gc_stale_ci_watch_locks(&home);
    assert_eq!(removed, 1, "only the stale lock should be removed");
    assert!(!stale_lock.exists(), "stale lock must be deleted");
    assert!(recent_lock.exists(), "recent lock must be preserved");
    assert!(json_file.exists(), "non-lock file must be preserved");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn gc_stale_ci_watch_locks_handles_missing_dir() {
    let home = tmp_home("gc-locks-nodir");
    let removed = gc_stale_ci_watch_locks(&home);
    assert_eq!(removed, 0);
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn resolve_source_repo_parses_gitdir_pointer() {
    let home = tmp_home("resolve-src");
    let fake_wt = home.join("wt");
    std::fs::create_dir_all(&fake_wt).unwrap();
    // Simulate .git file pointing to source/.git/worktrees/wt
    let source = home.join("source");
    let gitdir_target = source.join(".git").join("worktrees").join("wt");
    std::fs::create_dir_all(&gitdir_target).unwrap();
    std::fs::write(
        fake_wt.join(".git"),
        format!("gitdir: {}", gitdir_target.display()),
    )
    .unwrap();
    let resolved = resolve_source_repo(&fake_wt);
    assert!(resolved.is_some());
    assert_eq!(resolved.unwrap(), source);
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn resolve_source_repo_returns_none_for_regular_repo() {
    let home = tmp_home("resolve-none");
    let fake_dir = home.join("regular");
    std::fs::create_dir_all(&fake_dir).unwrap();
    // A regular .git directory, not a worktree
    std::fs::create_dir_all(fake_dir.join(".git")).unwrap();
    let resolved = resolve_source_repo(&fake_dir);
    assert!(resolved.is_none());
    std::fs::remove_dir_all(&home).ok();
}

// ── t-worktree-leak PR-2: force-reclaim backstop tests ──

fn backdate_lease(wt_path: &Path, days_ago: i64) {
    let marker = wt_path.join(MANAGED_MARKER);
    let content = std::fs::read_to_string(&marker).unwrap();
    let old = (chrono::Utc::now() - chrono::Duration::days(days_ago)).to_rfc3339();
    let new: String = content
        .lines()
        .map(|l| {
            if l.starts_with("leased_at=") {
                format!("leased_at={old}")
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&marker, new).unwrap();
}

#[test]
fn force_reclaim_dead_agent_past_cap_is_candidate() {
    let home = tmp_home("fr-dead");
    let repo = tmp_repo("fr-dead-repo");
    let lease = lease(&home, &repo, "dev-dead", "feat/x").expect("lease");
    backdate_lease(&lease.path, force_reclaim_age_days() + 2);
    let live: std::collections::HashSet<String> = std::collections::HashSet::new();
    let cand = evaluate_candidate(&home, &lease.path, &live);
    assert!(
        cand.is_some(),
        "dead agent, never-released, past cap → force-reclaim candidate"
    );
    assert_eq!(cand.unwrap().kind, GcKind::ForceReclaim);
    let _ = std::fs::remove_dir_all(&home);
}

/// PR-C follow-up ④ (8145a9): a FLAT depth-1 layout worktree
/// (`worktrees/<agent>-<enc-repo>/`, built by `repo action=checkout`, NOT the
/// nested `worktrees/<agent>/<branch>/` that `lease()` builds) whose agent is
/// dead and lease is aged past the force-reclaim cap MUST become a ForceReclaim
/// candidate — via `evaluate_candidate` directly AND through the full
/// `gc_candidates` enumeration. RED before the fix (the flat orphan leaked).
#[test]
fn force_reclaim_flat_layout_dead_agent_is_candidate() {
    let home = tmp_home("fr-flat");
    // Flat depth-1 dir directly under the managed root — no <agent>/<branch> nest.
    let flat = daemon_managed_worktree_root(&home)
        .join("claude-8145a9-_Users_suzuke_Documents_Hack_agend-terminal");
    std::fs::create_dir_all(&flat).unwrap();
    let old =
        (chrono::Utc::now() - chrono::Duration::days(force_reclaim_age_days() + 20)).to_rfc3339();
    // Marker mirrors the live specimen: authoritative agent=, aged leased_at,
    // NO released_at (never-released → force-reclaim arm).
    std::fs::write(
        flat.join(MANAGED_MARKER),
        format!("agent=claude-8145a9\nbranch=fix/medium-test-findings\nleased_at={old}\n"),
    )
    .unwrap();
    let live: std::collections::HashSet<String> = std::collections::HashSet::new(); // agent dead

    let direct = evaluate_candidate(&home, &flat, &live);
    assert!(
        direct.as_ref().map(|c| c.kind) == Some(GcKind::ForceReclaim),
        "flat-layout dead-agent aged orphan must be a ForceReclaim candidate (evaluate_candidate); got {direct:?}"
    );
    let cands = gc_candidates(&home);
    assert!(
        cands
            .iter()
            .any(|c| c.path == flat && c.kind == GcKind::ForceReclaim),
        "gc_candidates must enumerate + force-reclaim the flat orphan; got {cands:?}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// PR-C follow-up ④ (8145a9) RED: the ACTUAL leak — a HALF-ORPHANED flat
/// worktree whose `.git` gitlink points at a canonical `.git/worktrees/<name>`
/// entry that was already pruned, so `git status` in it FAILS. It IS a valid
/// ForceReclaim candidate, but `maybe_remove_candidate`'s pre-archive git-status
/// check errored → `Skipped{status_check_failed}` → it leaked forever. The fix
/// makes a status-check FAILURE never block a force-reclaim archive (fs-level +
/// recoverable). RED before the fix (Skipped); GREEN after (Removed + archived).
#[test]
fn force_reclaim_archives_half_orphaned_flat_worktree() {
    use crate::daemon::retention::worktrees::{maybe_remove_candidate, RemovalOutcome};
    let home = tmp_home("fr-orphan");
    let flat = daemon_managed_worktree_root(&home).join("claude-8145a9-enc");
    std::fs::create_dir_all(&flat).unwrap();
    // Binding-lock dir maybe_remove_candidate acquires (runtime_dir/<agent>/).
    std::fs::create_dir_all(crate::paths::runtime_dir(&home).join("claude-8145a9")).unwrap();
    let old =
        (chrono::Utc::now() - chrono::Duration::days(force_reclaim_age_days() + 20)).to_rfc3339();
    std::fs::write(
        flat.join(MANAGED_MARKER),
        format!("agent=claude-8145a9\nbranch=fix/x\nleased_at={old}\n"),
    )
    .unwrap();
    // Half-orphaned: gitlink → a canonical worktrees entry that does NOT exist →
    // `git status` errors (the 8145a9 leak's distinguishing trait).
    std::fs::write(
        flat.join(".git"),
        format!(
            "gitdir: {}/gone-repo/.git/worktrees/claude-8145a9-enc\n",
            home.display()
        ),
    )
    .unwrap();
    let live: std::collections::HashSet<String> = std::collections::HashSet::new();
    let cand = evaluate_candidate(&home, &flat, &live).expect("force-reclaim candidate");
    assert_eq!(cand.kind, GcKind::ForceReclaim);

    let outcome = maybe_remove_candidate(&home, &cand);
    assert!(
        matches!(outcome, RemovalOutcome::Removed),
        "half-orphaned flat force-reclaim must archive-to-trash (git-status failure \
         must not block force-reclaim), not Skip; got {outcome:?}"
    );
    assert!(
        !flat.exists(),
        "worktree dir must be moved out (archived to .trash) after reclaim"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// Append a malformed `released_at=` to a lease's marker and drop its binding
/// (a released worktree is unbound) — the #1882 WT-LEAK-1 corrupt-marker shape.
fn corrupt_released_at(home: &Path, agent: &str, wt_path: &Path) {
    crate::binding::unbind(home, agent);
    let marker = wt_path.join(MANAGED_MARKER);
    let mut content = std::fs::read_to_string(&marker).unwrap();
    content.push_str("released_at=not-a-timestamp\n");
    std::fs::write(&marker, content).unwrap();
}

/// §3.9 #1882 (WT-LEAK-1): a corrupt-`released_at` worktree that is ABANDONED
/// (no liveness, leased past the force-reclaim age cap) is now reclaimed via
/// the force-reclaim backstop — pre-fix it leaked forever (the clean-release
/// path returned None and the never-released arm was unreachable for a
/// `Some(garbage)` released_at). Regression-proof: revert the parse-Err
/// fall-through and this is None (leaked).
#[test]
fn force_reclaim_corrupt_marker_abandoned_is_candidate_1882() {
    let home = tmp_home("fr-corrupt-dead");
    let repo = tmp_repo("fr-corrupt-dead-repo");
    let lease = lease(&home, &repo, "dev-corrupt", "feat/x").expect("lease");
    corrupt_released_at(&home, "dev-corrupt", &lease.path);
    backdate_lease(&lease.path, force_reclaim_age_days() + 2);
    let live: std::collections::HashSet<String> = std::collections::HashSet::new();
    let cand = evaluate_candidate(&home, &lease.path, &live);
    assert_eq!(
        cand.map(|c| c.kind),
        Some(GcKind::ForceReclaim),
        "#1882: abandoned corrupt-marker worktree (no liveness, past cap) → force-reclaim, not leaked"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// §3.9 #1882 (WT-LEAK-1, no H1 regression): a corrupt-`released_at` worktree
/// whose agent has a LIVENESS signal is SPARED even past the age cap — the
/// force-reclaim liveness guard (not the unparseable grace window) protects a
/// worktree the operator may still be using.
#[test]
fn force_reclaim_corrupt_marker_spares_live_1882() {
    let home = tmp_home("fr-corrupt-live");
    let repo = tmp_repo("fr-corrupt-live-repo");
    let lease = lease(&home, &repo, "dev-corrupt-live", "feat/x").expect("lease");
    corrupt_released_at(&home, "dev-corrupt-live", &lease.path);
    backdate_lease(&lease.path, force_reclaim_age_days() + 2);
    let live: std::collections::HashSet<String> =
        ["dev-corrupt-live".to_string()].into_iter().collect();
    assert!(
        evaluate_candidate(&home, &lease.path, &live).is_none(),
        "#1882: a live agent's corrupt-marker worktree must be SPARED (no H1-style WIP destruction)"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn force_reclaim_spares_live_registry_agent() {
    // safety #1: any live signal → never reclaim, even past the cap.
    let home = tmp_home("fr-live");
    let repo = tmp_repo("fr-live-repo");
    let lease = lease(&home, &repo, "dev-live", "feat/x").expect("lease");
    backdate_lease(&lease.path, force_reclaim_age_days() + 2);
    let live: std::collections::HashSet<String> = ["dev-live".to_string()].into_iter().collect();
    assert!(
        evaluate_candidate(&home, &lease.path, &live).is_none(),
        "agent live in the registry → spared even past cap (liveness-AND-age)"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn force_reclaim_spares_ci_watch_subscriber() {
    // multi-signal: a ci-watch subscription is a liveness signal (not heartbeat).
    let home = tmp_home("fr-ciw");
    let repo = tmp_repo("fr-ciw-repo");
    let lease = lease(&home, &repo, "dev-ciw", "feat/x").expect("lease");
    backdate_lease(&lease.path, force_reclaim_age_days() + 2);
    let ci_dir = crate::daemon::ci_watch::ci_watches_dir(&home);
    std::fs::create_dir_all(&ci_dir).unwrap();
    std::fs::write(
        ci_dir.join("w.json"),
        serde_json::json!({
            "repo": "o/r", "branch": "feat/x",
            "subscribers": [{ "instance": "dev-ciw" }]
        })
        .to_string(),
    )
    .unwrap();
    let live: std::collections::HashSet<String> = std::collections::HashSet::new();
    assert!(
        evaluate_candidate(&home, &lease.path, &live).is_none(),
        "agent subscribed to a ci-watch → spared (multi-signal liveness, not just heartbeat)"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn force_reclaim_spares_recent_lease() {
    // dead agent but the lease is recent → not yet past the age cap.
    let home = tmp_home("fr-recent");
    let repo = tmp_repo("fr-recent-repo");
    let lease = lease(&home, &repo, "dev-recent", "feat/x").expect("lease");
    let live: std::collections::HashSet<String> = std::collections::HashSet::new();
    assert!(
        evaluate_candidate(&home, &lease.path, &live).is_none(),
        "recent lease → not yet reclaimable (age gate)"
    );
    let _ = std::fs::remove_dir_all(&home);
}

// codex gap ③: the heartbeat / PTY / waiting_on liveness signals + the
// read-failure → fail-toward-alive path (§3.9, safety-critical).

#[test]
fn force_reclaim_spares_recent_heartbeat() {
    let home = tmp_home("fr-hb");
    let repo = tmp_repo("fr-hb-repo");
    let agent = "fr-hb-agent";
    let lease = lease(&home, &repo, agent, "feat/x").expect("lease");
    backdate_lease(&lease.path, force_reclaim_age_days() + 2);
    crate::daemon::heartbeat_pair::update_with(agent, |p| {
        p.heartbeat_at_ms = crate::daemon::heartbeat_pair::now_ms();
    });
    let live: std::collections::HashSet<String> = std::collections::HashSet::new();
    assert!(
        evaluate_candidate(&home, &lease.path, &live).is_none(),
        "recent heartbeat → spared (heartbeat liveness signal)"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn force_reclaim_spares_recent_pty_input() {
    let home = tmp_home("fr-pty");
    let repo = tmp_repo("fr-pty-repo");
    let agent = "fr-pty-agent";
    let lease = lease(&home, &repo, agent, "feat/x").expect("lease");
    backdate_lease(&lease.path, force_reclaim_age_days() + 2);
    crate::daemon::heartbeat_pair::update_with(agent, |p| {
        p.last_input_at_ms = crate::daemon::heartbeat_pair::now_ms();
    });
    let live: std::collections::HashSet<String> = std::collections::HashSet::new();
    assert!(
        evaluate_candidate(&home, &lease.path, &live).is_none(),
        "recent PTY input → spared (PTY liveness signal)"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn force_reclaim_spares_declared_waiting_on() {
    let home = tmp_home("fr-wait");
    let repo = tmp_repo("fr-wait-repo");
    let agent = "fr-wait-agent";
    let lease = lease(&home, &repo, agent, "feat/x").expect("lease");
    backdate_lease(&lease.path, force_reclaim_age_days() + 2);
    crate::daemon::heartbeat_pair::update_with(agent, |p| {
        p.waiting_on_since_ms = Some(crate::daemon::heartbeat_pair::now_ms());
    });
    let live: std::collections::HashSet<String> = std::collections::HashSet::new();
    assert!(
        evaluate_candidate(&home, &lease.path, &live).is_none(),
        "declared waiting_on → spared (blocked-but-alive signal)"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn force_reclaim_ci_watch_read_failure_fails_alive() {
    let home = tmp_home("fr-ciwfail");
    let repo = tmp_repo("fr-ciwfail-repo");
    let agent = "fr-ciwfail-agent";
    let lease = lease(&home, &repo, agent, "feat/x").expect("lease");
    backdate_lease(&lease.path, force_reclaim_age_days() + 2);
    // An unparseable ci-watch file → the liveness read fails → fail-toward-alive.
    let ci_dir = crate::daemon::ci_watch::ci_watches_dir(&home);
    std::fs::create_dir_all(&ci_dir).unwrap();
    std::fs::write(ci_dir.join("corrupt.json"), "{ this is not json").unwrap();
    let live: std::collections::HashSet<String> = std::collections::HashSet::new();
    assert!(
        evaluate_candidate(&home, &lease.path, &live).is_none(),
        "unparseable ci-watch → fail-toward-alive → spared (never reclaim on uncertainty)"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn gc_run_force_reclaim_archives_never_hard_deletes() {
    // codex gap ① CRITICAL: the daemon gc_run/gc_remove_one path must route a
    // force-reclaim through the SAFE helper, never hard-delete. Proof: it is
    // ARCHIVED to .trash (recoverable) rather than removed — the old
    // `git worktree remove --force` would have left nothing behind.
    let home = tmp_home("fr-gcrun");
    let repo = tmp_repo("fr-gcrun-repo");
    let lease = lease(&home, &repo, "fr-gcrun-agent", "feat/x").expect("lease");
    let cand = GcCandidate {
        path: lease.path.clone(),
        agent: "fr-gcrun-agent".to_string(),
        reason: "fr".to_string(),
        kind: GcKind::ForceReclaim,
    };
    let result = gc_remove_one(&home, &cand);
    assert!(
        result.removed,
        "force-reclaim via gc_run should archive: {:?}",
        result.error
    );
    assert!(!lease.path.exists(), "worktree moved out");
    let trash = home.join(".trash").join("worktrees");
    assert!(
        std::fs::read_dir(&trash)
            .map(|d| d.flatten().count() > 0)
            .unwrap_or(false),
        "gc_run force-reclaim must ARCHIVE to .trash (recoverable), never hard-delete"
    );
    assert!(
        crate::binding::read(&home, "fr-gcrun-agent").is_none(),
        "binding unbound after force-reclaim"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// PR-D·D4 (spike-vet, NON-NEGOTIABLE): the UNCONDITIONAL `gc_remove_one`
/// archive-only invariant for ForceReclaim. t-worktree-leak PR-2's load-bearing
/// safety property — a ForceReclaim MUST NEVER be hard-deleted — currently rests
/// ONLY on the control-flow POSITION of the `kind == ForceReclaim` early-return
/// (gc.rs:706, before the hard-delete code). That is too fragile. This pins it
/// BEHAVIORALLY so it fail-LOUD even if the routing moves.
///
/// Unlike `gc_run_force_reclaim_archives_never_hard_deletes` (which uses a fresh
/// lease → a moved early-return trips the CleanRelease re-validation SKIP, not a
/// hard-delete), this backdates a dead-agent lease so EVERY hard-delete gate
/// WOULD pass: `evaluate_candidate` re-validation succeeds, the worktree's real
/// `.git` resolves its source repo, and `git worktree remove` would actually
/// hard-delete it. The archive-only routing is therefore the SOLE thing keeping
/// the dir recoverable — move it and the dir is hard-deleted → absent from
/// `.trash` → this test goes RED.
#[test]
fn force_reclaim_gc_remove_one_is_archive_only_unconditional() {
    let home = tmp_home("fr-archive-inv");
    let repo = tmp_repo("fr-archive-inv-repo");
    let lease = lease(&home, &repo, "fr-inv-agent", "feat/x").expect("lease");
    // Backdated + dead agent → this candidate passes EVERY hard-delete gate
    // (re-validation, source-repo resolve, `git worktree remove`). Only the
    // archive-only routing prevents an irrecoverable hard-delete.
    backdate_lease(&lease.path, force_reclaim_age_days() + 5);
    let live: std::collections::HashSet<String> = std::collections::HashSet::new();
    let cand = evaluate_candidate(&home, &lease.path, &live)
        .expect("backdated dead-agent never-released lease → ForceReclaim candidate");
    assert_eq!(
        cand.kind,
        GcKind::ForceReclaim,
        "precondition: the candidate must be a ForceReclaim"
    );

    let result = gc_remove_one(&home, &cand);
    assert!(
        result.removed,
        "ForceReclaim gc_remove_one must archive (Removed): {:?}",
        result.error
    );
    assert!(
        !lease.path.exists(),
        "worktree dir must leave its original path"
    );
    let trash = home.join(".trash").join("worktrees");
    let archived = std::fs::read_dir(&trash)
        .map(|d| d.flatten().count() > 0)
        .unwrap_or(false);
    assert!(
        archived,
        "INVARIANT: a ForceReclaim MUST be ARCHIVED to .trash (recoverable), NEVER \
         hard-deleted — the gc.rs:706 archive-only routing is load-bearing \
         (t-worktree-leak PR-2). An empty .trash ⇒ the dir was hard-deleted ⇒ the \
         routing regressed."
    );
    assert!(
        crate::binding::read(&home, "fr-inv-agent").is_none(),
        "binding unbound after force-reclaim archive"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// PR-D·D4 equivalence pin: the GC L3 judgment `evaluate_candidate` now delegates
/// to [`terminal_disposition`] reproduces the pre-D4 disposition table byte-for-
/// byte over the reachable `(ReclaimState, agent_alive)` domain. This locks the
/// exact `DispositionInput` `evaluate_candidate` builds (binding-absent GC path:
/// L0 pass-through, L1/L2 inert) AND the `Disposition → Option<GcKind>` map, so a
/// classifier drift fails LOUD here rather than silently changing which worktree
/// GC reclaims.
#[test]
fn gc_l3_delegation_equals_pre_d4_disposition() {
    use crate::worktree::disposition::{
        terminal_disposition, Disposition, DispositionInput, ReclaimState,
    };
    // The DispositionInput `evaluate_candidate` builds for the binding-absent GC
    // path — L0 pass-through, L1/L2 inert.
    let gc_input = |reclaim, agent_alive| DispositionInput {
        daemon_managed: true,
        pinned: false,
        in_use: Some(false),
        binding_present: false,
        release_decision: crate::daemon::auto_release::ReleaseDecision::SkipNotBound,
        releasable_by_invariant: false,
        agent_alive,
        reclaim,
    };
    // The map `evaluate_candidate` applies: Delete→CleanRelease, Archive→
    // ForceReclaim, Keep→None (Release is unreachable — binding_present=false).
    let to_kind = |d| match d {
        Disposition::Delete => Some(GcKind::CleanRelease),
        Disposition::Archive => Some(GcKind::ForceReclaim),
        Disposition::Keep | Disposition::Release => None,
    };

    // CleanRelease arm — past grace, clean, liveness INAPPLICABLE (Some(false)):
    // pre-D4 hard-deleted → CleanRelease candidate, regardless of real liveness.
    assert_eq!(
        to_kind(terminal_disposition(&gc_input(
            ReclaimState::CleanReleaseHardDelete,
            Some(false)
        ))),
        Some(GcKind::CleanRelease),
        "clean-release past grace → CleanRelease (hard-delete tier), the pre-D4 outcome"
    );
    // ForceReclaim arm — past boot+age. Dead → archive; live → spared (the L3
    // liveness gate now reproduces the pre-D4 native liveness-AND-age spare).
    assert_eq!(
        to_kind(terminal_disposition(&gc_input(
            ReclaimState::ForceReclaim,
            Some(false)
        ))),
        Some(GcKind::ForceReclaim),
        "abandoned (dead, past cap) → ForceReclaim archive tier, the pre-D4 outcome"
    );
    assert_eq!(
        to_kind(terminal_disposition(&gc_input(
            ReclaimState::ForceReclaim,
            Some(true)
        ))),
        None,
        "live agent past cap → spared (liveness-AND-age), the pre-D4 None"
    );
    // NotEligible (within grace / boot-grace / recent lease) → Keep → None.
    assert_eq!(
        to_kind(terminal_disposition(&gc_input(
            ReclaimState::NotEligible,
            Some(false)
        ))),
        None,
        "not eligible → spared, the pre-D4 None"
    );
}

#[test]
fn collect_managed_worktrees_finds_slash_branch_nested() {
    // reviewer-2 #4: a slash-branch worktree nests an extra level
    // (worktrees/<agent>/fix/xxx) and was missed by the old fixed-depth scan.
    let home = tmp_home("walk-slash");
    let root = daemon_managed_worktree_root(&home);
    let nested = root.join("dev").join("fix").join("xxx");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(nested.join(MANAGED_MARKER), "agent=dev\n").unwrap();
    let flat = root.join("dev").join("track-x");
    std::fs::create_dir_all(&flat).unwrap();
    std::fs::write(flat.join(MANAGED_MARKER), "agent=dev\n").unwrap();
    let mut out = Vec::new();
    collect_managed_worktrees(&root, MARKER_WALK_MAX_DEPTH, &mut out);
    assert!(
        out.contains(&nested),
        "slash-branch nested worktree must be enumerated (reviewer-2 #4)"
    );
    assert!(out.contains(&flat), "non-slash worktree still enumerated");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn boot_grace_predicate_suspends_only_when_recent_or_unknown() {
    // reviewer-2 #5: recent boot → suspend; aged boot → proceed; unknown →
    // conservative suspend.
    assert!(
        within_boot_grace(Some(1000), 1100, 600),
        "100s after boot, 600s grace → in grace (suspend)"
    );
    assert!(
        !within_boot_grace(Some(1000), 2000, 600),
        "1000s after boot → past grace (proceed)"
    );
    assert!(
        within_boot_grace(None, 2000, 600),
        "unknown boot time → conservative suspend"
    );
}

// ── #2234 Phase 2: layout-aware agent attribution + enumerate ──────────
/// The GC agent-attribution fix. The OLD fallback used the immediate PARENT
/// dir name, so a cure-(B) `<home>/workspace/<agent>` worktree resolved to
/// `"workspace"` (the root) → liveness keyed on a non-agent → a live agent's
/// cwd could be GC-reclaimed. Layout-aware strip-prefix returns the real agent.
#[test]
fn agent_from_layout_is_layout_aware_2234() {
    let home = tmp_home("agent-from-layout");
    // worktrees/<agent>/<slash-branch> → FIRST component is the agent.
    let nested = home.join("worktrees").join("dev").join("fix").join("x");
    assert_eq!(agent_from_layout(&home, &nested), Some("dev".to_string()));
    // workspace/<agent> (cure-(B)): the dir name IS the agent, NOT "workspace".
    let ws = crate::paths::workspace_dir(&home).join("dev2");
    assert_eq!(
        agent_from_layout(&home, &ws),
        Some("dev2".to_string()),
        "#2234: /workspace/<agent> must resolve to <agent>, not the parent 'workspace'"
    );
    // Off both managed roots → None (never guess via parent dir).
    assert_eq!(
        agent_from_layout(&home, std::path::Path::new("/tmp/elsewhere/x")),
        None
    );
    std::fs::remove_dir_all(&home).ok();
}

/// RED→GREEN end-to-end: a clean-released cure-(B) `workspace/<agent>` worktree
/// whose marker lacks `agent=` (forces the fallback) must yield a GcCandidate
/// whose `agent` is the workspace dir name — RED (old parent-file_name): the
/// candidate's agent was `"workspace"`, so the force-reclaim liveness guard
/// would key on a non-agent and could reclaim a LIVE agent's workspace cwd.
#[test]
fn evaluate_candidate_workspace_worktree_resolves_real_agent_2234() {
    let home = tmp_home("eval-ws-agent");
    let repo = tmp_repo("eval-ws-agent-repo");
    let wt = managed_workspace_worktree(&home, &repo, "devw", "fix/x");
    // Clean-released past grace, NO agent= field → exercises the path fallback.
    let old = (chrono::Utc::now() - chrono::Duration::hours(48)).to_rfc3339();
    std::fs::write(
        wt.join(MANAGED_MARKER),
        format!("leased_at={old}\nreleased_at={old}\n"),
    )
    .unwrap();
    let live = std::collections::HashSet::new();
    let cand =
        evaluate_candidate(&home, &wt, &live).expect("clean-released worktree is a candidate");
    assert_eq!(
        cand.agent, "devw",
        "#2234: agent must resolve to the workspace dir name 'devw', NOT the parent 'workspace'"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// no-miss union: a REGISTERED workspace worktree (seen via `git worktree
/// list`) AND an unregistered orphan marker dir under the worktrees root (seen
/// via the fs-scan) are BOTH enumerated, with correct `registered` flags +
/// layout-derived agents. Proves neither source alone suffices.
#[test]
fn enumerate_unions_registered_and_orphan_no_miss_2234() {
    let home = tmp_home("enum-union");
    let repo = tmp_repo("enum-union-repo");
    // (a) REGISTERED, cure-(B) workspace layout.
    let _ws = managed_workspace_worktree(&home, &repo, "devw", "feat/y");
    // (b) ORPHAN: a marker dir under worktrees root, NOT git-registered.
    let orphan = home.join("worktrees").join("devo").join("fix").join("z");
    std::fs::create_dir_all(&orphan).unwrap();
    std::fs::write(orphan.join(MANAGED_MARKER), "agent=devo\n").unwrap();

    let got = enumerate_managed_worktrees(&home, &repo);

    let ws = got
        .iter()
        .find(|w| w.agent.as_deref() == Some("devw"))
        .expect("registered workspace worktree must be enumerated (registry pass)");
    assert!(ws.registered, "workspace worktree is git-registered");

    let orp = got
        .iter()
        .find(|w| w.agent.as_deref() == Some("devo"))
        .expect("orphan marker dir must be enumerated (fs-scan — no-miss)");
    assert!(
        !orp.registered,
        "orphan dir is NOT git-registered (caught only by the fs-scan)"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// #2234 Phase 2 byte-identical-OFF: with no cure-(B) workspace worktree,
/// `fs_managed_worktrees` == the worktrees_root marker-walk (gc's prior scan).
#[test]
fn fs_managed_worktrees_off_byte_identical_2234() {
    let home = tmp_home("fsm-off");
    let wt = home.join("worktrees").join("dev").join("fix").join("x");
    std::fs::create_dir_all(&wt).unwrap();
    std::fs::write(wt.join(MANAGED_MARKER), "agent=dev\n").unwrap();
    let mut collected = Vec::new();
    collect_managed_worktrees(
        &daemon_managed_worktree_root(&home),
        MARKER_WALK_MAX_DEPTH,
        &mut collected,
    );
    assert_eq!(
        fs_managed_worktrees(&home),
        collected,
        "#2234 OFF: fs_managed == worktrees_root marker-walk (workspace part empty → byte-identical)"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #2234 Phase 2 (B) ON: a `workspace/<agent>` gitlink worktree is included.
#[test]
fn fs_managed_worktrees_includes_workspace_gitlink_2234() {
    let home = tmp_home("fsm-b");
    let repo = tmp_repo("fsm-b-repo");
    let ws = managed_workspace_worktree(&home, &repo, "devb", "feat/y");
    assert!(
        fs_managed_worktrees(&home).iter().any(|p| p == &ws),
        "#2234: cure-(B) workspace gitlink worktree must be enumerated by fs_managed_worktrees"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

// ── t-…50793-9: managed-worktree target/ retention sweep ──────────────
// These exercise the REAL sweep against on-disk fixtures. Unix-gated
// because they set past mtimes via `touch -t` and create symlinks; the
// helpers are #[cfg(unix)] for the same reason (Windows -D warnings would
// flag them as dead otherwise).

/// Age every entry under `p` (dirs+files) to a fixed past time (2025-01-01),
/// well over the 48h staleness window relative to the test clock.
#[cfg(unix)]
fn touch_old(p: &Path) {
    let _ = std::process::Command::new("touch")
        .args(["-t", "202501010000"])
        .arg(p)
        .status();
    if let Ok(entries) = std::fs::read_dir(p) {
        for e in entries.flatten() {
            touch_old(&e.path());
        }
    }
}

/// Create a daemon-managed worktree (`.agend-managed` marker) under
/// `home/worktrees/<agent>/<branch>` with a populated `target/`. `stale`
/// ages the whole `target/` tree past the window. Returns (worktree, target).
#[cfg(unix)]
fn mk_managed_target(home: &Path, agent: &str, branch: &str, stale: bool) -> (PathBuf, PathBuf) {
    let wt = daemon_managed_worktree_root(home).join(agent).join(branch);
    std::fs::create_dir_all(wt.join("target").join("debug")).unwrap();
    std::fs::write(
        wt.join(MANAGED_MARKER),
        format!(
            "agent={agent}\nbranch={branch}\nleased_at={}\n",
            chrono::Utc::now().to_rfc3339()
        ),
    )
    .unwrap();
    std::fs::write(wt.join("target").join("debug").join("app"), vec![0u8; 4096]).unwrap();
    if stale {
        touch_old(&wt.join("target"));
    }
    (wt.clone(), wt.join("target"))
}

/// ① Sweep a STALE managed `target/` — deleted; the worktree + marker survive.
#[cfg(unix)]
#[test]
fn target_sweep_reclaims_stale_managed_target() {
    let home = tmp_home("tgt-stale");
    let (wt, target) = mk_managed_target(&home, "dev-x", "feat/foo", true);
    assert!(target.exists());
    let age = std::time::Duration::from_secs(48 * 3600);

    let cands = target_sweep_candidates(&home, age, 0);
    assert_eq!(cands.len(), 1, "stale managed target/ must be a candidate");

    let results = target_sweep_run(&home, age, 0);
    assert!(
        results.iter().any(|r| r.removed),
        "stale target/ must be removed: {results:?}"
    );
    assert!(!target.exists(), "target/ must be deleted");
    assert!(
        wt.exists() && wt.join(MANAGED_MARKER).exists(),
        "worktree + marker MUST survive — only target/ is swept"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// ② NEVER sweep an unmanaged dir, and NEVER follow a `target` symlink that
/// escapes to canonical/operator data (the core footgun).
#[cfg(unix)]
#[test]
fn target_sweep_refuses_unmanaged_and_symlinked_canonical() {
    let home = tmp_home("tgt-safe");
    let age = std::time::Duration::from_secs(48 * 3600);

    // (a) unmanaged worktree dir (NO .agend-managed marker) with a stale target/.
    let unmanaged = daemon_managed_worktree_root(&home)
        .join("nomarker")
        .join("br");
    std::fs::create_dir_all(unmanaged.join("target")).unwrap();
    std::fs::write(unmanaged.join("target").join("f"), b"x").unwrap();
    touch_old(&unmanaged.join("target"));

    // (b) a "canonical" repo target OUTSIDE the managed roots, plus a managed
    //     worktree whose `target` is a SYMLINK pointing at it (escape attempt).
    let canonical = home.join("canonical-repo").join("target");
    std::fs::create_dir_all(&canonical).unwrap();
    std::fs::write(canonical.join("precious.bin"), b"operator-data").unwrap();
    touch_old(&canonical);
    let wt = daemon_managed_worktree_root(&home).join("dev-y").join("br");
    std::fs::create_dir_all(&wt).unwrap();
    std::fs::write(wt.join(MANAGED_MARKER), "agent=dev-y\nbranch=br\n").unwrap();
    std::os::unix::fs::symlink(&canonical, wt.join("target")).unwrap();

    let cands = target_sweep_candidates(&home, age, 0);
    assert!(
        cands.is_empty(),
        "unmanaged target + symlink-to-canonical must NOT be candidates: {cands:?}"
    );

    let results = target_sweep_run(&home, age, 0);
    assert!(
        results.iter().all(|r| !r.removed),
        "nothing must be deleted: {results:?}"
    );
    assert!(
        canonical.join("precious.bin").exists(),
        "canonical operator data MUST survive the symlink-escape attempt"
    );
    assert!(
        unmanaged.join("target").exists(),
        "unmanaged target/ MUST survive"
    );
    // Direct unit on the guard: a symlinked target is refused.
    assert!(
        validate_target_for_delete(&home, &wt.join("target")).is_err(),
        "symlinked target must be refused by validate_target_for_delete"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// ③ Skip an ACTIVE build (fresh mtime within the window) — not deleted.
#[cfg(unix)]
#[test]
fn target_sweep_skips_fresh_active_build_target() {
    let home = tmp_home("tgt-fresh");
    let (_, target) = mk_managed_target(&home, "dev-z", "feat/bar", false); // fresh
    let age = std::time::Duration::from_secs(48 * 3600);

    let cands = target_sweep_candidates(&home, age, 0);
    assert!(
        cands.is_empty(),
        "fresh (active-build) target/ must be skipped: {cands:?}"
    );
    let results = target_sweep_run(&home, age, 0);
    assert!(results.iter().all(|r| !r.removed));
    assert!(target.exists(), "active target/ MUST survive");
    std::fs::remove_dir_all(&home).ok();
}

/// ④ Dry-run previews the stale candidate WITHOUT deleting it.
#[cfg(unix)]
#[test]
#[serial_test::serial]
fn target_sweep_dry_run_previews_without_deleting() {
    let home = tmp_home("tgt-dry");
    let (_, target) = mk_managed_target(&home, "dev-d", "feat/baz", true); // stale
                                                                           // dry_run reads env config — pin to defaults (enabled, 48h).
    std::env::remove_var("AGEND_TARGET_GC_DISABLE");
    std::env::remove_var("AGEND_TARGET_GC_AGE_HOURS");
    std::env::remove_var("AGEND_TARGET_GC_MIN_SIZE_BYTES");

    let preview = target_sweep_dry_run(&home);
    assert_eq!(preview.len(), 1, "dry-run must preview the stale candidate");
    assert!(preview[0].size_bytes > 0, "preview reports a size");
    assert!(target.exists(), "dry-run MUST NOT delete");
    std::fs::remove_dir_all(&home).ok();
}

// ── Rework regression tests (r6 REJECT repros, PR #2398) ──────────────

/// R1 (r6 #1): a markerless `workspace/<agent>` gitlink worktree — caught by
/// the looser `fs_managed_worktrees` union — is NEVER swept. The sweep's
/// marker-strict enumerator walks `home/worktrees` only. neuter: revert
/// `target_sweep_candidates` to `fs_managed_worktrees(home)` ⇒ this goes RED
/// (the operator workspace target/ becomes a candidate + is deleted).
#[cfg(unix)]
#[test]
fn target_sweep_ignores_markerless_workspace_gitlink() {
    let home = tmp_home("tgt-ws-markerless");
    // The managed root must EXIST so the run reaches the enumerator (else
    // safe_managed_root short-circuits and the neuter is masked). Empty
    // home/worktrees ⇒ the marker-strict enumerator finds nothing; only the
    // looser union would (wrongly) pull in the workspace gitlink below.
    std::fs::create_dir_all(daemon_managed_worktree_root(&home)).unwrap();
    let ws = crate::paths::workspace_dir(&home).join("operator-owned");
    std::fs::create_dir_all(ws.join("target")).unwrap();
    std::fs::write(ws.join(".git"), b"gitdir: /elsewhere\n").unwrap(); // gitlink FILE, NO marker
    std::fs::write(ws.join("target").join("f"), vec![0u8; 2048]).unwrap();
    touch_old(&ws.join("target"));

    let age = std::time::Duration::from_secs(48 * 3600);
    let cands = target_sweep_candidates(&home, age, 0);
    assert!(
        cands.is_empty(),
        "markerless workspace gitlink must NOT be a candidate: {cands:?}"
    );
    let results = target_sweep_run(&home, age, 0);
    assert!(results.iter().all(|r| !r.removed));
    assert!(
        ws.join("target").exists(),
        "operator workspace target/ MUST survive"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// R2a (r6 #2, symlink-ROOT arm ISOLATED): `home/worktrees` symlinked to a
/// real dir INSIDE home — confinement (canon under home) WOULD pass, so
/// `safe_managed_root`'s SYMLINK arm is the SOLE guard. neuter: drop the
/// symlink arm ⇒ the inside-home root is enumerated + its stale managed
/// target swept (RED). (Isolates the symlink arm — r6's MEDIUM: the old
/// combined-gut neuter didn't, since confinement independently caught it.)
#[cfg(unix)]
#[test]
fn target_sweep_aborts_on_symlinked_root_inside_home() {
    let home = tmp_home("tgt-symroot-in");
    // Real dir INSIDE home holding a stale managed worktree (agent not live
    // → would be swept if enumeration reached it).
    let real_root = home.join("real-wt-root");
    let wt = real_root.join("dev-sa").join("br");
    std::fs::create_dir_all(wt.join("target")).unwrap();
    std::fs::write(wt.join(MANAGED_MARKER), "agent=dev-sa\nbranch=br\n").unwrap();
    std::fs::write(wt.join("target").join("f"), vec![0u8; 2048]).unwrap();
    touch_old(&wt.join("target"));
    std::os::unix::fs::symlink(&real_root, daemon_managed_worktree_root(&home)).unwrap();

    // Symlink arm rejects even though confinement-to-home would pass.
    assert!(
        safe_managed_root(&home).is_none(),
        "a symlinked managed root must be rejected by the symlink arm"
    );
    let age = std::time::Duration::from_secs(48 * 3600);
    assert!(target_sweep_candidates(&home, age, 0).is_empty());
    assert!(target_sweep_run(&home, age, 0).iter().all(|r| !r.removed));
    assert!(
        wt.join("target").exists(),
        "must not sweep through a symlinked managed root"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// R2b (r6 #2 / r4 — CONFINEMENT isolated, the real CRITICAL-2 guard): an
/// ancestor-escape — `home/worktrees` is REAL, but a child `<agent>` dir is a
/// SYMLINK to an external tree (outside home) with a real `target/`. The
/// safe_managed_root symlink arm does NOT fire (the ROOT is real); only
/// `validate_target_for_delete`'s canonical-root CONFINEMENT stands. neuter:
/// drop that confinement ⇒ external operator target swept (RED).
#[cfg(unix)]
#[test]
fn target_sweep_confinement_blocks_ancestor_escape() {
    let home = tmp_home("tgt-confine");
    std::fs::create_dir_all(daemon_managed_worktree_root(&home)).unwrap(); // REAL root
    let external = tmp_home("tgt-confine-ext");
    let ext_wt = external.join("wt");
    std::fs::create_dir_all(ext_wt.join("target")).unwrap();
    std::fs::write(ext_wt.join("target").join("precious.bin"), b"operator-data").unwrap();
    std::fs::write(ext_wt.join(MANAGED_MARKER), "agent=dev-ce\nbranch=br\n").unwrap();
    touch_old(&ext_wt.join("target"));
    // home/worktrees/dev-ce → external worktree (real root, SYMLINKED child).
    std::os::unix::fs::symlink(&ext_wt, daemon_managed_worktree_root(&home).join("dev-ce"))
        .unwrap();

    // Root is real → symlink arm does NOT fire; confinement is the sole guard.
    assert!(
        safe_managed_root(&home).is_some(),
        "a real managed root must pass safe_managed_root"
    );
    let escaping_target = daemon_managed_worktree_root(&home)
        .join("dev-ce")
        .join("target");
    assert!(
        validate_target_for_delete(&home, &escaping_target).is_err(),
        "confinement must reject a target that resolves outside the managed root"
    );
    let age = std::time::Duration::from_secs(48 * 3600);
    let results = target_sweep_run(&home, age, 0);
    assert!(results.iter().all(|r| !r.removed));
    assert!(
        ext_wt.join("target").join("precious.bin").exists(),
        "external operator data MUST survive (confinement)"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&external).ok();
}

/// R3 (r6 #4): a managed `target/` containing an UNREADABLE subdir (read_dir
/// errors) FAILS CLOSED — treated as active, NOT deleted. neuter: revert the
/// activity probe to return `false` on error ⇒ the dir is swept despite being
/// unreadable (RED).
#[cfg(unix)]
#[test]
fn target_sweep_fail_closed_on_unreadable_subdir() {
    use std::os::unix::fs::PermissionsExt;
    let home = tmp_home("tgt-failclosed");
    let (_, target) = mk_managed_target(&home, "dev-fc", "feat/fc", true); // stale
    let locked = target.join("locked");
    std::fs::create_dir_all(&locked).unwrap();
    std::fs::write(locked.join("x"), b"y").unwrap();
    touch_old(&target); // age the whole tree (incl. locked) past the window
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

    let age = std::time::Duration::from_secs(48 * 3600);
    let cands = target_sweep_candidates(&home, age, 0);
    let removed = target_sweep_run(&home, age, 0).iter().any(|r| r.removed);
    // Restore perms BEFORE asserting so cleanup always succeeds.
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).ok();

    assert!(
        cands.is_empty(),
        "target with an unreadable subdir must fail-closed (not a candidate): {cands:?}"
    );
    assert!(
        !removed,
        "fail-closed: target with an unreadable subdir must NOT be deleted"
    );
    assert!(target.exists(), "target/ MUST survive fail-closed");
    std::fs::remove_dir_all(&home).ok();
}

/// Write a binding.json for `agent` pointing at `worktree` (where
/// `binding::read` looks: `runtime_dir/<agent>/binding.json`).
#[cfg(unix)]
fn write_binding(home: &Path, agent: &str, worktree: &Path) {
    let dir = crate::paths::runtime_dir(home).join(agent);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("binding.json"),
        serde_json::json!({ "worktree": worktree.to_string_lossy() }).to_string(),
    )
    .unwrap();
}

/// FIX3 round-4 (r6 active-build TOCTOU): owner IN roster + bound HERE ⇒
/// PROTECTED, regardless of liveness — closes the bound-but-not-yet-live race
/// (the flappy liveness signal is gone). neuter: gut `predicate_protects`
/// (force not-protected) ⇒ the bound target is swept ⇒ RED.
#[cfg(unix)]
#[test]
fn target_sweep_protects_bound_in_roster() {
    let home = tmp_home("tgt-bound-roster");
    let (wt, target) = mk_managed_target(&home, "own-a", "feat/x", true); // stale
    write_binding(&home, "own-a", &wt);
    let roster = std::collections::HashSet::from(["own-a".to_string()]);

    let age = std::time::Duration::from_secs(48 * 3600);
    assert!(
        target_sweep_candidates_with_roster(&home, age, 0, &roster).is_empty(),
        "in-roster + bound-here must be PROTECTED"
    );
    assert!(target_sweep_run_with_roster(&home, age, 0, &roster)
        .iter()
        .all(|r| !r.removed));
    assert!(target.exists(), "bound-in-roster target/ MUST survive");
    std::fs::remove_dir_all(&home).ok();
}

/// Round-4: an instance GONE from the roster (deleted) is sweepable even with
/// a stale binding pointing here — it can never bind again (the real orphan
/// reclaim, e.g. claude-8145a9).
#[cfg(unix)]
#[test]
fn target_sweep_reclaims_instance_gone() {
    let home = tmp_home("tgt-gone");
    let (wt, target) = mk_managed_target(&home, "own-gone", "feat/y", true); // stale
    write_binding(&home, "own-gone", &wt); // stale binding points here...
    let roster = std::collections::HashSet::new(); // ...but owner NOT in roster (deleted)

    let age = std::time::Duration::from_secs(48 * 3600);
    assert_eq!(
        target_sweep_candidates_with_roster(&home, age, 0, &roster).len(),
        1,
        "a gone instance's stale-bound target must be sweepable"
    );
    assert!(target_sweep_run_with_roster(&home, age, 0, &roster)
        .iter()
        .any(|r| r.removed));
    assert!(
        !target.exists(),
        "gone-instance stale target/ must be reclaimed"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Round-4: a roster member that REBOUND AWAY (binding points elsewhere)
/// leaves this worktree sweepable.
#[cfg(unix)]
#[test]
fn target_sweep_reclaims_rebound_away() {
    let home = tmp_home("tgt-rebound");
    let (_wt, target) = mk_managed_target(&home, "own-reb", "feat/old", true); // stale
    let elsewhere = daemon_managed_worktree_root(&home)
        .join("own-reb")
        .join("feat-new");
    write_binding(&home, "own-reb", &elsewhere); // bound ELSEWHERE
    let roster = std::collections::HashSet::from(["own-reb".to_string()]);

    let age = std::time::Duration::from_secs(48 * 3600);
    assert_eq!(
        target_sweep_candidates_with_roster(&home, age, 0, &roster).len(),
        1,
        "a rebound-away worktree must be sweepable"
    );
    assert!(target_sweep_run_with_roster(&home, age, 0, &roster)
        .iter()
        .any(|r| r.removed));
    assert!(
        !target.exists(),
        "rebound-away stale target/ must be reclaimed"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Round-4 (fixes the None fail-open): a roster member whose binding.json
/// EXISTS but is UNREADABLE/malformed ⇒ fail-closed PROTECT. neuter: revert
/// the predicate's Err/None arm to `false` ⇒ swept ⇒ RED.
#[cfg(unix)]
#[test]
fn target_sweep_fail_closed_on_unreadable_binding() {
    let home = tmp_home("tgt-badbind");
    let (_wt, target) = mk_managed_target(&home, "own-bad", "feat/z", true); // stale
    let dir = crate::paths::runtime_dir(&home).join("own-bad");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("binding.json"), b"{ this is not valid json").unwrap();
    let roster = std::collections::HashSet::from(["own-bad".to_string()]);

    let age = std::time::Duration::from_secs(48 * 3600);
    assert!(
        target_sweep_candidates_with_roster(&home, age, 0, &roster).is_empty(),
        "an unreadable binding for a roster member must fail-closed PROTECT"
    );
    assert!(target_sweep_run_with_roster(&home, age, 0, &roster)
        .iter()
        .all(|r| !r.removed));
    assert!(
        target.exists(),
        "fail-closed: target/ MUST survive unreadable binding"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Round-4: the run pass HOLDS the owner's .binding.json.lock; a contended
/// lock (bind/release in flight) ⇒ SKIP this tick — never delete while the
/// binding could change under us. neuter: drop the try-lock ⇒ deletes despite
/// the held lock ⇒ RED.
#[cfg(unix)]
#[test]
fn target_sweep_skips_when_bind_lock_contended() {
    let home = tmp_home("tgt-lockcontend");
    // stale + NOT in roster ⇒ would be sweepable, but the held lock must veto.
    let (_wt, target) = mk_managed_target(&home, "own-lk", "feat/lk", true);
    let lock_path = crate::paths::runtime_dir(&home)
        .join("own-lk")
        .join(".binding.json.lock");
    let _held = crate::store::acquire_file_lock(&lock_path).expect("hold the bind lock");
    let roster = std::collections::HashSet::new();

    let age = std::time::Duration::from_secs(48 * 3600);
    let results = target_sweep_run_with_roster(&home, age, 0, &roster);
    assert!(
        results.iter().all(|r| !r.removed),
        "must skip while the bind lock is held: {results:?}"
    );
    assert!(
        target.exists(),
        "target/ MUST survive while the bind lock is held"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ─── #2550 W3 Wave1 pins: reconcile_orphan_leases (pre-refactor, zero prior
// coverage) — locks current field-extraction/corruption-tolerance/missing-
// field behavior before the binding_scan_all() extraction. ───────────────

#[tracing_test::traced_test]
#[test]
fn reconcile_orphan_leases_warns_when_worktree_path_missing_2550_w3() {
    let home = tmp_home("orphan-warns");
    let runtime = crate::paths::runtime_dir(&home).join("dev");
    std::fs::create_dir_all(&runtime).unwrap();
    let missing_wt = home.join("nonexistent-worktree");
    std::fs::write(
        runtime.join("binding.json"),
        serde_json::json!({"worktree": missing_wt.to_str().unwrap()}).to_string(),
    )
    .unwrap();

    reconcile_orphan_leases(&home);

    assert!(
        logs_contain("orphan lease"),
        "a binding pointing at a missing worktree path must warn"
    );
    assert!(logs_contain("dev"), "the warn must name the orphaned agent");
    std::fs::remove_dir_all(&home).ok();
}

#[tracing_test::traced_test]
#[test]
fn reconcile_orphan_leases_silent_when_worktree_path_exists_2550_w3() {
    let home = tmp_home("orphan-silent");
    let runtime = crate::paths::runtime_dir(&home).join("dev");
    std::fs::create_dir_all(&runtime).unwrap();
    let real_wt = home.join("real-worktree");
    std::fs::create_dir_all(&real_wt).unwrap();
    std::fs::write(
        runtime.join("binding.json"),
        serde_json::json!({"worktree": real_wt.to_str().unwrap()}).to_string(),
    )
    .unwrap();

    reconcile_orphan_leases(&home);

    assert!(
        !logs_contain("orphan lease"),
        "a binding whose worktree path exists must not be flagged as orphaned"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn reconcile_orphan_leases_tolerates_corrupt_binding_json_2550_w3() {
    let home = tmp_home("orphan-corrupt");
    let runtime = crate::paths::runtime_dir(&home).join("bad-agent");
    std::fs::create_dir_all(&runtime).unwrap();
    std::fs::write(runtime.join("binding.json"), b"not valid json").unwrap();

    reconcile_orphan_leases(&home); // must not panic
    std::fs::remove_dir_all(&home).ok();
}

#[tracing_test::traced_test]
#[test]
fn reconcile_orphan_leases_tolerates_missing_worktree_field_2550_w3() {
    let home = tmp_home("orphan-no-field");
    let runtime = crate::paths::runtime_dir(&home).join("dev");
    std::fs::create_dir_all(&runtime).unwrap();
    std::fs::write(
        runtime.join("binding.json"),
        serde_json::json!({"branch": "feat/x"}).to_string(),
    )
    .unwrap();

    reconcile_orphan_leases(&home);

    assert!(
        !logs_contain("orphan lease"),
        "a binding.json without a `worktree` field must not be flagged"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── #2158-adjacent: dirty-release WIP preservation ──────────────────────
//
// The daemon's AUTO-release already refuses to remove a dirty worktree
// (auto_release.rs SkipDirtyWorktree). But MANUAL release (`release_full`,
// backing `release_worktree`) removed a dirty worktree unconditionally,
// silently losing uncommitted WIP. RED-first: on pre-guard code
// `release_full_preserves_dirty_wip_to_recovery_ref` FAILS (no recovery ref).
// With the guard the WIP is snapshotted to `refs/agend/recovery/<branch>/<ts>`.

/// Recovery refs for `branch` (raw git — cfg(test) fixture; exempt from the
/// git-subprocess invariants which scan production `src/` portions only).
fn recovery_refs(repo: &Path, branch: &str) -> Vec<String> {
    let out = std::process::Command::new("git")
        .args([
            "for-each-ref",
            "--format=%(refname)",
            &format!("refs/agend/recovery/{branch}/"),
        ])
        .current_dir(repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("git for-each-ref");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

fn ls_tree_names(repo: &Path, git_ref: &str) -> String {
    let out = std::process::Command::new("git")
        .args(["ls-tree", "-r", "--name-only", git_ref])
        .current_dir(repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("git ls-tree");
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn git_in(wt: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(wt)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn release_full_preserves_dirty_wip_to_recovery_ref() {
    let home = tmp_home("release-dirty-preserve");
    let repo = tmp_repo("release-dirty-preserve-repo");
    let l = lease_bound(&home, &repo, "agent-dirty", "feat/dirty-wip");

    // Seed a tracked file on the branch so we can dirty it with a modification.
    std::fs::write(l.path.join("tracked.txt"), b"v1\n").unwrap();
    git_in(&l.path, &["add", "tracked.txt"]);
    git_in(
        &l.path,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-m",
            "seed tracked",
        ],
    );
    // Dirty: a tracked MODIFICATION + an UNTRACKED file (the loss-prone case).
    std::fs::write(l.path.join("tracked.txt"), b"v1\nMODIFIED-wip\n").unwrap();
    std::fs::write(l.path.join("untracked-wip.txt"), b"precious untracked").unwrap();
    assert!(
        crate::worktree::has_uncommitted_changes(&l.path),
        "precondition: worktree dirty"
    );

    let outcome = release_full(&home, "agent-dirty", false);
    assert!(outcome.released, "release must succeed: {outcome:?}");
    assert!(!l.path.exists(), "dirty worktree removed on release");

    // WIP must survive in exactly one recovery ref (RED on pre-guard code).
    let refs = recovery_refs(&repo, "feat/dirty-wip");
    assert_eq!(
        refs.len(),
        1,
        "exactly one recovery ref after dirty release: {refs:?}"
    );
    let files = ls_tree_names(&repo, &refs[0]);
    assert!(
        files.contains("untracked-wip.txt"),
        "untracked WIP captured in recovery ref tree: {files}"
    );
    assert!(
        files.contains("tracked.txt"),
        "tracked file captured in recovery ref tree: {files}"
    );
    let show = std::process::Command::new("git")
        .args(["show", &format!("{}:tracked.txt", refs[0])])
        .current_dir(&repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("git show");
    assert!(
        String::from_utf8_lossy(&show.stdout).contains("MODIFIED-wip"),
        "tracked modification content is recoverable from the ref"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn release_full_clean_worktree_creates_no_recovery_ref() {
    let home = tmp_home("release-clean-noref");
    let repo = tmp_repo("release-clean-noref-repo");
    let l = lease_bound(&home, &repo, "agent-clean", "feat/clean-rel");
    // A freshly-leased worktree carries only the untracked `.agend-managed`
    // marker (which `has_uncommitted_changes` reports as dirty but is NOT
    // preservable WIP) — releasing it must still create no recovery ref.
    let outcome = release_full(&home, "agent-clean", false);
    assert!(
        outcome.released && !l.path.exists(),
        "clean release succeeds"
    );
    assert!(
        recovery_refs(&repo, "feat/clean-rel").is_empty(),
        "clean release must create NO recovery ref (zero behaviour change)"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// Plant `<gitdir>/index.lock` (gitdir read from the worktree's `.git` gitlink)
/// so any index write (`git add -A`) fails — reviewer4's #2672 contended-index
/// counterexample.
fn plant_index_lock(wt_path: &Path) -> PathBuf {
    let gitlink = std::fs::read_to_string(wt_path.join(".git")).expect("read .git gitlink");
    let gitdir = gitlink
        .strip_prefix("gitdir:")
        .expect("gitlink form")
        .trim();
    let lock = Path::new(gitdir).join("index.lock");
    std::fs::write(&lock, b"").expect("plant index.lock");
    lock
}

/// reviewer4 #2672 (fail-OPEN regression): a dirty worktree whose WIP cannot be
/// snapshotted (contended `index.lock` → `git add -A` fails) must be FAIL-CLOSED —
/// `release_full` refuses to remove it (WIP recoverable in place), NOT a silent
/// `released:true` + evaporated WIP.
#[test]
fn release_full_refuses_when_dirty_wip_unpreservable() {
    let home = tmp_home("release-blocked");
    let repo = tmp_repo("release-blocked-repo");
    let l = lease_bound(&home, &repo, "agent-blk", "feat/blk");
    // Real preservable WIP (untracked), then jam the index so preservation fails.
    std::fs::write(l.path.join("precious-wip.txt"), b"must not vanish").unwrap();
    let _lock = plant_index_lock(&l.path);

    let outcome = release_full(&home, "agent-blk", false);
    assert!(
        !outcome.released,
        "must NOT report released on unpreservable WIP"
    );
    assert!(
        outcome
            .error
            .as_deref()
            .unwrap_or("")
            .contains("could not be preserved"),
        "error must name the refusal: {:?}",
        outcome.error
    );
    assert!(
        l.path.exists(),
        "worktree must NOT be removed (fail-closed)"
    );
    assert!(
        l.path.join("precious-wip.txt").exists(),
        "untracked WIP must survive in place"
    );
    assert!(
        crate::binding::read(&home, "agent-blk").is_some(),
        "binding must be kept so the operator can recover in place"
    );
    assert!(
        recovery_refs(&repo, "feat/blk").is_empty(),
        "no (partial) recovery ref on a Blocked release"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// #P3 (branch-residue) RED6 — pure KEEP/DELETE decision, testable without a
/// live gh/scm. An authoritative merged PR (`pr_merged=true`) makes the branch
/// delete-eligible IMMEDIATELY (`Ok`), even though it is NOT age-gated
/// (`squash_aged=false`) — the monotonic-PR fast path. Pre-#P3
/// `cleanup_merged_branch` had NO PR path at all, so a PR-merged branch under
/// the 24h floor was kept; this pins the new fast path plus the three SPLIT
/// keep reasons (the old blanket "not merged" text was misleading).
#[test]
fn merged_branch_disposition_split_reasons_and_pr_fast_path_red6() {
    // Authoritative merged PR → delete NOW, no age gate.
    assert!(
        merged_branch_disposition("feat/x", "main", false, true, false, false, false).is_ok(),
        "an authoritative merged PR must be delete-eligible now, no age gate"
    );
    // A plain merged ancestor is likewise eligible.
    assert!(
        merged_branch_disposition("feat/x", "main", true, false, false, false, false).is_ok(),
        "a merged ancestor stays delete-eligible"
    );
    // Nothing merged, no PR, not structural, detection ran → precise "not
    // merged" keep reason (NOT a gh-outage or age-floor reason).
    let err =
        merged_branch_disposition("feat/x", "main", false, false, false, false, false).unwrap_err();
    assert!(
        err.contains("not merged into 'main'"),
        "plain unmerged branch keeps with the 'not merged' reason: {err}"
    );
    // gh/detection unavailable (and not structurally squash) → fail-closed reason.
    let err =
        merged_branch_disposition("feat/x", "main", false, false, false, false, true).unwrap_err();
    assert!(
        err.contains("detection unavailable"),
        "a gh/remote outage keeps with the fail-closed detection reason: {err}"
    );
    // Structurally squash-merged but under the age floor → age-floor reason.
    let err =
        merged_branch_disposition("feat/x", "main", false, false, false, true, false).unwrap_err();
    assert!(
        err.contains("younger than"),
        "a young squash-merge keeps with the age-floor reason: {err}"
    );
}

/// #P3 (branch-residue) RED7 — hermetic fail-closed: a fixture repo with NO
/// github remote makes `pr_merge_status` return `Unknown` (extract_github_repo
/// None), and `cleanup_merged_branch` on such a repo for a young, non-merged
/// branch KEEPS it (falls back to the age-gated heuristic, which also declines)
/// with the fail-closed detection reason. Proves gh-unavailable → fail-closed
/// keep, never a delete. No real gh runs (no github remote to resolve).
#[test]
fn pr_merge_status_unknown_without_github_remote_keeps_branch_red7() {
    fn git(dir: &Path, args: &[&str]) {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("AGEND_GIT_BYPASS", "1")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git");
    }
    let repo = tmp_repo("red7-no-remote");
    // A young, non-merged branch with its OWN commit (diverges from main, so it
    // is NOT an ancestor → is_merged=false; committed now → under the age floor).
    git(&repo, &["checkout", "-b", "feat/red7"]);
    std::fs::write(repo.join("red7.txt"), "young unmerged work").ok();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "young unmerged work"]);
    git(&repo, &["checkout", "main"]);

    // No github remote configured → detection is Unknown (fail-closed), NOT a
    // NotMerged/Merged verdict.
    assert_eq!(
        crate::branch_sweep::pr_merge_status(&repo, "main", "feat/red7"),
        crate::branch_sweep::PrMergeStatus::Unknown,
        "no github remote → detection Unknown (fail-closed), not a merge verdict"
    );

    // And the release-path cleanup KEEPS the branch (never deletes on a
    // gh-unavailable young unmerged branch), surfacing the fail-closed reason.
    let (deleted, reason) = cleanup_merged_branch(&repo, "feat/red7", false);
    assert!(
        !deleted,
        "a gh-unavailable young unmerged branch must be KEPT, never deleted"
    );
    assert!(
        reason
            .as_deref()
            .is_some_and(|r| r.contains("detection unavailable")),
        "keep must carry the fail-closed detection reason: {reason:?}"
    );

    std::fs::remove_dir_all(&repo).ok();
}
