use super::*;
use crate::task_events::{
    ConfidenceScore, LinkSource, PrSnapshot, TaskEvent, TaskEventEnvelope, SCHEMA_VERSION,
};

fn tmp_home(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let home =
        std::env::temp_dir().join(format!("agend-catalog-{}-{tag}-{id}", std::process::id()));
    std::fs::create_dir_all(&home).expect("create temp home");
    home
}

fn envelope(seq: u64, event: TaskEvent) -> TaskEventEnvelope {
    envelope_from("writer", seq, event)
}

fn envelope_from(instance: &str, seq: u64, event: TaskEvent) -> TaskEventEnvelope {
    TaskEventEnvelope {
        schema_version: SCHEMA_VERSION,
        seq,
        timestamp: format!("2026-08-24T00:00:{:02}Z", seq % 60),
        instance: InstanceName::from(instance),
        emitter_id: None,
        event,
    }
}

fn envelope_at(timestamp: &str, seq: u64, event: TaskEvent) -> TaskEventEnvelope {
    let mut env = envelope(seq, event);
    env.timestamp = timestamp.to_string();
    env
}

fn created(task_id: &TaskId, title: &str) -> TaskEvent {
    TaskEvent::Created {
        task_id: task_id.clone(),
        title: title.into(),
        description: String::new(),
        priority: "normal".into(),
        owner: None,
        due_at: None,
        depends_on: Vec::new(),
        routed_to: None,
        branch: None,
        bind: None,
        eta_secs: None,
        tags: Vec::new(),
        parent_id: None,
    }
}

fn write_envelopes(path: &Path, envelopes: &[TaskEventEnvelope]) {
    let bytes = envelopes
        .iter()
        .map(|env| {
            format!(
                "{}\n",
                serde_json::to_string(env).expect("serialize envelope")
            )
        })
        .collect::<String>();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create event directory");
    }
    std::fs::write(path, bytes).expect("write envelopes");
}

fn rebuild_count() -> u64 {
    BOARD_REBUILDS.with(std::cell::Cell::get)
}

fn reset_archive_bytes_read() {
    ARCHIVE_BYTES_READ.with(|count| count.set(0));
}

fn archive_bytes_read() -> u64 {
    ARCHIVE_BYTES_READ.with(std::cell::Cell::get)
}

fn replay_with_metadata_events(count: u64) -> TaskBoardState {
    let task_id = TaskId::from("t-20260824000000000000-1-1");
    let mut state = TaskBoardState::default();
    assert!(state.apply(&envelope(
        1,
        TaskEvent::Created {
            task_id: task_id.clone(),
            title: "bounded projection".into(),
            description: "parity fixture".into(),
            priority: "high".into(),
            owner: Some(InstanceName::from("owner")),
            due_at: Some("2026-08-25T00:00:00Z".into()),
            depends_on: vec![TaskId::from("t-20260823000000000000-1-1")],
            routed_to: Some(InstanceName::from("lead")),
            branch: Some("fix/catalog".into()),
            bind: Some(true),
            eta_secs: Some(60),
            tags: vec!["catalog".into()],
            parent_id: None,
        },
    )));
    for seq in 2..=count + 1 {
        assert!(state.apply(&envelope(
            seq,
            TaskEvent::MetadataSet {
                task_id: task_id.clone(),
                by: InstanceName::from("writer"),
                key: "stable".into(),
                value: serde_json::json!(true),
            },
        )));
    }
    state
}

#[test]
fn projection_preserves_current_state_and_replay_high_water() {
    let task_id = TaskId::from("t-20260824000000000000-1-1");
    let replay = replay_with_metadata_events(20);
    let mut incumbent = serde_json::to_value(replay.tasks.get(&task_id).expect("replayed task"))
        .expect("serialize replayed task");
    incumbent
        .as_object_mut()
        .expect("task object")
        .remove("history");

    let projection = BoardProjection::from_replay(replay);
    let task = projection.task(&task_id).expect("projected task");
    let mut projected = serde_json::to_value(task).expect("serialize projected task");
    let projected_object = projected.as_object_mut().expect("projected task object");
    projected_object.remove("history_len");
    projected_object.remove("last_folded_event");
    projected_object.remove("recent_history");

    assert_eq!(projected, incumbent, "every current-state field must match");
    assert_eq!(task.history_len, 21);
    assert_eq!(task.recent_history.len(), RECENT_HISTORY_LIMIT);
    assert_eq!(task.recent_history.front().map(|entry| entry.seq), Some(6));
    assert_eq!(
        task.last_folded_event,
        Some((InstanceName::from("writer"), 21))
    );
    assert_eq!(
        projection.last_seq_for(&InstanceName::from("writer")),
        Some(21)
    );
    assert_eq!(projection.events_folded(), 21);
    assert_eq!(projection.tasks().count(), 1);
}

#[test]
fn shadow_equivalence_detects_current_state_and_high_water_mismatches() {
    let task_id = TaskId::from("t-20260824000000000000-1-1");
    let replay = replay_with_metadata_events(20);
    let projection = BoardProjection::from_replay(replay.clone());

    assert!(projection.matches_replay(&replay));

    macro_rules! field_mismatch {
        ($field:ident, $value:expr) => {{
            let mut changed = replay.clone();
            changed
                .tasks
                .get_mut(&task_id)
                .expect("replayed task")
                .$field = $value;
            assert!(
                !projection.matches_replay(&changed),
                "{} mismatch must be detected",
                stringify!($field)
            );
        }};
    }
    field_mismatch!(id, TaskId::from("different-id"));
    field_mismatch!(title, "different title".into());
    field_mismatch!(description, "different description".into());
    field_mismatch!(priority, "low".into());
    field_mismatch!(status, TaskStatus::Claimed);
    field_mismatch!(owner, None);
    field_mismatch!(linked_prs, vec![PrId(1)]);
    field_mismatch!(block_reason, Some("blocked".into()));
    field_mismatch!(created_by, InstanceName::from("different"));
    field_mismatch!(created_at, "different".into());
    field_mismatch!(updated_at, "different".into());
    field_mismatch!(due_at, None);
    field_mismatch!(depends_on, Vec::new());
    field_mismatch!(routed_to, None);
    field_mismatch!(result, Some("different".into()));
    field_mismatch!(superseded_by, Some(TaskId::from("successor")));
    field_mismatch!(branch, None);
    field_mismatch!(bind, Some(false));
    field_mismatch!(started_at, Some("different".into()));
    field_mismatch!(eta_secs, None);
    field_mismatch!(tags, Vec::new());
    field_mismatch!(parent_id, Some(TaskId::from("parent")));
    field_mismatch!(metadata, BTreeMap::new());

    let mut changed_history_len = replay.clone();
    let history = &mut changed_history_len
        .tasks
        .get_mut(&task_id)
        .expect("replayed task")
        .history;
    history.insert(0, history[0].clone());
    assert!(!projection.matches_replay(&changed_history_len));

    let mut changed_last_folded = projection.clone();
    Arc::make_mut(
        changed_last_folded
            .tasks
            .get_mut(&task_id)
            .expect("projected task"),
    )
    .last_folded_event = None;
    assert!(!changed_last_folded.matches_replay(&replay));

    let mut changed_recent = projection.clone();
    Arc::make_mut(
        changed_recent
            .tasks
            .get_mut(&task_id)
            .expect("projected task"),
    )
    .recent_history
    .back_mut()
    .expect("recent history")
    .kind = "different";
    assert!(!changed_recent.matches_replay(&replay));

    let mut changed_task_set = replay.clone();
    let extra_id = TaskId::from("extra-task");
    let mut extra = changed_task_set.tasks[&task_id].clone();
    extra.id = extra_id.clone();
    changed_task_set.tasks.insert(extra_id, extra);
    assert!(!projection.matches_replay(&changed_task_set));

    let mut changed_high_water = replay.clone();
    changed_high_water
        .last_seq_per_instance
        .insert(InstanceName::from("writer"), 999);
    assert!(!projection.matches_replay(&changed_high_water));

    let mut changed_event_count = replay;
    changed_event_count.events_folded += 1;
    assert!(!projection.matches_replay(&changed_event_count));
}

#[test]
fn newer_projection_is_not_compared_to_an_older_replay() {
    let replay = replay_with_metadata_events(20);
    let mut projection = BoardProjection::from_replay(replay.clone());
    projection.events_folded += 1;

    assert_eq!(projection.matches_current_replay(&replay), None);
    assert_eq!(
        BoardProjection::from_replay(replay.clone()).matches_current_replay(&replay),
        Some(true)
    );
}

