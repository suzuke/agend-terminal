use super::*;
use serial_test::serial;
use std::fs;
use std::sync::atomic::{AtomicU32, Ordering};

fn tmp_home(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-task-events-{}-{}-{}",
        std::process::id(),
        tag,
        id
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn sample_event(id: &str) -> TaskEvent {
    TaskEvent::Created {
        task_id: id.into(),
        title: format!("title for {id}"),
        description: "desc".to_string(),
        priority: "normal".to_string(),
        owner: None,
        due_at: None,
        depends_on: Vec::new(),
        routed_to: None,
        branch: None,
        bind: None,
        eta_secs: None,
        tags: vec![],
        parent_id: None,
    }
}

#[test]
fn append_assigns_monotonic_seq_per_instance() {
    let home = tmp_home("seq");
    let inst = InstanceName::from("dev-impl-1");
    let s1 = append(&home, &inst, sample_event("t-A")).unwrap();
    let s2 = append(&home, &inst, sample_event("t-B")).unwrap();
    let s3 = append(&home, &inst, sample_event("t-C")).unwrap();
    assert_eq!((s1, s2, s3), (1, 2, 3));
    fs::remove_dir_all(&home).ok();
}

#[test]
fn append_batch_atomic_consecutive_seqs() {
    let home = tmp_home("batch");
    let inst = InstanceName::from("a");
    let seqs = append_batch(
        &home,
        &inst,
        vec![
            sample_event("t-1"),
            sample_event("t-2"),
            sample_event("t-3"),
        ],
    )
    .unwrap();
    assert_eq!(seqs, vec![1, 2, 3]);
    let content = fs::read_to_string(home.join("task_events.jsonl")).unwrap();
    assert_eq!(content.lines().count(), 3);
    fs::remove_dir_all(&home).ok();
}

#[test]
fn seq_is_per_instance_isolated() {
    let home = tmp_home("isolate");
    let a = InstanceName::from("agent-a");
    let b = InstanceName::from("agent-b");
    let _ = append(&home, &a, sample_event("t-A1")).unwrap();
    let _ = append(&home, &a, sample_event("t-A2")).unwrap();
    let s_b1 = append(&home, &b, sample_event("t-B1")).unwrap();
    assert_eq!(s_b1, 1, "agent-b's seq is independent of agent-a");
    fs::remove_dir_all(&home).ok();
}

#[test]
fn replay_folds_basic_lifecycle() {
    let home = tmp_home("fold");
    let inst = InstanceName::from("u");
    append(&home, &inst, sample_event("t-X")).unwrap();
    append(
        &home,
        &inst,
        TaskEvent::Claimed {
            task_id: "t-X".into(),
            by: "agent".into(),
        },
    )
    .unwrap();
    append(
        &home,
        &inst,
        TaskEvent::Done {
            task_id: "t-X".into(),
            by: "agent".into(),
            source: DoneSource::OperatorManual {
                authored_at: chrono::Utc::now().to_rfc3339(),
                result: Some("ok".into()),
            },
        },
    )
    .unwrap();
    let state = replay(&home).unwrap();
    let task = state.tasks.get(&TaskId::from("t-X")).unwrap();
    assert_eq!(task.status, TaskStatus::Done);
    assert_eq!(task.history.len(), 3);
    fs::remove_dir_all(&home).ok();
}

/// Replay-determinism invariant #4 — a v(N) reader rejects v(N+k)
/// envelopes (k>0) rather than dropping unknown fields. Operators
/// running an older binary against a newer log fail loud.
#[test]
#[serial(task_replay_latch)] // #1990 item 4: shares the global fail-closed-emit latch
fn invariant_4_forward_compat_fail_closed() {
    let home = tmp_home("future");
    let log = home.join("task_events.jsonl");
    // Hand-craft a v999 envelope.
    let line = serde_json::json!({
        "schema_version": 999,
        "seq": 1,
        "timestamp": "2026-04-27T00:00:00Z",
        "instance": "test",
        "event": {"kind": "Unblocked", "task_id": "t-X"}
    });
    fs::write(&log, format!("{line}\n")).unwrap();
    let err = replay(&home).expect_err("must fail-closed on future schema");
    assert!(
        err.to_string().contains("forward-compat fail-closed"),
        "got: {err}"
    );
    fs::remove_dir_all(&home).ok();
}

#[test]
#[serial(task_replay_latch)] // #1990 item 4: shares the global fail-closed-emit latch
fn replay_rejects_unknown_event_variant() {
    let home = tmp_home("unknown");
    let log = home.join("task_events.jsonl");
    let line = serde_json::json!({
        "schema_version": 1,
        "seq": 1,
        "timestamp": "2026-04-27T00:00:00Z",
        "instance": "test",
        "event": {"kind": "TotallyMadeUpVariant", "task_id": "t-X"}
    });
    fs::write(&log, format!("{line}\n")).unwrap();
    let err = replay(&home).expect_err("must fail-closed on unknown variant");
    assert!(err.to_string().contains("replay aborts"), "got: {err}");
    fs::remove_dir_all(&home).ok();
}

// ── #1988: corrupt-line resilience ──────────────────────────────────

/// #1988 shape 1 — a CORRUPT (non-JSON) line in the MIDDLE of a real board
/// lifecycle (create → claim → update → done) must be SKIPPED, not abort the
/// whole replay. Goes through the real producer (`append`) for the good lines
/// and the real consumer (`replay`) — not a unit-injected `read_envelopes_strict`.
#[test]
fn replay_skips_corrupt_midfile_line_keeps_full_lifecycle() {
    let home = tmp_home("corrupt-skip");
    let inst = InstanceName::from("dev-impl-1");
    let log = home.join("task_events.jsonl");

    append(&home, &inst, sample_event("t-X")).unwrap();
    append(
        &home,
        &inst,
        TaskEvent::Claimed {
            task_id: "t-X".into(),
            by: "agent".into(),
        },
    )
    .unwrap();
    // Simulate a crash-torn / disk-glitched line landing mid-log.
    {
        use std::io::Write;
        let mut f = fs::OpenOptions::new().append(true).open(&log).unwrap();
        writeln!(f, "this is not valid json {{{{ truncated").unwrap();
    }
    append(
        &home,
        &inst,
        TaskEvent::DescriptionUpdated {
            task_id: "t-X".into(),
            by: "agent".into(),
            description: "updated desc".into(),
        },
    )
    .unwrap();
    append(
        &home,
        &inst,
        TaskEvent::Done {
            task_id: "t-X".into(),
            by: "agent".into(),
            source: DoneSource::OperatorManual {
                authored_at: chrono::Utc::now().to_rfc3339(),
                result: Some("ok".into()),
            },
        },
    )
    .unwrap();

    // The garbage really is in the log...
    let raw = fs::read_to_string(&log).unwrap();
    assert!(
        raw.contains("not valid json"),
        "fixture must contain the bad line"
    );
    // ...yet replay folds the full lifecycle, skipping it (no abort).
    let state = replay(&home).expect("corrupt mid-line must NOT brick replay");
    let task = state
        .tasks
        .get(&TaskId::from("t-X"))
        .expect("task survives the corrupt line");
    assert_eq!(task.status, TaskStatus::Done);
    fs::remove_dir_all(&home).ok();
}

/// #1988 shape 2 — a half-written TAIL line (crash mid-append) is quarantined
/// and rewritten out of the hot log by `recover_half_writes` (the real boot
/// entry), preserving every good event and leaving a forensic copy.
#[test]
fn recover_half_writes_quarantines_torn_tail() {
    let home = tmp_home("recover-tail");
    let inst = InstanceName::from("u");
    let log = home.join("task_events.jsonl");

    append(&home, &inst, sample_event("t-Y")).unwrap();
    append(
        &home,
        &inst,
        TaskEvent::Done {
            task_id: "t-Y".into(),
            by: "agent".into(),
            source: DoneSource::OperatorManual {
                authored_at: chrono::Utc::now().to_rfc3339(),
                result: None,
            },
        },
    )
    .unwrap();
    // A torn trailing fragment from a crash mid-append (not valid JSON).
    {
        use std::io::Write;
        let mut f = fs::OpenOptions::new().append(true).open(&log).unwrap();
        // Unique sentinel — a generic fragment like "timesta" is a substring
        // of the good lines' "timestamp" field and would give a false match.
        write!(f, "{{\"schema_version\":2,\"seq\":99,\"TORN_SENTINEL_ZZ").unwrap();
    }

    recover_half_writes(&home);

    // The torn line is gone from the hot log; every remaining line is valid JSON.
    let rewritten = fs::read_to_string(&log).unwrap();
    assert!(
        !rewritten.contains("TORN_SENTINEL_ZZ"),
        "torn tail must be removed"
    );
    for l in rewritten.lines().filter(|l| !l.trim().is_empty()) {
        serde_json::from_str::<serde_json::Value>(l).expect("every kept line is valid JSON");
    }
    // It is quarantined, not silently destroyed.
    let rec_root = home.join("task_events.recovery");
    let sub = fs::read_dir(&rec_root)
        .unwrap()
        .next()
        .expect("a recovery subdir exists")
        .unwrap()
        .path();
    let quarantined = fs::read_to_string(sub.join("task_events.jsonl")).unwrap();
    assert!(
        quarantined.contains("TORN_SENTINEL_ZZ"),
        "torn tail preserved for forensics"
    );
    // The board still replays cleanly with the good events.
    let state = replay(&home).expect("replay clean after recovery");
    assert_eq!(
        state.tasks.get(&TaskId::from("t-Y")).unwrap().status,
        TaskStatus::Done
    );
    fs::remove_dir_all(&home).ok();
}

/// #1988 shape 3 — recovery must NOT auto-drop a newer daemon's events: a
/// FUTURE-VERSION line is valid JSON, so `recover_half_writes` KEEPS it and the
/// read-path fail-closed gate still fires on replay. (Proves the corrupt-skip
/// and forward-compat-abort responsibilities stay cleanly separated.)
#[test]
#[serial(task_replay_latch)] // #1990 item 4: shares the global fail-closed-emit latch
fn recover_keeps_future_version_line_replay_still_fail_closed() {
    let home = tmp_home("recover-future");
    let inst = InstanceName::from("u");
    let log = home.join("task_events.jsonl");
    append(&home, &inst, sample_event("t-Z")).unwrap();
    let future = serde_json::json!({
        "schema_version": 999,
        "seq": 2,
        "timestamp": "2026-04-27T00:00:00Z",
        "instance": "newer-daemon",
        "event": {"kind": "Unblocked", "task_id": "t-Z"}
    });
    {
        use std::io::Write;
        let mut f = fs::OpenOptions::new().append(true).open(&log).unwrap();
        writeln!(f, "{future}").unwrap();
    }

    recover_half_writes(&home);

    let after = fs::read_to_string(&log).unwrap();
    assert!(
        after.contains("\"schema_version\":999"),
        "recovery must keep the future-version line (it is valid JSON, not garbage)"
    );
    let err = replay(&home).expect_err("future-version still fail-closed after recovery");
    assert!(
        err.to_string().contains("forward-compat fail-closed"),
        "got: {err}"
    );
    fs::remove_dir_all(&home).ok();
}

/// #1990 item 4: a fail-closed replay (the board freezes while the per-tick
/// callers swallow the Err) must surface ONE operator-visible event_log entry
/// per boot — and a second fail-closed replay in the same boot must NOT emit a
/// duplicate (latched). Serialized with the other latch-tripping tests so the
/// process-global latch reset is uninterrupted.
#[test]
#[serial(task_replay_latch)]
fn replay_fail_closed_surfaces_operator_event_once() {
    let home = tmp_home("failclosed-visible");
    REPLAY_FAILCLOSED_EVENT_EMITTED.store(false, std::sync::atomic::Ordering::Relaxed);
    // A future-version record → replay fail-closes (#1992 forward-compat).
    let line = serde_json::json!({
        "schema_version": 999,
        "seq": 1,
        "timestamp": "2026-04-27T00:00:00Z",
        "instance": "newer-daemon",
        "event": {"kind": "Unblocked", "task_id": "t-X"}
    });
    fs::write(home.join("task_events.jsonl"), format!("{line}\n")).unwrap();
    // Two fail-closed replays in the same boot (Err is not cached, so both
    // re-run replay_uncached and reach the surface helper).
    assert!(replay(&home).is_err());
    assert!(replay(&home).is_err());
    // Exactly one operator event was emitted (latched).
    let elog = fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
    assert_eq!(
        elog.matches("task_replay_fail_closed").count(),
        1,
        "fail-closed replay must surface exactly one operator event per boot, got: {elog}"
    );
    fs::remove_dir_all(&home).ok();
}

/// #1990 item 4 (reviewer-2 minor 1): the boundary that keeps disk jitter from
/// becoming a false alarm — a transient IO-class replay error (no "fail-closed"
/// substring) must NOT surface an operator event. Guards the substring
/// classifier against over-firing.
#[test]
#[serial(task_replay_latch)]
fn transient_io_error_does_not_surface_operator_event() {
    let home = tmp_home("io-no-surface");
    REPLAY_FAILCLOSED_EVENT_EMITTED.store(false, std::sync::atomic::Ordering::Relaxed);
    // A bare IO-class error (what a vanished/locked file yields) — not the
    // forward-compat "fail-closed" class.
    let io_err = anyhow::anyhow!("No such file or directory (os error 2)");
    surface_failclosed_replay_once(&home, &io_err);
    let elog = fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
    assert!(
        !elog.contains("task_replay_fail_closed"),
        "a transient IO error must NOT surface an operator event (false-alarm guard): {elog}"
    );
    fs::remove_dir_all(&home).ok();
}

#[test]
fn compact_archives_older_than_keep_threshold() {
    let home = tmp_home("compact");
    let inst = InstanceName::from("u");
    // Synthesise lines past the threshold without actually appending
    // 10001 events (slow). Instead bypass append, write directly.
    let log = home.join("task_events.jsonl");
    let mut lines = String::new();
    for i in 1..=(COMPACTION_KEEP + 5) {
        let env = TaskEventEnvelope {
            schema_version: SCHEMA_VERSION,
            seq: i as u64,
            timestamp: format!("2026-04-27T{:02}:00:00Z", i % 24),
            instance: inst.clone(),
            emitter_id: None,
            event: TaskEvent::Unblocked {
                task_id: format!("t-{i}").as_str().into(),
            },
        };
        lines.push_str(&serde_json::to_string(&env).unwrap());
        lines.push('\n');
    }
    fs::write(&log, lines).unwrap();
    compact(&home).unwrap();
    let kept = fs::read_to_string(&log).unwrap();
    assert_eq!(kept.lines().count(), COMPACTION_KEEP);
    let arc = archive_dir(&home);
    let entries: Vec<_> = fs::read_dir(&arc).unwrap().flatten().collect();
    assert_eq!(entries.len(), 1, "exactly one archive file expected");
    fs::remove_dir_all(&home).ok();
}

// ── Retention seq-safety (E1) ─────────────────────────────────────
//
// GATE (neuter-RED): an instance whose events are ALL archived must still
// get a non-colliding seq on its next append, so replay does NOT silently
// drop the new transition. Without the per-instance seq sidecar,
// `max_seq_for_instance` scans the hot log only → 0 for the archived
// instance → seq reuse → replay's `seq <= last_seen` idempotency drops it.
// (Neuter `load_seq_highwater` to always return an empty map ⇒ this RED.)
#[test]
#[serial]
fn retention_idle_instance_all_archived_no_seq_collision_gate() {
    let home = tmp_home("retain-collision");
    let board = board_root(&home, DEFAULT_PROJECT);
    let a = InstanceName::from("idle-agent");
    let b = InstanceName::from("busy-agent");
    let keep = 5;

    // A emits its only event, then B floods past `keep` so A's event archives.
    append(&home, &a, sample_event("t-A1")).unwrap();
    for i in 0..(keep + 2) {
        append(&home, &b, sample_event(&format!("t-B{i}"))).unwrap();
    }
    compact_at_with_keep(&board, keep).unwrap();

    // Precondition: A's events are out of the hot log (hot-only scan = 0).
    assert_eq!(
        max_seq_for_instance(&log_path(&board), &a).unwrap().0,
        0,
        "precondition: A's events must all be archived out of the hot log"
    );

    // A appends again — must get a fresh, non-colliding seq.
    append(&home, &a, sample_event("t-A2")).unwrap();

    let state = replay(&home).unwrap();
    assert!(
        state.tasks.contains_key(&TaskId::from("t-A2")),
        "re-appended transition was SILENTLY DROPPED (seq collision)"
    );
    assert!(
        state.tasks.contains_key(&TaskId::from("t-A1")),
        "the archived original transition must still replay"
    );
    fs::remove_dir_all(&home).ok();
}

// Variant: A's events land in an OLD archive segment while a LATER segment
// holds only OTHER instances. A "scan-newest-archive-segment" shortcut would
// miss A's high-water → collision; the full-coverage sidecar does not. This
// is why the fix is a persisted sidecar, not an archive re-scan.
#[test]
#[serial]
fn retention_idle_instance_in_old_archive_segment_no_collision() {
    let home = tmp_home("retain-old-seg");
    let board = board_root(&home, DEFAULT_PROJECT);
    let a = InstanceName::from("idle-agent");
    let b = InstanceName::from("busy-b");
    let c = InstanceName::from("busy-c");
    let keep = 3;

    // Segment 1: A + B → compact archives A (oldest) into segment 1.
    append(&home, &a, sample_event("t-A1")).unwrap();
    for i in 0..(keep + 1) {
        append(&home, &b, sample_event(&format!("t-B{i}"))).unwrap();
    }
    compact_at_with_keep(&board, keep).unwrap();

    // Segment 2: C floods → compact archives the leftovers into a NEWER
    // segment that contains NO A events.
    for i in 0..(keep + 1) {
        append(&home, &c, sample_event(&format!("t-C{i}"))).unwrap();
    }
    compact_at_with_keep(&board, keep).unwrap();

    assert_eq!(
        max_seq_for_instance(&log_path(&board), &a).unwrap().0,
        0,
        "precondition: A only in an OLD archive segment, not the hot log"
    );
    append(&home, &a, sample_event("t-A2")).unwrap();
    let state = replay(&home).unwrap();
    assert!(
        state.tasks.contains_key(&TaskId::from("t-A2")),
        "sidecar must cover A's high-water even from an OLD archive segment"
    );
    fs::remove_dir_all(&home).ok();
}

// (a) The wiring: a normal append that pushes the hot log past
// COMPACTION_HIGH_WATER triggers `maybe_compact_events`, trimming it back to
// COMPACTION_KEEP (#2389 E1 hysteresis — below HIGH_WATER it is NOT trimmed,
// see hysteresis_no_trim_between_keep_and_high_water).
// Direct-write COMPACTION_HIGH_WATER+1 (cheap), then ONE real append.
#[test]
#[serial]
fn retention_append_triggers_compaction_bounds_hot_log() {
    let home = tmp_home("retain-bound");
    let board = board_root(&home, DEFAULT_PROJECT);
    let inst = InstanceName::from("u");
    let log = log_path(&board);
    let mut lines = String::new();
    for i in 1..=(COMPACTION_HIGH_WATER + 1) {
        let env = TaskEventEnvelope {
            schema_version: SCHEMA_VERSION,
            seq: i as u64,
            timestamp: format!("2026-06-21T{:02}:00:00Z", i % 24),
            instance: inst.clone(),
            emitter_id: None,
            event: TaskEvent::Unblocked {
                task_id: format!("t-{i}").as_str().into(),
            },
        };
        lines.push_str(&serde_json::to_string(&env).unwrap());
        lines.push('\n');
    }
    fs::write(&log, lines).unwrap();
    assert!(fs::read_to_string(&log).unwrap().lines().count() > COMPACTION_HIGH_WATER);
    append(&home, &inst, sample_event("t-trigger")).unwrap();
    assert_eq!(
            fs::read_to_string(&log).unwrap().lines().count(),
            COMPACTION_KEEP,
            "an append past COMPACTION_HIGH_WATER must trigger compaction, trimming the hot log to COMPACTION_KEEP"
        );
    fs::remove_dir_all(&home).ok();
}

// ── #2389 E1 follow-up: compaction hysteresis ────────────────────────

/// #2389 E1 hysteresis (write-amplification fix): a hot log between
/// COMPACTION_KEEP and COMPACTION_HIGH_WATER must NOT be compacted — an
/// append there leaves the hot log untrimmed and writes NO archive segment.
/// RED pre-fix: compaction fired on EVERY append past COMPACTION_KEEP, so the
/// hot log would be trimmed to KEEP and a 1-line archive segment would exist.
#[test]
#[serial]
fn hysteresis_no_trim_between_keep_and_high_water() {
    let home = tmp_home("hyst-notrim");
    let board = board_root(&home, DEFAULT_PROJECT);
    let inst = InstanceName::from("u");
    let log = log_path(&board);
    // Direct-write KEEP+1 lines: above the trim target, below the trigger.
    let mut lines = String::new();
    for i in 1..=(COMPACTION_KEEP + 1) {
        let env = TaskEventEnvelope {
            schema_version: SCHEMA_VERSION,
            seq: i as u64,
            timestamp: format!("2026-06-21T{:02}:00:00Z", i % 24),
            instance: inst.clone(),
            emitter_id: None,
            event: TaskEvent::Unblocked {
                task_id: format!("t-{i}").as_str().into(),
            },
        };
        lines.push_str(&serde_json::to_string(&env).unwrap());
        lines.push('\n');
    }
    fs::write(&log, lines).unwrap();
    append(&home, &inst, sample_event("t-mid")).unwrap();
    let hot = fs::read_to_string(&log).unwrap().lines().count();
    assert_eq!(
        hot,
        COMPACTION_KEEP + 2,
        "between KEEP and HIGH_WATER the hot log must NOT be compacted (hysteresis); got {hot}"
    );
    let archives = fs::read_dir(archive_dir(&board))
        .map(|d| {
            d.flatten()
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
                .count()
        })
        .unwrap_or(0);
    assert_eq!(
        archives, 0,
        "no archive segment may be written below HIGH_WATER"
    );
    fs::remove_dir_all(&home).ok();
}

/// #2389 E1 hysteresis invariant — BOUNDED: across real appends that grow the
/// hot log past COMPACTION_HIGH_WATER it is trimmed back to KEEP and never
/// exceeds HIGH_WATER. Primes near the trigger with a cheap direct write, then
/// drives REAL appends across the boundary (exercising the append→gate wiring).
#[test]
#[serial]
fn hysteresis_steady_state_bounded() {
    let home = tmp_home("hyst-bounded");
    let board = board_root(&home, DEFAULT_PROJECT);
    let inst = InstanceName::from("u");
    let log = log_path(&board);
    // Prime to HIGH_WATER-1 (one below the trigger).
    let mut lines = String::new();
    for i in 1..=(COMPACTION_HIGH_WATER - 1) {
        let env = TaskEventEnvelope {
            schema_version: SCHEMA_VERSION,
            seq: i as u64,
            timestamp: format!("2026-06-21T{:02}:00:00Z", i % 24),
            instance: inst.clone(),
            emitter_id: None,
            event: TaskEvent::Unblocked {
                task_id: format!("t-{i}").as_str().into(),
            },
        };
        lines.push_str(&serde_json::to_string(&env).unwrap());
        lines.push('\n');
    }
    fs::write(&log, lines).unwrap();
    let hot = |b: &Path| fs::read_to_string(log_path(b)).unwrap().lines().count();
    // Append → HIGH_WATER (== trigger, not >) → no trim yet.
    append(&home, &inst, sample_event("t-x1")).unwrap();
    assert_eq!(
        hot(&board),
        COMPACTION_HIGH_WATER,
        "at HIGH_WATER: not yet trimmed"
    );
    // Append → HIGH_WATER+1 (> trigger) → trims to KEEP.
    append(&home, &inst, sample_event("t-x2")).unwrap();
    assert_eq!(
        hot(&board),
        COMPACTION_KEEP,
        "crossing HIGH_WATER must trim the hot log back to KEEP"
    );
    // Further appends stay bounded well under HIGH_WATER.
    for i in 0..5 {
        append(&home, &inst, sample_event(&format!("t-y{i}"))).unwrap();
        assert!(
            hot(&board) <= COMPACTION_HIGH_WATER,
            "hot log must stay bounded by HIGH_WATER"
        );
    }
    fs::remove_dir_all(&home).ok();
}

/// #2389 E1 hysteresis invariant — LOSSLESS: when a real append crosses
/// COMPACTION_HIGH_WATER and triggers compaction, replay still folds
/// archive+hot, so an early (now-archived) task AND a late (hot) task are both
/// reconstructed. Proves the append→gate→compact wiring archives, never drops.
#[test]
#[serial]
fn hysteresis_replay_lossless_across_compaction() {
    let home = tmp_home("hyst-lossless");
    let board = board_root(&home, DEFAULT_PROJECT);
    let inst = InstanceName::from("u");
    let log = log_path(&board);
    // A real early task — it will be archived when the crossing append trims.
    append(&home, &inst, sample_event("t-early")).unwrap();
    // Pad with filler (distinct instance) up to HIGH_WATER so the next real
    // append (HIGH_WATER+1) triggers a trim that archives the early slice.
    let existing = fs::read_to_string(&log).unwrap();
    let start = existing.lines().filter(|l| !l.trim().is_empty()).count() + 1;
    let mut lines = existing;
    for i in start..=COMPACTION_HIGH_WATER {
        let env = TaskEventEnvelope {
            schema_version: SCHEMA_VERSION,
            seq: i as u64,
            timestamp: format!("2026-06-21T{:02}:00:00Z", i % 24),
            instance: InstanceName::from("filler"),
            emitter_id: None,
            event: sample_event(&format!("f-{i}")),
        };
        lines.push_str(&serde_json::to_string(&env).unwrap());
        lines.push('\n');
    }
    fs::write(&log, lines).unwrap();
    // Crossing append (→ HIGH_WATER+1) triggers compaction to KEEP.
    append(&home, &inst, sample_event("t-late")).unwrap();
    assert_eq!(
        fs::read_to_string(&log).unwrap().lines().count(),
        COMPACTION_KEEP,
        "crossing append must compact to KEEP (so t-early is archived)"
    );
    let state = replay(&home).unwrap();
    assert!(
        state.tasks.contains_key(&TaskId::from("t-early")),
        "the archived early task must still replay (archive+hot fold = lossless)"
    );
    assert!(
        state.tasks.contains_key(&TaskId::from("t-late")),
        "the hot late task must replay"
    );
    fs::remove_dir_all(&home).ok();
}

/// #2389 E1 (reviewer item 5): the per-instance seq sidecar is a DERIVED
/// cache — a CORRUPT sidecar must rebuild from the full hot+archive scan, so
/// an instance whose events are all archived still gets a non-colliding seq
/// (else replay's `seq <= last_seen` idempotency SILENTLY DROPS the new
/// transition). Pins the rebuild-on-corrupt path end-to-end (item 5 was only
/// structurally read-confirmed, with no dedicated test).
#[test]
#[serial]
fn corrupt_sidecar_rebuilds_no_seq_collision() {
    let home = tmp_home("corrupt-sidecar");
    let board = board_root(&home, DEFAULT_PROJECT);
    let a = InstanceName::from("idle-agent");
    let b = InstanceName::from("busy-agent");
    let keep = 5;

    // A emits, B floods, compact → A's events archived out of the hot log.
    append(&home, &a, sample_event("t-A1")).unwrap();
    for i in 0..(keep + 2) {
        append(&home, &b, sample_event(&format!("t-B{i}"))).unwrap();
    }
    compact_at_with_keep(&board, keep).unwrap();
    assert_eq!(
        max_seq_for_instance(&log_path(&board), &a).unwrap().0,
        0,
        "precondition: A's events archived out of the hot log"
    );

    // CORRUPT the sidecar (valid after the appends above).
    let sidecar = seq_sidecar_path(&board);
    assert!(sidecar.exists(), "sidecar must exist after appends");
    fs::write(&sidecar, b"{ this is not valid json ]").unwrap();

    // A appends again — load_seq_highwater sees the corrupt file → rebuilds
    // from the hot+archive scan → A's high-water recovered → non-colliding seq.
    append(&home, &a, sample_event("t-A2")).unwrap();

    let state = replay(&home).unwrap();
    assert!(
        state.tasks.contains_key(&TaskId::from("t-A2")),
        "corrupt sidecar must rebuild → no seq collision → transition not dropped"
    );
    assert!(
        state.tasks.contains_key(&TaskId::from("t-A1")),
        "the archived original transition must still replay"
    );
    // The sidecar must be rebuilt to VALID JSON covering A's high-water.
    let rebuilt = fs::read_to_string(&sidecar).unwrap();
    let map: std::collections::BTreeMap<String, u64> =
        serde_json::from_str(&rebuilt).expect("sidecar must be rebuilt to valid JSON");
    assert!(
        map.get(a.as_str()).copied().unwrap_or(0) >= 2,
        "rebuilt sidecar must cover A's high-water (t-A1 then t-A2 ⇒ ≥2): {map:?}"
    );
    fs::remove_dir_all(&home).ok();
}

// (b) Compaction ARCHIVES (never drops): replay folds archive+hot, so the
// reconstructed state is identical before and after compaction.
#[test]
#[serial]
fn retention_replay_state_survives_compaction_zero_loss() {
    let home = tmp_home("retain-noloss");
    let board = board_root(&home, DEFAULT_PROJECT);
    let a = InstanceName::from("agent");
    let keep = 4;
    for i in 0..(keep + 4) {
        append(&home, &a, sample_event(&format!("t-{i}"))).unwrap();
    }
    let before = replay(&home).unwrap();
    compact_at_with_keep(&board, keep).unwrap();
    let after = replay(&home).unwrap();
    assert_eq!(
        before.tasks.keys().collect::<Vec<_>>(),
        after.tasks.keys().collect::<Vec<_>>(),
        "compaction archives (not drops) → replay state must be unchanged"
    );
    assert_eq!(
        before.tasks.len(),
        keep + 4,
        "all tasks present pre-compaction"
    );
    fs::remove_dir_all(&home).ok();
}

/// S1: `compact_at` rewrites the hot log and MUST invalidate the replay
/// cache (bump `REPLAY_GENERATION`), exactly like the append paths. Pre-fix
/// it was correct-by-accident (the shorter file changed the `(len, mtime)`
/// cache key); this asserts the explicit contract — the generation bump —
/// which a future cache-key change can't silently break. DISCRIMINATING:
/// without the added `invalidate_replay_cache()`, the generation is
/// unchanged across `compact_at` and this fails.
#[test]
#[serial]
fn compact_at_invalidates_replay_cache_s1() {
    let home = tmp_home("compact-invalidate");
    let inst = InstanceName::from("u");
    // Direct-write > COMPACTION_KEEP lines so compact_at actually rewrites
    // (mirrors compact_archives_older_than_keep_threshold).
    let log = home.join("task_events.jsonl");
    let mut lines = String::new();
    for i in 1..=(COMPACTION_KEEP + 5) {
        let env = TaskEventEnvelope {
            schema_version: SCHEMA_VERSION,
            seq: i as u64,
            timestamp: format!("2026-04-27T{:02}:00:00Z", i % 24),
            instance: inst.clone(),
            emitter_id: None,
            event: TaskEvent::Unblocked {
                task_id: format!("t-{i}").as_str().into(),
            },
        };
        lines.push_str(&serde_json::to_string(&env).unwrap());
        lines.push('\n');
    }
    fs::write(&log, lines).unwrap();
    // Warm the replay cache for this board.
    let _ = replay(&home).unwrap();
    let gen_before = REPLAY_GENERATION.load(Ordering::Acquire);
    compact(&home).unwrap();
    let gen_after = REPLAY_GENERATION.load(Ordering::Acquire);
    assert!(
        gen_after > gen_before,
        "compact_at must invalidate the replay cache (generation \
             {gen_before} → {gen_after}); without the explicit bump the stale \
             cache could outlive the compacted file"
    );
    // Sanity: compaction actually rewrote the hot log (kept exactly
    // COMPACTION_KEEP lines — so the generation bump above came from a real
    // rewrite, not a no-op) and a post-compact replay still succeeds.
    assert_eq!(
        fs::read_to_string(&log).unwrap().lines().count(),
        COMPACTION_KEEP,
        "compaction must shrink the hot log to COMPACTION_KEEP lines"
    );
    replay(&home).expect("post-compact replay must succeed");
    fs::remove_dir_all(&home).ok();
}

/// Replay-determinism invariant #1 — re-applying the same envelope
/// (e.g. duplicated line in a corrupted log) folds to identical state
/// as applying it once. Implemented via the `seq <= last_seen` skip
/// in [`TaskBoardState::apply`].
#[test]
fn invariant_1_idempotency() {
    let home = tmp_home("dedupe");
    let inst = InstanceName::from("u");
    append(&home, &inst, sample_event("t-D")).unwrap();
    // Manually duplicate the line to simulate a corrupted log.
    let log = home.join("task_events.jsonl");
    let content = fs::read_to_string(&log).unwrap();
    fs::write(&log, format!("{content}{content}")).unwrap();
    let state = replay(&home).unwrap();
    // Idempotency invariant: two copies of the same envelope fold to
    // identical state as one copy.
    assert_eq!(state.events_folded, 1);
    let task = state.tasks.get(&TaskId::from("t-D")).unwrap();
    assert_eq!(task.history.len(), 1);
    fs::remove_dir_all(&home).ok();
}

/// Replay-determinism invariant #2 — two readers fed the identical
/// log produce bit-identical state. Stronger than ordering: the test
/// JSON-serialises the entire fold and asserts equality, so any field
/// drift (timestamp, history shape, per-instance seq) surfaces.
#[test]
fn invariant_2_cross_process_determinism() {
    let home = tmp_home("xproc");
    let inst = InstanceName::from("u");
    let _ = append(&home, &inst, sample_event("t-1")).unwrap();
    let _ = append(&home, &inst, sample_event("t-2")).unwrap();
    let _ = append(
        &home,
        &inst,
        TaskEvent::Claimed {
            task_id: "t-1".into(),
            by: "u".into(),
        },
    )
    .unwrap();
    let s1 = replay(&home).unwrap();
    let s2 = replay(&home).unwrap();
    let j1 = serde_json::to_string(&s1).unwrap();
    let j2 = serde_json::to_string(&s2).unwrap();
    assert_eq!(j1, j2);
    fs::remove_dir_all(&home).ok();
}

/// Replay-determinism invariant #3 — back-compat: a v(N) reader
/// successfully parses v(N) envelopes (round-trip). After PR3 the
/// canonical writer emits v2 envelopes, so the round-trip path
/// alone exercises v2 → v2; this test additionally covers the
/// promised v2-reader-parses-v1-envelope contract.
#[test]
fn invariant_3_back_compat_v1_reader_parses_v1_envelope() {
    let home = tmp_home("backcompat");
    let inst = InstanceName::from("u");
    let _ = append(&home, &inst, sample_event("t-BC")).unwrap();
    let state = replay(&home).unwrap();
    assert!(state.tasks.contains_key(&TaskId::from("t-BC")));
    fs::remove_dir_all(&home).ok();
}

/// PR4 M2 (PR3 r1 dev-reviewer cross-vantage) — explicit v1 envelope
/// in a v2 reader's path. Hand-crafts the JSON line as a v1 emitter
/// would have written it (no `due_at` / `depends_on` / `routed_to` on
/// `Created`, `schema_version: 1`); asserts the v2 reader parses it
/// successfully via `#[serde(default)]` on the new fields. Defends
/// the silent migration regression — operator running a v2 binary
/// against an event log written entirely under v1 must observe state
/// identical to a v1 reader's view, not an error.
#[test]
fn invariant_3_v2_reader_parses_v1_envelope_explicit() {
    let home = tmp_home("v1_explicit");
    let log = home.join("task_events.jsonl");
    // Hand-crafted v1 line: `Created` without v2 fields, `schema_version: 1`.
    let v1_line = serde_json::json!({
        "schema_version": 1,
        "seq": 1,
        "timestamp": "2026-04-26T00:00:00Z",
        "instance": "v1-emitter",
        "event": {
            "kind": "Created",
            "task_id": "t-V1",
            "title": "v1-shaped task",
            "description": "no v2 fields",
            "priority": "normal",
            "owner": null
        }
    });
    fs::write(&log, format!("{v1_line}\n")).unwrap();
    let state = replay(&home).unwrap();
    let task = state
        .tasks
        .get(&TaskId::from("t-V1"))
        .expect("v2 reader must parse v1 Created envelope via serde defaults");
    assert_eq!(task.status, TaskStatus::Open);
    assert!(
        task.due_at.is_none(),
        "v1 envelope's missing due_at → None default"
    );
    assert!(
        task.depends_on.is_empty(),
        "v1 envelope's missing depends_on → empty default"
    );
    assert!(
        task.routed_to.is_none(),
        "v1 envelope's missing routed_to → None default"
    );
    fs::remove_dir_all(&home).ok();
}

/// Replay-determinism invariant #6 — replaying any prefix of the log
/// (events 1..N for every N) yields a valid state with predictable
/// status transitions. Future Phase 3 backfill emits dry-run snapshots
/// at arbitrary cursor points; this asserts every cursor is safe.
#[test]
fn invariant_6_snapshot_prefix_is_valid_state() {
    let home = tmp_home("prefix");
    let inst = InstanceName::from("u");
    append(&home, &inst, sample_event("t-P")).unwrap();
    append(
        &home,
        &inst,
        TaskEvent::Claimed {
            task_id: "t-P".into(),
            by: "u".into(),
        },
    )
    .unwrap();
    append(
        &home,
        &inst,
        TaskEvent::Done {
            task_id: "t-P".into(),
            by: "u".into(),
            source: DoneSource::OperatorManual {
                authored_at: chrono::Utc::now().to_rfc3339(),
                result: None,
            },
        },
    )
    .unwrap();

    let log = home.join("task_events.jsonl");
    let full = fs::read_to_string(&log).unwrap();
    let lines: Vec<&str> = full.lines().collect();
    assert_eq!(lines.len(), 3);

    for n in 1..=lines.len() {
        let prefix: String = lines[..n].iter().map(|l| format!("{l}\n")).collect();
        fs::write(&log, &prefix).unwrap();
        let state = replay(&home).unwrap_or_else(|_| panic!("prefix len {n} invalid"));
        assert_eq!(state.events_folded, n as u64);
        let task = state.tasks.get(&TaskId::from("t-P")).unwrap();
        let expected = match n {
            1 => TaskStatus::Open,
            2 => TaskStatus::Claimed,
            3 => TaskStatus::Done,
            _ => unreachable!(),
        };
        assert_eq!(task.status, expected, "prefix len {n}");
    }
    fs::remove_dir_all(&home).ok();
}

/// Replay-determinism invariant #7 — N readers on the same log all
/// observe identical state. Defends the "operator runs `task list`
/// and `task get` concurrently with daemon's MCP handler" workflow.
#[test]
fn invariant_7_concurrent_reader_coherence() {
    use std::sync::Arc;
    let home = Arc::new(tmp_home("concurrent"));
    let inst = InstanceName::from("u");
    for i in 1..=20 {
        append(&home, &inst, sample_event(&format!("t-{i}"))).unwrap();
    }
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let h = Arc::clone(&home);
            std::thread::spawn(move || serde_json::to_string(&replay(&h).unwrap()).unwrap())
        })
        .collect();
    let results: Vec<String> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    let first = &results[0];
    for r in &results[1..] {
        assert_eq!(r, first, "concurrent readers must observe identical state");
    }
    fs::remove_dir_all(&*home).ok();
}

