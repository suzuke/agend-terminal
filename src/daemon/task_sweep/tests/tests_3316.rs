use super::*;

fn legacy_fixture(tag: &str) -> (PathBuf, String) {
    let home = tmp_home(tag);
    let repo = home.join("legacy-repo");
    git_repo_with_origin(&repo, "https://github.com/org/legacy.git");
    write_sweep_fleet(
        &home,
        &format!(
            "  legacy:\n    members: [devA]\n    source_repo: {}\n    project_id: legacy-board\n",
            repo.display()
        ),
    );
    resolve_sweep_plan(&home, &load_config(&home)).unwrap();
    let entry = load_provenance(&home).entries.into_iter().next().unwrap();
    let key = provenance_key(&entry.project_id, &entry.repo, &entry.api_base);
    fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  devA:\n    backend: claude\nteams:\n",
    )
    .unwrap();
    (home, key)
}

fn set_legacy_since(home: &Path, timestamp: String) {
    let mut store = load_provenance(home);
    store.entries[0].legacy_since = Some(timestamp);
    save_provenance(home, &store).unwrap();
}

#[test]
fn fresh_legacy_transition_does_not_retire_from_old_observed_at_3316() {
    let (home, _) = legacy_fixture("legacy-fresh-transition");
    let mut store = load_provenance(&home);
    store.entries[0].observed_at = (chrono::Utc::now()
        - chrono::Duration::seconds(LEGACY_RETIREMENT_TTL_SECS + 1))
    .to_rfc3339();
    save_provenance(&home, &store).unwrap();

    let plan = resolve_sweep_plan(&home, &load_config(&home)).unwrap();
    assert_eq!(plan.boards.len(), 1);
    assert!(plan.boards[0].legacy_compat);
    let entry = &load_provenance(&home).entries[0];
    assert!(entry.legacy_since.is_some());
    assert!(entry.retired_at.is_none());
    fs::remove_dir_all(&home).ok();
}

#[test]
fn legacy_transition_retires_after_quiet_ttl_and_keeps_audit_entry_3316() {
    let (home, _) = legacy_fixture("legacy-ttl-retirement");
    resolve_sweep_plan(&home, &load_config(&home)).unwrap();
    set_legacy_since(
        &home,
        (chrono::Utc::now() - chrono::Duration::seconds(LEGACY_RETIREMENT_TTL_SECS + 1))
            .to_rfc3339(),
    );

    let plan = resolve_sweep_plan(&home, &load_config(&home)).unwrap();
    assert!(plan.boards.is_empty());
    let store = load_provenance(&home);
    assert_eq!(store.entries.len(), 1);
    assert_eq!(
        store.entries[0].retirement_reason.as_deref(),
        Some("quiet_ttl_elapsed")
    );
    fs::remove_dir_all(&home).ok();
}

#[test]
fn legacy_transition_with_nonterminal_tasks_never_retires_on_ttl_3316() {
    let (home, _) = legacy_fixture("legacy-nonterminal-retention");
    let created = crate::tasks::handle(
        &home,
        "devA",
        &serde_json::json!({
            "action": "create",
            "title": "legacy task",
            "project": "legacy-board"
        }),
    );
    assert!(created["id"].as_str().is_some());
    resolve_sweep_plan(&home, &load_config(&home)).unwrap();
    set_legacy_since(
        &home,
        (chrono::Utc::now() - chrono::Duration::seconds(LEGACY_RETIREMENT_TTL_SECS + 1))
            .to_rfc3339(),
    );

    let plan = resolve_sweep_plan(&home, &load_config(&home)).unwrap();
    assert_eq!(plan.boards.len(), 1);
    assert!(plan.boards[0].legacy_compat);
    assert!(load_provenance(&home).entries[0].retired_at.is_none());
    fs::remove_dir_all(&home).ok();
}

#[test]
fn operator_ack_retires_legacy_mapping_immediately_and_keeps_audit_3316() {
    let (home, key) = legacy_fixture("legacy-operator-ack");
    resolve_sweep_plan(&home, &load_config(&home)).unwrap();
    let response =
        handle_task_sweep_config(&home, &serde_json::json!({"acknowledge_provenance": [key]}));
    assert_eq!(
        response["provenance_acknowledgements"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let plan = resolve_sweep_plan(&home, &load_config(&home)).unwrap();
    assert!(plan.boards.is_empty());
    let store = load_provenance(&home);
    assert_eq!(store.entries.len(), 1);
    assert_eq!(
        store.entries[0].retirement_reason.as_deref(),
        Some("operator_acknowledged")
    );
    fs::remove_dir_all(&home).ok();
}

#[test]
fn config_tool_acknowledgement_round_trip_3316() {
    let home = tmp_home("config-acknowledgement");
    let result = handle_task_sweep_config(
        &home,
        &serde_json::json!({
            "acknowledge_provenance": ["board-a|org/repo|https://api.github.com"]
        }),
    );
    assert_eq!(
        result["provenance_acknowledgements"],
        serde_json::json!(["board-a|org/repo|https://api.github.com"])
    );
    fs::remove_dir_all(&home).ok();
}

#[cfg(unix)]
#[test]
fn board_a_close_path_failure_does_not_block_board_b_scan_3316() {
    use std::os::unix::fs::PermissionsExt;

    let home = tmp_home("board-close-error-isolation");
    let repo_a = home.join("repo-a");
    let repo_b = home.join("repo-b");
    git_repo_with_origin(&repo_a, "https://github.com/org/a.git");
    git_repo_with_origin(&repo_b, "https://github.com/org/b.git");
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
                "title": "board isolation task",
                "assignee": caller
            }),
        )["id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let task_a = make_task("devA");
    let task_b = make_task("devB");
    let board_a = crate::task_events::board_root(&home, "board-a");
    let lock_path = board_a.join("task_events.jsonl.lock");
    let mut permissions = fs::metadata(&lock_path).unwrap().permissions();
    permissions.set_mode(0o444);
    fs::set_permissions(&lock_path, permissions).unwrap();
    server.set_body(pr_json(&format!("Closes {task_a}\nCloses {task_b}")));

    sweep_tick(&home).unwrap();

    let task_a_state = crate::tasks::list_all_at(&home, &board_a)
        .into_iter()
        .find(|candidate| candidate.id == task_a)
        .unwrap();
    let board_b = crate::task_events::board_root(&home, "board-b");
    let task_b_state = crate::tasks::list_all_at(&home, &board_b)
        .into_iter()
        .find(|candidate| candidate.id == task_b)
        .unwrap();
    assert_eq!(task_a_state.status, crate::task_events::TaskStatus::Open);
    assert_eq!(task_b_state.status, crate::task_events::TaskStatus::Done);
    drop(server);
    fs::remove_dir_all(&home).ok();
}