#[test]
fn initial_catalog_build_waits_for_the_board_writer() {
    let home = tmp_home("initial-build-lock");
    let task_id = TaskId::from("t-20260825000000000000-1-9");
    super::super::append_at(
        &home,
        &InstanceName::from("writer"),
        created(&task_id, "seed"),
    )
    .expect("seed board");
    let lock_path = super::super::log_path(&home).with_extension("jsonl.lock");
    let writer_lock = crate::store::acquire_file_lock(&lock_path).expect("hold writer lock");
    let (tx, rx) = std::sync::mpsc::channel();
    let build_home = home.clone();
    let builder = std::thread::spawn(move || {
        tx.send(build_catalog(&build_home).snapshot_advisory().0)
            .expect("send phase");
    });

    assert!(
        rx.recv_timeout(std::time::Duration::from_millis(50))
            .is_err(),
        "initial build must not read through an active writer"
    );
    drop(writer_lock);
    assert_eq!(
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("build after writer releases"),
        Phase::Ready
    );
    builder.join().expect("join builder");
}

#[test]
fn projection_size_is_bounded_by_tasks_not_events() {
    let mut one_x = BoardProjection::from_replay(replay_with_metadata_events(100));
    let mut ten_x = BoardProjection::from_replay(replay_with_metadata_events(1_000));
    one_x.set_cursor(BoardCursor::from_folded_hot_log(1, 1, 1));
    ten_x.set_cursor(BoardCursor::from_folded_hot_log(1, 1, 1));
    let one_x_bytes = serde_json::to_vec(&one_x).expect("serialize 1x projection");
    let ten_x_bytes = serde_json::to_vec(&ten_x).expect("serialize 10x projection");

    assert_eq!(one_x.tasks().count(), ten_x.tasks().count());
    assert_eq!(
        one_x.tasks().next().expect("1x task").recent_history.len(),
        RECENT_HISTORY_LIMIT
    );
    assert_eq!(
        ten_x.tasks().next().expect("10x task").recent_history.len(),
        RECENT_HISTORY_LIMIT
    );
    assert!(
        ten_x_bytes.len() <= one_x_bytes.len() + 32,
        "10x events grew bounded projection from {} to {} bytes",
        one_x_bytes.len(),
        ten_x_bytes.len()
    );

    let checkpoint_bytes = |projection: &BoardProjection| {
        serde_json::to_vec(&CheckpointV1 {
            schema: CHECKPOINT_SCHEMA,
            board: super::super::DEFAULT_PROJECT.to_string(),
            cursor: *projection.cursor().expect("cursor"),
            tasks: projection
                .task_snapshots()
                .into_iter()
                .map(|task| task.as_ref().clone())
                .collect(),
            last_seq_per_instance: projection.last_seq_per_instance.clone(),
            events_folded: projection.events_folded,
            last_order_key: projection.last_order_key.clone(),
            written_at: "2026-08-25T00:00:00Z".into(),
        })
        .expect("serialize checkpoint")
    };
    let one_x_checkpoint = checkpoint_bytes(&one_x);
    let ten_x_checkpoint = checkpoint_bytes(&ten_x);
    let allowed_growth = one_x_checkpoint.len().div_ceil(20);
    assert!(
        ten_x_checkpoint.len() <= one_x_checkpoint.len() + allowed_growth,
        "10x events grew checkpoint by more than 5%: {} -> {} bytes",
        one_x_checkpoint.len(),
        ten_x_checkpoint.len()
    );
}

#[test]
fn task_snapshots_replace_only_the_changed_record() {
    let changed_id = TaskId::from("t-20260824000000000000-1-1");
    let untouched_id = TaskId::from("t-20260824000000000000-1-2");
    let create = |task_id: TaskId, title: &str| TaskEvent::Created {
        task_id,
        title: title.into(),
        description: "snapshot fixture".into(),
        priority: "normal".into(),
        owner: None,
        due_at: None,
        depends_on: Vec::new(),
        routed_to: None,
        branch: None,
        bind: None,
        eta_secs: None,
        tags: Vec::new(),
        parent_id: None,
    };
    let mut projection = BoardProjection::from_sorted_envelopes(&[
        envelope(1, create(changed_id.clone(), "changed")),
        envelope(2, create(untouched_id.clone(), "untouched")),
    ])
    .expect("initial projection");
    let changed_before = projection
        .task_snapshot(&changed_id)
        .expect("changed snapshot");
    let untouched_before = projection
        .task_snapshot(&untouched_id)
        .expect("untouched snapshot");
    let all_before = projection.task_snapshots();
    assert_eq!(
        all_before
            .iter()
            .map(|task| task.id.clone())
            .collect::<Vec<_>>(),
        vec![changed_id.clone(), untouched_id.clone()]
    );
    assert!(Arc::ptr_eq(&all_before[0], &changed_before));
    assert!(Arc::ptr_eq(&all_before[1], &untouched_before));

    projection
        .apply_ordered(&envelope(
            3,
            TaskEvent::MetadataSet {
                task_id: changed_id.clone(),
                by: InstanceName::from("writer"),
                key: "updated".into(),
                value: serde_json::json!(true),
            },
        ))
        .expect("ordered update");

    let changed_after = projection
        .task_snapshot(&changed_id)
        .expect("changed snapshot");
    let untouched_after = projection
        .task_snapshot(&untouched_id)
        .expect("untouched snapshot");
    let all_after = projection.task_snapshots();
    assert!(!std::sync::Arc::ptr_eq(&changed_before, &changed_after));
    assert!(std::sync::Arc::ptr_eq(&untouched_before, &untouched_after));
    assert!(!Arc::ptr_eq(&all_before[0], &all_after[0]));
    assert!(Arc::ptr_eq(&all_before[1], &all_after[1]));
    assert!(changed_before.metadata.is_empty());
    assert!(all_before[0].metadata.is_empty());
    assert_eq!(
        changed_after.metadata.get("updated"),
        Some(&serde_json::json!(true))
    );
}

#[test]
fn advisory_catalog_snapshot_is_phase_labelled_and_pointer_only() {
    let first_id = TaskId::from("t-20260824000000000000-1-1");
    let second_id = TaskId::from("t-20260824000000000000-1-2");
    let create = |task_id: TaskId| TaskEvent::Created {
        task_id,
        title: "advisory fixture".into(),
        description: String::new(),
        priority: "normal".into(),
        owner: None,
        due_at: None,
        depends_on: Vec::new(),
        routed_to: None,
        branch: None,
        bind: None,
        eta_secs: None,
        tags: Vec::new(),
        parent_id: None,
    };
    let first = BoardProjection::from_sorted_envelopes(&[envelope(1, create(first_id.clone()))])
        .expect("first board");
    let second = BoardProjection::from_sorted_envelopes(&[envelope(1, create(second_id.clone()))])
        .expect("second board");
    let first_snapshot = first.task_snapshot(&first_id).expect("first snapshot");
    let second_snapshot = second.task_snapshot(&second_id).expect("second snapshot");
    let catalog = StrictTaskCatalog::new(
        Phase::Building,
        BTreeMap::from([("a-board".into(), first), ("b-board".into(), second)]),
    );

    let (phase, snapshots) = catalog.snapshot_advisory();

    assert_eq!(phase, Phase::Building);
    assert_eq!(
        snapshots
            .iter()
            .map(|task| task.id.clone())
            .collect::<Vec<_>>(),
        vec![first_id, second_id]
    );
    assert!(Arc::ptr_eq(&snapshots[0], &first_snapshot));
    assert!(Arc::ptr_eq(&snapshots[1], &second_snapshot));
}