/// F2 (PR1 r2) — mixed-timezone envelopes must sort chronologically,
/// not lexically. `+09:00` carries an earlier absolute instant than
/// `Z`, even though the lexical comparison (`+` ≈ 0x2B vs `Z` ≈ 0x5A)
/// goes the other way. This test pins the regression: if the sort
/// ever reverts to string compare, the asserted Done-status flips.
#[test]
fn replay_sorts_chronologically_across_timezone_offsets() {
    let home = tmp_home("tz");
    let log = home.join("task_events.jsonl");
    // Event A: 2026-04-27T01:00:00+09:00 == 2026-04-26T16:00:00Z
    // Event B: 2026-04-27T00:00:00Z
    // Chronological: A precedes B by 8 hours.
    // Lexical (broken): "2026-04-27T00..." < "2026-04-27T01..." → B before A.
    let env_a = TaskEventEnvelope {
        schema_version: 1,
        seq: 1,
        timestamp: "2026-04-27T01:00:00+09:00".into(),
        instance: InstanceName::from("u"),
        emitter_id: None,
        event: sample_event("t-TZ"),
    };
    let env_b = TaskEventEnvelope {
        schema_version: 1,
        seq: 2,
        timestamp: "2026-04-27T00:00:00Z".into(),
        instance: InstanceName::from("u"),
        emitter_id: None,
        event: TaskEvent::Done {
            task_id: "t-TZ".into(),
            by: "u".into(),
            source: DoneSource::OperatorManual {
                authored_at: "2026-04-27T00:00:00Z".into(),
                result: None,
            },
        },
    };
    // Write in the wrong (lexical) order — replay must still produce
    // the chronologically-correct fold (Created → Done).
    let mut content = String::new();
    content.push_str(&serde_json::to_string(&env_b).unwrap());
    content.push('\n');
    content.push_str(&serde_json::to_string(&env_a).unwrap());
    content.push('\n');
    fs::write(&log, content).unwrap();

    let state = replay(&home).unwrap();
    let task = state.tasks.get(&TaskId::from("t-TZ")).unwrap();
    assert_eq!(
        task.status,
        TaskStatus::Done,
        "chronological sort: Created (16:00Z) precedes Done (00:00Z next-UTC-day) → final Done"
    );
    fs::remove_dir_all(&home).ok();
}

