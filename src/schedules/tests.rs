use super::*;

fn job_args(home: &Path) -> Value {
    serde_json::json!({"cron": "0 9 * * *", "message": "podcast", "timezone": "UTC",
        "job": {"backends": ["codex", "claude"], "artifact_directory": home.join("artifacts")}})
}

#[test]
fn job_schedule_roundtrip_and_legacy_default() {
    let home = tmp_home("job-roundtrip");
    let created = create(&home, "creator", &job_args(&home));
    assert!(created.get("error").is_none(), "{created}");
    let job = load(&home).schedules.remove(0);
    assert!(job.target.is_empty());
    assert_eq!(job.job.as_ref().unwrap().timeout_secs, 3600);
    assert_eq!(
        serde_json::from_value::<Schedule>(serde_json::to_value(&job).unwrap()).unwrap(),
        job
    );
    assert_eq!(orphan_schedules_for_target(&home, ""), 0);
    let legacy: Schedule = serde_json::from_value(cron_row("legacy", "creator", true)).unwrap();
    assert!(legacy.job.is_none());
    std::fs::remove_dir_all(home).unwrap();
}

#[test]
fn job_schedule_rejects_invalid_config_and_mode_changes() {
    let home = tmp_home("job-invalid");
    for (field, value) in [
        ("backends", serde_json::json!([])),
        ("backends", serde_json::json!(["codex", "codex"])),
        ("backends", serde_json::json!(["shell"])),
        ("artifact_directory", serde_json::json!("relative")),
        ("timeout_secs", serde_json::json!(59)),
        ("max_attempts", serde_json::json!(0)),
        ("retry_delay_secs", serde_json::json!(3601)),
        ("unknown", serde_json::json!(true)),
    ] {
        let mut args = job_args(&home);
        args["job"][field] = value;
        assert!(
            create(&home, "creator", &args).get("error").is_some(),
            "{args}"
        );
    }
    for (field, value) in [
        ("instance", serde_json::json!("creator")),
        ("linked_task_id", serde_json::json!("task")),
        ("replacement_key", serde_json::json!("key")),
        ("fire_strategy", serde_json::json!("until_success")),
    ] {
        let mut args = job_args(&home);
        args[field] = value;
        assert!(
            create(&home, "creator", &args).get("error").is_some(),
            "{args}"
        );
    }
    assert!(load(&home).schedules.is_empty());
    let created = create(&home, "creator", &job_args(&home));
    let id = &created["id"];
    for patch in [
        serde_json::json!({"id": id, "job": null}),
        serde_json::json!({"id": id, "instance": "creator"}),
    ] {
        assert!(update(&home, &patch).get("error").is_some());
    }
    let mut args = job_args(&home);
    args.as_object_mut().unwrap().remove("job");
    let legacy = create(&home, "creator", &args);
    assert!(update(
        &home,
        &serde_json::json!({"id": legacy["id"], "job": job_args(&home)["job"]})
    )
    .get("error")
    .is_some());
    std::fs::remove_dir_all(home).unwrap();
}

#[test]
fn job_survives_real_orphan_sweep_and_paused_retention() {
    let home = tmp_home("job-orphan-retention");
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  alive:\n    backend: claude\n",
    )
    .unwrap();
    let created = create(&home, "creator", &job_args(&home));
    assert!(created.get("error").is_none(), "{created}");
    crate::daemon::orphan_sweep::run(&home);
    let after = load(&home).schedules.remove(0);
    assert!(after.enabled);
    assert!(after.disabled_reason.is_none());
    assert!(after.run_history.is_empty());
    let mut paused = serde_json::to_value(after).unwrap();
    paused["enabled"] = serde_json::json!(false);
    paused["disabled_reason"] = serde_json::json!({"kind": "operator_paused"});
    paused["disabled_at"] =
        serde_json::json!((chrono::Utc::now() - chrono::Duration::days(30)).to_rfc3339());
    seed_schedule_rows(&home, serde_json::json!([paused]));
    assert_eq!(gc_disabled_schedules(&home), 0);
    assert!(load(&home).schedules[0].job.is_some());
    std::fs::remove_dir_all(home).unwrap();
}

#[test]
fn job_oneshot_is_not_consumed_by_legacy_boot_replay() {
    let home = tmp_home("job-boot");
    let mut row = cron_row("job-once", "", true);
    row["trigger"] = serde_json::json!({"kind": "once", "at": (chrono::Utc::now() - chrono::Duration::minutes(5)).to_rfc3339()});
    row["job"] = job_args(&home)["job"].clone();
    seed_schedule_rows(&home, serde_json::json!([row]));
    assert!(replay_missed_oneshots(&home).is_empty());
    assert!(load(&home).schedules[0].enabled);
    std::fs::remove_dir_all(home).unwrap();
}

fn tmp_home(name: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-schedules-test-{}-{}-{}",
        std::process::id(),
        name,
        id
    ));
    std::fs::create_dir_all(&dir).ok();
    dir
}