#[test]
fn catalog_route_is_ready_only_and_rejects_duplicate_ids() {
    let unique_id = TaskId::from("t-20260824000000000000-1-1");
    let duplicate_id = TaskId::from("t-20260824000000000000-1-2");
    let create = |task_id: TaskId| TaskEvent::Created {
        task_id,
        title: "route fixture".into(),
        description: String::new(),
        priority: "normal".into(),
        owner: None,
        due_at: None,
        depends_on: Vec::new(),
        routed_to: None,
        branch: None,
        bind: None,
        eta_secs: None,
        tags: Vec::new(),
        parent_id: None,
    };
    let first = BoardProjection::from_sorted_envelopes(&[
        envelope(1, create(unique_id.clone())),
        envelope(2, create(duplicate_id.clone())),
    ])
    .expect("first board");
    let unique_snapshot = first.task_snapshot(&unique_id).expect("unique snapshot");
    let second =
        BoardProjection::from_sorted_envelopes(&[envelope(1, create(duplicate_id.clone()))])
            .expect("second board");
    let boards = BTreeMap::from([
        ("a-board".into(), first.clone()),
        ("b-board".into(), second.clone()),
    ]);

    for phase in [
        Phase::Building,
        Phase::Unhealthy {
            since: "2026-08-24T00:00:00Z".into(),
            causes: vec!["fixture".into()],
        },
    ] {
        let catalog = StrictTaskCatalog::new(phase, boards.clone());
        assert!(matches!(
            catalog.route(&unique_id),
            Err(CatalogRouteError::Unreadable)
        ));
    }

    let catalog = StrictTaskCatalog::new(Phase::Ready, boards);
    let (board, task) = catalog.route(&unique_id).expect("unique route");
    assert_eq!(board, "a-board");
    assert!(Arc::ptr_eq(&task, &unique_snapshot));
    assert_eq!(
        catalog.route(&duplicate_id),
        Err(CatalogRouteError::Ambiguous {
            boards: vec!["a-board".into(), "b-board".into()],
        })
    );
    assert_eq!(
        catalog.route(&TaskId::from("missing")),
        Err(CatalogRouteError::NotFound)
    );
}

#[test]
fn catalog_list_reads_are_ready_only_ordered_and_pointer_only() {
    let first_id = TaskId::from("t-20260824000000000000-1-1");
    let second_id = TaskId::from("t-20260824000000000000-1-2");
    let create = |task_id: TaskId| TaskEvent::Created {
        task_id,
        title: "list fixture".into(),
        description: String::new(),
        priority: "normal".into(),
        owner: None,
        due_at: None,
        depends_on: Vec::new(),
        routed_to: None,
        branch: None,
        bind: None,
        eta_secs: None,
        tags: Vec::new(),
        parent_id: None,
    };
    let first = BoardProjection::from_sorted_envelopes(&[envelope(1, create(first_id.clone()))])
        .expect("first board");
    let second = BoardProjection::from_sorted_envelopes(&[envelope(1, create(second_id.clone()))])
        .expect("second board");
    let first_snapshot = first.task_snapshot(&first_id).expect("first snapshot");
    let second_snapshot = second.task_snapshot(&second_id).expect("second snapshot");
    let boards = BTreeMap::from([("a-board".into(), first), ("b-board".into(), second)]);

    let building = StrictTaskCatalog::new(Phase::Building, boards.clone());
    assert_eq!(building.all_tasks(), Err(CatalogRouteError::Unreadable));
    assert_eq!(
        building.board("a-board"),
        Err(CatalogRouteError::Unreadable)
    );

    let ready = StrictTaskCatalog::new(Phase::Ready, boards);
    let all = ready.all_tasks().expect("ready all-tasks snapshot");
    assert_eq!(
        all.iter().map(|task| task.id.clone()).collect::<Vec<_>>(),
        vec![first_id, second_id]
    );
    assert!(Arc::ptr_eq(&all[0], &first_snapshot));
    assert!(Arc::ptr_eq(&all[1], &second_snapshot));

    let board = ready.board("b-board").expect("ready board snapshot");
    assert_eq!(board.len(), 1);
    assert!(Arc::ptr_eq(&board[0], &second_snapshot));
    assert_eq!(ready.board("missing"), Err(CatalogRouteError::NotFound));
}

#[test]
fn catalog_statuses_are_ready_only_ordered_and_fail_closed() {
    let unique_id = TaskId::from("t-20260824000000000000-1-1");
    let duplicate_id = TaskId::from("t-20260824000000000000-1-2");
    let missing_id = TaskId::from("missing");
    let create = |task_id: TaskId| TaskEvent::Created {
        task_id,
        title: "status fixture".into(),
        description: String::new(),
        priority: "normal".into(),
        owner: None,
        due_at: None,
        depends_on: Vec::new(),
        routed_to: None,
        branch: None,
        bind: None,
        eta_secs: None,
        tags: Vec::new(),
        parent_id: None,
    };
    let first = BoardProjection::from_sorted_envelopes(&[
        envelope(1, create(unique_id.clone())),
        envelope(
            2,
            TaskEvent::Claimed {
                task_id: unique_id.clone(),
                by: InstanceName::from("owner"),
            },
        ),
        envelope(3, create(duplicate_id.clone())),
    ])
    .expect("first board");
    let second =
        BoardProjection::from_sorted_envelopes(&[envelope(1, create(duplicate_id.clone()))])
            .expect("second board");
    let boards = BTreeMap::from([("a-board".into(), first), ("b-board".into(), second)]);

    let requested = vec![unique_id.clone(), missing_id.clone(), unique_id.clone()];
    let building = StrictTaskCatalog::new(Phase::Building, boards.clone());
    assert_eq!(
        building.statuses(&requested),
        Err(CatalogRouteError::Unreadable)
    );

    let ready = StrictTaskCatalog::new(Phase::Ready, boards);
    assert_eq!(
        ready.statuses(&requested),
        Ok(vec![
            (unique_id, Some(TaskStatus::Claimed)),
            (missing_id, None),
            (
                TaskId::from("t-20260824000000000000-1-1"),
                Some(TaskStatus::Claimed),
            ),
        ])
    );
    assert_eq!(
        ready.statuses(std::slice::from_ref(&duplicate_id)),
        Err(CatalogRouteError::Ambiguous {
            boards: vec!["a-board".into(), "b-board".into()],
        })
    );
    assert_eq!(
        ready.statuses(&[TaskId::from("t-20260824000000000000-1-1"), duplicate_id,]),
        Err(CatalogRouteError::Ambiguous {
            boards: vec!["a-board".into(), "b-board".into()],
        })
    );
}

#[test]
fn hot_log_freshness_shapes_are_fail_closed() {
    let cursor = HotLogStamp {
        inode: identity_from_u64(7),
        len: 10,
        mtime_ns: 100,
    };

    assert_eq!(classify_hot_log(cursor, cursor), HotLogFreshness::Current);
    assert_eq!(
        classify_hot_log(
            cursor,
            HotLogStamp {
                inode: identity_from_u64(7),
                len: 14,
                mtime_ns: 101,
            },
        ),
        HotLogFreshness::CatchUp { start: 10, end: 14 }
    );
    assert_eq!(
        classify_hot_log(
            cursor,
            HotLogStamp {
                inode: identity_from_u64(7),
                len: 14,
                mtime_ns: 100,
            },
        ),
        HotLogFreshness::CatchUp { start: 10, end: 14 }
    );
    for stale in [
        HotLogStamp {
            inode: identity_from_u64(8),
            len: 10,
            mtime_ns: 100,
        },
        HotLogStamp {
            inode: identity_from_u64(8),
            len: 14,
            mtime_ns: 101,
        },
        HotLogStamp {
            inode: identity_from_u64(7),
            len: 9,
            mtime_ns: 101,
        },
        HotLogStamp {
            inode: identity_from_u64(7),
            len: 10,
            mtime_ns: 101,
        },
    ] {
        assert_eq!(classify_hot_log(cursor, stale), HotLogFreshness::Stale);
    }
}

#[test]
fn board_cursor_records_the_folded_hot_log_position() {
    let cursor = BoardCursor::from_folded_hot_log(7, 10, 100);

    assert_eq!(cursor.live_offset(), 10);
    assert_eq!(
        cursor.classify_observed(identity_from_u64(7), 10, 100),
        HotLogFreshness::Current
    );
    assert_eq!(
        cursor.classify_observed(identity_from_u64(7), 14, 101),
        HotLogFreshness::CatchUp { start: 10, end: 14 }
    );
}

#[test]
fn board_projection_keeps_its_optional_source_cursor() {
    let mut projection = BoardProjection::default();
    assert!(projection.cursor().is_none());

    projection.set_cursor(BoardCursor::from_folded_hot_log(7, 10, 100));

    assert_eq!(projection.cursor().map(BoardCursor::live_offset), Some(10));
}