#[test]
fn replay_ordering_is_deterministic() {
    let home = tmp_home("order");
    let log = home.join("task_events.jsonl");
    // Hand-shuffle order on disk; replay must sort by (timestamp,
    // instance, seq) and produce a stable fold regardless.
    let envs = [
        TaskEventEnvelope {
            schema_version: 1,
            seq: 2,
            timestamp: "2026-04-27T00:00:02Z".into(),
            instance: InstanceName::from("u"),
            emitter_id: None,
            event: TaskEvent::Claimed {
                task_id: "t-O".into(),
                by: "u".into(),
            },
        },
        TaskEventEnvelope {
            schema_version: 1,
            seq: 1,
            timestamp: "2026-04-27T00:00:01Z".into(),
            instance: InstanceName::from("u"),
            emitter_id: None,
            event: sample_event("t-O"),
        },
    ];
    let mut content = String::new();
    for e in &envs {
        content.push_str(&serde_json::to_string(e).unwrap());
        content.push('\n');
    }
    fs::write(&log, content).unwrap();
    let state = replay(&home).unwrap();
    let task = state.tasks.get(&TaskId::from("t-O")).unwrap();
    // Created applied before Claimed → final status Claimed.
    assert_eq!(task.status, TaskStatus::Claimed);
    fs::remove_dir_all(&home).ok();
}

