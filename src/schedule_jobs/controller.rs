use super::{read, replace, Attempt, Phase, Run};
use super::{JobRuntime, Observation};
use std::path::Path;

/// Single driver across tick threads/processes. Receipt writers only acquire
/// the short state lock. Lock order: driver -> state (released before runtime,
/// fleet, task or transport calls); no runtime service calls back under state.
pub(crate) fn tick(home: &Path, runtime: &impl JobRuntime, now: i64) -> anyhow::Result<()> {
    std::fs::create_dir_all(home)?;
    let Some(_driver) =
        crate::store::try_acquire_file_lock(&home.join("schedule-jobs-driver.lock"))?
    else {
        return Ok(());
    };
    for run in read(home)?.runs {
        if let Err(error) = step(home, runtime, &run, now) {
            tracing::error!(run_id = %run.id, %error, "job reconciliation failed; durable state retained");
            if let Some(current) = read(home)?.runs.into_iter().find(|r| r.id == run.id) {
                let mut updated = current.clone();
                updated.recovery_required |= error.to_string().contains("recovery_required");
                updated.error = Some(error.to_string());
                if updated != current {
                    replace(home, &current, updated)?;
                }
            }
        }
    }
    Ok(())
}

fn task_ok(value: serde_json::Value) -> anyhow::Result<()> {
    if let Some(error) = value.get("error") {
        anyhow::bail!("task projection: {error}")
    }
    Ok(())
}
fn ensure_task(home: &Path, run: &Run) -> anyhow::Result<()> {
    let id = run
        .task_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("missing run task identity"))?;
    match crate::tasks::load_routed(home, id) {
        Ok(_) => Ok(()),
        Err(crate::tasks::TaskRouteError::NotFound) => task_ok(crate::tasks::create_schedule_task(
            home,
            id,
            &serde_json::json!({
                "title": format!("Scheduled job {}", run.schedule_id),
                "description": format!("Run {}. Completion authority is schedule complete; task state alone does not complete this execution.\n{}",run.id,run.message),
                "tags":["schedule-job",run.id], "project":"default", "bind":false
            }),
        )),
        Err(e) => Err(anyhow::anyhow!("task route unavailable: {e}")),
    }
}
fn settle_task(home: &Path, run: &Run) -> anyhow::Result<()> {
    ensure_task(home, run)?;
    let id = run
        .task_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("missing task id"))?;
    let task = crate::tasks::load_routed(home, id).map_err(|e| anyhow::anyhow!("{e}"))?;
    let expected = if run.phase == Phase::Succeeded {
        crate::task_events::TaskStatus::Done
    } else {
        crate::task_events::TaskStatus::Cancelled
    };
    if task.record().status == expected {
        return Ok(());
    }
    let args = if run.phase == Phase::Succeeded {
        serde_json::json!({"action":"done","id":id,"result":run.result})
    } else {
        serde_json::json!({"action":"update","id":id,"status":"cancelled","result":run.error})
    };
    task_ok(crate::tasks::handle(home, "system:schedule_job", &args))
}
fn step(home: &Path, runtime: &impl JobRuntime, run: &Run, now: i64) -> anyhow::Result<()> {
    let mut next = run.clone();
    if run.recovery_required {
        reconcile_notification(home, run, |r| super::notification::send(home, r))?;
        return Ok(());
    }
    if matches!(run.phase, Phase::Succeeded | Phase::Failed) {
        if !run.task_settled {
            match settle_task(home, run) {
                Ok(()) => next.task_settled = true,
                Err(error) => next.error = Some(error.to_string()),
            }
        }
        if run.cleanup_pending {
            let stopped = match &run.attempt {
                Some(attempt) => runtime.stop(attempt),
                None => Ok(true),
            };
            match stopped {
                Ok(true) => next.cleanup_pending = false,
                Ok(false) => {}
                Err(error) => {
                    next.recovery_required |= error.to_string().contains("recovery_required");
                    next.error = Some(format!("cleanup pending: {error}"));
                }
            }
        }
        if next != *run {
            replace(home, run, next)?;
        }
        if let Some(current) = read(home)?.runs.into_iter().find(|r| r.id == run.id) {
            reconcile_notification(home, &current, |r| super::notification::send(home, r))?;
        }
        return Ok(());
    }
    if now >= run.deadline && run.phase != Phase::Stopping {
        next.error = Some("execution deadline exceeded".into());
        next.phase = if run.attempt.is_some() {
            Phase::Stopping
        } else {
            Phase::Failed
        };
        replace(home, run, next)?;
        return Ok(());
    }
    match run.phase {
        Phase::Queued | Phase::Waiting => {
            if now < run.next_attempt_at {
                return Ok(());
            }
            ensure_task(home, run)?;
            let number = run.previous_attempts.len() as u32 + 1;
            anyhow::ensure!(
                number <= run.config.max_attempts,
                "attempt budget exhausted"
            );
            next.attempt = Some(Attempt {
                number,
                name: format!("job-{}-{number}", &run.id[2..18]),
                uuid: None,
                backend: run.config.backends[(number as usize - 1) % run.config.backends.len()]
                    .clone(),
                started_at: now,
            });
            next.phase = Phase::Starting;
            replace(home, run, next)?;
        }
        Phase::Starting => {
            let attempt = run
                .attempt
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("starting without attempt"))?;
            match runtime.start(run, attempt) {
                Ok(uuid) => {
                    if let Some(a) = next.attempt.as_mut() {
                        a.uuid = Some(uuid);
                    }
                    next.phase = Phase::Dispatching;
                }
                Err(error) => {
                    next.phase = Phase::Stopping;
                    next.error = Some(format!("spawn failed: {error}"));
                }
            }
            replace(home, run, next)?;
        }
        Phase::Dispatching => {
            let attempt = run
                .attempt
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("dispatch without attempt"))?;
            match runtime.observe(attempt)? {
                Observation::Starting => return Ok(()),
                Observation::UsageLimited | Observation::Exited | Observation::Missing => {
                    next.phase = Phase::Stopping;
                    next.error = Some("worker unavailable before dispatch".into());
                }
                Observation::Running => {
                    prepare_task(home, run, attempt)?;
                    match runtime.dispatch(run, attempt) {
                        Ok(()) => next.phase = Phase::Running,
                        Err(e) => {
                            next.phase = Phase::Stopping;
                            next.error = Some(format!("dispatch failed: {e}"));
                        }
                    }
                }
            }
            replace(home, run, next)?;
        }
        Phase::Running => {
            let attempt = run
                .attempt
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("running without attempt"))?;
            let reason = match runtime.observe(attempt)? {
                Observation::UsageLimited => Some("backend usage limit"),
                Observation::Exited | Observation::Missing => {
                    Some("worker exited without completion receipt")
                }
                Observation::Starting | Observation::Running => None,
            };
            if let Some(reason) = reason {
                next.phase = Phase::Stopping;
                next.error = Some(reason.into());
                replace(home, run, next)?;
            }
        }
        Phase::Stopping => {
            let attempt = run
                .attempt
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("stopping without attempt"))?;
            if !runtime.stop(attempt)? {
                return Ok(());
            }
            next.previous_attempts.push(attempt.clone());
            next.attempt = None;
            if now >= run.deadline || attempt.number >= run.config.max_attempts {
                next.phase = Phase::Failed;
            } else {
                next.phase = Phase::Waiting;
                next.next_attempt_at = now.saturating_add(run.config.retry_delay_secs as i64);
            }
            replace(home, run, next)?;
        }
        Phase::Succeeded | Phase::Failed => unreachable!(),
    }
    Ok(())
}