#[test]
fn board_set_freshness_compares_names_and_fails_closed_on_missing() {
    let known = std::collections::BTreeSet::from(["default".to_string(), "research".to_string()]);

    assert_eq!(
        classify_board_set(&known, &known),
        BoardSetFreshness::Current
    );
    assert_eq!(
        classify_board_set(
            &known,
            &std::collections::BTreeSet::from([
                "default".to_string(),
                "research".to_string(),
                "support".to_string(),
            ]),
        ),
        BoardSetFreshness::New {
            names: vec!["support".to_string()],
        }
    );
    assert_eq!(
        classify_board_set(
            &known,
            &std::collections::BTreeSet::from(["default".to_string(), "support".to_string(),]),
        ),
        BoardSetFreshness::Missing {
            names: vec!["research".to_string()],
        }
    );
    assert_eq!(
        classify_board_set(
            &std::collections::BTreeSet::from([
                "a-second-one".to_string(),
                "default".to_string(),
                "research".to_string(),
            ]),
            &std::collections::BTreeSet::from(["default".to_string()]),
        ),
        BoardSetFreshness::Missing {
            names: vec!["a-second-one".to_string(), "research".to_string()],
        }
    );
}

#[test]
fn board_set_observation_updates_phase_without_discovering_or_folding() {
    let boards = BTreeMap::from([
        ("default".to_string(), BoardProjection::default()),
        ("research".to_string(), BoardProjection::default()),
    ]);

    let current = StrictTaskCatalog::new(Phase::Ready, boards.clone());
    assert_eq!(
        current.observe_board_set(
            &BTreeSet::from(["default".to_string(), "research".to_string()]),
            "2026-08-24T15:40:00Z",
        ),
        Ok(())
    );
    assert_eq!(current.snapshot_advisory().0, Phase::Ready);

    let building = StrictTaskCatalog::new(Phase::Building, boards.clone());
    assert_eq!(
        building.observe_board_set(
            &BTreeSet::from(["default".to_string(), "research".to_string()]),
            "2026-08-24T15:40:00Z",
        ),
        Err(CatalogRouteError::Unreadable)
    );
    assert_eq!(building.snapshot_advisory().0, Phase::Building);

    let added = StrictTaskCatalog::new(Phase::Ready, boards.clone());
    assert_eq!(
        added.observe_board_set(
            &BTreeSet::from([
                "default".to_string(),
                "research".to_string(),
                "support".to_string(),
            ]),
            "2026-08-24T15:40:00Z",
        ),
        Err(CatalogRouteError::Unreadable)
    );
    assert_eq!(added.snapshot_advisory().0, Phase::Building);

    let missing = StrictTaskCatalog::new(Phase::Ready, boards);
    assert_eq!(
        missing.observe_board_set(
            &BTreeSet::from(["support".to_string()]),
            "2026-08-24T15:40:00Z",
        ),
        Err(CatalogRouteError::Unreadable)
    );
    assert_eq!(
        missing.snapshot_advisory().0,
        Phase::Unhealthy {
            since: "2026-08-24T15:40:00Z".to_string(),
            causes: vec!["missing boards: default, research".to_string()],
        }
    );
}

#[test]
fn incremental_apply_matches_incumbent_replay() {
    let task_id = TaskId::from("t-20260824000000000000-1-1");
    let child_id = TaskId::from("t-20260824000000000000-1-2");
    let actor = InstanceName::from("owner");
    let snapshot = PrSnapshot {
        pr_state: "merged".into(),
        merge_sha: Some("abc123".into()),
        api_response_hash: "hash".into(),
        captured_at: "2026-08-24T00:00:00Z".into(),
    };
    let events = vec![
        TaskEvent::Created {
            task_id: task_id.clone(),
            title: "parent".into(),
            description: "original".into(),
            priority: "normal".into(),
            owner: None,
            due_at: None,
            depends_on: Vec::new(),
            routed_to: Some(InstanceName::from("lead")),
            branch: None,
            bind: Some(true),
            eta_secs: Some(60),
            tags: Vec::new(),
            parent_id: None,
        },
        TaskEvent::Created {
            task_id: child_id.clone(),
            title: "child".into(),
            description: "cascade fixture".into(),
            priority: "normal".into(),
            owner: None,
            due_at: None,
            depends_on: Vec::new(),
            routed_to: None,
            branch: None,
            bind: None,
            eta_secs: None,
            tags: Vec::new(),
            parent_id: Some(task_id.clone()),
        },
        TaskEvent::Claimed {
            task_id: task_id.clone(),
            by: actor.clone(),
        },
        TaskEvent::InProgress {
            task_id: task_id.clone(),
            by: actor.clone(),
        },
        TaskEvent::Blocked {
            task_id: task_id.clone(),
            reason: "waiting".into(),
        },
        TaskEvent::Unblocked {
            task_id: task_id.clone(),
        },
        TaskEvent::MovedToBacklog {
            task_id: task_id.clone(),
        },
        TaskEvent::Reopened {
            task_id: task_id.clone(),
            reason: "retry".into(),
            source_evidence: "test".into(),
        },
        TaskEvent::OwnerAssigned {
            task_id: task_id.clone(),
            by: actor.clone(),
            owner: Some(actor.clone()),
            routed_to: Some(InstanceName::from("lead")),
        },
        TaskEvent::PriorityChanged {
            task_id: task_id.clone(),
            by: actor.clone(),
            priority: "high".into(),
        },
        TaskEvent::DescriptionUpdated {
            task_id: task_id.clone(),
            by: actor.clone(),
            description: "updated".into(),
        },
        TaskEvent::TagsSet {
            task_id: task_id.clone(),
            tags: vec!["catalog".into()],
        },
        TaskEvent::ResultSet {
            task_id: task_id.clone(),
            by: actor.clone(),
            result: "explicit".into(),
        },
        TaskEvent::MetadataSet {
            task_id: task_id.clone(),
            by: actor.clone(),
            key: "key".into(),
            value: serde_json::json!("value"),
        },
        TaskEvent::BranchLinked {
            task_id: task_id.clone(),
            by: actor.clone(),
            branch: "fix/catalog".into(),
        },
        TaskEvent::Linked {
            task_id: task_id.clone(),
            pr_id: PrId(3347),
            source: LinkSource::Explicit {
                authored_at: "2026-08-24T00:00:00Z".into(),
            },
            snapshot: snapshot.clone(),
        },
        TaskEvent::MovedToReview {
            task_id: task_id.clone(),
        },
        TaskEvent::Unblocked {
            task_id: task_id.clone(),
        },
        TaskEvent::Verified {
            task_id: task_id.clone(),
            by_reviewer: InstanceName::from("reviewer"),
            verdict: "VERIFIED".into(),
        },
        TaskEvent::TaskCloseProposed {
            task_id: task_id.clone(),
            candidate: DoneSource::LegacyBackfill {
                sweep_id: "sweep".into(),
                reasoning: "fixture".into(),
                snapshot: Some(snapshot),
            },
            sweep_id: "sweep".into(),
            confidence: ConfidenceScore {
                total: 1.0,
                signal_count: 1,
                sub_scores: BTreeMap::new(),
            },
        },
        TaskEvent::Done {
            task_id: task_id.clone(),
            by: actor.clone(),
            source: DoneSource::ReportAutoClose {
                report_summary: "does not replace explicit".into(),
                closed_at: "2026-08-24T00:00:00Z".into(),
            },
        },
        TaskEvent::Released {
            task_id: task_id.clone(),
            reason: "release".into(),
        },
        TaskEvent::Superseded {
            task_id: task_id.clone(),
            by: actor.clone(),
            successor_id: TaskId::from("t-20260824000000000000-1-3"),
        },
        TaskEvent::Claimed {
            task_id: child_id.clone(),
            by: actor.clone(),
        },
        TaskEvent::Cancelled {
            task_id: task_id.clone(),
            by: actor,
            reason: "cancel tree".into(),
        },
    ];

    let mut incumbent = TaskBoardState::default();
    let mut projection = BoardProjection::default();
    for (index, event) in events.into_iter().enumerate() {
        let env = envelope(index as u64 + 1, event);
        assert!(incumbent.apply(&env));
        assert!(projection.apply(&env));
        assert_eq!(
            projection,
            BoardProjection::from_replay(incumbent.clone()),
            "projection diverged at event {} ({})",
            env.seq,
            env.event.kind_str()
        );
    }

    assert_eq!(
        projection.task(&child_id).expect("child").status,
        TaskStatus::Cancelled,
        "parent cancellation must cascade exactly like incumbent replay"
    );
}