// ── Sprint 24 P0 PR2 — F1 (deferred from PR1 r2) + Invariant #5 ──

/// Build a fresh `(home, instance)` test fixture seeded with a single
/// `Created` event so subsequent transitions have a task to mutate.
fn fixture_with_seeded_task(tag: &str) -> (PathBuf, InstanceName, TaskId) {
    let home = tmp_home(tag);
    let inst = InstanceName::from("u");
    let tid = TaskId::from("t-FIX");
    append(
        &home,
        &inst,
        TaskEvent::Created {
            task_id: tid.clone(),
            title: "fixture".into(),
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
        },
    )
    .unwrap();
    (home, inst, tid)
}

/// **F1 (PR1 r2 deferred to PR2)** — exhaustive state-machine table.
/// 7 statuses × 10 events = 70 cells; not all are interesting (Created
/// on an existing task is a documented no-op via `or_insert_with`),
/// but the test covers every cell explicitly so a future apply()
/// regression on any status × event pair fails-loud.
#[test]
fn state_machine_exhaustive_transitions() {
    // Helper: prime task into a target status by sequencing prior
    // events. Returns (home, instance, task_id) post-priming.
    fn prime(target: TaskStatus, tag: &str) -> (PathBuf, InstanceName, TaskId) {
        let (home, inst, tid) = fixture_with_seeded_task(tag);
        let priming: Vec<TaskEvent> = match target {
            TaskStatus::Open => vec![],
            TaskStatus::Claimed => vec![TaskEvent::Claimed {
                task_id: tid.clone(),
                by: inst.clone(),
            }],
            TaskStatus::InProgress => vec![TaskEvent::InProgress {
                task_id: tid.clone(),
                by: inst.clone(),
            }],
            TaskStatus::Verified => vec![TaskEvent::Verified {
                task_id: tid.clone(),
                by_reviewer: inst.clone(),
                verdict: "verified".into(),
            }],
            TaskStatus::Done => vec![TaskEvent::Done {
                task_id: tid.clone(),
                by: inst.clone(),
                source: DoneSource::OperatorManual {
                    authored_at: chrono::Utc::now().to_rfc3339(),
                    result: None,
                },
            }],
            TaskStatus::Cancelled => vec![TaskEvent::Cancelled {
                task_id: tid.clone(),
                by: inst.clone(),
                reason: "test".into(),
            }],
            TaskStatus::Blocked => vec![TaskEvent::Blocked {
                task_id: tid.clone(),
                reason: "test".into(),
            }],
            TaskStatus::Backlog => vec![TaskEvent::MovedToBacklog {
                task_id: tid.clone(),
            }],
            TaskStatus::InReview => vec![TaskEvent::MovedToReview {
                task_id: tid.clone(),
            }],
        };
        for e in priming {
            append(&home, &inst, e).unwrap();
        }
        (home, inst, tid)
    }

    // Helper: emit one event for the candidate transition.
    fn emit(home: &Path, inst: &InstanceName, tid: &TaskId, kind: &str) {
        let event = match kind {
            // Created on an existing task is a documented no-op via
            // `entry().or_insert_with` — applying it doesn't mutate
            // status. We still exercise the path here to pin the
            // invariant.
            "Created" => TaskEvent::Created {
                task_id: tid.clone(),
                title: "dup".into(),
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
            },
            "Claimed" => TaskEvent::Claimed {
                task_id: tid.clone(),
                by: inst.clone(),
            },
            "InProgress" => TaskEvent::InProgress {
                task_id: tid.clone(),
                by: inst.clone(),
            },
            "Verified" => TaskEvent::Verified {
                task_id: tid.clone(),
                by_reviewer: inst.clone(),
                verdict: "v".into(),
            },
            "Done" => TaskEvent::Done {
                task_id: tid.clone(),
                by: inst.clone(),
                source: DoneSource::OperatorManual {
                    authored_at: chrono::Utc::now().to_rfc3339(),
                    result: None,
                },
            },
            "Cancelled" => TaskEvent::Cancelled {
                task_id: tid.clone(),
                by: inst.clone(),
                reason: "t".into(),
            },
            "Linked" => TaskEvent::Linked {
                task_id: tid.clone(),
                pr_id: PrId(1),
                source: LinkSource::Explicit {
                    authored_at: chrono::Utc::now().to_rfc3339(),
                },
                snapshot: PrSnapshot {
                    pr_state: "merged".into(),
                    merge_sha: Some("aaaa".into()),
                    api_response_hash: "h".into(),
                    captured_at: chrono::Utc::now().to_rfc3339(),
                },
            },
            "Blocked" => TaskEvent::Blocked {
                task_id: tid.clone(),
                reason: "t".into(),
            },
            "Unblocked" => TaskEvent::Unblocked {
                task_id: tid.clone(),
            },
            "Reopened" => TaskEvent::Reopened {
                task_id: tid.clone(),
                reason: "t".into(),
                source_evidence: "t".into(),
            },
            "Released" => TaskEvent::Released {
                task_id: tid.clone(),
                reason: "t".into(),
            },
            _ => unreachable!(),
        };
        append(home, inst, event).unwrap();
    }

    // Full 7×10 expectation table. Each row asserts the post-event
    // status. Created/Linked never change status. Unblocked only
    // moves Blocked → Open. Reopened always normalises to Open.
    // Other events overwrite to their own target status (replay-side
    // is permissive per F3 contract).
    let table: &[(TaskStatus, &str, TaskStatus)] = &[
        // (current_status, event_kind, expected_next_status)
        (TaskStatus::Open, "Created", TaskStatus::Open),
        (TaskStatus::Open, "Claimed", TaskStatus::Claimed),
        (TaskStatus::Open, "InProgress", TaskStatus::InProgress),
        (TaskStatus::Open, "Verified", TaskStatus::Verified),
        (TaskStatus::Open, "Done", TaskStatus::Done),
        (TaskStatus::Open, "Cancelled", TaskStatus::Cancelled),
        (TaskStatus::Open, "Linked", TaskStatus::Open),
        (TaskStatus::Open, "Blocked", TaskStatus::Blocked),
        (TaskStatus::Open, "Unblocked", TaskStatus::Open),
        (TaskStatus::Open, "Reopened", TaskStatus::Open),
        (TaskStatus::Claimed, "Created", TaskStatus::Claimed),
        (TaskStatus::Claimed, "Claimed", TaskStatus::Claimed),
        (TaskStatus::Claimed, "InProgress", TaskStatus::InProgress),
        (TaskStatus::Claimed, "Verified", TaskStatus::Verified),
        (TaskStatus::Claimed, "Done", TaskStatus::Done),
        (TaskStatus::Claimed, "Cancelled", TaskStatus::Cancelled),
        (TaskStatus::Claimed, "Linked", TaskStatus::Claimed),
        (TaskStatus::Claimed, "Blocked", TaskStatus::Blocked),
        (TaskStatus::Claimed, "Unblocked", TaskStatus::Claimed),
        (TaskStatus::Claimed, "Reopened", TaskStatus::Open),
        (TaskStatus::InProgress, "Created", TaskStatus::InProgress),
        (TaskStatus::InProgress, "Claimed", TaskStatus::Claimed),
        (TaskStatus::InProgress, "InProgress", TaskStatus::InProgress),
        (TaskStatus::InProgress, "Verified", TaskStatus::Verified),
        (TaskStatus::InProgress, "Done", TaskStatus::Done),
        (TaskStatus::InProgress, "Cancelled", TaskStatus::Cancelled),
        (TaskStatus::InProgress, "Linked", TaskStatus::InProgress),
        (TaskStatus::InProgress, "Blocked", TaskStatus::Blocked),
        (TaskStatus::InProgress, "Unblocked", TaskStatus::InProgress),
        (TaskStatus::InProgress, "Reopened", TaskStatus::Open),
        (TaskStatus::Verified, "Created", TaskStatus::Verified),
        (TaskStatus::Verified, "Claimed", TaskStatus::Claimed),
        (TaskStatus::Verified, "InProgress", TaskStatus::InProgress),
        (TaskStatus::Verified, "Verified", TaskStatus::Verified),
        (TaskStatus::Verified, "Done", TaskStatus::Done),
        (TaskStatus::Verified, "Cancelled", TaskStatus::Cancelled),
        (TaskStatus::Verified, "Linked", TaskStatus::Verified),
        (TaskStatus::Verified, "Blocked", TaskStatus::Blocked),
        (TaskStatus::Verified, "Unblocked", TaskStatus::Verified),
        (TaskStatus::Verified, "Reopened", TaskStatus::Open),
        (TaskStatus::Done, "Created", TaskStatus::Done),
        (TaskStatus::Done, "Claimed", TaskStatus::Claimed),
        (TaskStatus::Done, "InProgress", TaskStatus::InProgress),
        (TaskStatus::Done, "Verified", TaskStatus::Verified),
        (TaskStatus::Done, "Done", TaskStatus::Done),
        (TaskStatus::Done, "Cancelled", TaskStatus::Cancelled),
        (TaskStatus::Done, "Linked", TaskStatus::Done),
        (TaskStatus::Done, "Blocked", TaskStatus::Blocked),
        (TaskStatus::Done, "Unblocked", TaskStatus::Done),
        (TaskStatus::Done, "Reopened", TaskStatus::Open),
        (TaskStatus::Cancelled, "Created", TaskStatus::Cancelled),
        (TaskStatus::Cancelled, "Claimed", TaskStatus::Claimed),
        (TaskStatus::Cancelled, "InProgress", TaskStatus::InProgress),
        (TaskStatus::Cancelled, "Verified", TaskStatus::Verified),
        (TaskStatus::Cancelled, "Done", TaskStatus::Done),
        (TaskStatus::Cancelled, "Cancelled", TaskStatus::Cancelled),
        (TaskStatus::Cancelled, "Linked", TaskStatus::Cancelled),
        (TaskStatus::Cancelled, "Blocked", TaskStatus::Blocked),
        (TaskStatus::Cancelled, "Unblocked", TaskStatus::Cancelled),
        (TaskStatus::Cancelled, "Reopened", TaskStatus::Open),
        (TaskStatus::Blocked, "Created", TaskStatus::Blocked),
        (TaskStatus::Blocked, "Claimed", TaskStatus::Claimed),
        (TaskStatus::Blocked, "InProgress", TaskStatus::InProgress),
        (TaskStatus::Blocked, "Verified", TaskStatus::Verified),
        (TaskStatus::Blocked, "Done", TaskStatus::Done),
        (TaskStatus::Blocked, "Cancelled", TaskStatus::Cancelled),
        (TaskStatus::Blocked, "Linked", TaskStatus::Blocked),
        (TaskStatus::Blocked, "Blocked", TaskStatus::Blocked),
        (TaskStatus::Blocked, "Unblocked", TaskStatus::Open),
        (TaskStatus::Blocked, "Reopened", TaskStatus::Open),
        // PR4 F3 (PR3 r1 reviewer-2 MEDIUM) — Released variant rows.
        // Released always normalises to Open (clears owner; distinct
        // from Reopened which preserves owner). Adding the 7 rows
        // closes the F1 7×10 → 7×11 expansion gap.
        (TaskStatus::Open, "Released", TaskStatus::Open),
        (TaskStatus::Claimed, "Released", TaskStatus::Open),
        (TaskStatus::InProgress, "Released", TaskStatus::Open),
        (TaskStatus::Verified, "Released", TaskStatus::Open),
        (TaskStatus::Done, "Released", TaskStatus::Open),
        (TaskStatus::Cancelled, "Released", TaskStatus::Open),
        (TaskStatus::Blocked, "Released", TaskStatus::Open),
    ];

    for (i, (start, evt, expected)) in table.iter().enumerate() {
        let (home, inst, tid) = prime(*start, &format!("sm_{i}"));
        emit(&home, &inst, &tid, evt);
        let state = replay(&home).unwrap();
        let actual = state.tasks.get(&tid).unwrap().status;
        assert_eq!(
            actual, *expected,
            "({:?}, {}) expected → {:?}, got {:?}",
            start, evt, expected, actual
        );
        fs::remove_dir_all(&home).ok();
    }
}