fn future_iso() -> String {
    (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339()
}

fn seed_schedule_rows(home: &Path, rows: Value) {
    let store = serde_json::json!({"schema_version": 2, "schedules": rows});
    std::fs::write(
        home.join("schedules.json"),
        serde_json::to_vec_pretty(&store).unwrap(),
    )
    .unwrap();
}

fn cron_row(id: &str, target: &str, enabled: bool) -> Value {
    serde_json::json!({
        "id": id,
        "message": "m",
        "target": target,
        "trigger": {"kind": "cron", "expr": "0 9 * * *"},
        "enabled": enabled,
        "timezone": "UTC",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:00Z",
        "run_history": []
    })
}

#[test]
fn manual_disable_and_reenable_have_typed_provenance() {
    let home = tmp_home("typed-manual-disable");
    let created = create(
        &home,
        "lead",
        &serde_json::json!({"cron": "0 9 * * *", "message": "m"}),
    );
    let id = created["id"].as_str().unwrap();

    assert_eq!(
        update(&home, &serde_json::json!({"id": id, "enabled": false}))["status"],
        "updated"
    );
    let disabled = list(&home, &serde_json::json!({}));
    assert_eq!(
        disabled["schedules"][0]["disabled_reason"]["kind"],
        "operator_paused"
    );
    assert!(disabled["schedules"][0]["disabled_at"].is_string());

    assert_eq!(
        update(&home, &serde_json::json!({"id": id, "enabled": true}))["status"],
        "updated"
    );
    let enabled = list(&home, &serde_json::json!({}));
    assert!(enabled["schedules"][0].get("disabled_reason").is_none());
    assert!(enabled["schedules"][0].get("disabled_at").is_none());
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn replay_and_stale_drop_have_distinct_typed_provenance() {
    let home = tmp_home("typed-replay-disable");
    let now = chrono::Utc::now();
    seed_schedule_rows(
        &home,
        serde_json::json!([
            {
                "id": "replay", "message": "m", "target": "lead",
                "trigger": {"kind": "once", "at": (now - chrono::Duration::hours(1)).to_rfc3339()},
                "enabled": true, "timezone": "UTC", "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:00:00Z", "run_history": []
            },
            {
                "id": "stale", "message": "m", "target": "lead",
                "trigger": {"kind": "once", "at": (now - chrono::Duration::hours(48)).to_rfc3339()},
                "enabled": true, "timezone": "UTC", "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:00:00Z", "run_history": []
            }
        ]),
    );
    assert_eq!(replay_missed_oneshots(&home).len(), 1);
    let listed = list(&home, &serde_json::json!({"full_history": true}));
    let rows = listed["schedules"].as_array().unwrap();
    let row = |id: &str| rows.iter().find(|row| row["id"] == id).unwrap();
    assert_eq!(
        row("replay")["disabled_reason"]["kind"],
        "one_shot_replayed"
    );
    assert_eq!(
        row("stale")["disabled_reason"]["kind"],
        "one_shot_stale_dropped"
    );
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn later_target_deletion_updates_already_disabled_provenance_idempotently() {
    let home = tmp_home("typed-orphan-after-pause");
    let mut row = cron_row("paused", "doomed", false);
    row["disabled_reason"] = serde_json::json!({"kind": "operator_paused"});
    row["disabled_at"] = serde_json::json!("2026-01-01T00:00:00Z");
    seed_schedule_rows(&home, serde_json::json!([row]));

    assert_eq!(orphan_schedules_for_target(&home, "doomed"), 1);
    assert_eq!(orphan_schedules_for_target(&home, "doomed"), 0);
    let listed = list(&home, &serde_json::json!({"full_history": true}));
    assert_eq!(
        listed["schedules"][0]["disabled_reason"],
        serde_json::json!({"kind": "target_orphaned", "target": "doomed"})
    );
    assert_eq!(
        listed["schedules"][0]["run_history"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn supersession_records_typed_successor_identity() {
    let home = tmp_home("typed-supersession");
    let first = create(
        &home,
        "lead",
        &serde_json::json!({
            "cron": "0 9 * * *", "message": "old", "replacement_key": "singleton"
        }),
    );
    let second = create(
        &home,
        "lead",
        &serde_json::json!({
            "cron": "0 9 * * *", "message": "new", "replacement_key": "singleton"
        }),
    );
    let listed = list(&home, &serde_json::json!({}));
    let rows = listed["schedules"].as_array().unwrap();
    let old = rows.iter().find(|row| row["id"] == first["id"]).unwrap();
    assert_eq!(
        old["disabled_reason"],
        serde_json::json!({"kind": "superseded", "successor_id": second["id"]})
    );
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn gc_archives_only_eligible_rows_at_seven_day_boundary() {
    let home = tmp_home("typed-gc-boundary");
    let now = chrono::DateTime::parse_from_rfc3339("2026-08-16T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let old = "2026-08-09T00:00:00Z";
    let fresh = "2026-08-09T00:00:01Z";
    let mut eligible = cron_row("superseded-old", "lead", false);
    eligible["disabled_reason"] = serde_json::json!({"kind": "superseded", "successor_id": "new"});
    eligible["disabled_at"] = serde_json::json!(old);
    let mut before_boundary = cron_row("superseded-fresh", "lead", false);
    before_boundary["disabled_reason"] =
        serde_json::json!({"kind": "superseded", "successor_id": "new"});
    before_boundary["disabled_at"] = serde_json::json!(fresh);
    let mut paused = cron_row("paused", "lead", false);
    paused["disabled_reason"] = serde_json::json!({"kind": "operator_paused"});
    paused["disabled_at"] = serde_json::json!(old);
    let mut orphaned = cron_row("orphaned-recurring", "gone", false);
    orphaned["disabled_reason"] = serde_json::json!({"kind": "target_orphaned", "target": "gone"});
    orphaned["disabled_at"] = serde_json::json!(old);
    let mut legacy = cron_row("legacy-unknown", "lead", false);
    legacy["updated_at"] = serde_json::json!(old);
    seed_schedule_rows(
        &home,
        serde_json::json!([eligible, before_boundary, paused, orphaned, legacy]),
    );

    assert_eq!(gc_disabled_schedules_at(&home, now), 1);
    let store = load(&home);
    let ids: std::collections::HashSet<_> =
        store.schedules.iter().map(|row| row.id.as_str()).collect();
    assert!(!ids.contains("superseded-old"));
    assert!(ids.contains("superseded-fresh"));
    assert!(ids.contains("paused"));
    assert!(ids.contains("orphaned-recurring"));
    assert!(ids.contains("legacy-unknown"));

    let archive = std::fs::read_to_string(home.join("schedules-archive.jsonl")).unwrap();
    let entry: Value = serde_json::from_str(archive.trim()).unwrap();
    assert_eq!(entry["schedule"]["id"], "superseded-old");
    assert_eq!(entry["schedule"]["disabled_reason"]["kind"], "superseded");
    assert_eq!(gc_disabled_schedules_at(&home, now), 0, "GC is idempotent");
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn gc_archive_failure_preserves_live_store_row() {
    let home = tmp_home("typed-gc-archive-failure");
    let now = chrono::DateTime::parse_from_rfc3339("2026-08-16T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let mut eligible = cron_row("eligible", "lead", false);
    eligible["disabled_reason"] = serde_json::json!({"kind": "superseded", "successor_id": "new"});
    eligible["disabled_at"] = serde_json::json!("2026-08-01T00:00:00Z");
    seed_schedule_rows(&home, serde_json::json!([eligible]));
    std::fs::create_dir(home.join("schedules-archive.jsonl")).unwrap();

    assert_eq!(gc_disabled_schedules_at(&home, now), 0);
    assert_eq!(
        load(&home).schedules.len(),
        1,
        "archive failure must fail closed"
    );
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn gc_reuses_exact_prior_archive_after_partial_store_failure() {
    let home = tmp_home("typed-gc-partial-retry");
    let now = chrono::DateTime::parse_from_rfc3339("2026-08-16T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let mut eligible = cron_row("eligible", "lead", false);
    eligible["disabled_reason"] = serde_json::json!({"kind": "superseded", "successor_id": "new"});
    eligible["disabled_at"] = serde_json::json!("2026-08-01T00:00:00Z");
    seed_schedule_rows(&home, serde_json::json!([eligible]));
    let schedule = load(&home).schedules.into_iter().next().unwrap();
    let entry = ScheduleArchiveEntry {
        archived_at: "2026-08-15T00:00:00Z".to_string(),
        schedule,
    };
    append_schedule_archive_durably(&home.join("schedules-archive.jsonl"), &[entry]).unwrap();

    assert_eq!(gc_disabled_schedules_at(&home, now), 1);
    assert!(load(&home).schedules.is_empty());
    assert_eq!(
        std::fs::read_to_string(home.join("schedules-archive.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1,
        "retry must reuse exact durable evidence rather than duplicate it"
    );
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn gc_recovers_after_truncated_archive_tail_before_deleting() {
    let home = tmp_home("typed-gc-truncated-archive");
    let now = chrono::DateTime::parse_from_rfc3339("2026-08-16T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let mut eligible = cron_row("eligible", "lead", false);
    eligible["disabled_reason"] = serde_json::json!({"kind": "superseded", "successor_id": "new"});
    eligible["disabled_at"] = serde_json::json!("2026-08-01T00:00:00Z");
    seed_schedule_rows(&home, serde_json::json!([eligible]));
    std::fs::write(home.join("schedules-archive.jsonl"), b"{\"truncated\":").unwrap();

    assert_eq!(gc_disabled_schedules_at(&home, now), 1);
    assert!(load(&home).schedules.is_empty());
    let archived = read_schedule_archive(&home.join("schedules-archive.jsonl"));
    assert_eq!(archived.len(), 1);
    assert_eq!(archived[0].schedule.id, "eligible");
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn gc_exact_row_cas_preserves_concurrently_reenabled_schedule() {
    let home = tmp_home("typed-gc-concurrent-reenable");
    let now = chrono::DateTime::parse_from_rfc3339("2026-08-16T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let mut eligible = cron_row("eligible", "lead", false);
    eligible["disabled_reason"] = serde_json::json!({"kind": "superseded", "successor_id": "new"});
    eligible["disabled_at"] = serde_json::json!("2026-08-01T00:00:00Z");
    seed_schedule_rows(&home, serde_json::json!([eligible]));

    let removed = gc_disabled_schedules_at_before_delete(&home, now, || {
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    assert_eq!(
                        update(
                            &home,
                            &serde_json::json!({"id": "eligible", "enabled": true})
                        )["status"],
                        "updated"
                    );
                })
                .join()
                .unwrap();
        });
    });

    assert_eq!(removed, 0);
    let schedules = load(&home).schedules;
    assert_eq!(schedules.len(), 1);
    assert!(schedules[0].enabled);
    assert!(schedules[0].disabled_reason.is_none());
    assert!(schedules[0].disabled_at.is_none());
    assert_eq!(
        read_schedule_archive(&home.join("schedules-archive.jsonl")).len(),
        1
    );
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn test_create_list_update_delete() {
    let home = tmp_home("crud");
    let r = create(
        &home,
        "agent1",
        &serde_json::json!({"cron": "0 9 * * *", "message": "hello", "label": "morning"}),
    );
    assert_eq!(r["status"], "created");
    let id = r["id"].as_str().expect("id").to_string();

    let listed = list(&home, &serde_json::json!({}));
    assert_eq!(listed["schedules"].as_array().expect("arr").len(), 1);
    assert_eq!(listed["schedules"][0]["label"], "morning");
    assert_eq!(listed["schedules"][0]["trigger"]["kind"], "cron");
    assert_eq!(listed["schedules"][0]["trigger"]["expr"], "0 9 * * *");

    // Update
    update(&home, &serde_json::json!({"id": id, "enabled": false}));
    let listed = list(&home, &serde_json::json!({}));
    assert_eq!(listed["schedules"][0]["enabled"], false);

    // Delete
    let r = delete(&home, &serde_json::json!({"id": id}));
    assert_eq!(r["status"], "deleted");
    assert!(list(&home, &serde_json::json!({}))["schedules"]
        .as_array()
        .expect("arr")
        .is_empty());

    std::fs::remove_dir_all(&home).ok();
}

/// A newer automatic monitor for the same target and daemon-auto kind must
/// retire the old recurring row atomically. This is the public create/list
/// path that previously left exact-main monitors firing forever.
#[test]
fn create_auto_supersedes_same_target_kind_and_preserves_scope() {
    let home = tmp_home("auto-supersede");
    let old = create(
        &home,
        "lead",
        &serde_json::json!({
            "cron": "0 * * * *",
            "instance": "lead",
            "message": "[AGEND-AUTO kind=exact-main-quiescence-monitor] old sha",
            "subject_ref": "git:old-sha"
        }),
    );
    let old_id = old["id"].as_str().expect("old id").to_string();

    // Simulate a row written before replacement identity was persisted.
    // Its AGEND-AUTO marker must still participate in supersession.
    let path = store_path(&home);
    let mut legacy: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read schedule store"))
            .expect("parse schedule store");
    legacy["schedules"][0]
        .as_object_mut()
        .expect("legacy schedule row")
        .remove("replacement_key");
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&legacy).expect("serialize legacy store"),
    )
    .expect("seed legacy schedule store");

    let other_target = create(
        &home,
        "lead",
        &serde_json::json!({
            "cron": "0 * * * *",
            "instance": "peer",
            "message": "[AGEND-AUTO kind=exact-main-quiescence-monitor] peer sha"
        }),
    );
    let other_target_id = other_target["id"]
        .as_str()
        .expect("other target id")
        .to_string();
    let other_kind = create(
        &home,
        "lead",
        &serde_json::json!({
            "cron": "0 * * * *",
            "instance": "lead",
            "message": "[AGEND-AUTO kind=another-monitor] keep me"
        }),
    );
    let other_kind_id = other_kind["id"]
        .as_str()
        .expect("other kind id")
        .to_string();

    let replacement = create(
        &home,
        "lead",
        &serde_json::json!({
            "cron": "0 * * * *",
            "instance": "lead",
            "message": "[AGEND-AUTO kind=exact-main-quiescence-monitor] new sha",
            "subject_ref": "git:new-sha"
        }),
    );
    let replacement_id = replacement["id"]
        .as_str()
        .expect("replacement id")
        .to_string();
    assert_eq!(
        replacement["superseded_ids"],
        serde_json::json!([old_id]),
        "create must report the exact rows it retired"
    );

    let listed = list(&home, &serde_json::json!({"full_history": true}));
    let rows = listed["schedules"].as_array().expect("schedule rows");
    let row = |id: &str| {
        rows.iter()
            .find(|row| row["id"] == id)
            .unwrap_or_else(|| panic!("missing schedule {id}"))
    };

    let old_row = row(&old_id);
    assert_eq!(old_row["enabled"], false);
    assert_eq!(
        old_row["run_history"][0]["status"],
        format!("superseded_by:{replacement_id}")
    );
    assert_eq!(
        old_row["replacement_key"],
        "agend-auto:exact-main-quiescence-monitor"
    );
    assert_eq!(old_row["subject_ref"], "git:old-sha");

    let replacement_row = row(&replacement_id);
    assert_eq!(replacement_row["enabled"], true);
    assert_eq!(
        replacement_row["replacement_key"],
        "agend-auto:exact-main-quiescence-monitor"
    );
    assert_eq!(replacement_row["subject_ref"], "git:new-sha");
    assert_eq!(row(&other_target_id)["enabled"], true);
    assert_eq!(row(&other_kind_id)["enabled"], true);

    std::fs::remove_dir_all(&home).ok();
}

/// Concurrent replacement creates are linearizable: the store keeps every
/// audit row, but exactly the last committed row remains enabled.
#[test]
fn concurrent_replacement_creates_leave_one_enabled_winner() {
    const CREATORS: usize = 16;

    let home = tmp_home("concurrent-supersede");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(CREATORS));
    let mut threads = Vec::new();
    for index in 0..CREATORS {
        let home = home.clone();
        let barrier = std::sync::Arc::clone(&barrier);
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            create(
                &home,
                "lead",
                &serde_json::json!({
                    "cron": "0 * * * *",
                    "instance": "lead",
                    "message": format!("monitor revision {index}"),
                    "replacement_key": "runtime-acceptance",
                    "subject_ref": format!("git:sha-{index}")
                }),
            )
        }));
    }
    let responses: Vec<Value> = threads
        .into_iter()
        .map(|thread| thread.join().expect("creator thread"))
        .collect();
    assert!(responses
        .iter()
        .all(|response| response["status"] == "created"));

    let listed = list(&home, &serde_json::json!({"full_history": true}));
    let rows = listed["schedules"].as_array().expect("schedule rows");
    assert_eq!(rows.len(), CREATORS, "no audit row may be lost");
    assert!(rows
        .iter()
        .all(|row| row["replacement_key"] == "runtime-acceptance"));
    assert_eq!(
        rows.iter().filter(|row| row["enabled"] == true).count(),
        1,
        "last committed replacement must be the sole enabled row"
    );
    assert_eq!(
        rows.iter()
            .filter(|row| row["enabled"] == false)
            .filter(|row| {
                row["run_history"].as_array().is_some_and(|history| {
                    history.iter().any(|run| {
                        run["status"]
                            .as_str()
                            .is_some_and(|status| status.starts_with("superseded_by:"))
                    })
                })
            })
            .count(),
        CREATORS - 1,
        "every losing row must carry a durable supersession audit"
    );

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn replacement_identity_is_bounded_and_create_only() {
    let home = tmp_home("replacement-identity-validation");
    let empty = create(
        &home,
        "lead",
        &serde_json::json!({
            "cron": "0 * * * *",
            "message": "monitor",
            "replacement_key": "   "
        }),
    );
    assert!(empty["error"]
        .as_str()
        .is_some_and(|error| error.contains("must not be empty")));

    let created = create(
        &home,
        "lead",
        &serde_json::json!({
            "cron": "0 * * * *",
            "instance": "lead",
            "message": "monitor",
            "replacement_key": "runtime-acceptance",
            "subject_ref": "git:sha"
        }),
    );
    let id = created["id"].as_str().expect("created id");
    let identity_update = update(
        &home,
        &serde_json::json!({"id": id, "subject_ref": "git:other"}),
    );
    assert!(identity_update["error"]
        .as_str()
        .is_some_and(|error| error.contains("create-only")));
    let target_update = update(&home, &serde_json::json!({"id": id, "instance": "other"}));
    assert!(target_update["error"]
        .as_str()
        .is_some_and(|error| error.contains("target is immutable")));

    std::fs::remove_dir_all(&home).ok();
}

/// #1720 ③: `schedule list` carries a computed `next_scheduled_fire_at` —
/// non-null + parseable for an enabled cron schedule, and null once disabled
/// (so an operator can see when each schedule is next due).
#[test]
fn list_includes_next_scheduled_fire_at_1720() {
    let home = tmp_home("next-fire");
    let r = create(
        &home,
        "agent1",
        &serde_json::json!({"cron": "0 9 * * *", "message": "hi", "timezone": "UTC"}),
    );
    let id = r["id"].as_str().expect("id").to_string();

    let listed = list(&home, &serde_json::json!({}));
    let next = listed["schedules"][0]["next_scheduled_fire_at"]
        .as_str()
        .expect("enabled cron must expose a next_scheduled_fire_at");
    let parsed = chrono::DateTime::parse_from_rfc3339(next).expect("RFC3339");
    assert!(
        parsed.with_timezone(&chrono::Utc) > chrono::Utc::now(),
        "next fire must be in the future: {next}"
    );

    // Disabled → null (won't fire).
    update(&home, &serde_json::json!({"id": id, "enabled": false}));
    let listed = list(&home, &serde_json::json!({}));
    assert!(
        listed["schedules"][0]["next_scheduled_fire_at"].is_null(),
        "a disabled schedule must report next_scheduled_fire_at = null"
    );

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_run_history() {
    let home = tmp_home("run_history");
    let r = create(
        &home,
        "a",
        &serde_json::json!({"cron": "* * * * *", "message": "test"}),
    );
    let id = r["id"].as_str().expect("id").to_string();

    record_run(&home, &id, "ok");
    record_run(&home, &id, "ok");
    record_run(&home, &id, "inject_failed");

    let listed = list(&home, &serde_json::json!({}));
    let history = listed["schedules"][0]["run_history"]
        .as_array()
        .expect("arr");
    assert_eq!(history.len(), 3);
    assert_eq!(history[2]["status"], "inject_failed");

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn detect_timezone_returns_parseable_iana_name() {
    // Downstream (`cron_tick.rs`) parses the result via `chrono_tz::Tz::from_str`.
    // Lock in that contract across platforms so a Windows run cannot silently
    // produce a Windows-only TZ name (e.g. "Taipei Standard Time") that the
    // cron tick would reject. The value is either whatever the CI host's
    // system TZ maps to, or the "UTC" fallback — both must parse.
    let tz = super::detect_timezone();
    assert!(!tz.is_empty(), "detect_timezone returned empty string");
    tz.parse::<chrono_tz::Tz>().unwrap_or_else(|e| {
        panic!("detect_timezone returned {tz:?} which chrono_tz cannot parse: {e}")
    });
}

#[test]
fn test_filter_by_target() {
    let home = tmp_home("filter_target");
    create(
        &home,
        "a",
        &serde_json::json!({"cron": "0 9 * * *", "message": "m1", "instance": "agent1"}),
    );
    create(
        &home,
        "a",
        &serde_json::json!({"cron": "0 10 * * *", "message": "m2", "instance": "agent2"}),
    );

    let listed = list(&home, &serde_json::json!({"instance": "agent1"}));
    assert_eq!(listed["schedules"].as_array().expect("arr").len(), 1);

    std::fs::remove_dir_all(&home).ok();
}

// --- v2 one-shot + migration tests ---

#[test]
fn create_with_run_at_stores_once_trigger() {
    let home = tmp_home("once_create");
    let at = future_iso();
    let r = create(
        &home,
        "a",
        &serde_json::json!({"run_at": at, "message": "once", "timezone": "UTC"}),
    );
    assert_eq!(r["status"], "created", "create response: {r}");

    let listed = list(&home, &serde_json::json!({}));
    let trig = &listed["schedules"][0]["trigger"];
    assert_eq!(trig["kind"], "once");
    assert!(!trig["at"].as_str().expect("at").is_empty());
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn create_rejects_both_cron_and_run_at() {
    let home = tmp_home("both");
    let at = future_iso();
    let r = create(
        &home,
        "a",
        &serde_json::json!({"cron": "* * * * *", "run_at": at, "message": "x"}),
    );
    assert!(r["error"]
        .as_str()
        .expect("err")
        .contains("mutually exclusive"));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn create_rejects_neither() {
    let home = tmp_home("neither");
    let r = create(&home, "a", &serde_json::json!({"message": "x"}));
    let e = r["error"].as_str().expect("err");
    assert!(e.contains("cron") && e.contains("run_at"), "got: {e}");
    std::fs::remove_dir_all(&home).ok();
}

// ── #1521: fire-strategy validation + backward-compat ──

/// #1608: create a REAL task on the event-sourced board (so the strict
/// `tasks::load_routed` finds it), NOT a `tasks/<id>.json` file. The old
/// `seed_task_file` wrote a file the real lookup never reads — which is
/// exactly why this test passed while the feature was broken. Mirrors the
/// task subsystem's own `create_task` helper (tasks/handler.rs).
fn seed_real_task(home: &Path, id: &str) {
    crate::task_events::append(
        home,
        &crate::task_events::InstanceName::from("test:operator"),
        crate::task_events::TaskEvent::Created {
            task_id: crate::task_events::TaskId(id.into()),
            title: "test task".into(),
            description: String::new(),
            priority: "normal".into(),
            owner: None,
            due_at: None,
            depends_on: Vec::new(),
            routed_to: None,
            branch: None,
            bind: None,
            eta_secs: None,
            tags: vec![],
            parent_id: None,
            governing_decision_id: None,
            review_class: None,
        },
    )
    .expect("seed real task");
    // Sanity: the authoritative lookup the fix relies on must now resolve.
    assert!(
        crate::tasks::load_routed(home, id).is_ok(),
        "seeded task '{id}' must be visible to tasks::load_routed"
    );
}

#[test]
fn create_default_fire_strategy_is_always_backward_compat() {
    let home = tmp_home("fs-default");
    let r = create(
        &home,
        "a",
        &serde_json::json!({"cron": "0 9 * * *", "message": "x"}),
    );
    assert_eq!(r["status"], "created", "resp: {r}");
    assert_eq!(load(&home).schedules[0].fire_strategy, FireStrategy::Always);
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn create_until_success_without_task_rejected() {
    let home = tmp_home("fs-notask");
    let r = create(
        &home,
        "a",
        &serde_json::json!({"cron": "0 9 * * *", "message": "x",
            "fire_strategy": "until_success"}),
    );
    assert!(
        r["error"].as_str().expect("err").contains("linked_task_id"),
        "got: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn create_until_success_with_missing_task_rejected() {
    let home = tmp_home("fs-missingtask");
    let r = create(
        &home,
        "a",
        &serde_json::json!({"cron": "0 9 * * *", "message": "x",
            "fire_strategy": "until_success", "linked_task_id": "t-nope"}),
    );
    assert!(
        r["error"].as_str().expect("err").contains("not found"),
        "got: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn create_until_success_with_existing_task_ok() {
    let home = tmp_home("fs-ok");
    seed_real_task(&home, "t-real");
    let r = create(
        &home,
        "a",
        &serde_json::json!({"cron": "0 9 * * *", "message": "x",
            "fire_strategy": "until_success", "linked_task_id": "t-real"}),
    );
    assert_eq!(r["status"], "created", "resp: {r}");
    let s = &load(&home).schedules[0];
    assert_eq!(s.fire_strategy, FireStrategy::UntilSuccess);
    assert_eq!(s.linked_task_id.as_deref(), Some("t-real"));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn update_away_from_until_success_clears_date_but_retains_link() {
    let home = tmp_home("3246-transition-away");
    seed_real_task(&home, "t-live");
    let created = create(
        &home,
        "a",
        &serde_json::json!({
            "cron": "0 9 * * *",
            "message": "x",
            "fire_strategy": "until_success",
            "linked_task_id": "t-live"
        }),
    );
    let id = created["id"].as_str().expect("id");
    mark_success_today(&home, id, "2026-08-14");

    let updated = update(
        &home,
        &serde_json::json!({"id": id, "fire_strategy": "always"}),
    );
    assert_eq!(updated["status"], "updated", "resp: {updated}");
    let stored = load(&home);
    let schedule = &stored.schedules[0];
    assert_eq!(schedule.fire_strategy, FireStrategy::Always);
    assert_eq!(schedule.linked_task_id.as_deref(), Some("t-live"));
    assert!(schedule.last_success_date.is_none());
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn list_masks_legacy_date_and_reports_link_existence() {
    let home = tmp_home("3246-list-projection");
    seed_real_task(&home, "t-live");
    let created = create(
        &home,
        "a",
        &serde_json::json!({
            "cron": "0 9 * * *",
            "message": "x",
            "linked_task_id": "t-live"
        }),
    );
    let id = created["id"].as_str().expect("id").to_string();
    crate::store::mutate_versioned(&store_path(&home), |store: &mut ScheduleStore| {
        let schedule = store.schedules.iter_mut().find(|s| s.id == id).unwrap();
        schedule.last_success_date = Some("2026-07-01".into());
        Ok(())
    })
    .expect("seed legacy fossil");

    assert_eq!(
        load(&home).schedules[0].last_success_date.as_deref(),
        Some("2026-07-01"),
        "load must preserve legacy bytes"
    );
    let listed = list(&home, &serde_json::json!({}));
    let row = &listed["schedules"][0];
    assert!(row.get("last_success_date").is_none(), "row: {row}");
    assert_eq!(row["linked_task_exists"], true, "row: {row}");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn list_reports_dangling_link_and_same_update_can_repoint_when_arming() {
    let home = tmp_home("3246-repoint");
    seed_real_task(&home, "t-live");
    let created = create(
        &home,
        "a",
        &serde_json::json!({
            "cron": "0 9 * * *",
            "message": "x",
            "linked_task_id": "t-missing"
        }),
    );
    let id = created["id"].as_str().expect("id");
    let before = list(&home, &serde_json::json!({}));
    assert_eq!(before["schedules"][0]["linked_task_exists"], false);

    let rejected = update(
        &home,
        &serde_json::json!({"id": id, "fire_strategy": "until_success"}),
    );
    assert!(rejected["error"]
        .as_str()
        .is_some_and(|e| e.contains("not found")));
    assert_eq!(load(&home).schedules[0].fire_strategy, FireStrategy::Always);

    let armed = update(
        &home,
        &serde_json::json!({
            "id": id,
            "fire_strategy": "until_success",
            "linked_task_id": "t-live"
        }),
    );
    assert_eq!(armed["status"], "updated", "resp: {armed}");
    let after = list(&home, &serde_json::json!({}));
    assert_eq!(after["schedules"][0]["linked_task_exists"], true);
    std::fs::remove_dir_all(&home).ok();
}

/// #1608: pin every branch of `validate_fire_strategy` directly — the
/// happy path (`until_success` + an existing event-sourced task) was
/// permanently unreachable before this fix because the old check probed a
/// non-existent `tasks/<id>.json` file.
#[test]
fn validate_fire_strategy_all_branches() {
    let home = tmp_home("fs-validate");
    seed_real_task(&home, "t-live");

    // until_success + a real task on the board → Ok (the regression).
    assert!(
        validate_fire_strategy(&home, FireStrategy::UntilSuccess, Some("t-live")).is_ok(),
        "until_success with an existing task must validate"
    );
    // until_success + non-existent / empty / missing id → Err.
    assert!(validate_fire_strategy(&home, FireStrategy::UntilSuccess, Some("t-ghost")).is_err());
    assert!(validate_fire_strategy(&home, FireStrategy::UntilSuccess, Some("")).is_err());
    assert!(validate_fire_strategy(&home, FireStrategy::UntilSuccess, None).is_err());
    // always → Ok regardless of linked_task_id (early return, unaffected).
    assert!(validate_fire_strategy(&home, FireStrategy::Always, None).is_ok());
    assert!(validate_fire_strategy(&home, FireStrategy::Always, Some("t-ghost")).is_ok());

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn schedule_row_without_fire_fields_loads_as_always() {
    // v1/v2 rows predating #1521 carry no fire_strategy/linked_task_id.
    let raw: ScheduleRaw = serde_json::from_value(serde_json::json!({
        "id": "s-old", "message": "m", "cron": "0 9 * * *",
    }))
    .expect("legacy row deserializes");
    let sched: Schedule = raw.into();
    assert_eq!(sched.fire_strategy, FireStrategy::Always);
    assert!(sched.linked_task_id.is_none());
    assert!(sched.last_success_date.is_none());
}

#[test]
fn create_rejects_past_run_at() {
    let home = tmp_home("past");
    let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    let r = create(
        &home,
        "a",
        &serde_json::json!({"run_at": past, "message": "x"}),
    );
    assert!(r["error"].as_str().expect("err").contains("future"));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn create_accepts_naive_run_at_with_timezone() {
    let home = tmp_home("naive");
    // Future wall-clock in Taipei; 10 years ahead to stay future-relative.
    let at = "2036-04-21T15:30:00";
    let r = create(
        &home,
        "a",
        &serde_json::json!({
            "run_at": at,
            "message": "x",
            "timezone": "Asia/Taipei",
        }),
    );
    assert_eq!(r["status"], "created", "resp: {r}");
    let listed = list(&home, &serde_json::json!({}));
    let at_out = listed["schedules"][0]["trigger"]["at"]
        .as_str()
        .expect("at");
    // Taipei is UTC+08:00, no DST — the stored RFC 3339 must reflect that.
    assert!(
        at_out.ends_with("+08:00"),
        "expected +08:00 offset, got: {at_out}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn invalid_cron_rejected() {
    let home = tmp_home("badcron");
    let r = create(
        &home,
        "a",
        &serde_json::json!({"cron": "not a cron", "message": "x"}),
    );
    assert!(r["error"].as_str().expect("err").contains("invalid cron"));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn update_can_switch_trigger_kind() {
    let home = tmp_home("switch");
    let r = create(
        &home,
        "a",
        &serde_json::json!({"cron": "0 9 * * *", "message": "x"}),
    );
    let id = r["id"].as_str().expect("id").to_string();

    let at = future_iso();
    let upd = update(&home, &serde_json::json!({"id": id, "run_at": at}));
    assert_eq!(upd["status"], "updated", "resp: {upd}");
    let listed = list(&home, &serde_json::json!({}));
    assert_eq!(listed["schedules"][0]["trigger"]["kind"], "once");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn legacy_v1_file_migrates_on_load() {
    let home = tmp_home("migrate");
    // Hand-write a v1 schedules.json — top-level `cron` on each row,
    // schema_version omitted (= 0 legacy).
    let v1 = r#"{
        "schedules": [
            {
                "id": "s-legacy",
                "cron": "0 9 * * *",
                "message": "legacy",
                "target": "a",
                "label": null,
                "timezone": "UTC",
                "enabled": true,
                "created_by": "test",
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:00:00Z",
                "run_history": []
            }
        ]
    }"#;
    std::fs::write(home.join("schedules.json"), v1).expect("seed");

    // Reading via list() should surface the legacy row as a v2 Cron trigger.
    let listed = list(&home, &serde_json::json!({}));
    let row = &listed["schedules"][0];
    assert_eq!(row["id"], "s-legacy");
    assert_eq!(row["trigger"]["kind"], "cron");
    assert_eq!(row["trigger"]["expr"], "0 9 * * *");

    // And a write-path call (update) must stamp schema_version=2 on save.
    let upd = update(
        &home,
        &serde_json::json!({"id": "s-legacy", "enabled": false}),
    );
    assert_eq!(upd["status"], "updated");
    let on_disk = std::fs::read_to_string(home.join("schedules.json")).expect("read");
    assert!(
        on_disk.contains("\"schema_version\": 2"),
        "migrated file must stamp v2; got: {on_disk}"
    );
    // And the `cron` field must be gone / `trigger` present.
    assert!(on_disk.contains("\"trigger\""), "post-save: {on_disk}");

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_replay_missed_oneshot_on_load() {
    let home = tmp_home("replay_missed");
    // Seed a one-shot that fired 1 hour ago (within 24h window)
    let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    let store = serde_json::json!({
        "schema_version": 2,
        "schedules": [{
            "id": "s-missed",
            "trigger": {"kind": "once", "at": past},
            "message": "replay me",
            "target": "agent1",
            "label": "test",
            "timezone": "UTC",
            "enabled": true,
            "created_by": "test",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "run_history": []
        }]
    });
    std::fs::write(home.join("schedules.json"), store.to_string()).ok();

    let replayed = replay_missed_oneshots(&home);
    assert_eq!(
        replayed.len(),
        1,
        "missed one-shot within 24h must be replayed"
    );
    assert_eq!(replayed[0].id, "s-missed");
    assert_eq!(replayed[0].message, "replay me");

    // Schedule must be disabled after replay
    let listed = list(&home, &serde_json::json!({}));
    assert_eq!(listed["schedules"][0]["enabled"], false);
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_drop_stale_oneshot() {
    let home = tmp_home("stale_drop");
    // Seed a one-shot that fired 48 hours ago (beyond 24h cutoff)
    let stale = (chrono::Utc::now() - chrono::Duration::hours(48)).to_rfc3339();
    let store = serde_json::json!({
        "schema_version": 2,
        "schedules": [{
            "id": "s-stale",
            "trigger": {"kind": "once", "at": stale},
            "message": "too old",
            "target": "agent1",
            "label": null,
            "timezone": "UTC",
            "enabled": true,
            "created_by": "test",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "run_history": []
        }]
    });
    std::fs::write(home.join("schedules.json"), store.to_string()).ok();

    let replayed = replay_missed_oneshots(&home);
    assert!(
        replayed.is_empty(),
        "stale one-shot (>24h) must NOT be replayed"
    );

    // Schedule must still be disabled
    let listed = list(&home, &serde_json::json!({}));
    assert_eq!(listed["schedules"][0]["enabled"], false);
    // run_history should record stale_dropped
    let history = listed["schedules"][0]["run_history"]
        .as_array()
        .expect("arr");
    assert_eq!(history[0]["status"], "stale_dropped");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_cron_not_replayed() {
    let home = tmp_home("cron_skip");
    let store = serde_json::json!({
        "schema_version": 2,
        "schedules": [{
            "id": "s-cron",
            "trigger": {"kind": "cron", "expr": "0 9 * * *"},
            "message": "daily",
            "target": "agent1",
            "label": null,
            "timezone": "UTC",
            "enabled": true,
            "created_by": "test",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "run_history": []
        }]
    });
    std::fs::write(home.join("schedules.json"), store.to_string()).ok();

    let replayed = replay_missed_oneshots(&home);
    assert!(replayed.is_empty(), "cron schedules must NOT be replayed");

    // Cron schedule must remain enabled
    let listed = list(&home, &serde_json::json!({}));
    assert_eq!(listed["schedules"][0]["enabled"], true);
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_replay_fires_message_to_inbox_on_restart() {
    // Integration test: simulate daemon restart with a missed one-shot.
    // Pre-seed schedules.json with a one-shot whose run_at is 1 hour ago.
    // Call replay_missed_oneshots (as daemon startup would), then fire
    // each returned schedule into the inbox (no live agent → inbox path).
    // Verify: message appears in inbox AND schedule is disabled.
    let home = tmp_home("restart_replay");
    let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    let store = serde_json::json!({
        "schema_version": 2,
        "schedules": [{
            "id": "s-restart",
            "trigger": {"kind": "once", "at": past},
            "message": "check inbox after restart",
            "target": "agent-replay",
            "label": "restart-test",
            "timezone": "UTC",
            "enabled": true,
            "created_by": "test",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "run_history": []
        }]
    });
    std::fs::write(home.join("schedules.json"), store.to_string()).ok();

    // Simulate what replay_missed_at_startup does:
    let missed = replay_missed_oneshots(&home);
    assert_eq!(missed.len(), 1);
    for sched in &missed {
        let _ = crate::inbox::enqueue(
            &home,
            &sched.target,
            crate::inbox::InboxMessage::new_system(
                "system:schedule",
                "schedule_replay",
                sched.message.clone(),
            ),
        );
    }

    // Verify message landed in inbox
    let msgs = crate::inbox::drain(&home, "agent-replay");
    assert_eq!(msgs.len(), 1, "replayed message must appear in inbox");
    assert_eq!(msgs[0].text, "check inbox after restart");
    assert_eq!(msgs[0].kind.as_deref(), Some("schedule_replay"));

    // Verify schedule is disabled
    let listed = list(&home, &serde_json::json!({}));
    assert_eq!(listed["schedules"][0]["enabled"], false);
    // run_history should record "replayed"
    let history = listed["schedules"][0]["run_history"]
        .as_array()
        .expect("arr");
    assert_eq!(history[0]["status"], "replayed");

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn schedule_ids_unique_when_created_rapidly() {
    // H3: two schedules created in rapid succession must have distinct IDs
    let home = tmp_home("id_unique");
    let args1 = serde_json::json!({"message": "a", "cron": "0 * * * *"});
    let args2 = serde_json::json!({"message": "b", "cron": "0 * * * *"});
    let r1 = create(&home, "test", &args1);
    let r2 = create(&home, "test", &args2);
    let id1 = r1["id"].as_str().expect("id1");
    let id2 = r2["id"].as_str().expect("id2");
    assert_ne!(id1, id2, "rapid-fire schedule IDs must be unique");
    std::fs::remove_dir_all(&home).ok();
}

// ── #1488 cascade: orphan schedules when their target is deleted ──

fn seed_two_schedules(home: &Path) {
    let store = serde_json::json!({
        "schema_version": 2,
        "schedules": [
            {"id": "s-doomed", "message": "m", "target": "doomed",
             "trigger": {"kind": "cron", "expr": "0 9 * * *"}, "enabled": true,
             "timezone": "UTC", "created_at": "2026-01-01T00:00:00Z",
             "updated_at": "2026-01-01T00:00:00Z", "run_history": []},
            {"id": "s-alive", "message": "m", "target": "alive",
             "trigger": {"kind": "cron", "expr": "0 9 * * *"}, "enabled": true,
             "timezone": "UTC", "created_at": "2026-01-01T00:00:00Z",
             "updated_at": "2026-01-01T00:00:00Z", "run_history": []}
        ]
    });
    std::fs::write(
        home.join("schedules.json"),
        serde_json::to_string_pretty(&store).unwrap(),
    )
    .unwrap();
}

#[test]
fn orphan_schedules_disables_target_and_marks_history_leaving_others() {
    let home = tmp_home("orphan-sched");
    seed_two_schedules(&home);
    let n = orphan_schedules_for_target(&home, "doomed");
    assert_eq!(n, 1, "exactly the doomed-targeting schedule is orphaned");
    let store = load(&home);
    let doomed = store.schedules.iter().find(|s| s.id == "s-doomed").unwrap();
    assert!(!doomed.enabled, "doomed schedule must be disabled");
    assert!(
        doomed
            .run_history
            .last()
            .is_some_and(|r| r.status.contains("orphaned")),
        "doomed schedule must carry an orphaned run_history marker"
    );
    let alive = store.schedules.iter().find(|s| s.id == "s-alive").unwrap();
    assert!(alive.enabled, "unrelated schedule must stay enabled");
    assert!(alive.run_history.is_empty(), "unrelated schedule untouched");
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn orphan_schedules_is_idempotent_no_double_marking() {
    let home = tmp_home("orphan-sched-idem");
    seed_two_schedules(&home);
    assert_eq!(orphan_schedules_for_target(&home, "doomed"), 1);
    // Second sweep: already disabled → no-op, no extra history entry.
    assert_eq!(
        orphan_schedules_for_target(&home, "doomed"),
        0,
        "re-sweep of an already-orphaned schedule must be a no-op"
    );
    let store = load(&home);
    let doomed = store.schedules.iter().find(|s| s.id == "s-doomed").unwrap();
    assert_eq!(
        doomed.run_history.len(),
        1,
        "idempotent: run_history must not grow on repeated sweeps"
    );
    std::fs::remove_dir_all(home).ok();
}

// ── #2037 (2): list run_history cap ──

/// Default list trims run_history to the newest 3 with `runs_total`;
/// `full_history=true` returns everything stored.
#[test]
fn list_caps_run_history_with_optin_2037() {
    let home = tmp_home("2037-history-cap");
    let resp = create(
        &home,
        "lead",
        &serde_json::json!({"cron": "0 0 * * *", "message": "m", "instance": "dev"}),
    );
    let id = resp["id"].as_str().expect("created").to_string();
    // Seed 6 runs directly through the store (the list cap is the unit
    // under test, not the runner).
    crate::store::mutate_versioned(&store_path(&home), |store: &mut ScheduleStore| {
        let sched = store
            .schedules
            .iter_mut()
            .find(|s| s.id == id)
            .expect("schedule");
        for i in 0..6 {
            sched.run_history.push(ScheduleRun {
                triggered_at: format!("2026-06-11T00:0{i}:00Z"),
                status: "ok".to_string(),
            });
        }
        Ok(())
    })
    .expect("seed runs");
    let trimmed = list(&home, &serde_json::json!({}));
    let row = &trimmed["schedules"][0];
    assert_eq!(row["runs_total"].as_u64(), Some(6), "{row}");
    assert_eq!(
        row["run_history"].as_array().expect("array").len(),
        3,
        "default trims to newest 3: {row}"
    );
    assert_eq!(
        row["run_history"][2]["triggered_at"].as_str(),
        Some("2026-06-11T00:05:00Z"),
        "kept entries are the NEWEST tail"
    );
    let full = list(&home, &serde_json::json!({"full_history": true}));
    assert_eq!(
        full["schedules"][0]["run_history"]
            .as_array()
            .expect("array")
            .len(),
        6,
        "full_history opt-in returns all stored runs"
    );
    std::fs::remove_dir_all(&home).ok();
}
