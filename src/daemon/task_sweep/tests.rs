use super::*;
use std::fs;
use std::sync::atomic::{AtomicU32, Ordering};

#[path = "tests/pull_list_server.rs"]
mod pull_list_server_harness;
#[path = "tests/tests_3316.rs"]
mod tests_3316;
use pull_list_server_harness::pull_list_server;

fn tmp_home(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-task-sweep-{}-{}-{}",
        std::process::id(),
        tag,
        id
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

// ── #2117 P2: per-board sweep enumeration ──────────────────────

/// Init a git repo at `dir` with a single GitHub `origin` remote so
/// `derive_repo_from_remote` resolves a slug (the sweep's board→repo map).
fn git_repo_with_origin(dir: &Path, origin: &str) {
    fs::create_dir_all(dir).unwrap();
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .ok();
    };
    git(&["init", "-b", "main"]);
    git(&["remote", "add", "origin", origin]);
}

fn pr_json(body: &str) -> String {
    serde_json::json!([{
        "number": 3316,
        "title": "fix: override-aware task sweep",
        "state": "closed",
        "merge_commit_sha": "abc3316",
        "merged_at": "2026-08-22T00:00:00Z",
        "body": body,
        "user": {"login": "test-user"}
    }])
    .to_string()
}

fn write_sweep_fleet(home: &Path, teams: &str) {
    fs::write(
            crate::fleet::fleet_yaml_path(home),
            format!(
                "instances:\n  devA:\n    backend: claude\n    github_login: test-user\n  devB:\n    backend: claude\n    github_login: test-user\nteams:\n{teams}"
            ),
        )
        .unwrap();
}