/// **Replay-determinism invariant #5 (PR1 r2 deferred to PR2)** —
/// sweep-replay associativity: applying sweep events on top of an
/// existing log produces the same fold as inserting them anywhere
/// chronologically before the rest. Defends the future "sweep
/// daemon emits Linked/Done events while the operator emits manual
/// transitions" interleaving.
#[test]
fn invariant_5_sweep_replay_associativity() {
    let inst = InstanceName::from("u");
    let sweep = InstanceName::from("system:task_sweep");

    // Scenario A: replay(operator events) then add sweep events on top.
    let home_a = tmp_home("assoc_a");
    append(
        &home_a,
        &inst,
        TaskEvent::Created {
            task_id: TaskId::from("t-S1"),
            title: "s1".into(),
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
        },
    )
    .unwrap();
    append(
        &home_a,
        &inst,
        TaskEvent::Claimed {
            task_id: TaskId::from("t-S1"),
            by: inst.clone(),
        },
    )
    .unwrap();
    // Sweep emits Linked + Done on top.
    append(
        &home_a,
        &sweep,
        TaskEvent::Linked {
            task_id: TaskId::from("t-S1"),
            pr_id: PrId(42),
            source: LinkSource::SweepDiscovery {
                sweep_id: "sw1".into(),
            },
            snapshot: PrSnapshot {
                pr_state: "merged".into(),
                merge_sha: Some("abc".into()),
                api_response_hash: "h".into(),
                captured_at: chrono::Utc::now().to_rfc3339(),
            },
        },
    )
    .unwrap();
    append(
        &home_a,
        &sweep,
        TaskEvent::Done {
            task_id: TaskId::from("t-S1"),
            by: inst.clone(),
            source: DoneSource::PrMerged {
                pr_id: PrId(42),
                merge_sha: "abc".into(),
                merged_at: chrono::Utc::now().to_rfc3339(),
                snapshot: PrSnapshot {
                    pr_state: "merged".into(),
                    merge_sha: Some("abc".into()),
                    api_response_hash: "h".into(),
                    captured_at: chrono::Utc::now().to_rfc3339(),
                },
            },
        },
    )
    .unwrap();
    let state_a = replay(&home_a).unwrap();

    // Scenario B: same events but interleaved differently — sweep
    // events appear in the middle, not at the end. Replay's
    // chronological+seq sort canonicalises ordering, so the fold
    // result must match scenario A.
    let home_b = tmp_home("assoc_b");
    let log_b = home_b.join("task_events.jsonl");
    // Hand-craft the envelope sequence in a different file order:
    let envs = vec![
        TaskEventEnvelope {
            schema_version: SCHEMA_VERSION,
            seq: 1,
            timestamp: "2026-04-27T00:00:01Z".into(),
            instance: inst.clone(),
            emitter_id: None,
            event: TaskEvent::Created {
                task_id: TaskId::from("t-S1"),
                title: "s1".into(),
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
            },
        },
        // Sweep Linked appears BEFORE operator Claimed in file order
        // but with later timestamp — replay sort still applies it
        // after Claimed because the sort key is timestamp.
        TaskEventEnvelope {
            schema_version: SCHEMA_VERSION,
            seq: 1,
            timestamp: "2026-04-27T00:00:03Z".into(),
            instance: sweep.clone(),
            emitter_id: None,
            event: TaskEvent::Linked {
                task_id: TaskId::from("t-S1"),
                pr_id: PrId(42),
                source: LinkSource::SweepDiscovery {
                    sweep_id: "sw1".into(),
                },
                snapshot: PrSnapshot {
                    pr_state: "merged".into(),
                    merge_sha: Some("abc".into()),
                    api_response_hash: "h".into(),
                    captured_at: "2026-04-27T00:00:03Z".into(),
                },
            },
        },
        TaskEventEnvelope {
            schema_version: SCHEMA_VERSION,
            seq: 2,
            timestamp: "2026-04-27T00:00:02Z".into(),
            instance: inst.clone(),
            emitter_id: None,
            event: TaskEvent::Claimed {
                task_id: TaskId::from("t-S1"),
                by: inst.clone(),
            },
        },
        TaskEventEnvelope {
            schema_version: SCHEMA_VERSION,
            seq: 2,
            timestamp: "2026-04-27T00:00:04Z".into(),
            instance: sweep.clone(),
            emitter_id: None,
            event: TaskEvent::Done {
                task_id: TaskId::from("t-S1"),
                by: inst.clone(),
                source: DoneSource::PrMerged {
                    pr_id: PrId(42),
                    merge_sha: "abc".into(),
                    merged_at: "2026-04-27T00:00:04Z".into(),
                    snapshot: PrSnapshot {
                        pr_state: "merged".into(),
                        merge_sha: Some("abc".into()),
                        api_response_hash: "h".into(),
                        captured_at: "2026-04-27T00:00:04Z".into(),
                    },
                },
            },
        },
    ];
    let mut content = String::new();
    for e in &envs {
        content.push_str(&serde_json::to_string(e).unwrap());
        content.push('\n');
    }
    fs::write(&log_b, content).unwrap();
    let state_b = replay(&home_b).unwrap();

    // Final task status & linked PRs identical regardless of file
    // order. Histories may differ in absolute timestamps but the
    // ordered status transition is the invariant.
    assert_eq!(
        state_a.tasks.get(&TaskId::from("t-S1")).unwrap().status,
        state_b.tasks.get(&TaskId::from("t-S1")).unwrap().status,
        "associativity: operator-then-sweep == interleaved"
    );
    assert_eq!(
        state_a.tasks.get(&TaskId::from("t-S1")).unwrap().linked_prs,
        state_b.tasks.get(&TaskId::from("t-S1")).unwrap().linked_prs
    );
    fs::remove_dir_all(&home_a).ok();
    fs::remove_dir_all(&home_b).ok();
}

