use super::*;
use std::sync::Mutex;
pub(super) struct TempHome(std::path::PathBuf);
impl TempHome {
    pub(super) fn new() -> Self {
        Self(home())
    }
    pub(super) fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct Fake {
    starts: Mutex<u32>,
    sends: Mutex<u32>,
    stops: Mutex<u32>,
    limited: Mutex<bool>,
    refuse_stop: Mutex<bool>,
}
impl JobRuntime for Fake {
    fn start(&self, _: &Run, a: &Attempt) -> anyhow::Result<String> {
        *self.starts.lock().unwrap() += 1;
        Ok(format!("uuid-{}", a.number))
    }
    fn observe(&self, _: &Attempt) -> anyhow::Result<Observation> {
        Ok(if *self.limited.lock().unwrap() {
            Observation::UsageLimited
        } else {
            Observation::Running
        })
    }
    fn stop(&self, _: &Attempt) -> anyhow::Result<bool> {
        *self.stops.lock().unwrap() += 1;
        Ok(!*self.refuse_stop.lock().unwrap())
    }
    fn dispatch(&self, _: &Run, _: &Attempt) -> anyhow::Result<()> {
        *self.sends.lock().unwrap() += 1;
        Ok(())
    }
}
fn home() -> std::path::PathBuf {
    let h = std::env::temp_dir().join(format!("job-tests-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&h).unwrap();
    std::fs::write(h.join("fleet.yaml"), "instances: {}\n").unwrap();
    h
}
fn schedule(home: &Path) -> crate::schedules::Schedule {
    serde_json::from_value(serde_json::json!({"id":"schedule-test","target":"","message":"summarize","created_at":"2026-01-01T00:00:00Z","timezone":"UTC","trigger":{"kind":"cron","expr":"* * * * *"},"job":{"backends":["codex","claude"],"artifact_directory":home.join("artifacts"),"retry_delay_secs":1}})).unwrap()
}
fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-09-09T10:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}
#[test]
fn admission_replays_once_and_overlap_advances_own_watermark() {
    let h = home();
    let s = schedule(&h);
    admit_due(&h, &s, now()).unwrap();
    admit_due(&h, &s, now()).unwrap();
    assert_eq!(read(&h).unwrap().runs.len(), 1);
    admit_due(&h, &s, now() + chrono::Duration::minutes(1)).unwrap();
    admit_due(&h, &s, now() + chrono::Duration::minutes(1)).unwrap();
    let st = read(&h).unwrap();
    assert_eq!(st.overlap_skips[&s.id], 1);
    assert_eq!(st.runs.len(), 1);
    std::fs::remove_dir_all(h).unwrap();
}
#[test]
fn failed_admission_keeps_occurrence_recoverable_and_corruption_fails_closed() {
    let h = home();
    let s = schedule(&h);
    crate::store::fail_next_atomic_write_for_test(&state_path(&h));
    assert!(admit_due(&h, &s, now()).is_err());
    admit_due(&h, &s, now()).unwrap();
    assert_eq!(read(&h).unwrap().runs.len(), 1);
    std::fs::write(state_path(&h), b"broken").unwrap();
    assert!(admit_due(&h, &s, now()).is_err());
    assert!(owns_worker(&h, "some-worker"));
    assert_eq!(std::fs::read(state_path(&h)).unwrap(), b"broken");
    std::fs::remove_dir_all(h).unwrap();
}
#[test]
fn concurrent_admission_creates_one_run() {
    let h = home();
    let s = schedule(&h);
    std::thread::scope(|scope| {
        for _ in 0..12 {
            let h = &h;
            let s = &s;
            scope.spawn(move || admit_due(h, s, now()).unwrap());
        }
    });
    assert_eq!(read(&h).unwrap().runs.len(), 1);
    std::fs::remove_dir_all(h).unwrap();
}
#[test]
fn receipt_is_fenced_and_survives_restart_before_projection() {
    let h = home();
    admit_due(&h, &schedule(&h), now()).unwrap();
    let mut r = read(&h).unwrap().runs[0].clone();
    let old = r.clone();
    r.phase = Phase::Running;
    r.attempt = Some(Attempt {
        number: 1,
        name: "worker".into(),
        uuid: Some("uuid".into()),
        backend: "codex".into(),
        started_at: 0,
    });
    replace(&h, &old, r.clone()).unwrap();
    assert!(complete_as(&h, &r.id, 2, "worker", "uuid", "done").is_err());
    assert!(complete_as(&h, &r.id, 1, "worker", "other", "done").is_err());
    complete_as(&h, &r.id, 1, "worker", "uuid", "done").unwrap();
    let receipt = read(&h).unwrap().runs[0].clone();
    assert_eq!(receipt.phase, Phase::Succeeded);
    assert!(receipt.cleanup_pending);
    assert!(!receipt.task_settled);
    assert!(!replace(&h, &old, r.clone()).unwrap());
    complete_as(&h, &r.id, 1, "worker", "uuid", "done").unwrap();
    std::fs::remove_dir_all(h).unwrap();
}
#[test]
fn refused_exit_blocks_fallback_and_completion() {
    let h = home();
    admit_due(&h, &schedule(&h), now()).unwrap();
    let old = read(&h).unwrap().runs[0].clone();
    let mut r = old.clone();
    r.phase = Phase::Stopping;
    r.attempt = Some(Attempt {
        number: 1,
        name: "worker".into(),
        uuid: Some("uuid".into()),
        backend: "codex".into(),
        started_at: 0,
    });
    replace(&h, &old, r.clone()).unwrap();
    let rt = Fake::default();
    *rt.refuse_stop.lock().unwrap() = true;
    tick(&h, &rt, now().timestamp()).unwrap();
    tick(&h, &rt, now().timestamp()).unwrap();
    assert_eq!(read(&h).unwrap().runs[0].phase, Phase::Stopping);
    assert_eq!(*rt.starts.lock().unwrap(), 0);
    assert!(complete_as(&h, &r.id, 1, "worker", "uuid", "late").is_err());
    *rt.refuse_stop.lock().unwrap() = false;
    tick(&h, &rt, now().timestamp()).unwrap();
    assert_eq!(read(&h).unwrap().runs[0].phase, Phase::Waiting);
    std::fs::remove_dir_all(h).unwrap();
}

fn advance_to_running(h: &Path, rt: &Fake, clock: i64) {
    tick(h, rt, clock).unwrap();
    let r = read(h).unwrap().runs[0].clone();
    assert_eq!(r.phase, Phase::Starting, "{:?}", r.error);
    let name = &r.attempt.as_ref().unwrap().name;
    crate::fleet::add_instance_to_yaml(
        h,
        name,
        &crate::fleet::InstanceYamlEntry {
            backend: Some("codex".into()),
            created_by: Some("system:schedule_job".into()),
            ..Default::default()
        },
    )
    .unwrap();
    tick(h, rt, clock).unwrap();
    assert_eq!(read(h).unwrap().runs[0].phase, Phase::Dispatching);
    tick(h, rt, clock).unwrap();
    assert_eq!(read(h).unwrap().runs[0].phase, Phase::Running);
}
#[test]
fn full_task_flow_fallback_and_completion_cleanup_are_restart_idempotent() {
    let h = home();
    let rt = Fake::default();
    let clock = now().timestamp();
    admit_due(&h, &schedule(&h), now()).unwrap();
    advance_to_running(&h, &rt, clock);
    *rt.limited.lock().unwrap() = true;
    tick(&h, &rt, clock).unwrap();
    assert_eq!(read(&h).unwrap().runs[0].phase, Phase::Stopping);
    tick(&h, &rt, clock).unwrap();
    *rt.limited.lock().unwrap() = false;
    advance_to_running(&h, &rt, clock + 2);
    let r = read(&h).unwrap().runs[0].clone();
    let a = r.attempt.as_ref().unwrap();
    assert_eq!(a.backend, "claude");
    complete_as(
        &h,
        &r.id,
        u64::from(a.number),
        &a.name,
        a.uuid.as_deref().unwrap(),
        "delivered",
    )
    .unwrap();
    // Model a crash after the actual task Done append but before Run projection.
    let done = crate::tasks::handle(
        &h,
        "system:schedule_job",
        &serde_json::json!({"action":"done","id":r.task_id,"result":"delivered"}),
    );
    assert!(done.get("error").is_none(), "{done}");
    *rt.refuse_stop.lock().unwrap() = true;
    tick(&h, &rt, clock + 2).unwrap();
    let r = read(&h).unwrap().runs[0].clone();
    assert!(r.task_settled);
    assert!(r.cleanup_pending);
    *rt.refuse_stop.lock().unwrap() = false;
    tick(&h, &rt, clock + 3).unwrap();
    let r = read(&h).unwrap().runs[0].clone();
    assert!(!r.cleanup_pending);
    assert_eq!(r.phase, Phase::Succeeded);
    assert_eq!(*rt.starts.lock().unwrap(), 2);
    std::fs::remove_dir_all(h).unwrap();
}

#[test]
fn real_cron_entry_admits_jobs_even_with_future_legacy_cursor() {
    let h = home();
    let s = schedule(&h);
    crate::store::save_atomic(
        &h.join("schedules.json"),
        &serde_json::json!({"schema_version":2,"schedules":[s]}),
    )
    .unwrap();
    std::fs::write(h.join(".schedule_last_check"), "2099-01-01T00:00:00Z").unwrap();
    crate::store::fail_next_atomic_write_for_test(&state_path(&h));
    crate::daemon::cron_tick::check_schedules(&h);
    assert!(read(&h).unwrap().runs.is_empty());
    crate::daemon::cron_tick::check_schedules(&h);
    assert_eq!(read(&h).unwrap().runs.len(), 1);
    crate::daemon::cron_tick::check_schedules(&h);
    assert_eq!(read(&h).unwrap().runs.len(), 1);
    std::fs::remove_dir_all(h).unwrap();
}
#[test]
fn lost_notification_receipt_becomes_unknown_without_resending() {
    let h = home();
    admit_due(&h, &schedule(&h), now()).unwrap();
    let old = read(&h).unwrap().runs[0].clone();
    let mut next = old.clone();
    next.phase = Phase::Succeeded;
    next.notification = NotificationState::Pending;
    replace(&h, &old, next).unwrap();
    let run = read(&h).unwrap().runs[0].clone();
    let sent = std::sync::atomic::AtomicU32::new(0);
    assert!(controller::reconcile_notification(&h, &run, |_| {
        sent.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::store::fail_next_atomic_write_for_test(&state_path(&h));
        Ok("message-1".into())
    })
    .is_err());
    let run = read(&h).unwrap().runs[0].clone();
    assert_eq!(run.notification, NotificationState::Sending);
    controller::reconcile_notification(&h, &run, |_| panic!("must not resend")).unwrap();
    let run = read(&h).unwrap().runs[0].clone();
    assert_eq!(run.notification, NotificationState::Unknown);
    controller::reconcile_notification(&h, &run, |_| panic!("must not resend")).unwrap();
    assert_eq!(sent.load(std::sync::atomic::Ordering::SeqCst), 1);
    std::fs::remove_dir_all(h).unwrap();
}

#[test]
fn subsecond_one_shot_is_not_early_or_dropped() {
    let h = home();
    let mut s = schedule(&h);
    s.created_at = "2026-09-09T10:00:00.100Z".into();
    s.trigger = crate::schedules::Trigger::Once {
        at: "2026-09-09T10:00:00.500Z".into(),
    };
    admit_due(&h, &s, now() + chrono::Duration::milliseconds(200)).unwrap();
    assert!(read(&h).unwrap().runs.is_empty());
    admit_due(&h, &s, now() + chrono::Duration::milliseconds(600)).unwrap();
    assert_eq!(read(&h).unwrap().runs.len(), 1);
    std::fs::remove_dir_all(h).unwrap();
}

#[test]
fn completion_and_stop_race_has_one_winner() {
    struct RacingRuntime(std::sync::Barrier);
    impl JobRuntime for RacingRuntime {
        fn start(&self, _: &Run, _: &Attempt) -> anyhow::Result<String> {
            panic!("race must not spawn a replacement")
        }
        fn observe(&self, _: &Attempt) -> anyhow::Result<Observation> {
            self.0.wait();
            Ok(Observation::UsageLimited)
        }
        fn stop(&self, _: &Attempt) -> anyhow::Result<bool> {
            panic!("this tick must only decide whether to enter Stopping")
        }
        fn dispatch(&self, _: &Run, _: &Attempt) -> anyhow::Result<()> {
            panic!("running attempt must not redispatch")
        }
    }
    for _ in 0..12 {
        let h = home();
        admit_due(&h, &schedule(&h), now()).unwrap();
        let old = read(&h).unwrap().runs[0].clone();
        let uuid = crate::types::InstanceId::new().full();
        std::fs::write(
            h.join("fleet.yaml"),
            format!("instances:\n  worker:\n    id: {uuid}\n    backend: codex\n"),
        )
        .unwrap();
        let mut active = old.clone();
        active.phase = Phase::Running;
        active.attempt = Some(Attempt {
            number: 1,
            name: "worker".into(),
            uuid: Some(uuid),
            backend: "codex".into(),
            started_at: now().timestamp(),
        });
        replace(&h, &old, active).unwrap();
        let rt = RacingRuntime(std::sync::Barrier::new(2));
        // The actual controller has read Running before releasing the API
        // caller. This races its Stopping CAS against authenticated receipt
        // resolution, rather than hand-feeding a replacement state.
        let completed = std::thread::scope(|scope| {
            let driver = scope.spawn(|| tick(&h, &rt, now().timestamp()).unwrap());
            rt.0.wait();
            let result = complete(
                &h,
                "worker",
                &serde_json::json!({
                    "run_id":old.id,"attempt_id":1,"result":"done"
                }),
            );
            driver.join().unwrap();
            result.get("error").is_none()
        });
        assert_eq!(
            read(&h).unwrap().runs[0].phase,
            if completed {
                Phase::Succeeded
            } else {
                Phase::Stopping
            }
        );
        std::fs::remove_dir_all(h).unwrap();
    }
}

#[test]
fn every_second_cron_admits_latest_due_at_exact_and_fractional_ticks() {
    // Exercise the same admission entry cron_tick calls, with deterministic
    // clock samples. A fractional upper bound must not choose a future second
    // and then discard it instead of admitting the latest due occurrence.
    for offset_ms in [0, 1, 500, 999] {
        let h = home();
        let mut s = schedule(&h);
        s.trigger = crate::schedules::Trigger::Cron {
            expr: "* * * * * *".into(),
        };
        let tick_at = now() + chrono::Duration::milliseconds(offset_ms);
        admit_due(&h, &s, tick_at).unwrap();
        let state = read(&h).unwrap();
        assert_eq!(state.runs.len(), 1, "missing occurrence at {tick_at}");
        assert_eq!(state.runs[0].scheduled_at, now().timestamp_millis());
        assert_eq!(state.watermarks[&s.id], now().timestamp_millis());
        // Repeated scans of this fraction of the second are idempotent.
        admit_due(&h, &s, tick_at).unwrap();
        assert_eq!(read(&h).unwrap().runs.len(), 1);
        std::fs::remove_dir_all(h).unwrap();
    }
}

#[test]
fn conservative_recovery_requires_creator_confirmation_and_never_retries() {
    struct Unproven;
    impl JobRuntime for Unproven {
        fn start(&self, _: &Run, _: &Attempt) -> anyhow::Result<String> {
            panic!("no retry")
        }
        fn observe(&self, _: &Attempt) -> anyhow::Result<Observation> {
            panic!("frozen")
        }
        fn dispatch(&self, _: &Run, _: &Attempt) -> anyhow::Result<()> {
            panic!("no redispatch")
        }
        fn stop(&self, _: &Attempt) -> anyhow::Result<bool> {
            anyhow::bail!("recovery_required: no containment")
        }
    }
    for succeeded in [false, true] {
        let h = TempHome::new();
        let h = h.path();
        admit_due(h, &schedule(h), now()).unwrap();
        advance_to_running(h, &Fake::default(), now().timestamp());
        let old = read(h).unwrap().runs[0].clone();
        let mut pending = old.clone();
        pending.phase = if succeeded {
            Phase::Succeeded
        } else {
            Phase::Stopping
        };
        pending.result = succeeded.then(|| "already delivered".into());
        pending.cleanup_pending = succeeded;
        replace(h, &old, pending).unwrap();
        tick(h, &Unproven, now().timestamp()).unwrap();
        let run = read(h).unwrap().runs[0].clone();
        assert!(run.recovery_required);
        tick(h, &Unproven, now().timestamp() + 99999).unwrap();
        let attempt = run.attempt.as_ref().unwrap();
        let args = serde_json::json!({"run_id":run.id,"attempt_id":attempt.number,
            "cleanup_confirmed":true,"result":"all tools stopped; delivery checked"});
        assert!(resolve_recovery(h, &attempt.name, &args)
            .get("error")
            .is_some());
        assert!(resolve_recovery(h, "other", &args).get("error").is_some());
        assert!(
            resolve_recovery(h, &run.created_by, &args)
                .get("error")
                .is_some(),
            "worker still exists"
        );
        crate::fleet::remove_instance_from_yaml(h, &attempt.name).unwrap();
        let mut unconfirmed = args.clone();
        unconfirmed["cleanup_confirmed"] = false.into();
        assert!(resolve_recovery(h, &run.created_by, &unconfirmed)
            .get("error")
            .is_some());
        let mut stale = args.clone();
        stale["attempt_id"] = 999.into();
        assert!(resolve_recovery(h, &run.created_by, &stale)
            .get("error")
            .is_some());
        let resolved = resolve_recovery(h, &run.created_by, &args);
        assert!(resolved.get("error").is_none(), "{resolved}");
        tick(h, &Unproven, now().timestamp() + 99999).unwrap();
        let resolved = read(h).unwrap().runs[0].clone();
        assert!(!resolved.active());
        assert_eq!(
            resolved.phase,
            if succeeded {
                Phase::Succeeded
            } else {
                Phase::Failed
            }
        );
        assert_eq!(resolved.result, run.result);
        assert_eq!(resolved.attempt, run.attempt);
        assert!(resolve_recovery(h, &run.created_by, &args)
            .get("error")
            .is_some());
    }
}