#[test]
fn red_sweep_tick_routes_to_team_project_override_3316() {
    let home = tmp_home("red-override-entrypoint");
    let repo = home.join("Projects").join("simple-edge-tts");
    git_repo_with_origin(&repo, "https://github.com/org/simple-edge-tts.git");
    write_sweep_fleet(
        &home,
        &format!(
            "  edge:\n    members: [devA]\n    source_repo: {}\n    project_id: simple-edge-tts\n",
            repo.display()
        ),
    );
    let server = pull_list_server("[]".to_string());
    handle_task_sweep_config(&home, &serde_json::json!({"api_base_url": server.base_url}));
    resolve_sweep_plan(&home, &load_config(&home)).unwrap();
    let task = crate::tasks::handle(
        &home,
        "devA",
        &serde_json::json!({
            "action": "create",
            "title": "override-routed task",
            "assignee": "devA"
        }),
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    server.set_body(pr_json(&format!("Closes {task}")));

    sweep_tick(&home).unwrap();

    let board = crate::task_events::board_root(&home, "simple-edge-tts");
    let routed = crate::tasks::list_all_at(&home, &board)
        .into_iter()
        .find(|candidate| candidate.id == task)
        .expect("override-routed task must remain on the override board");
    assert_eq!(
        routed.status,
        crate::task_events::TaskStatus::Done,
        "real sweep entry point must scan the same board task CRUD selected"
    );
    drop(server);
    fs::remove_dir_all(&home).ok();
}

#[test]
fn red_sweep_tick_deduplicates_same_repo_fetch_across_boards_3316() {
    let home = tmp_home("red-fetch-cache-entrypoint");
    let repo_a = home.join("srcA");
    let repo_b = home.join("srcB");
    git_repo_with_origin(&repo_a, "https://github.com/org/shared.git");
    git_repo_with_origin(&repo_b, "git@github.com:org/shared.git");
    write_sweep_fleet(
            &home,
            &format!(
                "  teamA:\n    members: [devA]\n    source_repo: {}\n    project_id: board-a\n  teamB:\n    members: [devB]\n    source_repo: {}\n    project_id: board-b\n",
                repo_a.display(),
                repo_b.display()
            ),
        );
    let server = pull_list_server("[]".to_string());
    handle_task_sweep_config(&home, &serde_json::json!({"api_base_url": server.base_url}));
    resolve_sweep_plan(&home, &load_config(&home)).unwrap();
    let make_task = |caller: &str| {
        crate::tasks::handle(
            &home,
            caller,
            &serde_json::json!({
                "action": "create",
                "title": "shared-repo task",
                "assignee": caller
            }),
        )["id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let task_a = make_task("devA");
    let task_b = make_task("devB");
    server.set_body(pr_json(&format!("Closes {task_a}\nCloses {task_b}")));

    sweep_tick(&home).unwrap();

    assert_eq!(
        server.requests.load(Ordering::Acquire),
        1,
        "same normalized repo/api must issue one PR fetch while retaining board-local scans"
    );
    for (project, task_id) in [("board-a", task_a), ("board-b", task_b)] {
        let board = crate::task_events::board_root(&home, project);
        let task = crate::tasks::list_all_at(&home, &board)
            .into_iter()
            .find(|candidate| candidate.id == task_id)
            .unwrap();
        assert_eq!(task.status, crate::task_events::TaskStatus::Done);
    }
    drop(server);
    fs::remove_dir_all(&home).ok();
}

#[test]
fn baseline_without_provenance_fails_closed_and_surfaces_health_3316() {
    let home = tmp_home("baseline-unverified");
    let repo = home.join("Projects").join("baseline");
    git_repo_with_origin(&repo, "https://github.com/org/baseline.git");
    write_sweep_fleet(
            &home,
            &format!(
                "  baseline:\n    members: [devA]\n    source_repo: {}\n    project_id: baseline-board\n",
                repo.display()
            ),
        );
    let server = pull_list_server("[]".to_string());
    handle_task_sweep_config(&home, &serde_json::json!({"api_base_url": server.base_url}));
    let task = crate::tasks::handle(
        &home,
        "devA",
        &serde_json::json!({
            "action": "create",
            "title": "unverified baseline task",
            "assignee": "devA"
        }),
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    server.set_body(pr_json(&format!("Closes {task}")));

    sweep_tick(&home).unwrap();

    assert_eq!(server.requests.load(Ordering::Acquire), 0);
    let board = crate::task_events::board_root(&home, "baseline-board");
    let task = crate::tasks::list_all_at(&home, &board)
        .into_iter()
        .find(|candidate| candidate.id == task)
        .unwrap();
    assert_eq!(task.status, crate::task_events::TaskStatus::Open);
    let health = task_sweep_health(&home);
    assert!(health["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["code"] == "BASELINE_UNVERIFIED"));
    drop(server);
    fs::remove_dir_all(&home).ok();
}

#[test]
fn conflicting_team_repo_claims_fail_closed_3316() {
    let home = tmp_home("conflicting-claims");
    let repo_a = home.join("repo-a");
    let repo_b = home.join("repo-b");
    git_repo_with_origin(&repo_a, "https://github.com/org/a.git");
    git_repo_with_origin(&repo_b, "https://github.com/org/b.git");
    write_sweep_fleet(
            &home,
            &format!(
                "  teamA:\n    members: [devA]\n    source_repo: {}\n    project_id: same-board\n  teamB:\n    members: [devB]\n    source_repo: {}\n    project_id: same-board\n",
                repo_a.display(),
                repo_b.display()
            ),
        );
    let server = pull_list_server("[]".to_string());
    handle_task_sweep_config(&home, &serde_json::json!({"api_base_url": server.base_url}));

    sweep_tick(&home).unwrap();

    assert_eq!(server.requests.load(Ordering::Acquire), 0);
    let health = task_sweep_health(&home);
    assert!(health["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["code"] == "CONFLICT_FAIL_CLOSED"));
    drop(server);
    fs::remove_dir_all(&home).ok();
}

#[test]
fn explicit_unmapped_project_is_named_access_with_health_evidence_3316() {
    let home = tmp_home("manual-unmapped");
    let created = crate::tasks::handle(
        &home,
        "devA",
        &serde_json::json!({
            "action": "create",
            "title": "named project task",
            "project": "operator-board"
        }),
    );
    assert!(created["id"].as_str().is_some());
    let health = task_sweep_health(&home);
    let entry = health["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["code"] == "MANUAL_UNMAPPED")
        .unwrap();
    assert_eq!(entry["project_id"], "operator-board");
    assert_eq!(entry["non_terminal_tasks"], 1);
    fs::remove_dir_all(&home).ok();
}

/// #2117 P2: the sweep enumerates ONE board per project — each fleet team's
/// `source_repo` board paired with ITS OWN derived GitHub repo, plus the
/// DEFAULT (home) board paired with `cfg.repo`. This 1:1 (board, repo)
/// pairing is what makes a merged PR in repo A match only board A's tasks
/// (#2105): the sweep never scans board B against repo A.
#[test]
fn resolve_sweep_boards_pairs_each_project_with_its_own_repo_2117_p2() {
    let home = tmp_home("p2-sweep-boards");
    let repo_a = home.join("srcA");
    let repo_b = home.join("srcB");
    git_repo_with_origin(&repo_a, "https://github.com/orgA/projA.git");
    git_repo_with_origin(&repo_b, "https://github.com/orgB/projB.git");
    fs::write(
        crate::fleet::fleet_yaml_path(&home),
        format!(
            "instances:\n  devA:\n    backend: claude\n  devB:\n    backend: claude\n\
                 teams:\n  teamA:\n    members:\n      - devA\n    source_repo: {}\n\
                 \x20 teamB:\n    members:\n      - devB\n    source_repo: {}\n",
            repo_a.display(),
            repo_b.display()
        ),
    )
    .unwrap();

    let cfg = SweepConfig {
        repo: Some("operator/primary".to_string()),
        ..Default::default()
    };
    let boards: std::collections::HashMap<String, String> =
        resolve_sweep_boards(&home, &cfg).into_iter().collect();

    // DEFAULT (home) board ← cfg.repo (operator override / single-project
    // fallback) — NEVER a project repo.
    assert_eq!(
        boards
            .get(crate::task_events::DEFAULT_PROJECT)
            .map(String::as_str),
        Some("operator/primary"),
        "default board must map to cfg.repo: {boards:?}"
    );
    // Each project board ← ITS OWN derived (canonical, lowercased) repo.
    let pa = crate::tasks::project_id_from_source_repo(&repo_a);
    let pb = crate::tasks::project_id_from_source_repo(&repo_b);
    assert_eq!(
        boards.get(&pa).map(String::as_str),
        Some("orga/proja"),
        "teamA board must pair with orgA/projA: {boards:?}"
    );
    assert_eq!(
        boards.get(&pb).map(String::as_str),
        Some("orgb/projb"),
        "teamB board must pair with orgB/projB: {boards:?}"
    );
    assert_eq!(
        boards.len(),
        3,
        "exactly default + 2 project boards: {boards:?}"
    );

    fs::remove_dir_all(&home).ok();
}

/// #2117 P2 byte-identical: with NO per-team `source_repo` (the
/// single-project shape every pre-P2 test assumes), the board set is exactly
/// `[(DEFAULT, cfg.repo)]` → board == home → the legacy single-repo tick.
#[test]
fn resolve_sweep_boards_single_project_is_default_only_2117_p2() {
    let home = tmp_home("p2-sweep-single");
    let cfg = SweepConfig {
        repo: Some("operator/primary".to_string()),
        ..Default::default()
    };
    let boards = resolve_sweep_boards(&home, &cfg);
    assert_eq!(
        boards,
        vec![(
            crate::task_events::DEFAULT_PROJECT.to_string(),
            "operator/primary".to_string()
        )],
        "single-project must resolve to exactly the DEFAULT board ← cfg.repo"
    );
    // And no repo configured at all → empty → tick is a no-op.
    let empty = resolve_sweep_boards(&home, &SweepConfig::default());
    assert!(
        empty.is_empty(),
        "no repo + no teams → no boards: {empty:?}"
    );
    fs::remove_dir_all(&home).ok();
}

/// #2117 close-isolation e2e (the #2125-review gap reviewer-2+4 both flagged):
/// a merged PR swept against board A auto-closes ONLY board A's task, even when
/// its body's `Closes t-…` marker also references a task on board B (#2105).
/// Exercises the real close path through the `sweep_board_with_prs` seam (no
/// GitHub round-trip) with a representative two-board fixture built via the real
/// `tasks::handle` create path.
#[test]
fn sweep_closes_only_same_board_task_2117_close_isolation() {
    let home = tmp_home("close-isolation");
    // Two project boards, each with an OPEN task created + assigned to
    // "test-user" (so the sweep's authorship gate passes via direct-name
    // compare — `make_pr_meta` stamps `author_login = "test-user"`).
    let mk = |project: &str| -> String {
        crate::tasks::handle(
                &home,
                "test-user",
                &serde_json::json!({"action": "create", "title": "t", "assignee": "test-user", "project": project}),
            )["id"]
                .as_str()
                .unwrap()
                .to_string()
    };
    let ta = mk("orgA/projA");
    let tb = mk("orgB/projB");

    // A merged PR in repo A whose body references BOTH boards' tasks.
    let pr = make_pr_meta(1, "fix", &format!("Closes {ta}\nCloses {tb}"));

    // Sweep board A only.
    let scanned =
        sweep_board_with_prs(&home, "orgA/projA", std::slice::from_ref(&pr), None, false).unwrap();
    assert!(
        scanned,
        "board A had a merged PR + an open task → full scan ran"
    );

    let status_on = |project: &str, id: &str| -> crate::task_events::TaskStatus {
        crate::tasks::list_all_at(&home, &crate::task_events::board_root(&home, project))
            .into_iter()
            .find(|t| t.id == id)
            .unwrap_or_else(|| panic!("task {id} not found on board {project}"))
            .status
    };
    // Board A's task auto-closed; board B's task UNTOUCHED — the sweep matched
    // the markers only against board A's `open_ids` (#2105 cross-board isolation).
    assert_eq!(
        status_on("orgA/projA", &ta),
        crate::task_events::TaskStatus::Done,
        "board A's task must auto-close"
    );
    assert_ne!(
        status_on("orgB/projB", &tb),
        crate::task_events::TaskStatus::Done,
        "board B's task must NOT be closed by repo A's PR (#2105 isolation)"
    );

    fs::remove_dir_all(&home).ok();
}

/// #78445-2 (d): a merged-PR sweep auto-close is a terminal transition — it must
/// settle the closed task's obligation stores (dispatch_idle sidecar AND
/// dispatch_tracking rows) via `task_terminal_cleanup`. This path previously
/// cleared NEITHER (reviewer4 #2679).
#[test]
fn sweep_close_settles_obligation_stores_78445_2() {
    let home = tmp_home("sweep-close-settle");
    let ta = crate::tasks::handle(
            &home,
            "test-user",
            &serde_json::json!({"action": "create", "title": "t", "assignee": "test-user", "project": "orgA/projA"}),
        )["id"]
            .as_str()
            .unwrap()
            .to_string();
    // Obligation rows for the task the merged PR will close.
    crate::daemon::dispatch_idle::record_dispatch(&home, "lead", "rev-a", Some(&ta), "task", 900);
    crate::dispatch_tracking::track_dispatch(
        &home,
        crate::dispatch_tracking::DispatchEntry {
            task_id: Some(ta.clone()),
            from: "lead".into(),
            to: "rev-a".into(),
            from_id: None,
            to_id: None,
            delegated_at: chrono::Utc::now().to_rfc3339(),
            status: "pending".into(),
        },
    );

    let pr = make_pr_meta(1, "fix", &format!("Closes {ta}"));
    sweep_board_with_prs(&home, "orgA/projA", std::slice::from_ref(&pr), None, false).unwrap();

    assert!(
        crate::daemon::dispatch_idle::list_pending(&home)
            .iter()
            .all(|d| d.correlation_id.as_deref() != Some(ta.as_str())),
        "merged-PR close must clear the dispatch_idle sidecar"
    );
    assert!(
        !crate::dispatch_tracking::active_target_names(&home).contains(&"rev-a".to_string()),
        "merged-PR close must settle the dispatch_tracking row"
    );
    fs::remove_dir_all(&home).ok();
}

// ── Sprint 56 Track F (#496): authorship gate via github_login ──

fn task_with(created_by: &str, assignee: Option<&str>) -> crate::tasks::Task {
    crate::tasks::Task {
        id: "t-1-1".into(),
        title: "x".into(),
        description: String::new(),
        status: crate::task_events::TaskStatus::Open,
        priority: crate::task_events::TaskPriority::Normal,
        assignee: assignee.map(str::to_string),
        routed_to: None,
        created_by: created_by.into(),
        depends_on: Vec::new(),
        result: None,
        created_at: "2026-05-08T00:00:00Z".into(),
        updated_at: "2026-05-08T00:00:00Z".into(),
        due_at: None,
        branch: None,
        started_at: None,
        eta_secs: None,
        auto_release_on_verdict: None,
        tags: vec![],
        parent_id: None,
        metadata: std::collections::BTreeMap::new(),
    }
}

fn fleet_with_login(instance: &str, github_login: &str) -> crate::fleet::FleetConfig {
    let mut instances = std::collections::HashMap::new();
    instances.insert(
        instance.into(),
        crate::fleet::InstanceConfig {
            github_login: Some(github_login.into()),
            ..Default::default()
        },
    );
    crate::fleet::FleetConfig {
        instances,
        ..Default::default()
    }
}

/// Lead-spec #1: with `github_login: alice` mapped on instance `dev`,
/// a PR by `alice` against a task created by `dev` must close.
#[test]
fn mapping_present_compares_against_github_login() {
    let task = task_with("dev", None);
    let fleet = fleet_with_login("dev", "alice");
    assert!(compute_author_ok("alice", &task, Some(&fleet)));
    // Case-insensitive — GitHub usernames are.
    assert!(compute_author_ok("Alice", &task, Some(&fleet)));
}

/// Lead-spec #2: no `github_login` mapping AND no fleet config — the
/// helper falls back to direct string compare against the agend
/// instance name. Compat path for deployments where instance name
/// happens to equal the operator's GitHub login (the implicit
/// assumption pre-Track-F).
#[test]
fn mapping_absent_falls_back_to_direct_compare() {
    let task = task_with("dev", None);
    // No fleet at all.
    assert!(compute_author_ok("dev", &task, None));
    assert!(!compute_author_ok("alice", &task, None));
    // Fleet present but no mapping for this instance.
    let mut fleet = crate::fleet::FleetConfig::default();
    fleet.instances.insert(
        "other".into(),
        crate::fleet::InstanceConfig {
            github_login: Some("bob".into()),
            ..Default::default()
        },
    );
    assert!(compute_author_ok("dev", &task, Some(&fleet)));
    assert!(!compute_author_ok("alice", &task, Some(&fleet)));
}

/// Lead-spec #3: when `github_login: alice` is mapped, a PR by `bob`
/// must NOT close — the security gate (PR-220 defense) is preserved
/// even after Track F's namespace fix.
#[test]
fn mapping_present_mismatch_does_not_close() {
    let task = task_with("dev", None);
    let fleet = fleet_with_login("dev", "alice");
    assert!(!compute_author_ok("bob", &task, Some(&fleet)));
    // The PR author equals the agend instance name string but the
    // mapping says the real login is `alice` — direct-name match
    // must NOT bypass the mapping when one is configured.
    assert!(!compute_author_ok("dev", &task, Some(&fleet)));
}

/// Mapping resolves through the assignee branch too — a task claimed
/// by `dev-impl` with `github_login: bob` must accept a PR by `bob`
/// even when the creator's mapping doesn't match.
#[test]
fn mapping_resolves_assignee_branch() {
    let task = task_with("lead", Some("dev-impl"));
    let mut fleet = crate::fleet::FleetConfig::default();
    fleet.instances.insert(
        "lead".into(),
        crate::fleet::InstanceConfig {
            github_login: Some("alice".into()),
            ..Default::default()
        },
    );
    fleet.instances.insert(
        "dev-impl".into(),
        crate::fleet::InstanceConfig {
            github_login: Some("bob".into()),
            ..Default::default()
        },
    );
    assert!(compute_author_ok("alice", &task, Some(&fleet))); // creator branch
    assert!(compute_author_ok("bob", &task, Some(&fleet))); // assignee branch
    assert!(!compute_author_ok("eve", &task, Some(&fleet))); // neither
}

/// Resolver: returns mapped login when fleet has it.
#[test]
fn resolve_github_login_returns_mapped() {
    let fleet = fleet_with_login("dev", "alice");
    assert_eq!(resolve_github_login(Some(&fleet), "dev"), Some("alice"));
}

/// Resolver: returns None when instance is not in the fleet (so the
/// caller's fall-back path engages rather than a false-negative).
#[test]
fn resolve_github_login_returns_none_for_unknown_instance() {
    let fleet = fleet_with_login("dev", "alice");
    assert_eq!(resolve_github_login(Some(&fleet), "ghost"), None);
    assert_eq!(resolve_github_login(None, "dev"), None);
}

/// Must-have #1 — HTML-comment injection: a directive hidden inside
/// a comment must NOT extract. Real-world task IDs use digit-only
/// segments (`t-<ts>-<seq>`); the test fixture uses the same shape.
#[test]
fn html_comment_injection_stripped() {
    let body = "Closes t-12345-1\n<!-- Closes t-99999-2 -->";
    let sanitised = crate::daemon::utils::strip_html_comments(body);
    let markers = extract_closes_markers(&sanitised);
    assert_eq!(markers, vec!["t-12345-1".to_string()]);
}

/// Must-have #1 — unterminated comment drops tail entirely; even a
/// trailing valid marker after a partial `<!--` is dropped (an
/// adversary playing fuzzing tricks can't slip a directive through).
#[test]
fn html_comment_unterminated_drops_tail() {
    let body = "Closes t-1-1\n<!-- partial Closes t-2-2";
    let sanitised = crate::daemon::utils::strip_html_comments(body);
    let markers = extract_closes_markers(&sanitised);
    assert_eq!(markers, vec!["t-1-1".to_string()]);
}

/// Must-have #2 — non-ASCII codepoint inside the task ID portion
/// (e.g. zero-width-joiner mimicking a digit) NEVER matches the
/// strict ASCII-digit regex.
#[test]
fn non_ascii_task_id_rejected() {
    // U+200B (zero-width-space) between digits.
    let body = "Closes t-12\u{200B}3-4";
    let markers = extract_closes_markers(body);
    assert!(
        markers.is_empty(),
        "non-ASCII codepoint must defeat the regex"
    );
}

/// Multiple markers in one PR body are all extracted.
#[test]
fn multiple_markers_extracted() {
    let body = "Closes t-1-1\nCloses t-2-2\nCloses t-3-3";
    let markers = extract_closes_markers(body);
    assert_eq!(
        markers,
        vec![
            "t-1-1".to_string(),
            "t-2-2".to_string(),
            "t-3-3".to_string()
        ]
    );
}

/// CR-2026-06-14 regression pin: a task id minted by the REAL
/// `tasks::handle(create)` path (now the three-segment cross-process-unique
/// `t-<ts>-<pid>-<seq>`) must round-trip through the `Closes`-marker
/// extractor WHOLE. With the pre-fix two-group regex the `\b` truncated it to
/// `t-<ts>-<pid>` (≠ the real id) → `open_ids.get(marker)` missed → the
/// PR-merge auto-close never fired for any new-format task. The narrow
/// id-format unit repro missed this cross-module consumer; this pins it.
#[test]
fn closes_marker_round_trips_real_minted_three_segment_id() {
    let home = std::env::temp_dir().join(format!(
        "agend-sweep-closes-roundtrip-{}-{}",
        std::process::id(),
        "rt"
    ));
    std::fs::create_dir_all(&home).expect("create temp home");
    let created = crate::tasks::handle(
        &home,
        "operator",
        &serde_json::json!({ "action": "create", "title": "round-trip me" }),
    );
    let id = created["id"]
        .as_str()
        .expect("create returns id")
        .to_string();
    // Sanity: the minted id is the new three-segment shape.
    assert_eq!(
        id.matches('-').count(),
        3,
        "expected three-segment id `t-<ts>-<pid>-<seq>`, got {id}"
    );

    let body = format!("Closes {id}");
    let markers = extract_closes_markers(&body);
    assert_eq!(
        markers,
        vec![id.clone()],
        "the extractor must return the WHOLE minted id, not a truncated prefix"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// `parse_pr_meta` rejects the PR if `user.login` is missing — our
/// authorship anchor (must-have #3) requires it. Better to drop the
/// PR than fall back to git trailer (the bug class we're defending).
#[test]
fn parse_rejects_missing_user_login() {
    let v = serde_json::json!({
        "number": 1,
        "state": "closed",
        "merge_commit_sha": "abc",
        "merged_at": "2026-04-27T00:00:00Z",
        "body": "",
        "user": {} // login missing
    });
    assert!(parse_pr_meta(&v, "h".into()).is_none());
}

/// Must-have #5 — `parse_pr_meta` carries the merge SHA so callers
/// can stamp it onto `PrSnapshot.merge_sha` at decision-time.
#[test]
fn parse_carries_merge_sha() {
    let v = serde_json::json!({
        "number": 42,
        "state": "closed",
        "merge_commit_sha": "abcdef1234",
        "merged_at": "2026-04-27T00:00:00Z",
        "body": "Closes t-1-1",
        "user": {"login": "dev-impl-1"}
    });
    let meta = parse_pr_meta(&v, "fakehash".into()).unwrap();
    assert_eq!(meta.merge_commit_sha.as_deref(), Some("abcdef1234"));
    assert_eq!(meta.author_login, "dev-impl-1");
    assert!(meta.merged); // merged_at non-null
}

/// `task_sweep_config` empty-string `repo` disables sweep (sets to
/// `None`) — operator's escape hatch when the repo identifier is
/// wrong and they want to clear it without `unsetenv`.
#[test]
fn config_tool_empty_repo_disables() {
    let home = tmp_home("config_empty");
    handle_task_sweep_config(&home, &serde_json::json!({"repository": "x/y"}));
    let r = handle_task_sweep_config(&home, &serde_json::json!({"repository": ""}));
    assert_eq!(r["repo"], serde_json::Value::Null);
    fs::remove_dir_all(&home).ok();
}

/// #1619: the merged-PR list URL is built from a configurable API
/// base (self-hosted GitHub Enterprise support) — NOT pinned to
/// #PR-C site-6: the client-side `count_checks_not_passed` must
/// reproduce the prior `--jq`
/// `[.[] | select(.state != "SUCCESS" and .state != "SKIPPED")] | length`
/// EXACTLY — incl. the fail-closed treatment of null / empty / unknown
/// states as not-passed. (gh-failure / unparseable → pr_checks Err →
/// the `Err(_)` violation arm in check_ci_green, also fail-closed.)
#[test]
fn count_checks_not_passed_reproduces_jq() {
    use crate::scm::CheckState;
    let mk = |state: &str| CheckState {
        name: "c".to_string(),
        state: state.to_string(),
    };
    // empty → 0 (all-passed → None violation).
    assert_eq!(count_checks_not_passed(&[]), 0);
    // all SUCCESS / SKIPPED → 0.
    assert_eq!(
        count_checks_not_passed(&[mk("SUCCESS"), mk("SKIPPED"), mk("SUCCESS")]),
        0
    );
    // mixed: one FAILURE among greens → 1.
    assert_eq!(
        count_checks_not_passed(&[mk("SUCCESS"), mk("FAILURE"), mk("SKIPPED")]),
        1
    );
    // unknown states count as not-passed.
    assert_eq!(
        count_checks_not_passed(&[mk("PENDING"), mk("NEUTRAL"), mk("CANCELLED")]),
        3
    );
    // null/empty state (parse_checks keeps it as "") → not-passed.
    assert_eq!(count_checks_not_passed(&[mk(""), mk("SUCCESS")]), 1);
    // case-sensitive: lowercase "success" is NOT the green sentinel.
    assert_eq!(count_checks_not_passed(&[mk("success")]), 1);
}

/// github.com. Trailing slashes on the base are trimmed.
#[test]
fn build_merged_prs_url_uses_configurable_base() {
    // Default github.com base — byte-identical to the pre-#1619 URL.
    assert_eq!(
            build_merged_prs_url(DEFAULT_GITHUB_API_BASE, "suzuke/agend-terminal"),
            "https://api.github.com/repos/suzuke/agend-terminal/pulls?state=closed&sort=updated&direction=desc&per_page=30"
        );
    // Self-hosted GHE base.
    assert_eq!(
            build_merged_prs_url("https://ghe.corp.example.com/api/v3", "team/proj"),
            "https://ghe.corp.example.com/api/v3/repos/team/proj/pulls?state=closed&sort=updated&direction=desc&per_page=30"
        );
    // Trailing slash on the base is trimmed (no double slash).
    assert_eq!(
            build_merged_prs_url("https://ghe.corp.example.com/api/v3/", "team/proj"),
            "https://ghe.corp.example.com/api/v3/repos/team/proj/pulls?state=closed&sort=updated&direction=desc&per_page=30"
        );
}

/// #1619: `api_base_url` round-trips through the config tool and an
/// empty string resets it to the github.com default (`None`).
#[test]
fn config_tool_api_base_url_round_trip() {
    let home = tmp_home("config_api_base");
    let r1 = handle_task_sweep_config(
        &home,
        &serde_json::json!({"api_base_url": "https://ghe.corp.example.com/api/v3"}),
    );
    assert_eq!(r1["api_base_url"], "https://ghe.corp.example.com/api/v3");
    // Reset to default with empty string.
    let r2 = handle_task_sweep_config(&home, &serde_json::json!({"api_base_url": ""}));
    assert_eq!(r2["api_base_url"], serde_json::Value::Null);
    // Default (unset) deserializes to None.
    assert!(SweepConfig::default().api_base_url.is_none());
    fs::remove_dir_all(&home).ok();
}

/// `sweep_tick` short-circuits when the config is missing or has no
/// repo set. Operator sees a no-op rather than a noisy GitHub call.
#[test]
fn tick_no_op_when_repo_unconfigured() {
    let home = tmp_home("tick_no_op");
    // No config file written. Tick must complete without error and
    // without writing to task_events.jsonl.
    sweep_tick(&home).unwrap();
    assert!(!home.join("task_events.jsonl").exists());
    fs::remove_dir_all(&home).ok();
}

/// `sha256_hex` produces a 64-char hex string. Forensic hash
/// fingerprint must be a deterministic crypto-grade digest so a
/// later auditor can correlate against an archived API response.
#[test]
fn sha256_hex_is_deterministic_64_hex() {
    let h1 = crate::daemon::utils::sha256_hex(b"hello");
    let h2 = crate::daemon::utils::sha256_hex(b"hello");
    assert_eq!(h1, h2);
    assert_eq!(h1.len(), 64);
    assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
    let h3 = crate::daemon::utils::sha256_hex(b"world");
    assert_ne!(h1, h3);
}

/// REJECT criteria (per dev-reviewer m-25) — exercise the
/// closes-marker extractor against the **actual `Closes t-` commit
/// messages on origin/main**. Defends against three failure modes
/// the unit-test fixtures alone can't catch:
///
/// 1. Real PR descriptions contain prose around the marker that
///    might trip the regex if the boundary check is loose.
/// 2. A future contributor edits a PR description AFTER merge to
///    add a malicious `Closes t-victim`; this real-repo audit lets
///    a subsequent CI run surface the regression.
/// 3. The on-main commit messages prove that PR1 + the wider Sprint
///    24 P0 wave actually used the marker convention sweep depends
///    on, not just the unit-test mocks.
///
/// Skipped (with diagnostic stderr) when `git` binary unavailable or
/// the cwd isn't a git repo. Tests should never *fail* due to env
/// shape — operator-run CI on a fresh checkout always has both.
#[test]
fn closes_markers_extract_cleanly_from_actual_main() {
    let output = match std::process::Command::new("git")
        .args([
            "log",
            "origin/main",
            "--grep=Closes t-",
            "--format=%B",
            "--all-match",
        ])
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("skip: git binary unavailable: {e}");
            return;
        }
    };
    if !output.status.success() {
        eprintln!(
            "skip: `git log` failed (likely no `origin/main` ref locally): exit={:?}",
            output.status.code()
        );
        return;
    }
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    if text.trim().is_empty() {
        eprintln!("skip: no `Closes t-` commits on origin/main yet");
        return;
    }
    let markers = extract_closes_markers(&text);
    // Every extracted marker MUST conform to strict format. A
    // regression where the extractor accepts (e.g.) `t-foo-1`
    // surfaces here. CR-2026-06-14: accept both the legacy two-segment id
    // and the three-segment cross-process-unique id `t-<ts>-<pid>-<seq>`.
    let strict = regex::Regex::new(r"^t-[0-9]+-[0-9]+(?:-[0-9]+)?$").unwrap();
    for m in &markers {
        assert!(
            strict.is_match(m),
            "marker `{m}` extracted from real-main commit but fails strict format check"
        );
    }
    eprintln!(
            "real-repo audit: extracted {} markers from {} bytes of `Closes t-` commit messages on origin/main",
            markers.len(),
            text.len()
        );
}

// ─── Issue #664 compliance scanner tests ─────────────────────────

fn make_pr_meta(number: u64, title: &str, body: &str) -> PrMeta {
    PrMeta {
        number,
        title: title.to_string(),
        state: "closed".to_string(),
        merged: true,
        merge_commit_sha: Some("abc123".to_string()),
        merged_at: Some("2026-05-12T00:00:00Z".to_string()),
        body: body.to_string(),
        author_login: "test-user".to_string(),
        api_response_hash: "deadbeef".to_string(),
    }
}

#[test]
fn compliance_review_verdict_detected() {
    let pr = make_pr_meta(1, "fix: something", "Review VERIFIED by reviewer-codex");
    assert!(has_review_verdict(&pr));
}

#[test]
fn compliance_review_verdict_missing() {
    let pr = make_pr_meta(2, "fix: something", "No review info here");
    assert!(!has_review_verdict(&pr));
}

#[test]
fn compliance_scope_linkage_task_id() {
    let pr = make_pr_meta(3, "fix: something", "Implements t-20260511-12");
    assert!(has_scope_linkage(&pr));
}

#[test]
fn compliance_scope_linkage_closes_issue() {
    let pr = make_pr_meta(4, "fix: something", "Closes #664");
    assert!(has_scope_linkage(&pr));
}

#[test]
fn compliance_scope_linkage_missing() {
    let pr = make_pr_meta(5, "fix: something", "Just a fix without linkage");
    assert!(!has_scope_linkage(&pr));
}

#[test]
fn compliance_docs_only_exception() {
    let files = vec!["docs/SKILLS.md".to_string(), "README.md".to_string()];
    assert!(is_docs_only_pr(&files));
}

#[test]
fn compliance_non_docs_pr() {
    let files = vec!["src/agent.rs".to_string(), "docs/README.md".to_string()];
    assert!(!is_docs_only_pr(&files));
}

#[test]
fn compliance_feature_flag_off() {
    let home = tmp_home("compliance-off");
    fs::create_dir_all(&home).unwrap();
    let cfg = SweepConfig {
        repo: Some("test/repo".to_string()),
        paused: false,
        dry_run: false,
        compliance_mode: "off".to_string(),
        last_seen_merged_at: None,
        alerted_prs: Vec::new(),
        api_base_url: None,
        provenance_acknowledgements: Vec::new(),
    };
    save_config(&home, &cfg).unwrap();
    let violations = compliance_sweep(&home, "test/repo", &[]);
    assert!(
        violations.is_empty(),
        "off mode should produce no violations"
    );
    fs::remove_dir_all(&home).ok();
}

#[test]
fn compliance_cursor_persisted() {
    let home = tmp_home("compliance-cursor");
    fs::create_dir_all(&home).unwrap();
    let mut cfg = SweepConfig {
        repo: Some("test/repo".to_string()),
        paused: false,
        dry_run: false,
        compliance_mode: "warn".to_string(),
        last_seen_merged_at: None,
        alerted_prs: Vec::new(),
        api_base_url: None,
        provenance_acknowledgements: Vec::new(),
    };
    // Simulate cursor update (done by compliance_sweep at end)
    cfg.last_seen_merged_at = Some("2026-05-12T00:00:00Z".to_string());
    cfg.alerted_prs.push(10);
    save_config(&home, &cfg).unwrap();

    let updated = load_config(&home);
    assert_eq!(
        updated.last_seen_merged_at.as_deref(),
        Some("2026-05-12T00:00:00Z")
    );
    assert!(updated.alerted_prs.contains(&10));
    fs::remove_dir_all(&home).ok();
}