// ── Sprint 46 P3: audit trail round-trip tests ──────────────────

#[test]
fn emitter_id_round_trips_through_serde() {
    let env = TaskEventEnvelope {
        schema_version: SCHEMA_VERSION,
        seq: 1,
        timestamp: "2026-05-04T00:00:00Z".into(),
        instance: InstanceName::from("dev"),
        emitter_id: Some("a1b2c3d4-e5f6-7890-abcd-ef1234567890".into()),
        event: sample_event("t-rt"),
    };
    let json = serde_json::to_string(&env).expect("serialize");
    assert!(json.contains("a1b2c3d4"));
    let deser: TaskEventEnvelope = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(
        deser.emitter_id.as_deref(),
        Some("a1b2c3d4-e5f6-7890-abcd-ef1234567890")
    );
}

#[test]
fn emitter_id_none_omitted_from_json() {
    let env = TaskEventEnvelope {
        schema_version: SCHEMA_VERSION,
        seq: 1,
        timestamp: "2026-05-04T00:00:00Z".into(),
        instance: InstanceName::from("dev"),
        emitter_id: None,
        event: sample_event("t-rt2"),
    };
    let json = serde_json::to_string(&env).expect("serialize");
    assert!(
        !json.contains("emitter_id"),
        "None emitter_id must be omitted"
    );
    let deser: TaskEventEnvelope = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(deser.emitter_id, None);
}

// ── Sprint 55 P0-C — bind opt-out flag schema tests ─────────────

#[test]
fn created_event_v1_envelope_default_bind_none() {
    // Pre-P0-C envelopes have no `bind` field; serde must default to
    // None so existing tasks.json migrations + any v1 log replay
    // continue to work exactly as before.
    let v1_json = r#"{
            "kind": "Created",
            "task_id": "t-v1",
            "title": "v1 task",
            "description": "",
            "priority": "normal",
            "owner": null,
            "due_at": null,
            "depends_on": [],
            "routed_to": null,
            "branch": null
        }"#;
    let event: TaskEvent = serde_json::from_str(v1_json).expect("v1 envelope must deserialize");
    match event {
        TaskEvent::Created { bind, .. } => assert_eq!(bind, None),
        _ => panic!("expected Created variant"),
    }
}

