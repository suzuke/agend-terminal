use super::*;
use crate::daemon::pr_state::gh_poll::tests::MockGhPoller;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

fn tmp_home(tag: &str) -> std::path::PathBuf {
    let home =
        std::env::temp_dir().join(format!("agend-watch-settle-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).expect("create tmp home");
    std::fs::create_dir_all(home.join("inbox")).ok();
    std::fs::write(crate::fleet::fleet_yaml_path(&home), "instances: {}\n").ok();
    home
}

fn write_merged_pr_state(home: &std::path::Path, repo: &str, branch: &str, head: &str) {
    let mut s = tests::new_state(head, ReviewClass::Single);
    s.repo = repo.to_string();
    s.branch = branch.to_string();
    s.merge_state = MergeState::Merged {
        merge_commit: format!("merge-{head}"),
        merged_at: chrono::Utc::now().to_rfc3339(),
    };
    s.pr_author = "dev".to_string();
    save(home, &s).expect("save pr_state");
}

fn write_watch(home: &std::path::Path, repo: &str, branch: &str, head_sha: &str) {
    write_watch_with_gen(
        home,
        repo,
        branch,
        head_sha,
        &uuid::Uuid::new_v4().to_string(),
    );
}

fn write_watch_with_gen(
    home: &std::path::Path,
    repo: &str,
    branch: &str,
    head_sha: &str,
    generation_id: &str,
) {
    let ci_dir = crate::daemon::ci_watch::ci_watches_dir(home);
    std::fs::create_dir_all(&ci_dir).ok();
    let fname = crate::daemon::ci_watch::watch_filename(repo, branch);
    let watch = serde_json::json!({
        "repo": repo,
        "branch": branch,
        "head_sha": head_sha,
        "generation_id": generation_id,
        "interval_secs": 60,
        "subscribers": [{"instance": "dev-agent", "subscribed_at": "2026-01-01T00:00:00Z"}],
    });
    std::fs::write(
        ci_dir.join(&fname),
        serde_json::to_string_pretty(&watch).expect("json"),
    )
    .expect("write watch");
}

fn watch_exists(home: &std::path::Path, repo: &str, branch: &str) -> bool {
    let ci_dir = crate::daemon::ci_watch::ci_watches_dir(home);
    let fname = crate::daemon::ci_watch::watch_filename(repo, branch);
    ci_dir.join(&fname).exists()
}

fn write_exact_head_watch(
    home: &std::path::Path,
    repo: &str,
    branch: &str,
    head_sha: &str,
    target_head_sha: &str,
) {
    let ci_dir = crate::daemon::ci_watch::ci_watches_dir(home);
    std::fs::create_dir_all(&ci_dir).ok();
    let fname = crate::daemon::ci_watch::watch_filename_exact_head(repo, branch, target_head_sha);
    let watch = serde_json::json!({
        "repo": repo,
        "branch": branch,
        "head_sha": head_sha,
        "target_head_sha": target_head_sha,
        "interval_secs": 60,
        "subscribers": [{"instance": "dev-agent"}],
    });
    std::fs::write(
        ci_dir.join(&fname),
        serde_json::to_string_pretty(&watch).expect("json"),
    )
    .expect("write exact-head watch");
}

fn exact_head_watch_exists(
    home: &std::path::Path,
    repo: &str,
    branch: &str,
    target_head_sha: &str,
) -> bool {
    let ci_dir = crate::daemon::ci_watch::ci_watches_dir(home);
    let fname = crate::daemon::ci_watch::watch_filename_exact_head(repo, branch, target_head_sha);
    ci_dir.join(&fname).exists()
}

fn null_registry() -> crate::agent::AgentRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}

fn run_scan(home: &std::path::Path) {
    let registry = null_registry();
    let poller = MockGhPoller::new(vec![Ok(vec![])]);
    scanner::scan_and_emit_with(home, &registry, &poller);
}

/// R1: A merged PR with matching head_sha removes the feature watch.
#[test]
fn terminal_pr_merged_feature_watch_removed_by_cas() {
    let home = tmp_home("r1-merged");
    let repo = "owner/repo";
    let branch = "feat/r1";
    let head = "abc1234def5678";

    write_merged_pr_state(&home, repo, branch, head);
    write_watch(&home, repo, branch, head);
    assert!(
        watch_exists(&home, repo, branch),
        "precondition: watch exists"
    );

    tests::write_team_fleet(&home, "lead", &["dev"]);
    run_scan(&home);

    assert!(
        !watch_exists(&home, repo, branch),
        "feature watch must be removed after merged terminal with matching head"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// R2: Head advanced (watch.head_sha != terminal head) preserves watch.
#[test]
fn head_advanced_watch_preserved_by_cas_mismatch() {
    let home = tmp_home("r2-mismatch");
    let repo = "owner/repo";
    let branch = "feat/r2";

    write_merged_pr_state(&home, repo, branch, "terminal-head-aaa");
    write_watch(&home, repo, branch, "different-head-bbb");
    assert!(watch_exists(&home, repo, branch), "precondition");

    tests::write_team_fleet(&home, "lead", &["dev"]);
    run_scan(&home);

    assert!(
        watch_exists(&home, repo, branch),
        "watch must be preserved when head_sha doesn't match terminal head"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// R3a: A regular-keyed watch on a protected ref (main) is preserved by
/// the is_protected_ref guard, even when repo+branch+head all match.
#[test]
fn protected_ref_guard_preserves_main_watch() {
    let home = tmp_home("r3a-protected");
    let repo = "owner/repo";
    let head = "main-head-abc";

    // Synthesize a terminal state for "main" AND a regular-keyed main watch.
    write_merged_pr_state(&home, repo, "main", head);
    write_watch(&home, repo, "main", head);
    assert!(watch_exists(&home, repo, "main"), "precondition");

    tests::write_team_fleet(&home, "lead", &["dev"]);
    run_scan(&home);

    assert!(
        watch_exists(&home, repo, "main"),
        "regular main watch must be preserved by is_protected_ref guard"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// R3b: Exact-head main watch (different filename key) is untouched by
/// feature terminal settlement.
#[test]
fn exact_head_main_watch_isolated_from_feature_terminal() {
    let home = tmp_home("r3b-exact");
    let repo = "owner/repo";
    let head = "main-head-abc";

    write_merged_pr_state(&home, repo, "feat/r3b", head);
    write_exact_head_watch(&home, repo, "main", head, head);
    assert!(
        exact_head_watch_exists(&home, repo, "main", head),
        "precondition"
    );

    tests::write_team_fleet(&home, "lead", &["dev"]);
    run_scan(&home);

    assert!(
        exact_head_watch_exists(&home, repo, "main", head),
        "exact-head main watch must NOT be removed by feature terminal"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// R4: Watch already gone → idempotent no-op, no error.
#[test]
fn scanner_watch_removal_idempotent_when_already_gone() {
    let home = tmp_home("r4-idempotent");
    let repo = "owner/repo";
    let branch = "feat/r4";

    write_merged_pr_state(&home, repo, branch, "head-r4");
    // No watch file — scanner should not error.
    assert!(!watch_exists(&home, repo, branch), "precondition: no watch");

    tests::write_team_fleet(&home, "lead", &["dev"]);
    run_scan(&home);
    // No panic, no error — test passes if we get here.

    std::fs::remove_dir_all(&home).ok();
}

/// R5: Restart/re-scan settles even when terminal emit ledger already exists
/// (replay-suppressed path). The deferred_watch_settle captures on BOTH
/// first-emit and replay-suppressed arms.
#[test]
fn restart_rescan_settles_when_ledger_already_emitted() {
    let home = tmp_home("r5-restart");
    let repo = "owner/repo";
    let branch = "feat/r5";
    let head = "head-r5-restart";

    write_merged_pr_state(&home, repo, branch, head);
    write_watch(&home, repo, branch, head);
    tests::write_team_fleet(&home, "lead", &["dev"]);

    // First scan: emits terminal event + removes watch.
    run_scan(&home);
    assert!(
        !watch_exists(&home, repo, branch),
        "first scan must remove watch"
    );

    // Simulate restart: re-create watch (as if poller re-armed it somehow)
    // and re-create the pr_state file (as if lingering CI re-created it).
    write_merged_pr_state(&home, repo, branch, head);
    write_watch(&home, repo, branch, head);

    // Second scan: terminal emit ledger says "already emitted" → replay
    // suppressed. But deferred_watch_settle should still capture and settle.
    run_scan(&home);
    assert!(
        !watch_exists(&home, repo, branch),
        "re-scan (replay-suppressed) must still remove watch via deferred settlement"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// R6: New generation watch on same branch after removal — the JSON-only
/// removal under lock must not corrupt a newly written watch file for a
/// different head (stale flush regression).
#[test]
fn stale_flush_new_generation_no_cross_corruption() {
    let home = tmp_home("r6-generation");
    let repo = "owner/repo";
    let branch = "feat/r6";
    let old_head = "old-head-gen1";
    let new_head = "new-head-gen2";

    // Generation 1: merged PR, matching watch → removed.
    write_merged_pr_state(&home, repo, branch, old_head);
    write_watch(&home, repo, branch, old_head);
    tests::write_team_fleet(&home, "lead", &["dev"]);
    run_scan(&home);
    assert!(!watch_exists(&home, repo, branch), "gen1 watch removed");

    // Generation 2: new PR on same branch, new watch with different head.
    write_watch(&home, repo, branch, new_head);
    assert!(watch_exists(&home, repo, branch), "gen2 watch written");

    // Re-scan with old terminal state — CAS should NOT match new head.
    // The pr_state was removed by first scan; re-create with old head to
    // simulate a stale re-observation.
    write_merged_pr_state(&home, repo, branch, old_head);
    run_scan(&home);

    // Gen2 watch must survive — old terminal head doesn't match new watch head.
    assert!(
        watch_exists(&home, repo, branch),
        "gen2 watch with different head must survive old-generation terminal"
    );
    // Verify the watch content is uncorrupted.
    let ci_dir = crate::daemon::ci_watch::ci_watches_dir(&home);
    let fname = crate::daemon::ci_watch::watch_filename(repo, branch);
    let content = std::fs::read_to_string(ci_dir.join(&fname)).expect("read gen2 watch");
    let watch: serde_json::Value = serde_json::from_str(&content).expect("parse gen2 watch");
    assert_eq!(
        watch["head_sha"].as_str(),
        Some(new_head),
        "gen2 watch content must be uncorrupted"
    );

    std::fs::remove_dir_all(&home).ok();
}

fn write_closed_unmerged_pr_state(home: &std::path::Path, repo: &str, branch: &str, head: &str) {
    let mut s = tests::new_state(head, ReviewClass::Single);
    s.repo = repo.to_string();
    s.branch = branch.to_string();
    s.merge_state = MergeState::ClosedUnmerged {
        closed_at: chrono::Utc::now().to_rfc3339(),
    };
    s.pr_author = "dev".to_string();
    save(home, &s).expect("save pr_state");
}

/// R7: ClosedUnmerged terminal also removes matching feature watch.
#[test]
fn closed_unmerged_terminal_removes_matching_watch() {
    let home = tmp_home("r7-closed");
    let repo = "owner/repo";
    let branch = "feat/r7";
    let head = "head-r7-closed";

    write_closed_unmerged_pr_state(&home, repo, branch, head);
    write_watch(&home, repo, branch, head);
    assert!(watch_exists(&home, repo, branch), "precondition");

    tests::write_team_fleet(&home, "lead", &["dev"]);
    run_scan(&home);

    assert!(
        !watch_exists(&home, repo, branch),
        "closed-unmerged terminal must also remove matching feature watch"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// R8: Deterministic concurrency proof — a stale flush_watch_state with gen1's
/// generation_id cannot overwrite a gen2 watch created after settlement.
#[test]
fn stale_flush_across_settlement_and_new_generation_thread() {
    use crate::daemon::ci_watch::{ci_watches_dir, flush_watch_state, watch_filename, WatchState};
    use std::sync::{Arc, Barrier};

    let home = tmp_home("r8-concurrent");
    let repo = "owner/repo";
    let branch = "feat/r8";
    let gen1_id = "gen1-uuid-aaa";
    let gen2_id = "gen2-uuid-bbb";
    let gen1_head = "gen1-head";
    let gen2_head = "gen2-head";

    write_watch_with_gen(&home, repo, branch, gen1_head, gen1_id);
    let ci_dir = ci_watches_dir(&home);
    let fname = watch_filename(repo, branch);
    let watch_path = ci_dir.join(&fname);
    let snapshot: WatchState =
        serde_json::from_str(&std::fs::read_to_string(&watch_path).expect("read gen1"))
            .expect("parse gen1");

    // Settle gen1.
    std::fs::remove_file(&watch_path).expect("remove gen1");

    // Create gen2 with different generation.
    write_watch_with_gen(&home, repo, branch, gen2_head, gen2_id);

    // Stale worker thread flushes gen1 snapshot.
    let barrier = Arc::new(Barrier::new(2));
    let b2 = barrier.clone();
    let wp = watch_path.clone();
    let snap = snapshot.clone();
    let t = std::thread::spawn(move || {
        b2.wait();
        flush_watch_state(&wp, &snap, snap.generation_id.as_deref());
    });
    barrier.wait();
    t.join().expect("worker thread");

    // Gen2 must be uncorrupted.
    let content = std::fs::read_to_string(&watch_path).expect("read after flush");
    let after: WatchState = serde_json::from_str(&content).expect("parse after flush");
    assert_eq!(after.generation_id.as_deref(), Some(gen2_id));
    assert_eq!(after.head_sha.as_deref(), Some(gen2_head));

    std::fs::remove_dir_all(&home).ok();
}
