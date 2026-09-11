//! Schedule storage — CRUD for cron + one-shot schedules. Execution via
//! daemon::check_schedules().

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use std::str::FromStr;
use std::sync::OnceLock;

/// Cache for the detected system timezone string. Originally each call leaked
/// a `Box<str>`, so repeated `detect_timezone()` invocations (e.g. `create` /
/// `update` in quick succession, or a daemon that rereads env) grew the heap
/// unboundedly. `OnceLock` caches the first successful detection for process
/// lifetime and keeps `&'static str` without leaking on every call.
static DETECTED_TZ: OnceLock<String> = OnceLock::new();

/// Return the detected system timezone as a stable `&'static str`.
///
/// Precedence: `TZ` env at first call → `iana_time_zone::get_timezone()`
/// → `"UTC"`. The detection runs once; later mutations of `$TZ` are
/// intentionally ignored, because (a) schedules carry their own per-row
/// `timezone` field that is the real source of truth for cron evaluation,
/// and (b) leaking a new string on every env toggle was the original P2-6 bug.
///
/// `iana-time-zone` resolves an IANA name on all supported platforms:
/// Linux reads `/etc/localtime`, macOS calls CoreFoundation, Windows reads
/// the registry and maps Windows TZ names to IANA. This replaces the old
/// Unix-only `/etc/localtime` symlink parse, which silently fell through
/// to UTC on Windows.
pub fn detect_timezone() -> &'static str {
    DETECTED_TZ
        .get_or_init(|| {
            if let Ok(tz) = std::env::var("TZ") {
                if !tz.is_empty() {
                    return tz;
                }
            }
            if let Ok(tz) = iana_time_zone::get_timezone() {
                if !tz.is_empty() {
                    return tz;
                }
            }
            "UTC".to_string()
        })
        .as_str()
}

/// How a schedule decides when to fire.
///
/// Serialised as an externally-tagged JSON object:
/// - `{"kind":"cron","expr":"0 9 * * *"}`
/// - `{"kind":"once","at":"2026-04-21T15:30:00+08:00"}`
///
/// The `Once` variant stores an RFC 3339 timestamp with offset so the
/// on-disk shape is self-contained — the enclosing `Schedule.timezone`
/// is only used for display / future updates, not to re-resolve `at`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Trigger {
    Cron { expr: String },
    Once { at: String },
}

/// #1521: how a recurring schedule decides whether to KEEP firing within a
/// calendar day (in the schedule's timezone).
///
/// - `Always` (default, backward-compatible) — fire every time the trigger
///   lands.
/// - `UntilSuccess` — a "remind until done" reminder: once the linked task
///   reaches `done`, suppress further fires for the rest of that day; re-fire
///   the next day (and resume immediately if the task is reopened). Requires a
///   `linked_task_id` that exists (enforced at create/update).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum FireStrategy {
    #[default]
    Always,
    UntilSuccess,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(from = "ScheduleRaw")]
pub struct Schedule {
    pub id: String,
    pub trigger: Trigger,
    pub message: String,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<crate::schedule_jobs::config::JobConfig>,
    pub label: Option<String>,
    pub timezone: String,
    pub enabled: bool,
    pub created_by: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub run_history: Vec<ScheduleRun>,
    /// #1521: fire-strategy. Defaults to `Always` (existing rows unchanged).
    #[serde(default)]
    pub fire_strategy: FireStrategy,
    /// #1521: task whose completion suppresses further fires (UntilSuccess).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linked_task_id: Option<String>,
    /// #1521: calendar day (`YYYY-MM-DD`, schedule tz) the linked task was last
    /// observed `done` — suppresses re-fires for the rest of that day.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_success_date: Option<String>,
    /// Logical singleton scope. Creating a newer schedule for the same target
    /// and key atomically disables older enabled rows while retaining them for
    /// audit. AGEND-AUTO messages derive this from their `kind` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement_key: Option<String>,
    /// Immutable revision/subject being monitored (for example `git:<sha>`).
    /// Informational identity; replacement is governed by `replacement_key`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_ref: Option<String>,
    /// Typed authority for why this row is disabled. Legacy rows have no
    /// provenance and are therefore never eligible for automatic retention.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<DisabledReason>,
    /// RFC3339 instant at which `disabled_reason` most recently became true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_at: Option<String>,
}

/// #3280: typed schedule-disable provenance. Retention decisions switch only
/// on these variants; `run_history.status` remains operator-facing evidence and
/// is never parsed as deletion authority.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DisabledReason {
    OperatorPaused,
    OneShotFired,
    OneShotMissed,
    OneShotReplayed,
    OneShotStaleDropped,
    Superseded { successor_id: String },
    TargetOrphaned { target: String },
    LinkedTaskMissing { task_id: Option<String> },
}

impl Schedule {
    fn disable_at(&mut self, reason: DisabledReason, now: &str) {
        self.enabled = false;
        self.disabled_reason = Some(reason);
        self.disabled_at = Some(now.to_string());
        self.updated_at = now.to_string();
    }

    fn enable_at(&mut self, now: &str) {
        self.enabled = true;
        self.disabled_reason = None;
        self.disabled_at = None;
        self.updated_at = now.to_string();
    }
}