#[test]
fn created_event_round_trips_bind_some_false_through_replay() {
    // Append a Created event with bind=Some(false), replay the log,
    // and verify TaskRecord.bind preserves the opt-out signal end to
    // end (event log → apply → in-memory record).
    let home = tmp_home("p0c_bind_round_trip");
    let inst = InstanceName::from("dev");
    append(
        &home,
        &inst,
        TaskEvent::Created {
            task_id: TaskId::from("t-rca"),
            title: "rca task".into(),
            description: String::new(),
            priority: "normal".into(),
            owner: None,
            due_at: None,
            depends_on: Vec::new(),
            routed_to: None,
            branch: None,
            bind: Some(false),
            eta_secs: None,
            tags: vec![],
            parent_id: None,
        },
    )
    .unwrap();
    let state = replay(&home).expect("replay");
    let task = state
        .tasks
        .get(&TaskId::from("t-rca"))
        .expect("task in state");
    assert_eq!(task.bind, Some(false));
    std::fs::remove_dir_all(&home).ok();
}

// ─────────────────────────────────────────────────────────────
// Sprint 59 Wave 1 PR-1 (#9 task stall watchdog) — schema field
// tests pinning eta_secs round-trip + dispatched_at semantics.
// ─────────────────────────────────────────────────────────────

