//! Task-board fallback for the post-merge watch resolve — the
//! Release-before-merge ordering fix.
//!
//! Contract: `post_merge_receipt_and_watch` resolves the actionable assignee
//! for a merged PR branch in two stages — live bindings first (unchanged),
//! then a task-board lookup. The fallback exists because the fleet protocol's
//! Release-before-merge deletes the implementer's binding.json BEFORE the
//! orchestrator runs `repo.merge`, so under the normal pipeline the binding
//! scan misses exactly the merge it must cover (the "skipped: no task-linked
//! binding" false-negative). Fail-closed semantics carry over: 0 or ≥2
//! unique matches → skip; unreadable/duplicate boards → skip.

use super::watch::handle_watch_ci;
use serde_json::json;

fn tmp_home(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let h = std::env::temp_dir().join(format!(
        "agend-pmw-board-fallback-{}-{}-{}",
        std::process::id(),
        tag,
        id
    ));
    let _ = std::fs::remove_dir_all(&h);
    std::fs::create_dir_all(&h).unwrap();
    h
}

const REPO: &str = "suzuke/agend-terminal";

fn seed_fleet(home: &std::path::Path, instances: &[&str]) {
    let yaml = format!(
        "instances:\n{}\n",
        instances
            .iter()
            .map(|i| format!("  {i}:\n    backend: claude\n"))
            .collect::<String>()
    );
    std::fs::write(crate::fleet::fleet_yaml_path(home), yaml).unwrap();
}

/// A real git repo whose `origin` canonicalizes to [`REPO`] — needed because
/// both resolve stages canonicalize source_repo through `git remote get-url`.
fn make_source_repo(home: &std::path::Path) -> std::path::PathBuf {
    let source_repo = home.join("source-repo");
    std::fs::create_dir_all(&source_repo).unwrap();
    for (args, _env_bypass) in [
        (vec!["init", "-b", "main"], true),
        (
            vec![
                "remote",
                "add",
                "origin",
                &format!("https://github.com/{REPO}.git"),
            ],
            true,
        ),
    ] {
        std::process::Command::new("git")
            .args(&args)
            .current_dir(&source_repo)
            .env("AGENTIC_GIT_BYPASS", "1")
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .ok();
    }
    source_repo
}

/// Seed a team that owns a board resolving to REPO, plus its members.
fn seed_team_with_source(home: &std::path::Path, team: &str, members: &[&str]) {
    // Members must exist as instances (one-agent-one-team invariant).
    seed_fleet(home, members);
    let source_repo = make_source_repo(home);
    let member_list = members
        .iter()
        .map(|m| format!("\"{m}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let yaml = format!(
        "instances:\n{}\nteams:\n  {team}:\n    members: [{member_list}]\n    orchestrator: \"{}\"\n    source_repo: {}\n",
        members
            .iter()
            .map(|i| format!("  {i}:\n    backend: claude\n"))
            .collect::<String>(),
        members.first().unwrap_or(&"lead"),
        source_repo.display()
    );
    std::fs::write(crate::fleet::fleet_yaml_path(home), yaml).unwrap();
}

/// Seed an open-work task with a branch + assignee on the DEFAULT board via
/// the public tasks API (producer-faithful: the same path production writes).
fn seed_task(home: &std::path::Path, title: &str, branch: &str, assignee: &str) -> String {
    let created = crate::tasks::handle(
        home,
        "lead",
        &json!({
            "action": "create",
            "title": title,
            "branch": branch,
            "assignee": assignee,
        }),
    );
    let id = created["task"]["id"]
        .as_str()
        .or_else(|| created["id"].as_str())
        .expect("created task id")
        .to_string();
    // create leaves the task open/unassigned-eligible; claim to pin the
    // assignee exactly as a dispatch would.
    if !assignee.is_empty() {
        crate::tasks::handle(home, assignee, &json!({"action": "claim", "task_id": id}));
    }
    id
}

fn watch_path(home: &std::path::Path, repo: &str, sha: &str) -> std::path::PathBuf {
    home.join("ci-watches")
        .join(crate::daemon::ci_watch::watch_filename_exact_head(
            repo, "main", sha,
        ))
}

// ── AC1: binding released but the task board carries the branch ──

#[test]
fn post_merge_resolves_from_task_board_after_binding_release() {
    let home = tmp_home("board-fallback-hit");
    let sha = "b".repeat(40);
    seed_team_with_source(&home, "team", &["lead", "dev"]);
    let task_id = seed_task(&home, "impl work", "fix/board-x", "dev");
    // NO binding seeded — Release-before-merge already removed it.

    let diag =
        super::merge::post_merge_receipt_and_watch(&home, REPO, &sha, 7, "fix/board-x", "lead");

    assert_eq!(
        diag["receipt"], "persisted",
        "task-board fallback must persist the receipt: {diag}"
    );
    assert_eq!(
        diag["assignee"], "dev",
        "fallback must resolve the task assignee: {diag}"
    );
    assert_eq!(diag["watch"], "armed", "watch must be armed: {diag}");
    let watch: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(watch_path(&home, REPO, &sha)).unwrap())
            .unwrap();
    assert_eq!(watch["task_id"], task_id);
    assert_eq!(watch["next_after_ci"], "lead");
    let receipt = crate::merge_receipt::find(&home, REPO, &sha, &task_id);
    assert!(receipt.is_some(), "receipt findable on disk");
    assert_eq!(receipt.unwrap().task_assignee, "dev");
    std::fs::remove_dir_all(&home).ok();
}