/// On-wire representation that accepts both v1 (top-level `cron`) and v2
/// (nested `trigger`) rows. Enables transparent schema-v1→v2 migration on
/// load without touching the generic `store` module. The `From` impl picks
/// `trigger` when present, otherwise falls back to `cron`, defaulting to an
/// empty cron expression if both are missing (which surfaces later as an
/// "invalid cron" log in the daemon tick rather than a panic on load).
#[derive(Debug, Clone, Deserialize)]
struct ScheduleRaw {
    id: String,
    #[serde(default)]
    trigger: Option<Trigger>,
    #[serde(default)]
    cron: Option<String>,
    message: String,
    #[serde(default)]
    target: String,
    #[serde(default)]
    job: Option<crate::schedule_jobs::config::JobConfig>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    timezone: String,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    created_by: String,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    updated_at: String,
    #[serde(default)]
    run_history: Vec<ScheduleRun>,
    #[serde(default)]
    fire_strategy: FireStrategy,
    #[serde(default)]
    linked_task_id: Option<String>,
    #[serde(default)]
    last_success_date: Option<String>,
    #[serde(default)]
    replacement_key: Option<String>,
    #[serde(default)]
    subject_ref: Option<String>,
    #[serde(default)]
    disabled_reason: Option<DisabledReason>,
    #[serde(default)]
    disabled_at: Option<String>,
}

fn default_true() -> bool {
    true
}

impl From<ScheduleRaw> for Schedule {
    fn from(r: ScheduleRaw) -> Self {
        let trigger = r.trigger.unwrap_or_else(|| Trigger::Cron {
            expr: r.cron.unwrap_or_default(),
        });
        Schedule {
            id: r.id,
            trigger,
            message: r.message,
            target: r.target,
            job: r.job,
            label: r.label,
            timezone: r.timezone,
            enabled: r.enabled,
            created_by: r.created_by,
            created_at: r.created_at,
            updated_at: r.updated_at,
            run_history: r.run_history,
            fire_strategy: r.fire_strategy,
            linked_task_id: r.linked_task_id,
            last_success_date: r.last_success_date,
            replacement_key: r.replacement_key,
            subject_ref: r.subject_ref,
            disabled_reason: r.disabled_reason,
            disabled_at: r.disabled_at,
        }
    }
}

const MAX_REPLACEMENT_KEY_BYTES: usize = 128;
const MAX_SUBJECT_REF_BYTES: usize = 512;

fn optional_identity_arg(
    args: &Value,
    field: &str,
    max_bytes: usize,
) -> Result<Option<String>, String> {
    let Some(value) = args.get(field) else {
        return Ok(None);
    };
    let Some(raw) = value.as_str() else {
        return Err(format!("'{field}' must be a string"));
    };
    let normalized = raw.trim();
    if normalized.is_empty() {
        return Err(format!("'{field}' must not be empty"));
    }
    if normalized.len() > max_bytes {
        return Err(format!("'{field}' exceeds the {max_bytes}-byte limit"));
    }
    if normalized.chars().any(char::is_control) {
        return Err(format!("'{field}' must not contain control characters"));
    }
    Ok(Some(normalized.to_string()))
}

/// The daemon-auto marker already defines keep-latest identity for queued
/// nudges. Reuse that `kind` as the default schedule replacement scope so a
/// recurring automatic monitor cannot survive creation of its successor.
fn auto_replacement_key(message: &str) -> Option<String> {
    let rest = message.strip_prefix(crate::agent::DAEMON_AUTO_INJECT_MARKER)?;
    let rest = rest.strip_prefix(" kind=")?;
    let end = rest.find(']')?;
    let kind = &rest[..end];
    if kind.is_empty()
        || !kind
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return None;
    }
    Some(format!("agend-auto:{kind}"))
}

fn effective_replacement_key(schedule: &Schedule) -> Option<String> {
    if schedule.job.is_some() {
        return None;
    }
    schedule
        .replacement_key
        .clone()
        .or_else(|| auto_replacement_key(&schedule.message))
}

/// #1521: a `linked_task_id` is required for `UntilSuccess` and the task must
/// already exist on the board — a bare-message reminder has no completion to
/// gate on, so we reject it at create/update rather than silently degrade.
/// #1608: existence is decided by the task subsystem's authoritative lookup,
/// NOT a `tasks/<id>.json` file probe — AgEnD's task board is event-sourced and
/// keeps no per-task JSON, so the old filesystem check always returned `false`
/// and made `fire_strategy=until_success` permanently unreachable. The old name
/// (`task_file_exists`) baked in that wrong filesystem abstraction.
fn task_exists(home: &Path, task_id: &str) -> bool {
    // #2760: strict route — existence means the id resolves to EXACTLY ONE readable
    // board. Any route error (NotFound / Unreadable / Ambiguous) → `false` →
    // fail-closed reject of `fire_strategy=until_success` (never gate on a task
    // whose board cannot be uniquely proven).
    !task_id.is_empty() && crate::tasks::load_routed(home, task_id).is_ok()
}