#[test]
fn task_schema_dispatched_at_set_on_status_in_progress_transition() {
    // Lead spec name: dispatched_at must be auto-set the FIRST
    // time the task transitions to in_progress.
    let home = tmp_home("schema-dispatched-at");
    let inst = InstanceName::from("test");
    let tid = TaskId::from("t-disp");
    append(
        &home,
        &inst,
        TaskEvent::Created {
            task_id: tid.clone(),
            title: "x".into(),
            description: String::new(),
            priority: "normal".into(),
            owner: None,
            due_at: None,
            depends_on: Vec::new(),
            routed_to: None,
            branch: None,
            bind: None,
            eta_secs: Some(60),
            tags: vec![],
            parent_id: None,
        },
    )
    .unwrap();
    // Pre-claim: dispatched_at is None.
    let pre = replay(&home).unwrap();
    let pre_t = pre.tasks.get(&tid).unwrap();
    assert!(pre_t.started_at.is_none(), "pre-claim: no dispatched_at");

    append(
        &home,
        &inst,
        TaskEvent::Claimed {
            task_id: tid.clone(),
            by: inst.clone(),
        },
    )
    .unwrap();
    // Post-claim, pre-in_progress: still None.
    let mid = replay(&home).unwrap();
    assert!(mid.tasks.get(&tid).unwrap().started_at.is_none());

    append(
        &home,
        &inst,
        TaskEvent::InProgress {
            task_id: tid.clone(),
            by: inst.clone(),
        },
    )
    .unwrap();
    // Post-in_progress: dispatched_at is set.
    let post = replay(&home).unwrap();
    let post_t = post.tasks.get(&tid).unwrap();
    assert!(
        post_t.started_at.is_some(),
        "in_progress must set dispatched_at: {post_t:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn task_schema_dispatched_at_idempotent_on_subsequent_in_progress() {
    // Defensive: a Released → Claimed → InProgress cycle must
    // NOT overwrite the original dispatched_at — anti-stall
    // scanner cares about "when did work first start", not the
    // latest checkpoint.
    let home = tmp_home("schema-disp-idem");
    let inst = InstanceName::from("test");
    let tid = TaskId::from("t-disp-idem");
    append(
        &home,
        &inst,
        TaskEvent::Created {
            task_id: tid.clone(),
            title: "x".into(),
            description: String::new(),
            priority: "normal".into(),
            owner: None,
            due_at: None,
            depends_on: Vec::new(),
            routed_to: None,
            branch: None,
            bind: None,
            eta_secs: Some(60),
            tags: vec![],
            parent_id: None,
        },
    )
    .unwrap();
    append(
        &home,
        &inst,
        TaskEvent::Claimed {
            task_id: tid.clone(),
            by: inst.clone(),
        },
    )
    .unwrap();
    append(
        &home,
        &inst,
        TaskEvent::InProgress {
            task_id: tid.clone(),
            by: inst.clone(),
        },
    )
    .unwrap();
    let first_dispatched = replay(&home)
        .unwrap()
        .tasks
        .get(&tid)
        .unwrap()
        .started_at
        .clone();
    assert!(first_dispatched.is_some());

    // Release → Claim → InProgress again. dispatched_at must
    // remain unchanged.
    append(
        &home,
        &inst,
        TaskEvent::Released {
            task_id: tid.clone(),
            reason: "test".into(),
        },
    )
    .unwrap();
    append(
        &home,
        &inst,
        TaskEvent::Claimed {
            task_id: tid.clone(),
            by: inst.clone(),
        },
    )
    .unwrap();
    // Sleep briefly so a hypothetical overwrite would surface
    // as a different timestamp.
    std::thread::sleep(std::time::Duration::from_millis(50));
    append(
        &home,
        &inst,
        TaskEvent::InProgress {
            task_id: tid.clone(),
            by: inst.clone(),
        },
    )
    .unwrap();
    let second_dispatched = replay(&home)
        .unwrap()
        .tasks
        .get(&tid)
        .unwrap()
        .started_at
        .clone();
    assert_eq!(
        first_dispatched, second_dispatched,
        "dispatched_at must NOT be overwritten on re-entry"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn task_schema_eta_secs_round_trips_from_created_event() {
    // Defensive: eta_secs supplied at Created event must
    // surface on TaskRecord post-replay.
    let home = tmp_home("schema-eta-rt");
    let inst = InstanceName::from("test");
    let tid = TaskId::from("t-eta-rt");
    append(
        &home,
        &inst,
        TaskEvent::Created {
            task_id: tid.clone(),
            title: "x".into(),
            description: String::new(),
            priority: "normal".into(),
            owner: None,
            due_at: None,
            depends_on: Vec::new(),
            routed_to: None,
            branch: None,
            bind: None,
            eta_secs: Some(7200),
            tags: vec![],
            parent_id: None,
        },
    )
    .unwrap();
    let task = replay(&home).unwrap().tasks.get(&tid).cloned().unwrap();
    assert_eq!(task.eta_secs, Some(7200), "eta_secs must round-trip");
    std::fs::remove_dir_all(&home).ok();
}

/// H10: after compaction atomically replaces the hot file, the next append
/// must still produce the correct monotonic seq and replay must fold both
/// events. `max_seq_for_instance` always re-scans the on-disk file, so the
/// replace is observed with no stale high-water mark.
#[test]
fn append_after_compaction_produces_correct_monotonic_seq() {
    let home = tmp_home("compact-seq");
    let inst = InstanceName::from("dev");

    // Append events past the compaction threshold
    let total = COMPACTION_KEEP + 10;
    let log = home.join("task_events.jsonl");
    let mut lines = String::new();
    for i in 1..=total {
        let env = TaskEventEnvelope {
            schema_version: SCHEMA_VERSION,
            seq: i as u64,
            timestamp: format!("2026-05-24T{:02}:{:02}:00Z", (i / 60) % 24, i % 60),
            instance: inst.clone(),
            emitter_id: None,
            event: TaskEvent::Unblocked {
                task_id: format!("t-{i}").as_str().into(),
            },
        };
        lines.push_str(&serde_json::to_string(&env).unwrap());
        lines.push('\n');
    }
    fs::write(&log, &lines).unwrap();

    let seq_before = append(&home, &inst, sample_event("t-pre-compact")).unwrap();
    assert_eq!(seq_before, total as u64 + 1);

    // Compact — atomically rewrites the hot file (keeps the latest events).
    compact(&home).unwrap();

    // Post-compaction append must still produce correct monotonic seq
    let seq_after = append(&home, &inst, sample_event("t-post-compact")).unwrap();
    assert_eq!(
        seq_after,
        seq_before + 1,
        "post-compaction seq must be monotonically next"
    );

    // Replay sees both events
    let state = replay(&home).unwrap();
    assert!(state.tasks.contains_key(&TaskId::from("t-pre-compact")));
    assert!(state.tasks.contains_key(&TaskId::from("t-post-compact")));
    fs::remove_dir_all(&home).ok();
}

// ── parent_id tree structure tests ──────────────────────────────

#[test]
fn parent_id_round_trips_through_replay() {
    let home = tmp_home("parent-rt");
    let inst = InstanceName::from("dev");
    append(
        &home,
        &inst,
        TaskEvent::Created {
            task_id: TaskId::from("t-parent"),
            title: "parent".into(),
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
        },
    )
    .unwrap();
    append(
        &home,
        &inst,
        TaskEvent::Created {
            task_id: TaskId::from("t-child"),
            title: "child".into(),
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
            parent_id: Some(TaskId::from("t-parent")),
        },
    )
    .unwrap();
    let state = replay(&home).unwrap();
    let parent = state.tasks.get(&TaskId::from("t-parent")).unwrap();
    assert_eq!(parent.parent_id, None);
    let child = state.tasks.get(&TaskId::from("t-child")).unwrap();
    assert_eq!(child.parent_id, Some(TaskId::from("t-parent")));
    fs::remove_dir_all(&home).ok();
}

#[test]
fn parent_id_v1_envelope_defaults_to_none() {
    let home = tmp_home("parent-v1");
    let log = home.join("task_events.jsonl");
    let v1_line = serde_json::json!({
        "schema_version": 1,
        "seq": 1,
        "timestamp": "2026-05-25T00:00:00Z",
        "instance": "v1-emitter",
        "event": {
            "kind": "Created",
            "task_id": "t-old",
            "title": "old task",
            "description": "",
            "priority": "normal",
            "owner": null
        }
    });
    fs::write(&log, format!("{v1_line}\n")).unwrap();
    let state = replay(&home).unwrap();
    let task = state.tasks.get(&TaskId::from("t-old")).unwrap();
    assert_eq!(task.parent_id, None, "v1 envelope missing parent_id → None");
    fs::remove_dir_all(&home).ok();
}

#[test]
fn cascade_cancel_cancels_open_and_claimed_children() {
    let home = tmp_home("cascade");
    let inst = InstanceName::from("dev");
    append(
        &home,
        &inst,
        TaskEvent::Created {
            task_id: TaskId::from("t-root"),
            title: "root".into(),
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
        },
    )
    .unwrap();
    append(
        &home,
        &inst,
        TaskEvent::Created {
            task_id: TaskId::from("t-child-open"),
            title: "open child".into(),
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
            parent_id: Some(TaskId::from("t-root")),
        },
    )
    .unwrap();
    append(
        &home,
        &inst,
        TaskEvent::Created {
            task_id: TaskId::from("t-child-claimed"),
            title: "claimed child".into(),
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
            parent_id: Some(TaskId::from("t-root")),
        },
    )
    .unwrap();
    append(
        &home,
        &inst,
        TaskEvent::Claimed {
            task_id: TaskId::from("t-child-claimed"),
            by: inst.clone(),
        },
    )
    .unwrap();
    append(
        &home,
        &inst,
        TaskEvent::Created {
            task_id: TaskId::from("t-child-done"),
            title: "done child".into(),
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
            parent_id: Some(TaskId::from("t-root")),
        },
    )
    .unwrap();
    append(
        &home,
        &inst,
        TaskEvent::Done {
            task_id: TaskId::from("t-child-done"),
            by: inst.clone(),
            source: DoneSource::OperatorManual {
                authored_at: chrono::Utc::now().to_rfc3339(),
                result: None,
            },
        },
    )
    .unwrap();
    append(
        &home,
        &inst,
        TaskEvent::Created {
            task_id: TaskId::from("t-child-inprog"),
            title: "in-progress child".into(),
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
            parent_id: Some(TaskId::from("t-root")),
        },
    )
    .unwrap();
    append(
        &home,
        &inst,
        TaskEvent::InProgress {
            task_id: TaskId::from("t-child-inprog"),
            by: inst.clone(),
        },
    )
    .unwrap();
    append(
        &home,
        &inst,
        TaskEvent::Created {
            task_id: TaskId::from("t-unrelated"),
            title: "unrelated".into(),
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
        },
    )
    .unwrap();

    // Cancel the parent
    append(
        &home,
        &inst,
        TaskEvent::Cancelled {
            task_id: TaskId::from("t-root"),
            by: inst.clone(),
            reason: "test cascade".into(),
        },
    )
    .unwrap();

    let state = replay(&home).unwrap();
    assert_eq!(
        state.tasks.get(&TaskId::from("t-root")).unwrap().status,
        TaskStatus::Cancelled
    );
    assert_eq!(
        state
            .tasks
            .get(&TaskId::from("t-child-open"))
            .unwrap()
            .status,
        TaskStatus::Cancelled,
        "open child must be cascade-cancelled"
    );
    assert_eq!(
        state
            .tasks
            .get(&TaskId::from("t-child-claimed"))
            .unwrap()
            .status,
        TaskStatus::Cancelled,
        "claimed child must be cascade-cancelled"
    );
    assert_eq!(
        state
            .tasks
            .get(&TaskId::from("t-child-done"))
            .unwrap()
            .status,
        TaskStatus::Done,
        "done child must NOT be cascade-cancelled"
    );
    assert_eq!(
        state
            .tasks
            .get(&TaskId::from("t-child-inprog"))
            .unwrap()
            .status,
        TaskStatus::InProgress,
        "in-progress child must NOT be cascade-cancelled"
    );
    assert_eq!(
        state
            .tasks
            .get(&TaskId::from("t-unrelated"))
            .unwrap()
            .status,
        TaskStatus::Open,
        "unrelated task must NOT be affected"
    );
    fs::remove_dir_all(&home).ok();
}

// ── Perf Group 1A: replay cache tests ──────────────────────────

#[test]
fn board_root_default_is_home_real_project_is_subtree() {
    // #2117 P0 seam: the default/fleet/empty project maps to `home` itself
    // (this is what makes the whole refactor byte-identical), while a real
    // project id resolves to its own isolated subtree under `home/boards/`.
    let home = tmp_home("board-root");
    assert_eq!(board_root(&home, DEFAULT_PROJECT), home);
    assert_eq!(board_root(&home, "fleet"), home);
    assert_eq!(board_root(&home, ""), home);

    let proj = board_root(&home, "owner/repo");
    assert_ne!(
        proj, home,
        "a real project must not collide with the home board"
    );
    assert!(proj.starts_with(home.join("boards")));
    let slug = proj.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        !slug.contains('/'),
        "slug must be filesystem-safe, got {slug:?}"
    );
    // Deterministic / round-trippable: same id → same root; distinct ids → distinct roots.
    assert_eq!(board_root(&home, "owner/repo"), proj);
    assert_ne!(board_root(&home, "other/repo"), proj);
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn replay_cache_hit_returns_same_result() {
    let home = tmp_home("cache-hit");
    let inst = InstanceName::from("a");
    append(&home, &inst, sample_event("t-1")).unwrap();

    let r1 = replay(&home).unwrap();
    let r2 = replay(&home).unwrap();
    assert_eq!(r1.tasks.len(), r2.tasks.len());
    assert_eq!(r1.events_folded, r2.events_folded);
    fs::remove_dir_all(&home).ok();
}

#[test]
fn replay_cache_invalidated_after_append() {
    let home = tmp_home("cache-inv");
    let inst = InstanceName::from("a");
    append(&home, &inst, sample_event("t-1")).unwrap();

    let r1 = replay(&home).unwrap();
    assert_eq!(r1.tasks.len(), 1);

    append(&home, &inst, sample_event("t-2")).unwrap();

    let r2 = replay(&home).unwrap();
    assert_eq!(r2.tasks.len(), 2, "cache must be invalidated after append");
    fs::remove_dir_all(&home).ok();
}

// ── Perf Group 1B: sort_envelopes Schwartzian consistency ──────

#[test]
fn sort_envelopes_schwartzian_matches_naive() {
    let mk = |ts: &str, inst: &str, seq: u64| TaskEventEnvelope {
        schema_version: SCHEMA_VERSION,
        seq,
        timestamp: ts.to_string(),
        instance: InstanceName::from(inst),
        emitter_id: None,
        event: sample_event("t-sort"),
    };
    let mut envelopes = vec![
        mk("2026-05-26T03:00:00Z", "b", 2),
        mk("2026-05-26T01:00:00Z", "a", 1),
        mk("2026-05-26T02:00:00Z", "a", 1),
        mk("2026-05-26T02:00:00Z", "b", 1),
        mk("2026-05-26T02:00:00Z", "a", 2),
    ];
    sort_envelopes(&mut envelopes);
    let order: Vec<(&str, &str, u64)> = envelopes
        .iter()
        .map(|e| (e.timestamp.as_str(), e.instance.0.as_str(), e.seq))
        .collect();
    assert_eq!(
        order,
        vec![
            ("2026-05-26T01:00:00Z", "a", 1),
            ("2026-05-26T02:00:00Z", "a", 1),
            ("2026-05-26T02:00:00Z", "a", 2),
            ("2026-05-26T02:00:00Z", "b", 1),
            ("2026-05-26T03:00:00Z", "b", 2),
        ]
    );
}

#[test]
fn sort_envelopes_reverse_and_interleaved() {
    let mk = |ts: &str, inst: &str, seq: u64| TaskEventEnvelope {
        schema_version: SCHEMA_VERSION,
        seq,
        timestamp: ts.to_string(),
        instance: InstanceName::from(inst),
        emitter_id: None,
        event: sample_event("t-perm"),
    };
    // Reverse order input
    let mut rev = vec![
        mk("2026-05-26T04:00:00Z", "z", 3),
        mk("2026-05-26T03:00:00Z", "y", 2),
        mk("2026-05-26T02:00:00Z", "x", 1),
        mk("2026-05-26T01:00:00Z", "w", 1),
    ];
    sort_envelopes(&mut rev);
    let ts: Vec<&str> = rev.iter().map(|e| e.timestamp.as_str()).collect();
    assert_eq!(
        ts,
        vec![
            "2026-05-26T01:00:00Z",
            "2026-05-26T02:00:00Z",
            "2026-05-26T03:00:00Z",
            "2026-05-26T04:00:00Z",
        ]
    );

    // Interleaved: same timestamp, different instances and seqs
    let mut interleaved = vec![
        mk("2026-05-26T01:00:00Z", "c", 2),
        mk("2026-05-26T01:00:00Z", "a", 3),
        mk("2026-05-26T01:00:00Z", "b", 1),
        mk("2026-05-26T01:00:00Z", "a", 1),
        mk("2026-05-26T01:00:00Z", "a", 2),
        mk("2026-05-26T01:00:00Z", "c", 1),
    ];
    sort_envelopes(&mut interleaved);
    let order: Vec<(&str, u64)> = interleaved
        .iter()
        .map(|e| (e.instance.0.as_str(), e.seq))
        .collect();
    assert_eq!(
        order,
        vec![("a", 1), ("a", 2), ("a", 3), ("b", 1), ("c", 1), ("c", 2)]
    );
}

/// #2760 (codex ruling m-…-1154): `TaskId::parse_canonical` is the single typed
/// authority for "is this string a real task id" — anchored full-string, accepting
/// `t-<ts>-<seq>` (legacy 2-segment) and `t-<ts>-<pid>-<seq>` (3-segment), all
/// segments NUMERIC. Rejects synthetic / query / partial correlations. Pins both
/// grammar sides + the `FromStr` alias, so the suppression/authority gates
/// (dispatch-idle, auto-close) that key off it cannot silently drift.
#[test]
fn task_id_parse_canonical_accepts_canonical_rejects_non_task() {
    // Accept: 3-segment generated + 2-segment legacy.
    for ok in [
        "t-20260101000000000000-1-1",
        "t-1942-1",
        "t-0-0-0",
        "t-2498-70517",
    ] {
        assert_eq!(
            TaskId::parse_canonical(ok).map(|t| t.0),
            Some(ok.to_string()),
            "'{ok}' is a canonical task id and must parse"
        );
        assert!(ok.parse::<TaskId>().is_ok(), "FromStr must accept '{ok}'");
    }
    // Reject: synthetic / query / partial / non-numeric / substring / empty.
    for bad in [
        "t-fires",
        "t-cancel",
        "t-1942-ir",
        "corr-query-7",
        "real-id",
        "t-",
        "t-1",
        "t-1-",
        "",
        " t-1-1",
        "t-1-1 ",
        "xt-1-1",
        "t-1-1-2-3",
    ] {
        assert_eq!(
            TaskId::parse_canonical(bad),
            None,
            "'{bad}' is NOT a canonical task id and must be rejected (non-task → gates fail-open)"
        );
        assert!(
            bad.parse::<TaskId>().is_err(),
            "FromStr must reject '{bad}'"
        );
    }
}
