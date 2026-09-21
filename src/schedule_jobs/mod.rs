//! Durable executions for daemon-owned schedules. No process or network work
//! runs while the state lock is held. Corrupt state is an error, never an empty
//! queue: losing ownership would permit a second worker to publish results.
pub(crate) mod config;
mod controller;
pub(crate) mod delivery;
pub(crate) mod notification;
pub(crate) mod runtime;
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;

use chrono::{DateTime, Utc};
use config::JobConfig;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

pub(crate) use runtime::{JobRuntime, Observation};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Attempt {
    pub number: u32,
    pub name: String,
    pub uuid: Option<String>,
    pub backend: String,
    pub started_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct DispatchIntent {
    pub run_id: String,
    pub attempt_number: u32,
    pub revision: u64,
    pub intent_id: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    Queued,
    Starting,
    Dispatching,
    Running,
    Stopping,
    Waiting,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NotificationState {
    NotRequested,
    Pending,
    Sending,
    Sent,
    Unknown,
}

/// Outcome of the worker's opt-in business delivery (`schedule action=deliver`).
/// Kept separate from execution success so a Run is never recorded as delivered
/// without the transport actually accepting every chunk.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DeliveryState {
    /// No delivery configured, or the worker had nothing to deliver.
    #[default]
    NotRequested,
    /// A send is in flight (or was interrupted); not yet a success.
    Sending,
    /// Every chunk was accepted; `receipts` holds one transport id per chunk.
    Sent { receipts: Vec<String> },
    /// The transport refused or failed; `error` is the reason. Retry with
    /// `deliver` after fixing the cause.
    Failed { error: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Run {
    pub id: String,
    pub schedule_id: String,
    /// UTC epoch milliseconds, including subsecond one-shot occurrences.
    pub scheduled_at: i64,
    pub created_by: String,
    pub message: String,
    pub config: JobConfig,
    pub phase: Phase,
    pub revision: u64,
    /// Durable two-phase dispatch claim. The worker message may be sent after
    /// this claim and before its result is committed; the idempotency key in
    /// `ManagedRuntime::dispatch` makes that retry safe.
    #[serde(default)]
    pub dispatch_intent: Option<DispatchIntent>,
    pub attempt: Option<Attempt>,
    pub previous_attempts: Vec<Attempt>,
    pub task_id: Option<String>,
    pub result: Option<String>,
    pub error: Option<String>,
    pub next_attempt_at: i64,
    pub deadline: i64,
    pub cleanup_pending: bool,
    /// Epoch seconds when the bounded auto-cleanup retry window started. Set on
    /// the first cleanup tick of an opt-in successful Run; `None` otherwise.
    #[serde(default)]
    pub cleanup_started_at: Option<i64>,
    #[serde(default)]
    pub recovery_required: bool,
    #[serde(default)]
    pub recovery_resolution: Option<String>,
    pub task_settled: bool,
    pub notification: NotificationState,
    pub notification_receipt: Option<String>,
    pub notification_error: Option<String>,
    #[serde(default)]
    pub delivery: DeliveryState,
}
impl Run {
    fn active(&self) -> bool {
        !matches!(self.phase, Phase::Succeeded | Phase::Failed)
            || self.cleanup_pending
            || self.recovery_required
    }
}

#[derive(Default, Serialize, Deserialize)]
struct JobStore {
    version: u32,
    watermarks: BTreeMap<String, i64>,
    runs: Vec<Run>,
    overlap_skips: BTreeMap<String, u64>,
}
fn state_path(home: &Path) -> std::path::PathBuf {
    home.join("schedule-jobs.json")
}
fn read(home: &Path) -> anyhow::Result<JobStore> {
    match std::fs::read(state_path(home)) {
        Ok(bytes) => {
            let state: JobStore = serde_json::from_slice(&bytes)?;
            anyhow::ensure!(state.version == 1, "unsupported schedule job state version");
            Ok(state)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(JobStore {
            version: 1,
            ..Default::default()
        }),
        Err(e) => Err(e.into()),
    }
}
fn mutate<T>(home: &Path, f: impl FnOnce(&mut JobStore) -> anyhow::Result<T>) -> anyhow::Result<T> {
    std::fs::create_dir_all(home)?;
    let _lock = crate::store::acquire_file_lock(&state_path(home).with_extension("lock"))?;
    let mut state = read(home)?;
    let result = f(&mut state)?;
    crate::store::save_atomic(&state_path(home), &state)?;
    Ok(result)
}

/// Fail closed for recovery owners if the ownership ledger cannot be read.
pub(crate) fn owns_worker(home: &Path, name: &str) -> bool {
    match read(home) {
        Ok(state) => state.runs.iter().any(|r| {
            r.attempt
                .iter()
                .chain(r.previous_attempts.iter())
                .any(|a| a.name == name)
        }),
        Err(error) => {
            tracing::error!(%error, "job ownership unreadable; refusing independent recovery");
            true
        }
    }
}

/// A Job has its own watermark. Advancing the legacy reminder cursor cannot
/// acknowledge a failed Job admission. Downtime is coalesced to the latest due
/// occurrence, including one-shots; no historical burst is created.
pub(crate) fn admit_due(
    home: &Path,
    schedule: &crate::schedules::Schedule,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    use std::str::FromStr;
    let Some(config) = schedule.job.as_ref() else {
        return Ok(());
    };
    if !schedule.enabled {
        return Ok(());
    }
    config.validate_for_home(home).map_err(anyhow::Error::msg)?;
    let created = DateTime::parse_from_rfc3339(&schedule.created_at)?.with_timezone(&Utc);
    let occurrence = match &schedule.trigger {
        crate::schedules::Trigger::Once { at } => {
            let at = DateTime::parse_from_rfc3339(at)?.with_timezone(&Utc);
            (at <= now && at > created).then_some(at.timestamp_millis())
        }
        crate::schedules::Trigger::Cron { expr } => {
            let expr = if expr.split_whitespace().count() == 5 {
                format!("0 {expr}")
            } else {
                expr.clone()
            };
            let cron = cron::Schedule::from_str(&expr)?;
            let tz: chrono_tz::Tz = schedule.timezone.parse().map_err(anyhow::Error::msg)?;
            // Cron's reverse iterator includes the current integer second
            // only for a fractional bound. Use the next WHOLE second as an
            // exclusive bound so both exact and fractional ticks select <= now.
            let exclusive_upper = now
                - chrono::Duration::nanoseconds(i64::from(now.timestamp_subsec_nanos()))
                + chrono::Duration::seconds(1);
            cron.after(&exclusive_upper.with_timezone(&tz))
                .next_back()
                .filter(|d| d.with_timezone(&Utc) > created && d.with_timezone(&Utc) <= now)
                .map(|d| d.timestamp_millis())
        }
    };
    let Some(at) = occurrence else {
        return Ok(());
    };
    mutate(home, |state| {
        if state
            .watermarks
            .get(&schedule.id)
            .is_some_and(|last| *last >= at)
        {
            return Ok(());
        }
        state.watermarks.insert(schedule.id.clone(), at);
        if state
            .runs
            .iter()
            .any(|r| r.schedule_id == schedule.id && r.active())
        {
            *state.overlap_skips.entry(schedule.id.clone()).or_default() += 1;
            return Ok(());
        }
        let uuid = uuid::Uuid::new_v4();
        state.runs.push(Run {
            id: format!("j-{}", uuid.simple()),
            schedule_id: schedule.id.clone(),
            scheduled_at: at,
            created_by: schedule.created_by.clone(),
            message: schedule.message.clone(),
            config: config.clone(),
            phase: Phase::Queued,
            revision: 0,
            dispatch_intent: None,
            attempt: None,
            previous_attempts: vec![],
            task_id: Some(format!("t-{}-{}-0", now.timestamp_micros(), uuid.as_u128())),
            result: None,
            error: None,
            next_attempt_at: now.timestamp(),
            deadline: now.timestamp().saturating_add(config.timeout_secs as i64),
            cleanup_pending: false,
            cleanup_started_at: None,
            recovery_required: false,
            recovery_resolution: None,
            task_settled: false,
            notification: if config.notification.is_some() {
                NotificationState::Pending
            } else {
                NotificationState::NotRequested
            },
            notification_receipt: None,
            notification_error: None,
            delivery: DeliveryState::NotRequested,
        });
        Ok(())
    })
}

pub(crate) fn list(home: &Path, schedule_id: Option<&str>) -> serde_json::Value {
    match read(home) {
        Ok(state) => {
            serde_json::json!({"runs":state.runs.iter().filter(|r| schedule_id.is_none_or(|id| r.schedule_id == id)).collect::<Vec<_>>(), "overlap_skips":state.overlap_skips})
        }
        Err(e) => serde_json::json!({"error":e.to_string()}),
    }
}

/// Caller name is supplied by the authenticated MCP adapter, never by args.
/// Compare the server-resolved UUID and attempt while holding the receipt lock.
pub(crate) fn complete(home: &Path, caller: &str, args: &serde_json::Value) -> serde_json::Value {
    let result = (|| -> anyhow::Result<()> {
        let id = args["run_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing run_id"))?;
        let number = args["attempt_id"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("missing attempt_id"))?;
        let result = args["result"]
            .as_str()
            .filter(|s| !s.trim().is_empty() && s.len() <= 65536)
            .ok_or_else(|| anyhow::anyhow!("nonempty result <= 65536 bytes required"))?;
        let uuid = crate::fleet::resolve_uuid(home, caller)
            .ok_or_else(|| anyhow::anyhow!("unknown completion caller"))?
            .full();
        complete_as(home, id, number, caller, &uuid, result)
    })();
    match result {
        Ok(()) => serde_json::json!({"status":"completion_recorded"}),
        Err(e) => serde_json::json!({"error":e.to_string()}),
    }
}
fn complete_as(
    home: &Path,
    id: &str,
    number: u64,
    caller: &str,
    uuid: &str,
    result: &str,
) -> anyhow::Result<()> {
    mutate(home, |state| {
        let run = state
            .runs
            .iter_mut()
            .find(|r| r.id == id)
            .ok_or_else(|| anyhow::anyhow!("unknown run"))?;
        let a = run
            .attempt
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no active attempt"))?;
        anyhow::ensure!(
            a.name == caller && a.uuid.as_deref() == Some(uuid) && u64::from(a.number) == number,
            "stale or unauthorized attempt"
        );
        if run.phase == Phase::Succeeded && run.result.as_deref() == Some(result) {
            return Ok(());
        }
        anyhow::ensure!(
            !run.recovery_required && matches!(run.phase, Phase::Running | Phase::Dispatching),
            "attempt is not accepting completion"
        );
        anyhow::ensure!(
            !matches!(
                run.delivery,
                DeliveryState::Sending | DeliveryState::Failed { .. }
            ),
            "business delivery must succeed (or be skipped) before completion"
        );
        run.result = Some(result.into());
        run.phase = Phase::Succeeded;
        run.dispatch_intent = None;
        run.cleanup_pending = true;
        run.revision += 1;
        Ok(())
    })
}

/// Worker-invoked business delivery. The daemon never derives the destination
/// from the creator or worker: it uses only the validated `job.worker_topic`.
/// Completion is refused while a delivery is in flight or has failed, so a
/// worker only finishes after delivery is either skipped or fully accepted.
pub(crate) fn deliver(
    home: &Path,
    caller: &str,
    args: &serde_json::Value,
    content: &str,
) -> serde_json::Value {
    let result = (|| -> anyhow::Result<DeliveryState> {
        let id = args["run_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing run_id"))?;
        let number = args["attempt_id"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("missing attempt_id"))?;
        anyhow::ensure!(
            !content.trim().is_empty(),
            "nonempty delivery content required"
        );
        anyhow::ensure!(content.len() <= 1_048_576, "delivery content exceeds 1 MiB");
        let uuid = crate::fleet::resolve_uuid(home, caller)
            .ok_or_else(|| anyhow::anyhow!("unknown delivery caller"))?
            .full();
        // Validate the explicit endpoint against the fleet BEFORE the durable
        // claim and outside the state lock. An illegal/undeclared group is
        // rejected here and never recorded as an attempt.
        let endpoint = read(home)?
            .runs
            .into_iter()
            .find(|r| r.id == id)
            .and_then(|r| r.config.worker_topic)
            .ok_or_else(|| {
                anyhow::anyhow!("this job has no business delivery endpoint configured")
            })?;
        endpoint
            .validate_for_home(home)
            .map_err(anyhow::Error::msg)?;
        let Some(run) = claim_delivery(home, id, number, caller, &uuid)? else {
            return Ok(DeliveryState::Sent { receipts: vec![] });
        };
        let outcome = match delivery::send(home, &run, content) {
            Ok(receipts) => DeliveryState::Sent { receipts },
            Err(error) => DeliveryState::Failed {
                error: error.to_string(),
            },
        };
        finish_delivery(home, id, &outcome)?;
        Ok(outcome)
    })();
    match result {
        Ok(DeliveryState::Sent { receipts }) => {
            serde_json::json!({"status":"delivered","receipts":receipts})
        }
        Ok(DeliveryState::Failed { error }) => {
            serde_json::json!({"error":error,"code":"delivery_failed","delivery":"failed"})
        }
        Ok(_) => serde_json::json!({"error":"unexpected delivery state","code":"delivery_failed"}),
        Err(error) => serde_json::json!({"error":error.to_string()}),
    }
}

/// Durably mark a delivery in flight, returning the Run snapshot to send. Fails
/// closed on ambiguity: a stale `Sending` must be inspected rather than blindly
/// re-sent. `Ok(None)` means the transport already accepted this Run's content.
fn claim_delivery(
    home: &Path,
    id: &str,
    number: u64,
    caller: &str,
    uuid: &str,
) -> anyhow::Result<Option<Run>> {
    mutate(home, |state| {
        let run = state
            .runs
            .iter_mut()
            .find(|r| r.id == id)
            .ok_or_else(|| anyhow::anyhow!("unknown run"))?;
        let a = run
            .attempt
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no active attempt"))?;
        anyhow::ensure!(
            a.name == caller && a.uuid.as_deref() == Some(uuid) && u64::from(a.number) == number,
            "stale or unauthorized attempt"
        );
        anyhow::ensure!(
            !run.recovery_required && matches!(run.phase, Phase::Running | Phase::Dispatching),
            "attempt is not accepting delivery"
        );
        anyhow::ensure!(
            run.config.worker_topic.is_some(),
            "this job has no business delivery endpoint configured"
        );
        match &run.delivery {
            DeliveryState::Sent { .. } => return Ok(None),
            DeliveryState::Sending => anyhow::bail!(
                "a previous delivery attempt is unresolved; inspect runs before retrying"
            ),
            DeliveryState::NotRequested | DeliveryState::Failed { .. } => {}
        }
        run.delivery = DeliveryState::Sending;
        run.revision += 1;
        Ok(Some(run.clone()))
    })
}

fn finish_delivery(home: &Path, id: &str, outcome: &DeliveryState) -> anyhow::Result<()> {
    mutate(home, |state| {
        let run = state
            .runs
            .iter_mut()
            .find(|r| r.id == id)
            .ok_or_else(|| anyhow::anyhow!("run disappeared during delivery"))?;
        run.delivery = outcome.clone();
        run.revision += 1;
        Ok(())
    })
}

fn replace(home: &Path, old: &Run, mut new: Run) -> anyhow::Result<bool> {
    mutate(home, |state| {
        let current = state
            .runs
            .iter_mut()
            .find(|r| r.id == old.id)
            .ok_or_else(|| anyhow::anyhow!("run disappeared"))?;
        if current.revision != old.revision {
            return Ok(false);
        }
        new.revision = old.revision + 1;
        *current = new;
        Ok(true)
    })
}

pub(crate) use controller::tick;

/// Operator-gated acknowledgement after externally verifying all tool work
/// stopped and delivery was reconciled. This action never kills or retries.
pub(crate) fn resolve_recovery(home: &Path, args: &serde_json::Value) -> serde_json::Value {
    let caller = "operator";
    let result = (|| -> anyhow::Result<()> {
        let id = args["run_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing run_id"))?;
        let number = args["attempt_id"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("missing attempt_id"))?;
        anyhow::ensure!(args["cleanup_confirmed"].as_bool() == Some(true), "explicit cleanup_confirmed=true required after verifying all worker tools stopped and delivery reconciled");
        let note = args["result"]
            .as_str()
            .filter(|s| !s.trim().is_empty() && s.len() <= 65536)
            .ok_or_else(|| {
                anyhow::anyhow!("nonempty recovery audit note <= 65536 bytes required")
            })?;
        let run = read(home)?
            .runs
            .into_iter()
            .find(|r| r.id == id)
            .ok_or_else(|| anyhow::anyhow!("unknown run"))?;
        let attempt = run
            .attempt
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no active attempt"))?;
        anyhow::ensure!(
            run.recovery_required && u64::from(attempt.number) == number,
            "stale or non-recovering attempt"
        );
        let _permit = crate::mcp::handlers::dispatch_hook::LifecyclePermit::acquire(
            home,
            &attempt.name,
            crate::mcp::handlers::dispatch_hook::LifecycleOperation::Delete,
        )
        .map_err(anyhow::Error::msg)?;
        let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))?;
        anyhow::ensure!(
            !fleet.instances.contains_key(&attempt.name),
            "delete the externally stopped worker before acknowledging recovery"
        );
        let mut next = run.clone();
        next.recovery_required = false;
        next.cleanup_pending = false;
        if next.phase != Phase::Succeeded {
            next.phase = Phase::Failed;
        }
        next.recovery_resolution = Some(format!("Recovery acknowledged by {caller}: {note}"));
        anyhow::ensure!(
            replace(home, &run, next)?,
            "run changed during recovery; inspect current state"
        );
        Ok(())
    })();
    match result {
        Ok(()) => serde_json::json!({"status":"recovery_resolved"}),
        Err(error) => serde_json::json!({"error":error.to_string()}),
    }
}