/// #1521: validate a (fire_strategy, linked_task_id) pair. `Ok(())` when the
/// combination is legal; `Err(msg)` (operator-facing) otherwise.
fn validate_fire_strategy(
    home: &Path,
    fire_strategy: FireStrategy,
    linked_task_id: Option<&str>,
) -> Result<(), String> {
    if fire_strategy != FireStrategy::UntilSuccess {
        return Ok(());
    }
    match linked_task_id {
        Some(id) if task_exists(home, id) => Ok(()),
        Some(id) => Err(format!(
            "fire_strategy=until_success requires an existing linked_task_id (task '{id}' not found; point linked_task_id at an existing task in the same update)"
        )),
        None => {
            Err("fire_strategy=until_success requires 'linked_task_id'".to_string())
        }
    }
}

/// #1521: parse the `fire_strategy` arg ("always" | "until_success").
fn fire_strategy_from_args(args: &Value) -> Result<Option<FireStrategy>, String> {
    match args.get("fire_strategy").and_then(|v| v.as_str()) {
        None => Ok(None),
        Some("always") => Ok(Some(FireStrategy::Always)),
        Some("until_success") => Ok(Some(FireStrategy::UntilSuccess)),
        Some(other) => Err(format!(
            "invalid fire_strategy {other:?} (expected 'always' or 'until_success')"
        )),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScheduleRun {
    pub triggered_at: String,
    pub status: String, // "ok" or error message
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct ScheduleStore {
    #[serde(default)]
    pub(crate) schema_version: u32,
    pub(crate) schedules: Vec<Schedule>,
}

impl crate::store::SchemaVersioned for ScheduleStore {
    /// v1: `{cron: String}` on each row.
    /// v2 adds typed triggers; disable provenance stays optional and additive.
    /// Keeping v2 lets older binaries ignore it instead of emptying a future store.
    const CURRENT: u32 = 2;
    fn version_mut(&mut self) -> &mut u32 {
        &mut self.schema_version
    }
}

fn store_path(home: &Path) -> std::path::PathBuf {
    crate::store::store_path(home, "schedules.json")
}

pub(crate) fn load(home: &Path) -> ScheduleStore {
    crate::store::load_versioned(
        &store_path(home),
        <ScheduleStore as crate::store::SchemaVersioned>::CURRENT,
    )
}

/// Scan for enabled one-shot schedules whose `run_at` is in the past.
/// Schedules missed by ≤24h are returned for replay; older ones are
/// discarded with a warn log. All matched schedules are disabled.
pub fn replay_missed_oneshots(home: &Path) -> Vec<Schedule> {
    let now = chrono::Utc::now();
    let cutoff = now - chrono::Duration::hours(24);
    let mut to_replay = Vec::new();

    persist_or_log!(
        crate::store::mutate_versioned(&store_path(home), |store: &mut ScheduleStore| {
            for sched in store.schedules.iter_mut() {
                if !sched.enabled || sched.job.is_some() {
                    continue;
                }
                let at = match &sched.trigger {
                    Trigger::Once { at } => at.clone(),
                    Trigger::Cron { .. } => continue,
                };
                let at_utc = match chrono::DateTime::parse_from_rfc3339(&at) {
                    Ok(dt) => dt.with_timezone(&chrono::Utc),
                    Err(_) => continue,
                };
                if at_utc >= now {
                    continue; // not missed yet
                }
                let now_str = now.to_rfc3339();
                if at_utc < cutoff {
                    sched.disable_at(DisabledReason::OneShotStaleDropped, &now_str);
                    tracing::warn!(
                        id = %sched.id,
                        run_at = %at,
                        "dropping stale one-shot schedule (>24h past)"
                    );
                    sched.run_history.push(ScheduleRun {
                        triggered_at: now.to_rfc3339(),
                        status: "stale_dropped".to_string(),
                    });
                } else {
                    sched.disable_at(DisabledReason::OneShotReplayed, &now_str);
                    sched.run_history.push(ScheduleRun {
                        triggered_at: now.to_rfc3339(),
                        status: "replayed".to_string(),
                    });
                    to_replay.push(sched.clone());
                }
            }
            Ok(())
        }),
        "schedule_replay_missed"
    );
    to_replay
}

/// Disable a schedule with explicit machine-readable authority.
pub fn disable(home: &Path, schedule_id: &str, reason: DisabledReason) {
    let sid = schedule_id.to_string();
    persist_or_log!(
        crate::store::mutate_versioned(&store_path(home), |store: &mut ScheduleStore| {
            if let Some(sched) = store.schedules.iter_mut().find(|s| s.id == sid) {
                let now = chrono::Utc::now().to_rfc3339();
                sched.disable_at(reason.clone(), &now);
            }
            Ok(())
        }),
        "schedule_set_enabled"
    );
}

/// #1521: record that an `UntilSuccess` schedule's linked task was observed
/// `done` on `date` (`YYYY-MM-DD`, schedule tz) — suppresses further fires for
/// the rest of that calendar day.
pub fn mark_success_today(home: &Path, schedule_id: &str, date: &str) {
    let sid = schedule_id.to_string();
    let d = date.to_string();
    persist_or_log!(
        crate::store::mutate_versioned(&store_path(home), |store: &mut ScheduleStore| {
            if let Some(sched) = store.schedules.iter_mut().find(|s| s.id == sid) {
                sched.last_success_date = Some(d.clone());
            }
            Ok(())
        }),
        "schedule_mark_success_today"
    );
}

/// #1488: when an instance is deleted, disable every schedule that targets it
/// and mark it orphaned in `run_history` — but DON'T delete the row, so the
/// operator can re-target a still-useful schedule (e.g. an AI-Scout cron) at a
/// surviving instance. Already-disabled rows are updated too: target deletion
/// is newer, stronger provenance than a prior pause/supersession. Idempotent:
/// an exact `TargetOrphaned` row is left untouched. Returns the number of rows
/// whose typed provenance changed.
pub fn orphan_schedules_for_target(home: &Path, deleted_target: &str) -> usize {
    let target = deleted_target.to_string();
    let mut orphaned = 0usize;
    persist_or_log!(
        crate::store::mutate_versioned(&store_path(home), |store: &mut ScheduleStore| {
            let now = chrono::Utc::now().to_rfc3339();
            for sched in store.schedules.iter_mut() {
                if sched.job.is_some() || sched.target != target {
                    continue;
                }
                let reason = DisabledReason::TargetOrphaned {
                    target: target.clone(),
                };
                if !sched.enabled && sched.disabled_reason.as_ref() == Some(&reason) {
                    continue;
                }
                sched.disable_at(reason, &now);
                sched.run_history.push(ScheduleRun {
                    triggered_at: now.clone(),
                    status: format!("orphaned: target instance '{target}' deleted"),
                });
                orphaned += 1;
            }
            Ok(())
        }),
        "schedule_orphan_for_target"
    );
    if orphaned > 0 {
        tracing::info!(
            target = %deleted_target,
            count = orphaned,
            "#1488: disabled + marked orphaned schedules targeting deleted instance"
        );
    }
    orphaned
}

/// Normalise a 5-field cron to the 6-field form the `cron` crate expects
/// (prepend a "0" seconds column). Idempotent for 6-field input.
fn normalise_cron(expr: &str) -> String {
    if expr.split_whitespace().count() == 5 {
        format!("0 {expr}")
    } else {
        expr.to_string()
    }
}

fn validate_cron(expr: &str) -> Result<(), String> {
    let full = normalise_cron(expr);
    cron::Schedule::from_str(&full).map_err(|_| format!("invalid cron expression: {expr}"))?;
    Ok(())
}

/// Parse a `run_at` field into an RFC 3339 timestamp with offset.
///
/// Accepts either a fully-qualified RFC 3339 string (e.g.
/// `"2026-04-21T15:30:00+08:00"`) or a naive local datetime that we
/// resolve against `tz_name` (e.g. `"2026-04-21T15:30:00"` +
/// `"Asia/Taipei"`). Rejects ambiguous / non-existent DST edges.
fn parse_run_at(raw: &str, tz_name: &str) -> Result<String, String> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Ok(dt.to_rfc3339());
    }
    let naive = chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M"))
        .map_err(|e| format!("invalid run_at {raw:?}: {e} (expected ISO 8601)"))?;
    let tz: chrono_tz::Tz = tz_name
        .parse()
        .map_err(|_| format!("unknown timezone: {tz_name}"))?;
    use chrono::TimeZone;
    match tz.from_local_datetime(&naive).single() {
        Some(dt) => Ok(dt.to_rfc3339()),
        None => Err(format!(
            "run_at {raw:?} is ambiguous or nonexistent in {tz_name} (DST edge)"
        )),
    }
}

/// Build a `Trigger` from the caller's args, enforcing mutual exclusion
/// between `cron` and `run_at`. Returns `Err` on validation failure with a
/// user-facing message; the caller wraps this into a JSON error response.
fn trigger_from_args(args: &Value, tz_name: &str) -> Result<Trigger, String> {
    let cron = args["cron"].as_str();
    let run_at = args["run_at"].as_str();
    match (cron, run_at) {
        (Some(_), Some(_)) => Err("'cron' and 'run_at' are mutually exclusive".into()),
        (Some(c), None) => {
            validate_cron(c)?;
            Ok(Trigger::Cron {
                expr: c.to_string(),
            })
        }
        (None, Some(r)) => {
            let parsed = parse_run_at(r, tz_name)?;
            let at_utc = chrono::DateTime::parse_from_rfc3339(&parsed)
                .map_err(|e| format!("internal: round-tripped run_at unparseable: {e}"))?
                .with_timezone(&chrono::Utc);
            if at_utc <= chrono::Utc::now() {
                return Err(format!("run_at {r:?} must be in the future"));
            }
            Ok(Trigger::Once { at: parsed })
        }
        (None, None) => Err("missing 'cron' or 'run_at'".into()),
    }
}

fn job_from_args(
    home: &Path,
    args: &Value,
) -> Result<Option<crate::schedule_jobs::config::JobConfig>, String> {
    let Some(value) = args.get("job") else {
        return Ok(None);
    };
    let job: crate::schedule_jobs::config::JobConfig =
        serde_json::from_value(value.clone()).map_err(|e| format!("invalid job: {e}"))?;
    job.validate_for_home(home)?;
    Ok(Some(job))
}

fn validate_job_options(args: &Value) -> Result<(), String> {
    if args.get("instance").is_some()
        || args.get("linked_task_id").is_some()
        || args.get("replacement_key").is_some()
        || args.get("fire_strategy").is_some_and(|v| v != "always")
    {
        return Err(
            "job is incompatible with instance, linked_task_id, replacement_key, or until_success"
                .into(),
        );
    }
    Ok(())
}

pub fn create(home: &Path, instance_name: &str, args: &Value) -> Value {
    let job = match job_from_args(home, args) {
        Ok(job) => job,
        Err(e) => return serde_json::json!({"error": e}),
    };
    if job.is_some() {
        if let Err(e) = validate_job_options(args) {
            return serde_json::json!({"error": e});
        }
    }
    let message = match args["message"].as_str() {
        Some(m) => m,
        None => return serde_json::json!({"error": "missing 'message'"}),
    };
    let timezone = args["timezone"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| detect_timezone())
        .to_string();
    let trigger = match trigger_from_args(args, &timezone) {
        Ok(t) => t,
        Err(e) => return serde_json::json!({"error": e}),
    };
    // #1521: fire-strategy (default Always) + optional linked task.
    let fire_strategy = match fire_strategy_from_args(args) {
        Ok(fs) => fs.unwrap_or_default(),
        Err(e) => return serde_json::json!({"error": e}),
    };
    let linked_task_id = args["linked_task_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from);
    if let Err(e) = validate_fire_strategy(home, fire_strategy, linked_task_id.as_deref()) {
        return serde_json::json!({"error": e});
    }
    let replacement_key =
        match optional_identity_arg(args, "replacement_key", MAX_REPLACEMENT_KEY_BYTES) {
            Ok(Some(key)) => Some(key),
            Ok(None) if job.is_some() => None,
            Ok(None) => auto_replacement_key(message),
            Err(error) => return serde_json::json!({"error": error}),
        };
    let subject_ref = match optional_identity_arg(args, "subject_ref", MAX_SUBJECT_REF_BYTES) {
        Ok(value) => value,
        Err(error) => return serde_json::json!({"error": error}),
    };
    let now = chrono::Utc::now();
    let now_str = now.to_rfc3339();
    // H3: microsecond precision + counter to prevent same-second collision
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let id = format!("s-{}-{}", now.format("%Y%m%d%H%M%S%6f"), seq);
    let schedule = Schedule {
        id: id.clone(),
        trigger,
        message: message.to_string(),
        target: if job.is_some() {
            String::new()
        } else {
            args["instance"]
                .as_str()
                .unwrap_or(instance_name)
                .to_string()
        },
        job,
        label: args["label"].as_str().map(String::from),
        timezone,
        enabled: true,
        created_by: instance_name.to_string(),
        created_at: now_str.clone(),
        updated_at: now_str,
        run_history: Vec::new(),
        fire_strategy,
        linked_task_id,
        last_success_date: None,
        replacement_key,
        subject_ref,
        disabled_reason: None,
        disabled_at: None,
    };
    let supersession_target = schedule.target.clone();
    let supersession_key = schedule.replacement_key.clone();
    let supersession_id = schedule.id.clone();
    let supersession_at = schedule.updated_at.clone();
    let mut superseded_ids = Vec::new();
    match crate::store::mutate_versioned(&store_path(home), |store: &mut ScheduleStore| {
        if let Some(key) = supersession_key.as_deref() {
            for existing in &mut store.schedules {
                if !existing.enabled || existing.target != supersession_target {
                    continue;
                }
                if effective_replacement_key(existing).as_deref() != Some(key) {
                    continue;
                }
                existing.disable_at(
                    DisabledReason::Superseded {
                        successor_id: supersession_id.clone(),
                    },
                    &supersession_at,
                );
                if existing.replacement_key.is_none() {
                    existing.replacement_key = Some(key.to_string());
                }
                existing.run_history.push(ScheduleRun {
                    triggered_at: supersession_at.clone(),
                    status: format!("superseded_by:{supersession_id}"),
                });
                if existing.run_history.len() > 50 {
                    let excess = existing.run_history.len() - 50;
                    existing.run_history.drain(..excess);
                }
                superseded_ids.push(existing.id.clone());
            }
        }
        store.schedules.push(schedule);
        Ok(())
    }) {
        Ok(()) => serde_json::json!({
            "id": id,
            "status": "created",
            "superseded_ids": superseded_ids
        }),
        Err(e) => serde_json::json!({"error": format!("{e}")}),
    }
}

/// #1720 ③ visibility: the next UTC instant this schedule will fire (RFC 3339),
/// or `None` when it is disabled, a one-shot already in the past, or has an
/// unparseable trigger/timezone. Computed at list time (not persisted) so an
/// operator can see when each schedule is next due — directly answering "did it
/// drop, or is it just not due yet?". Mirrors `cron_tick::check_schedules`'
/// normalisation (5-field cron → prepend seconds) + tz resolution so the
/// displayed instant matches the engine's actual firing.
fn next_fire_at(schedule: &Schedule) -> Option<String> {
    if !schedule.enabled {
        return None;
    }
    let now = chrono::Utc::now();
    match &schedule.trigger {
        Trigger::Once { at } => {
            let when = chrono::DateTime::parse_from_rfc3339(at)
                .ok()?
                .with_timezone(&chrono::Utc);
            (when > now).then(|| when.to_rfc3339())
        }
        Trigger::Cron { expr } => {
            let tz_name = if schedule.timezone.is_empty() {
                detect_timezone()
            } else {
                schedule.timezone.as_str()
            };
            let tz: chrono_tz::Tz = tz_name.parse().ok()?;
            let full = if expr.split_whitespace().count() == 5 {
                format!("0 {expr}")
            } else {
                expr.clone()
            };
            let parsed = cron::Schedule::from_str(&full).ok()?;
            parsed
                .after(&now.with_timezone(&tz))
                .next()
                .map(|next| next.with_timezone(&chrono::Utc).to_rfc3339())
        }
    }
}

pub fn list(home: &Path, args: &Value) -> Value {
    let store = load(home);
    let target_filter = args["instance"].as_str();
    // #2037 (2): the store caps run_history at 50 PER SCHEDULE — serializing
    // it whole made `list` responses balloon (operator hit: most of the
    // payload was history nobody asked for). Default to the newest
    // RUN_HISTORY_LIST_CAP entries; `full_history=true` opts back in. The
    // truncated row carries `runs_total` so the cut is visible.
    const RUN_HISTORY_LIST_CAP: usize = 3;
    let full_history = args["full_history"].as_bool().unwrap_or(false);
    // #1720 ③: each row carries a computed `next_scheduled_fire_at` (not stored)
    // so operators can see when it next fires.
    let schedules: Vec<Value> = store
        .schedules
        .iter()
        .filter(|s| target_filter.is_none_or(|t| s.target == t))
        .map(|s| {
            let mut v = serde_json::to_value(s).unwrap_or(Value::Null);
            if let Value::Object(map) = &mut v {
                if s.fire_strategy != FireStrategy::UntilSuccess {
                    map.remove("last_success_date");
                }
                map.insert(
                    "linked_task_exists".to_string(),
                    Value::Bool(
                        s.linked_task_id
                            .as_deref()
                            .is_some_and(|task_id| task_exists(home, task_id)),
                    ),
                );
                map.insert(
                    "next_scheduled_fire_at".to_string(),
                    next_fire_at(s).map_or(Value::Null, Value::String),
                );
                if !full_history {
                    let total = s.run_history.len();
                    map.insert("runs_total".to_string(), serde_json::json!(total));
                    if total > RUN_HISTORY_LIST_CAP {
                        let tail: Vec<Value> = s.run_history[total - RUN_HISTORY_LIST_CAP..]
                            .iter()
                            .map(|r| serde_json::to_value(r).unwrap_or(Value::Null))
                            .collect();
                        map.insert("run_history".to_string(), Value::Array(tail));
                    }
                }
            }
            v
        })
        .collect();
    serde_json::json!({"schedules": schedules})
}

pub fn update(home: &Path, args: &Value) -> Value {
    let id = match args["id"].as_str() {
        Some(i) => i.to_string(),
        None => return serde_json::json!({"error": "missing 'id'"}),
    };
    if args.get("replacement_key").is_some() || args.get("subject_ref").is_some() {
        return serde_json::json!({
            "error": "'replacement_key' and 'subject_ref' are create-only identity fields; create a replacement schedule instead"
        });
    }

    // Pre-validate trigger change (if any) outside the store lock so
    // errors return without touching the file.
    let has_cron = args.get("cron").and_then(|v| v.as_str()).is_some();
    let has_run_at = args.get("run_at").and_then(|v| v.as_str()).is_some();
    if has_cron && has_run_at {
        return serde_json::json!({"error": "'cron' and 'run_at' are mutually exclusive"});
    }

    let new_job = match job_from_args(home, args) {
        Ok(job) => job,
        Err(e) => return serde_json::json!({"error": e}),
    };
    let new_message = args["message"].as_str().map(String::from);
    let new_target = args["instance"].as_str().map(String::from);
    let new_label = args["label"].as_str().map(String::from);
    let new_tz = args["timezone"].as_str().map(String::from);
    let new_enabled = args["enabled"].as_bool();
    let new_cron = args["cron"].as_str().map(String::from);
    let new_run_at = args["run_at"].as_str().map(String::from);
    // #1521: fire-strategy / linked task changes (validated against the
    // resulting state inside the store lock below). `Some(None)` for
    // `linked_task_id` means "clear"; key absent means "unchanged".
    let new_fire_strategy = match fire_strategy_from_args(args) {
        Ok(fs) => fs,
        Err(e) => return serde_json::json!({"error": e}),
    };
    let new_linked_task_id: Option<Option<String>> = args
        .get("linked_task_id")
        .map(|v| v.as_str().filter(|s| !s.is_empty()).map(String::from));

    if let Some(ref c) = new_cron {
        if let Err(e) = validate_cron(c) {
            return serde_json::json!({"error": e});
        }
    }

    match crate::store::mutate_versioned(&store_path(home), |store: &mut ScheduleStore| match store
        .schedules
        .iter_mut()
        .find(|s| s.id == id)
    {
        Some(schedule) => {
            if schedule.job.is_some() {
                validate_job_options(args).map_err(anyhow::Error::msg)?;
                if let Some(ref job) = new_job {
                    schedule.job = Some(job.clone());
                }
            } else if new_job.is_some() {
                return Err(anyhow::anyhow!(
                    "schedule mode is immutable; create a new job schedule"
                ));
            }
            let previous_fire_strategy = schedule.fire_strategy;
            // Materialize the effective key before applying message edits so a
            // migrated AGEND-AUTO row cannot silently change replacement scope.
            if schedule.job.is_none() && schedule.replacement_key.is_none() {
                schedule.replacement_key = auto_replacement_key(&schedule.message);
            }
            if new_target
                .as_deref()
                .is_some_and(|target| target != schedule.target)
                && schedule.replacement_key.is_some()
            {
                return Err(anyhow::anyhow!(
                    "a keyed schedule's target is immutable; create a replacement schedule instead"
                ));
            }
            if let Some(ref m) = new_message {
                schedule.message.clone_from(m);
            }
            if let Some(ref t) = new_target {
                schedule.target.clone_from(t);
            }
            if let Some(ref l) = new_label {
                schedule.label = Some(l.clone());
            }
            if let Some(ref tz) = new_tz {
                schedule.timezone.clone_from(tz);
            }
            if let Some(e) = new_enabled {
                let now = chrono::Utc::now().to_rfc3339();
                if e {
                    schedule.enable_at(&now);
                } else {
                    schedule.disable_at(DisabledReason::OperatorPaused, &now);
                }
            }
            if let Some(ref c) = new_cron {
                schedule.trigger = Trigger::Cron { expr: c.clone() };
            }
            if let Some(ref r) = new_run_at {
                let tz_for_parse = schedule.timezone.clone();
                let parsed = match parse_run_at(r, &tz_for_parse) {
                    Ok(p) => p,
                    Err(e) => return Err(anyhow::anyhow!(e)),
                };
                let at_utc = chrono::DateTime::parse_from_rfc3339(&parsed)
                    .map(|dt| dt.with_timezone(&chrono::Utc));
                if let Ok(at) = at_utc {
                    if at <= chrono::Utc::now() {
                        return Err(anyhow::anyhow!("run_at {r:?} must be in the future"));
                    }
                }
                schedule.trigger = Trigger::Once { at: parsed };
            }
            // #1521: apply fire-strategy / linked-task changes, then validate
            // the RESULTING combination (UntilSuccess ⇒ existing linked task).
            if let Some(fs) = new_fire_strategy {
                schedule.fire_strategy = fs;
            }
            if let Some(ref lt) = new_linked_task_id {
                // Re-pointing (or clearing) the task invalidates a prior
                // same-day success suppression.
                schedule.linked_task_id = lt.clone();
                schedule.last_success_date = None;
            }
            if previous_fire_strategy == FireStrategy::UntilSuccess
                && schedule.fire_strategy != FireStrategy::UntilSuccess
            {
                schedule.last_success_date = None;
            }
            if let Err(e) = validate_fire_strategy(
                home,
                schedule.fire_strategy,
                schedule.linked_task_id.as_deref(),
            ) {
                return Err(anyhow::anyhow!(e));
            }
            schedule.updated_at = chrono::Utc::now().to_rfc3339();
            Ok(true)
        }
        None => Ok(false),
    }) {
        Ok(true) => serde_json::json!({"id": id, "status": "updated"}),
        Ok(false) => serde_json::json!({"error": format!("schedule '{id}' not found")}),
        Err(e) => serde_json::json!({"error": format!("{e}")}),
    }
}

/// Record a schedule execution result. Called by daemon after cron trigger.
pub fn record_run(home: &Path, schedule_id: &str, status: &str) {
    let sid = schedule_id.to_string();
    let st = status.to_string();
    let _ = crate::store::mutate_versioned(&store_path(home), |store: &mut ScheduleStore| {
        if let Some(sched) = store.schedules.iter_mut().find(|s| s.id == sid) {
            sched.run_history.push(ScheduleRun {
                triggered_at: chrono::Utc::now().to_rfc3339(),
                status: st.clone(),
            });
            // Keep last 50 runs only
            if sched.run_history.len() > 50 {
                let excess = sched.run_history.len() - 50;
                sched.run_history.drain(..excess);
            }
        }
        Ok(())
    });
}

pub fn delete(home: &Path, args: &Value) -> Value {
    let id = match args["id"].as_str() {
        Some(i) => i.to_string(),
        None => return serde_json::json!({"error": "missing 'id'"}),
    };
    match crate::store::mutate_versioned(&store_path(home), |store: &mut ScheduleStore| {
        let before = store.schedules.len();
        store.schedules.retain(|s| s.id != id);
        Ok(store.schedules.len() < before)
    }) {
        Ok(true) => serde_json::json!({"id": id, "status": "deleted"}),
        Ok(false) => serde_json::json!({"error": format!("schedule '{id}' not found")}),
        Err(e) => serde_json::json!({"error": format!("{e}")}),
    }
}

const DISABLED_SCHEDULE_RETENTION_DAYS: i64 = 7;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ScheduleArchiveEntry {
    archived_at: String,
    schedule: Schedule,
}

fn schedule_gc_eligible(schedule: &Schedule) -> bool {
    if schedule.enabled {
        return false;
    }
    match schedule.disabled_reason.as_ref() {
        Some(
            DisabledReason::OneShotFired
            | DisabledReason::OneShotMissed
            | DisabledReason::OneShotReplayed
            | DisabledReason::OneShotStaleDropped
            | DisabledReason::Superseded { .. },
        ) => true,
        // A deleted target cannot be retargeted usefully for an already-spent
        // one-shot. Recurring rows remain protected for operator retargeting.
        Some(DisabledReason::TargetOrphaned { .. }) => {
            matches!(schedule.trigger, Trigger::Once { .. })
        }
        // Operator-paused, linked-task-repairable, and legacy/unproven rows are
        // deliberately outside automatic deletion authority.
        Some(DisabledReason::OperatorPaused | DisabledReason::LinkedTaskMissing { .. }) | None => {
            false
        }
    }
}

fn append_schedule_archive_durably(
    path: &Path,
    entries: &[ScheduleArchiveEntry],
) -> std::io::Result<()> {
    use std::io::{Read, Seek, Write};
    if entries.is_empty() {
        return Ok(());
    }
    let mut body = Vec::new();
    for entry in entries {
        serde_json::to_writer(&mut body, entry).map_err(std::io::Error::other)?;
        body.push(b'\n');
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)?;
    if file.metadata()?.len() > 0 {
        file.seek(std::io::SeekFrom::End(-1))?;
        let mut tail = [0u8; 1];
        file.read_exact(&mut tail)?;
        if tail[0] != b'\n' {
            // A prior crash may have left a partial JSON record. Start the
            // retry on a fresh line so the newly durable snapshot is readable.
            file.write_all(b"\n")?;
        }
    }
    file.write_all(&body)?;
    file.sync_all()?;
    crate::store::fsync_parent_dir_checked(path)?;
    Ok(())
}

fn read_schedule_archive(path: &Path) -> Vec<ScheduleArchiveEntry> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    content
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Archive and remove disabled historical schedules older than seven days.
///
/// The full typed row is fsync'd before the live store is changed. Removal uses
/// exact-row CAS semantics: if a concurrent update/re-enable changes the row
/// after candidate selection, the archived stale snapshot cannot authorize its
/// deletion. An exact archived snapshot from a prior partial pass is reusable,
/// making retries idempotent without duplicate archive lines.
pub fn gc_disabled_schedules(home: &Path) -> usize {
    gc_disabled_schedules_at(home, chrono::Utc::now())
}

fn gc_disabled_schedules_at(home: &Path, now: chrono::DateTime<chrono::Utc>) -> usize {
    gc_disabled_schedules_at_before_delete(home, now, || {})
}

fn gc_disabled_schedules_at_before_delete(
    home: &Path,
    now: chrono::DateTime<chrono::Utc>,
    before_delete: impl FnOnce(),
) -> usize {
    let cutoff = now - chrono::Duration::days(DISABLED_SCHEDULE_RETENTION_DAYS);
    let candidates: Vec<Schedule> = load(home)
        .schedules
        .into_iter()
        .filter(|schedule| {
            schedule_gc_eligible(schedule)
                && schedule
                    .disabled_at
                    .as_deref()
                    .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
                    .is_some_and(|at| at.with_timezone(&chrono::Utc) <= cutoff)
        })
        .collect();
    if candidates.is_empty() {
        return 0;
    }

    let archive_path = home.join("schedules-archive.jsonl");
    let archive_lock_path = home.join("schedules-archive.jsonl.lock");
    let _archive_lock = match crate::store::acquire_file_lock(&archive_lock_path) {
        Ok(lock) => lock,
        Err(error) => {
            tracing::warn!(error = %error, "schedule GC: archive lock failed; preserving rows");
            return 0;
        }
    };
    let mut archived = read_schedule_archive(&archive_path);
    let missing: Vec<ScheduleArchiveEntry> = candidates
        .iter()
        .filter(|candidate| !archived.iter().any(|entry| entry.schedule == **candidate))
        .cloned()
        .map(|schedule| ScheduleArchiveEntry {
            archived_at: now.to_rfc3339(),
            schedule,
        })
        .collect();
    if let Err(error) = append_schedule_archive_durably(&archive_path, &missing) {
        tracing::warn!(
            path = %archive_path.display(),
            error = %error,
            count = missing.len(),
            "schedule GC: durable archive failed; preserving all live rows"
        );
        return 0;
    }
    archived.extend(missing);
    before_delete();

    let mut removed_ids = Vec::new();
    let result = crate::store::mutate_versioned(&store_path(home), |store: &mut ScheduleStore| {
        store.schedules.retain(|schedule| {
            let exact_candidate = candidates.iter().any(|candidate| candidate == schedule);
            let exact_archive = archived.iter().any(|entry| entry.schedule == *schedule);
            if exact_candidate && exact_archive {
                removed_ids.push(schedule.id.clone());
                false
            } else {
                true
            }
        });
        Ok(())
    });
    if let Err(error) = result {
        tracing::warn!(error = %error, "schedule GC: live-store CAS failed; archived rows retained for retry");
        return 0;
    }
    for id in &removed_ids {
        crate::event_log::log(
            home,
            "schedule_archived",
            id,
            "full typed row durably archived before retention deletion",
        );
    }
    removed_ids.len()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