// ── AC2a: zero board matches → fail-closed skip ──

#[test]
fn post_merge_board_fallback_no_match_skips() {
    let home = tmp_home("board-fallback-zero");
    let sha = "c".repeat(40);
    seed_team_with_source(&home, "team", &["lead", "dev"]);
    seed_task(&home, "other work", "fix/other-branch", "dev");

    let diag = super::merge::post_merge_receipt_and_watch(
        &home,
        REPO,
        &sha,
        8,
        "fix/no-such-branch",
        "lead",
    );

    assert!(
        diag["skipped"].as_str().is_some(),
        "0 board matches → skip: {diag}"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── AC2b: ≥2 board matches → fail-closed (ambiguous) ──

#[test]
fn post_merge_board_fallback_ambiguous_skips() {
    let home = tmp_home("board-fallback-ambig");
    let sha = "d".repeat(40);
    seed_team_with_source(&home, "team", &["lead", "dev1", "dev2"]);
    seed_task(&home, "work one", "fix/shared-board", "dev1");
    seed_task(&home, "work two", "fix/shared-board", "dev2");

    let diag = super::merge::post_merge_receipt_and_watch(
        &home,
        REPO,
        &sha,
        9,
        "fix/shared-board",
        "lead",
    );

    assert!(
        diag["skipped"].as_str().is_some(),
        "≥2 board matches → fail closed: {diag}"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── AC2c: cancelled tasks are not open work → skip ──
// (`done` is gated on an assignee binding/receipt — the assignee-completion
// guard — so cancel is the reachable terminal transition in this harness.)

#[test]
fn post_merge_board_fallback_ignores_terminal_tasks() {
    let home = tmp_home("board-fallback-terminal");
    let sha = "e".repeat(40);
    seed_team_with_source(&home, "team", &["lead", "dev"]);
    let task_id = seed_task(&home, "finished work", "fix/done-branch", "dev");
    crate::tasks::handle(
        &home,
        "lead",
        &json!({"action": "update", "task_id": task_id, "status": "cancelled"}),
    );

    let diag =
        super::merge::post_merge_receipt_and_watch(&home, REPO, &sha, 10, "fix/done-branch", "");

    assert!(
        diag["skipped"].as_str().is_some(),
        "terminal-status task must not arm a post-merge watch: {diag}"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── AC3: live-binding priority unchanged (byte-identical regression) ──

#[test]
fn post_merge_live_binding_still_wins_over_board() {
    let home = tmp_home("binding-priority");
    let sha = "f".repeat(40);
    seed_team_with_source(&home, "team", &["lead", "dev"]);
    // Board says dev2 owns the branch…
    seed_task(&home, "board record", "fix/priority-branch", "lead");
    // …but the LIVE binding says dev (bound, not yet released). The binding
    // stage is authoritative and unchanged: receipt carries the binding's
    // agent + task id.
    let source_repo = make_source_repo(&home);
    let dir = crate::paths::runtime_dir(&home).join("dev");
    std::fs::create_dir_all(&dir).unwrap();
    let binding = json!({
        "task_id": "t-binding-task",
        "branch": "fix/priority-branch",
        "issued_at": "2026-08-23T00:00:00Z",
        "worktree": "/tmp/fake-wt",
        "source_repo": source_repo.display().to_string(),
    });
    std::fs::write(
        dir.join("binding.json"),
        serde_json::to_vec_pretty(&binding).unwrap(),
    )
    .unwrap();

    let diag = super::merge::post_merge_receipt_and_watch(
        &home,
        REPO,
        &sha,
        11,
        "fix/priority-branch",
        "lead",
    );

    assert_eq!(
        diag["assignee"], "dev",
        "live binding wins over the task board: {diag}"
    );
    let receipt = crate::merge_receipt::find(&home, REPO, &sha, "t-binding-task");
    assert!(receipt.is_some(), "receipt uses the BINDING's task id");
    std::fs::remove_dir_all(&home).ok();
}

// ── Repo mismatch: board owned by a DIFFERENT repo never matches ──

#[test]
fn post_merge_board_fallback_repo_mismatch_skips() {
    let other_repo = "someone-else/other-repo";
    let home = tmp_home("board-fallback-repo-mismatch");
    let sha = "1a".repeat(20);
    // Team owns a board whose source_repo points at another repo entirely.
    seed_fleet(&home, &["lead", "dev"]);
    let source_repo = home.join("other-repo-src");
    std::fs::create_dir_all(&source_repo).unwrap();
    std::process::Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(&source_repo)
        .env("AGENTIC_GIT_BYPASS", "1")
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .ok();
    std::process::Command::new("git")
        .args([
            "remote",
            "add",
            "origin",
            &format!("https://github.com/{other_repo}.git"),
        ])
        .current_dir(&source_repo)
        .env("AGENTIC_GIT_BYPASS", "1")
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .ok();
    let yaml = format!(
        "instances:\n  lead:\n    backend: claude\n  dev:\n    backend: claude\nteams:\n  team:\n    members: [\"lead\", \"dev\"]\n    orchestrator: \"lead\"\n    source_repo: {}\n",
        source_repo.display()
    );
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
    seed_task(&home, "foreign board work", "fix/mismatch", "dev");

    let diag =
        super::merge::post_merge_receipt_and_watch(&home, REPO, &sha, 12, "fix/mismatch", "lead");

    assert!(
        diag["skipped"].as_str().is_some(),
        "board owned by another repo must NOT match: {diag}"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── Sanity: handle_watch_ci import stays used by this module's contract tests ──

#[test]
fn watch_helper_still_rejects_missing_receipt() {
    // Pins the sibling contract this file shares: notification-only arming
    // without a receipt fails (guards against accidental allowlist drift).
    let home = tmp_home("watch-no-receipt-sanity");
    let sha = "2a".repeat(20);
    seed_fleet(&home, &["dev"]);
    let args = json!({
        "repository": REPO, "branch": "main",
        "head_sha": sha, "task_id": "t-none",
        "notification_only": true,
    });
    let r = handle_watch_ci(&home, &args, "dev");
    assert!(r.get("error").is_some(), "no receipt → reject: {r}");
    std::fs::remove_dir_all(&home).ok();
}