#[test]
fn incremental_apply_dedupes_per_instance_and_bounds_history() {
    let task_id = TaskId::from("t-20260824000000000000-1-1");
    let created = envelope(
        1,
        TaskEvent::Created {
            task_id: task_id.clone(),
            title: "bounded".into(),
            description: "incremental".into(),
            priority: "normal".into(),
            owner: None,
            due_at: None,
            depends_on: Vec::new(),
            routed_to: None,
            branch: None,
            bind: None,
            eta_secs: None,
            tags: Vec::new(),
            parent_id: None,
        },
    );
    let mut projection = BoardProjection::default();
    assert!(projection.apply(&created));
    assert!(!projection.apply(&created));

    for seq in 2..=25 {
        assert!(projection.apply(&envelope(
            seq,
            TaskEvent::MetadataSet {
                task_id: task_id.clone(),
                by: InstanceName::from("writer"),
                key: "seq".into(),
                value: serde_json::json!(seq),
            },
        )));
    }
    let other = envelope_from(
        "other-writer",
        2,
        TaskEvent::TagsSet {
            task_id: task_id.clone(),
            tags: vec!["other".into()],
        },
    );
    assert!(projection.apply(&other));
    assert!(!projection.apply(&envelope_from(
        "other-writer",
        1,
        TaskEvent::TagsSet {
            task_id: task_id.clone(),
            tags: vec!["stale".into()],
        },
    )));

    let task = projection.task(&task_id).expect("task");
    assert_eq!(task.history_len, 26);
    assert_eq!(task.recent_history.len(), RECENT_HISTORY_LIMIT);
    assert_eq!(task.recent_history.front().map(|entry| entry.seq), Some(11));
    assert_eq!(task.recent_history.back().map(|entry| entry.seq), Some(2));
    assert_eq!(projection.events_folded(), 26);
    assert_eq!(
        projection.last_seq_for(&InstanceName::from("writer")),
        Some(25)
    );
    assert_eq!(
        projection.last_seq_for(&InstanceName::from("other-writer")),
        Some(2)
    );
}

#[test]
fn ordered_apply_rejects_non_advancing_keys_without_mutation() {
    let task_id = TaskId::from("t-20260824000000000000-1-1");
    let created = envelope_at(
        "2026-08-24T00:00:02Z",
        1,
        TaskEvent::Created {
            task_id: task_id.clone(),
            title: "ordered".into(),
            description: "tail gate".into(),
            priority: "normal".into(),
            owner: None,
            due_at: None,
            depends_on: Vec::new(),
            routed_to: None,
            branch: None,
            bind: None,
            eta_secs: None,
            tags: Vec::new(),
            parent_id: None,
        },
    );
    let stale = envelope_at(
        "2026-08-24T00:00:01Z",
        2,
        TaskEvent::TagsSet {
            task_id: task_id.clone(),
            tags: vec!["stale".into()],
        },
    );
    let invalid = envelope_at(
        "not-a-timestamp",
        3,
        TaskEvent::TagsSet {
            task_id: task_id.clone(),
            tags: vec!["invalid".into()],
        },
    );
    let mut later_lower_instance = envelope_from(
        "aaa",
        2,
        TaskEvent::TagsSet {
            task_id: task_id.clone(),
            tags: vec!["later".into()],
        },
    );
    later_lower_instance.timestamp = "2026-08-24T00:00:03Z".into();
    let mut lower_instance = envelope_from(
        "AAA",
        99,
        TaskEvent::TagsSet {
            task_id: task_id.clone(),
            tags: vec!["wrong-instance".into()],
        },
    );
    lower_instance.timestamp = later_lower_instance.timestamp.clone();
    let mut lower_seq = envelope_from(
        "aaa",
        1,
        TaskEvent::TagsSet {
            task_id,
            tags: vec!["wrong-seq".into()],
        },
    );
    lower_seq.timestamp = later_lower_instance.timestamp.clone();

    let mut projection = BoardProjection::default();
    assert_eq!(projection.apply_ordered(&created), Ok(true));
    let accepted = projection.clone();

    assert!(matches!(
        projection.apply_ordered(&stale),
        Err(OrderedApplyError::OutOfOrder { .. })
    ));
    assert_eq!(projection, accepted);
    assert!(matches!(
        projection.apply_ordered(&created),
        Err(OrderedApplyError::OutOfOrder { .. })
    ));
    assert_eq!(projection, accepted);
    assert_eq!(
        projection.apply_ordered(&invalid),
        Err(OrderedApplyError::InvalidTimestamp(
            "not-a-timestamp".into()
        ))
    );
    assert_eq!(projection, accepted);

    assert_eq!(projection.apply_ordered(&later_lower_instance), Ok(true));
    let advanced = projection.clone();
    assert!(matches!(
        projection.apply_ordered(&lower_instance),
        Err(OrderedApplyError::OutOfOrder { .. })
    ));
    assert_eq!(projection, advanced);
    assert!(matches!(
        projection.apply_ordered(&lower_seq),
        Err(OrderedApplyError::OutOfOrder { .. })
    ));
    assert_eq!(projection, advanced);
}

#[test]
fn canonical_initial_fold_matches_replay_and_seeds_order_cursor() {
    let task_id = TaskId::from("t-20260824000000000000-1-1");
    let tagged = |instance: &str, timestamp: &str, seq: u64, tag: &str| {
        let mut env = envelope_from(
            instance,
            seq,
            TaskEvent::TagsSet {
                task_id: task_id.clone(),
                tags: vec![tag.into()],
            },
        );
        env.timestamp = timestamp.into();
        env
    };
    let mut envelopes = vec![
        tagged("AAA", "2026-08-24T00:00:03Z", 1, "last"),
        tagged("zzz", "2026-08-24T00:00:02Z", 1, "third"),
        tagged("aaa", "2026-08-24T00:00:02Z", 2, "second"),
        tagged("aaa", "2026-08-24T00:00:02Z", 1, "first"),
        envelope_at(
            "2026-08-24T00:00:01Z",
            1,
            TaskEvent::Created {
                task_id: task_id.clone(),
                title: "initial fold".into(),
                description: "cursor seed".into(),
                priority: "normal".into(),
                owner: None,
                due_at: None,
                depends_on: Vec::new(),
                routed_to: None,
                branch: None,
                bind: None,
                eta_secs: None,
                tags: Vec::new(),
                parent_id: None,
            },
        ),
    ];
    let mut order_key_sorted = envelopes.clone();
    order_key_sorted.sort_by_key(|env| OrderKey::from_envelope(env).expect("valid key"));
    super::super::sort_envelopes(&mut envelopes);
    let identity = |env: &TaskEventEnvelope| (env.timestamp.clone(), env.instance.clone(), env.seq);
    assert_eq!(
        order_key_sorted.iter().map(identity).collect::<Vec<_>>(),
        envelopes.iter().map(identity).collect::<Vec<_>>()
    );

    let mut incumbent = TaskBoardState::default();
    for env in &envelopes {
        assert!(incumbent.apply(env));
    }
    let projection = BoardProjection::from_sorted_envelopes(&envelopes).expect("initial fold");
    let mut expected = BoardProjection::from_replay(incumbent);
    expected.last_order_key = projection.last_order_key.clone();
    assert_eq!(projection, expected);

    let mut stale = tagged("zzz", "2026-08-24T00:00:02Z", 2, "stale");
    stale.timestamp = "2026-08-24T00:00:02Z".into();
    let mut projection = projection;
    let accepted = projection.clone();
    assert!(matches!(
        projection.apply_ordered(&stale),
        Err(OrderedApplyError::OutOfOrder { .. })
    ));
    assert_eq!(projection, accepted);

    let mut malformed = envelopes;
    malformed[1].timestamp = "not-a-timestamp".into();
    assert_eq!(
        BoardProjection::from_sorted_envelopes(&malformed),
        Err(OrderedApplyError::InvalidTimestamp(
            "not-a-timestamp".into()
        ))
    );
}