fn prepare_task(home: &Path, run: &Run, attempt: &Attempt) -> anyhow::Result<()> {
    use crate::task_events::TaskStatus;
    let id = run
        .task_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("missing task id"))?;
    let task = crate::tasks::load_routed(home, id).map_err(|e| anyhow::anyhow!("{e}"))?;
    task_ok(crate::tasks::handle(
        home,
        "system:schedule_job",
        &serde_json::json!({
            "action":"update","id":id,"assignee":attempt.name
        }),
    ))?;
    let status = task.record().status;
    if status == TaskStatus::InProgress {
        return Ok(());
    }
    if status == TaskStatus::Done {
        task_ok(crate::tasks::handle(
            home,
            "system:schedule_job",
            &serde_json::json!({"action":"update","id":id,"status":"open"}),
        ))?;
    }
    task_ok(crate::tasks::handle(
        home,
        "system:schedule_job",
        &serde_json::json!({"action":"update","id":id,"status":"in_progress"}),
    ))
}

/// Persist intent before sending. If a send response or its receipt is lost,
/// surface Unknown instead of blindly delivering the same notification again.
pub(super) fn reconcile_notification(
    home: &Path,
    run: &Run,
    send: impl FnOnce(&Run) -> anyhow::Result<String>,
) -> anyhow::Result<()> {
    use super::NotificationState;
    let mut next = run.clone();
    match run.notification {
        NotificationState::Pending => {
            next.notification = NotificationState::Sending;
            if !replace(home, run, next.clone())? {
                return Ok(());
            }
            next.revision = run.revision + 1;
            let mut sent = next.clone();
            match send(&next) {
                Ok(id) => {
                    sent.notification = NotificationState::Sent;
                    sent.notification_receipt = Some(id);
                }
                Err(error) => {
                    sent.notification = NotificationState::Unknown;
                    sent.notification_error = Some(error.to_string());
                }
            }
            replace(home, &next, sent)?;
        }
        NotificationState::Sending => {
            next.notification = NotificationState::Unknown;
            next.notification_error = Some(
                "notification was interrupted; delivery must be checked before resending".into(),
            );
            replace(home, run, next)?;
        }
        NotificationState::NotRequested | NotificationState::Sent | NotificationState::Unknown => {}
    }
    Ok(())
}