#[test]
fn canonical_batch_is_atomic_and_matches_sequential_apply() {
    let task_id = TaskId::from("t-20260824000000000000-1-1");
    let event = |timestamp: &str, seq: u64, tag: &str| {
        envelope_at(
            timestamp,
            seq,
            TaskEvent::TagsSet {
                task_id: task_id.clone(),
                tags: vec![tag.into()],
            },
        )
    };
    let created = envelope_at(
        "2026-08-24T00:00:01Z",
        1,
        TaskEvent::Created {
            task_id: task_id.clone(),
            title: "catch-up".into(),
            description: "atomic batch".into(),
            priority: "normal".into(),
            owner: None,
            due_at: None,
            depends_on: Vec::new(),
            routed_to: None,
            branch: None,
            bind: None,
            eta_secs: None,
            tags: Vec::new(),
            parent_id: None,
        },
    );
    let valid = vec![
        event("2026-08-24T00:00:02Z", 2, "second"),
        event("2026-08-24T00:00:03Z", 3, "third"),
    ];

    let mut projection = BoardProjection::default();
    projection.apply_ordered(&created).expect("seed");
    let seed = projection.clone();

    let mut malformed = valid.clone();
    malformed[1].timestamp = "not-a-timestamp".into();
    assert_eq!(
        projection.apply_ordered_batch(&malformed),
        Err(OrderedApplyError::InvalidTimestamp(
            "not-a-timestamp".into()
        ))
    );
    assert_eq!(projection, seed);

    assert!(matches!(
        projection.apply_ordered_batch(std::slice::from_ref(&created)),
        Err(OrderedApplyError::OutOfOrder { .. })
    ));
    assert_eq!(projection, seed);

    let out_of_order = vec![valid[1].clone(), valid[0].clone()];
    assert!(matches!(
        projection.apply_ordered_batch(&out_of_order),
        Err(OrderedApplyError::OutOfOrder { .. })
    ));
    assert_eq!(projection, seed);

    let mut sequential = seed;
    for env in &valid {
        sequential.apply_ordered(env).expect("ordered event");
    }
    projection
        .apply_ordered_batch(&valid)
        .expect("ordered batch");
    assert_eq!(projection, sequential);
}

#[test]
fn home_catalog_is_persistent_and_catches_up_appended_tail() {
    let home = tmp_home("catch-up");
    let task_id = TaskId::from("t-20260825000000000000-1-1");
    let writer = InstanceName::from("writer");

    super::super::append_at(&home, &writer, created(&task_id, "before")).expect("seed task");
    let first = for_home(&home);
    let second = for_home(&home);
    assert!(
        Arc::ptr_eq(&first, &second),
        "registry must retain one catalog"
    );
    assert_eq!(
        first.route(&task_id).expect("initial route").1.title,
        "before"
    );

    super::super::append_at(
        &home,
        &writer,
        TaskEvent::DescriptionUpdated {
            task_id: task_id.clone(),
            by: writer.clone(),
            description: "after".into(),
        },
    )
    .expect("append update");

    assert_eq!(
        first
            .route(&task_id)
            .expect("tail catch-up route")
            .1
            .description,
        "after"
    );
}

#[test]
fn authority_fails_closed_when_known_hot_log_is_replaced() {
    let home = tmp_home("replaced-log");
    let task_id = TaskId::from("t-20260825000000000000-2-1");
    let writer = InstanceName::from("writer");
    super::super::append_at(&home, &writer, created(&task_id, "known")).expect("seed task");
    let catalog = for_home(&home);
    catalog.route(&task_id).expect("initial route");

    std::fs::write(super::super::log_path(&home), b"").expect("replace hot log");
    assert!(matches!(
        catalog.route(&task_id),
        Err(CatalogRouteError::Unreadable)
    ));
    assert_ne!(
        catalog.snapshot_advisory().0,
        Phase::Ready,
        "the asynchronous rebuild may already have advanced Unhealthy to Building"
    );
}

#[cfg(windows)]
#[test]
fn authority_fails_closed_when_replace_file_preserves_creation_time() {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

    let home = tmp_home("replace-file-identity");
    let task_id = TaskId::from("t-20260825000000000000-2-2");
    let writer = InstanceName::from("writer");
    super::super::append_at(&home, &writer, created(&task_id, "known")).expect("seed task");
    let catalog = for_home(&home);
    catalog.route(&task_id).expect("initial route");

    let log_path = super::super::log_path(&home);
    let replacement_path = log_path.with_extension("replacement");
    let mut replacement = std::fs::read(&log_path).expect("read original log");
    replacement.extend_from_slice(
        format!(
            "{}\n",
            serde_json::to_string(&envelope_at(
                "2099-01-01T00:00:00Z",
                2,
                TaskEvent::DescriptionUpdated {
                    task_id: task_id.clone(),
                    by: writer,
                    description: "replacement tail".into(),
                }
            ))
            .expect("serialize replacement tail")
        )
        .as_bytes(),
    );
    std::fs::write(&replacement_path, replacement).expect("write replacement log");

    let log_wide = log_path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let replacement_wide = replacement_path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: both paths are valid, NUL-terminated UTF-16 buffers for this call.
    let replaced = unsafe {
        ReplaceFileW(
            log_wide.as_ptr(),
            replacement_wide.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_ne!(
        replaced,
        0,
        "ReplaceFileW failed: {}",
        std::io::Error::last_os_error()
    );

    assert!(matches!(
        catalog.route(&task_id),
        Err(CatalogRouteError::Unreadable)
    ));
    match catalog.snapshot_advisory().0 {
        Phase::Unhealthy { causes, .. } => assert!(
            causes.iter().any(|cause| cause.contains("identity")),
            "unexpected causes: {causes:?}"
        ),
        Phase::Building => {}
        phase => panic!("unexpected phase: {phase:?}"),
    }
}

#[test]
fn authority_discovers_new_board_before_answering() {
    let home = tmp_home("new-board");
    let default_id = TaskId::from("t-20260825000000000000-3-1");
    let project_id = TaskId::from("t-20260825000000000000-3-2");
    let writer = InstanceName::from("writer");
    super::super::append_at(&home, &writer, created(&default_id, "default")).expect("seed default");
    let catalog = for_home(&home);
    catalog.route(&default_id).expect("initial default route");

    let project = super::super::board_root(&home, "project");
    std::fs::create_dir_all(&project).expect("project board");
    super::super::append_at(&project, &writer, created(&project_id, "project"))
        .expect("seed project");

    let (board, task) = catalog.route(&project_id).expect("new board folded");
    assert_eq!(board, "project");
    assert_eq!(task.title, "project");
    assert_eq!(catalog.snapshot_advisory().0, Phase::Ready);
}

#[test]
fn commit_advances_canonical_order_past_a_future_cursor() {
    let home = tmp_home("future-cursor");
    let task_id = TaskId::from("t-20260825000000000000-4-1");
    let writer = InstanceName::from("writer");
    let seeded = envelope_at("2099-01-01T00:00:00Z", 1, created(&task_id, "future"));
    std::fs::write(
        super::super::log_path(&home),
        format!("{}\n", serde_json::to_string(&seeded).expect("serialize")),
    )
    .expect("seed future event");

    super::super::append_at(
        &home,
        &writer,
        TaskEvent::DescriptionUpdated {
            task_id: task_id.clone(),
            by: writer.clone(),
            description: "ordered".into(),
        },
    )
    .expect("ordered commit");

    let envelopes = super::super::stream_envelopes_at(&home).expect("stream events");
    let keys = envelopes
        .iter()
        .map(OrderKey::from_envelope)
        .collect::<Result<Vec<_>, _>>()
        .expect("valid keys");
    assert!(keys.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(
        for_home(&home)
            .route(&task_id)
            .expect("route")
            .1
            .description,
        "ordered"
    );
}

#[test]
fn commit_refuses_unreadable_board_without_writing_bytes() {
    let home = tmp_home("write-gate");
    let log = super::super::log_path(&home);
    let future = serde_json::json!({
        "schema_version": super::super::SCHEMA_VERSION + 1,
        "seq": 1,
        "timestamp": "2026-08-25T00:00:00Z",
        "instance": "future",
        "event": {
            "kind": "Created",
            "task_id": "t-20260825000000000000-5-1",
            "title": "future",
            "description": "",
            "priority": "normal",
            "owner": null
        }
    });
    let original = format!("{future}\n");
    std::fs::write(&log, &original).expect("future log");

    let result = super::super::append_at(
        &home,
        &InstanceName::from("writer"),
        created(&TaskId::from("t-20260825000000000000-5-2"), "must not land"),
    );
    assert!(result.is_err());
    assert_eq!(std::fs::read_to_string(log).expect("read log"), original);
}

#[test]
fn adoption_writes_manifest_and_checkpoint_without_existing_archive_dir() {
    let home = tmp_home("adoption-files");
    assert!(!super::super::archive_dir(&home).exists());

    let projection =
        load_board_projection(&home, super::super::DEFAULT_PROJECT).expect("adopt empty board");

    assert_eq!(projection.events_folded(), 0);
    assert!(manifest_path(&home).is_file());
    assert!(checkpoint_path(&home).is_file());
    assert_eq!(
        load_manifest(&home)
            .expect("load manifest")
            .expect("manifest")
            .archives,
        Vec::new()
    );
}

#[test]
fn checkpoint_reload_skips_full_rebuild_and_folds_only_hot_tail() {
    let home = tmp_home("checkpoint-tail");
    let task_id = TaskId::from("t-20260825000000000000-6-1");
    let writer = InstanceName::from("writer");
    let first = envelope(1, created(&task_id, "checkpoint"));
    write_envelopes(&super::super::log_path(&home), std::slice::from_ref(&first));

    let before = rebuild_count();
    let adopted =
        load_board_projection(&home, super::super::DEFAULT_PROJECT).expect("initial adoption");
    assert_eq!(rebuild_count(), before + 1);
    assert_eq!(adopted.task(&task_id).expect("task").description, "");

    let update = envelope_at(
        "2026-08-25T00:01:00Z",
        2,
        TaskEvent::DescriptionUpdated {
            task_id: task_id.clone(),
            by: writer,
            description: "tail".into(),
        },
    );
    let mut log = std::fs::OpenOptions::new()
        .append(true)
        .open(super::super::log_path(&home))
        .expect("open hot log");
    use std::io::Write as _;
    writeln!(
        log,
        "{}",
        serde_json::to_string(&update).expect("serialize tail")
    )
    .expect("append tail");

    let reloaded =
        load_board_projection(&home, super::super::DEFAULT_PROJECT).expect("checkpoint reload");
    assert_eq!(rebuild_count(), before + 1, "must not replay full history");
    assert_eq!(
        reloaded.task(&task_id).expect("tail task").description,
        "tail"
    );
}

#[test]
fn checkpoint_second_boot_reads_zero_archive_bytes() {
    let home = tmp_home("checkpoint-zero-archive-read");
    let task_id = TaskId::from("t-20260825000000000000-6-2");
    let archive = super::super::archive_dir(&home).join("0001.jsonl");
    write_envelopes(&archive, &[envelope(1, created(&task_id, "archive"))]);

    load_board_projection(&home, super::super::DEFAULT_PROJECT).expect("adopt archive");
    reset_archive_bytes_read();
    let reloaded =
        load_board_projection(&home, super::super::DEFAULT_PROJECT).expect("checkpoint reload");

    assert_eq!(archive_bytes_read(), 0);
    assert_eq!(reloaded.task(&task_id).expect("task").title, "archive");
}

#[test]
fn invalid_checkpoint_is_scrubbed_rebuilt_and_replaced() {
    let home = tmp_home("checkpoint-heal");
    let task_id = TaskId::from("t-20260825000000000000-7-1");
    write_envelopes(
        &super::super::log_path(&home),
        &[envelope(1, created(&task_id, "heal"))],
    );
    load_board_projection(&home, super::super::DEFAULT_PROJECT).expect("adopt");

    let path = checkpoint_path(&home);
    let mut checkpoint: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("read checkpoint"))
            .expect("parse checkpoint");
    checkpoint["tasks"][0]["recent_history"][0]["kind"] =
        serde_json::Value::String("future_kind".into());
    std::fs::write(
        &path,
        serde_json::to_vec(&checkpoint).expect("serialize tamper"),
    )
    .expect("tamper checkpoint");

    let before = rebuild_count();
    let healed = load_board_projection(&home, super::super::DEFAULT_PROJECT)
        .expect("rebuild invalid checkpoint");
    assert_eq!(rebuild_count(), before + 1);
    assert_eq!(healed.task(&task_id).expect("healed task").title, "heal");
    let replaced: CheckpointV1 =
        serde_json::from_slice(&std::fs::read(path).expect("read replacement"))
            .expect("valid replacement");
    assert_eq!(replaced.schema, CHECKPOINT_SCHEMA);
}

#[test]
fn manifest_digest_mismatch_fails_closed_when_rebuild_is_required() {
    let home = tmp_home("manifest-digest");
    let task_id = TaskId::from("t-20260825000000000000-8-1");
    let archive = super::super::archive_dir(&home).join("0001.jsonl");
    write_envelopes(&archive, &[envelope(1, created(&task_id, "archive"))]);
    load_board_projection(&home, super::super::DEFAULT_PROJECT).expect("adopt archive");

    let manifest_file = manifest_path(&home);
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_file).expect("read manifest"))
            .expect("parse manifest");
    manifest["archives"][0]["digest_sha256"] = serde_json::Value::String("00".repeat(32));
    std::fs::write(
        &manifest_file,
        serde_json::to_vec(&manifest).expect("serialize manifest"),
    )
    .expect("tamper manifest");
    std::fs::remove_file(checkpoint_path(&home)).expect("remove checkpoint");

    let error = load_board_projection(&home, super::super::DEFAULT_PROJECT)
        .expect_err("digest mismatch must fail closed");
    assert!(error.contains("archive digest mismatch"), "{error}");
}

#[test]
fn out_of_order_tail_fails_closed_then_background_rebuild_restores_authority() {
    let home = tmp_home("out-of-order-rebuild");
    let task_id = TaskId::from("t-20260825000000000000-3-5");
    let writer = InstanceName::from("writer");
    super::super::append_at(&home, &writer, created(&task_id, "known")).expect("seed");
    let catalog = for_home(&home);
    catalog.route(&task_id).expect("initial route");

    let mut stale = envelope_from(
        "external",
        1,
        TaskEvent::DescriptionUpdated {
            task_id: task_id.clone(),
            by: writer,
            description: "stale".into(),
        },
    );
    stale.timestamp = "2000-01-01T00:00:00Z".into();
    let mut log = std::fs::OpenOptions::new()
        .append(true)
        .open(super::super::log_path(&home))
        .expect("open log");
    use std::io::Write as _;
    writeln!(log, "{}", serde_json::to_string(&stale).expect("serialize"))
        .expect("append stale tail");
    log.sync_all().expect("sync tail");

    assert!(matches!(
        catalog.route(&task_id),
        Err(CatalogRouteError::Unreadable)
    ));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while catalog.snapshot_advisory().0 != Phase::Ready && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert_eq!(catalog.snapshot_advisory().0, Phase::Ready);
    assert_eq!(
        catalog.route(&task_id).expect("restored route").1.title,
        "known"
    );
}

#[test]
fn durable_line_ahead_of_catalog_is_folded_without_reminting_its_seq() {
    use std::io::Write as _;

    let home = tmp_home("crash-after-fsync");
    let task_id = TaskId::from("t-20260825000000000000-4-2");
    let writer = InstanceName::from("writer");
    super::super::append_at(&home, &writer, created(&task_id, "before")).expect("seed");
    let catalog = for_home(&home);
    catalog.route(&task_id).expect("initial route");

    let durable = envelope_at(
        "2099-01-01T00:00:00Z",
        2,
        TaskEvent::DescriptionUpdated {
            task_id: task_id.clone(),
            by: writer.clone(),
            description: "durable".into(),
        },
    );
    let mut log = std::fs::OpenOptions::new()
        .append(true)
        .open(super::super::log_path(&home))
        .expect("open log");
    writeln!(
        log,
        "{}",
        serde_json::to_string(&durable).expect("serialize")
    )
    .expect("append durable line");
    log.sync_all().expect("durable point");

    super::super::append_at(
        &home,
        &writer,
        TaskEvent::TagsSet {
            task_id: task_id.clone(),
            tags: vec!["after-restart".into()],
        },
    )
    .expect("catch up and commit");

    let envelopes = super::super::stream_envelopes_at(&home).expect("stream");
    assert_eq!(
        envelopes.iter().map(|env| env.seq).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    let task = catalog.route(&task_id).expect("caught-up route").1;
    assert_eq!(task.description, "durable");
    assert_eq!(task.tags, vec!["after-restart"]);
}

#[test]
fn checked_commit_uses_catalog_state_without_replaying_history() {
    let home = tmp_home("commit-no-archive-replay");
    let task_id = TaskId::from("t-20260825000000000000-4-3");
    let writer = InstanceName::from("writer");
    let archive = super::super::archive_dir(&home).join("0001.jsonl");
    write_envelopes(&archive, &[envelope(1, created(&task_id, "archived"))]);

    let projection =
        load_board_projection(&home, super::super::DEFAULT_PROJECT).expect("adopt archived board");
    let key = std::fs::canonicalize(&home).expect("canonical home");
    CATALOGS.lock().insert(
        key.clone(),
        Arc::new(StrictTaskCatalog::with_home(
            Some(key.clone()),
            Phase::Ready,
            BTreeMap::from([(super::super::DEFAULT_PROJECT.to_string(), projection)]),
        )),
    );
    super::super::reset_replay_uncached_calls();

    let result = super::super::append_checked_at(
        &home,
        &writer,
        TaskEvent::DescriptionUpdated {
            task_id: task_id.clone(),
            by: writer.clone(),
            description: "updated".into(),
        },
        |state| {
            let task = state.tasks.get(&task_id).ok_or("task missing")?;
            (task.title == "archived")
                .then_some(())
                .ok_or_else(|| "catalog state mismatch".to_string())
        },
    )
    .expect("checked commit");

    assert!(result.is_ok());
    assert_eq!(
        super::super::replay_uncached_calls(),
        0,
        "commit must not replay task history"
    );
    CATALOGS.lock().remove(&key);
}

#[test]
fn commit_refuses_building_catalog_without_writing_bytes() {
    let home = tmp_home("building-write-gate");
    let key = std::fs::canonicalize(&home).expect("canonical home");
    CATALOGS.lock().insert(
        key.clone(),
        Arc::new(StrictTaskCatalog::with_home(
            Some(key.clone()),
            Phase::Building,
            BTreeMap::new(),
        )),
    );

    let result = super::super::append_at(
        &home,
        &InstanceName::from("writer"),
        created(&TaskId::from("t-20260825000000000000-5-3"), "must not land"),
    );
    assert!(result.is_err());
    assert!(!super::super::log_path(&home).exists());
    CATALOGS.lock().remove(&key);
}

#[test]
fn future_checkpoint_schema_is_ignored_and_rebuilt() {
    let home = tmp_home("checkpoint-schema");
    let task_id = TaskId::from("t-20260825000000000000-7-2");
    write_envelopes(
        &super::super::log_path(&home),
        &[envelope(1, created(&task_id, "schema"))],
    );
    load_board_projection(&home, super::super::DEFAULT_PROJECT).expect("adopt");

    let path = checkpoint_path(&home);
    let mut checkpoint: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("read checkpoint"))
            .expect("parse checkpoint");
    checkpoint["schema"] = serde_json::Value::from(CHECKPOINT_SCHEMA + 1);
    std::fs::write(
        &path,
        serde_json::to_vec(&checkpoint).expect("serialize checkpoint"),
    )
    .expect("write future checkpoint");

    let before = rebuild_count();
    let rebuilt = load_board_projection(&home, super::super::DEFAULT_PROJECT)
        .expect("rebuild future checkpoint");
    assert_eq!(rebuild_count(), before + 1);
    assert_eq!(
        rebuilt.task(&task_id).expect("rebuilt task").title,
        "schema"
    );
    let replaced: CheckpointV1 =
        serde_json::from_slice(&std::fs::read(path).expect("read replacement"))
            .expect("valid replacement");
    assert_eq!(replaced.schema, CHECKPOINT_SCHEMA);
}

#[test]
fn archive_adoption_runs_in_background_and_publishes_ready_catalog() {
    let home = tmp_home("background-adoption");
    let task_id = TaskId::from("t-20260825000000000000-9-1");
    let archive = super::super::archive_dir(&home).join("0001.jsonl");
    write_envelopes(&archive, &[envelope(1, created(&task_id, "adopted"))]);

    let catalog = for_home(&home);
    if catalog.snapshot_advisory().0 == Phase::Building {
        assert!(matches!(
            catalog.route(&task_id),
            Err(CatalogRouteError::Unreadable)
        ));
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while catalog.snapshot_advisory().0 == Phase::Building && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }

    assert_eq!(catalog.snapshot_advisory().0, Phase::Ready);
    assert_eq!(
        catalog.route(&task_id).expect("adopted route").1.title,
        "adopted"
    );
    assert!(manifest_path(&home).is_file());
    assert!(checkpoint_path(&home).is_file());
}

#[test]
fn rebuild_retries_rotation_and_publishes_concurrent_tail() {
    let home = tmp_home("rebuild-concurrent");
    let task_id = TaskId::from("t-20260825000000000000-9-2");
    let writer = InstanceName::from("writer");
    write_envelopes(
        &super::super::log_path(&home),
        &[envelope(1, created(&task_id, "before"))],
    );
    let catalog =
        StrictTaskCatalog::with_home(Some(home.clone()), Phase::Building, BTreeMap::new());

    catalog
        .rebuild_from_disk_with_hook(&home, |attempt| {
            if attempt != 0 {
                return;
            }
            let update = envelope_at(
                "2026-08-25T00:01:00Z",
                2,
                TaskEvent::DescriptionUpdated {
                    task_id: task_id.clone(),
                    by: writer.clone(),
                    description: "after".into(),
                },
            );
            let log_path = super::super::log_path(&home);
            let mut bytes = std::fs::read(&log_path).expect("read first snapshot");
            bytes.extend_from_slice(
                format!(
                    "{}\n",
                    serde_json::to_string(&update).expect("serialize update")
                )
                .as_bytes(),
            );
            crate::store::atomic_write(&log_path, &bytes).expect("rotate concurrent log");
        })
        .expect("second rebuild attempt stabilizes");

    assert_eq!(catalog.snapshot_advisory().0, Phase::Ready);
    assert_eq!(
        catalog
            .route(&task_id)
            .expect("rebuilt route")
            .1
            .description,
        "after"
    );
}

#[test]
fn rebuild_fails_closed_after_three_rotations() {
    let home = tmp_home("rebuild-attempt-budget");
    let task_id = TaskId::from("t-20260825000000000000-9-3");
    write_envelopes(
        &super::super::log_path(&home),
        &[envelope(1, created(&task_id, "unstable"))],
    );
    let catalog =
        StrictTaskCatalog::with_home(Some(home.clone()), Phase::Building, BTreeMap::new());

    assert!(matches!(
        catalog.rebuild_from_disk_with_hook(&home, |_| {
            let log_path = super::super::log_path(&home);
            let bytes = std::fs::read(&log_path).expect("read log");
            crate::store::atomic_write(&log_path, &bytes).expect("rotate log");
        }),
        Err(CatalogRouteError::Unreadable)
    ));
    let Phase::Unhealthy { causes, .. } = catalog.snapshot_advisory().0 else {
        panic!("rebuild must publish Unhealthy after its bounded retries");
    };
    assert!(causes[0].contains("exhausted 3 attempts"), "{causes:?}");
}

#[test]
fn compaction_catches_up_external_tail_before_rotating_hot_log() {
    use std::io::Write as _;

    let home = tmp_home("compact-catch-up");
    let task_id = TaskId::from("t-20260825000000000000-9-4");
    let writer = InstanceName::from("writer");
    super::super::append_at(&home, &writer, created(&task_id, "before")).expect("seed");
    let catalog = for_home(&home);
    catalog.route(&task_id).expect("initial route");

    let update = envelope_at(
        "2099-01-01T00:00:00Z",
        2,
        TaskEvent::DescriptionUpdated {
            task_id: task_id.clone(),
            by: writer,
            description: "external tail".into(),
        },
    );
    let mut log = std::fs::OpenOptions::new()
        .append(true)
        .open(super::super::log_path(&home))
        .expect("open log");
    writeln!(
        log,
        "{}",
        serde_json::to_string(&update).expect("serialize")
    )
    .expect("append external update");
    log.sync_all().expect("sync update");

    compact_at_with_keep(&home, 1).expect("compact");
    assert_eq!(
        catalog
            .route(&task_id)
            .expect("route after compact")
            .1
            .description,
        "external tail"
    );
    let before = rebuild_count();
    let checkpoint = load_board_projection(&home, super::super::DEFAULT_PROJECT)
        .expect("post-compaction checkpoint");
    assert_eq!(
        rebuild_count(),
        before,
        "checkpoint must avoid archive replay"
    );
    assert_eq!(
        checkpoint
            .task(&task_id)
            .expect("checkpoint task")
            .description,
        "external tail"
    );
}
