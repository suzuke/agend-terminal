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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "ScheduleRaw")]
pub struct Schedule {
    pub id: String,
    pub trigger: Trigger,
    pub message: String,
    pub target: String,
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
        }
    }
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
    !task_id.is_empty() && crate::tasks::load_by_id(home, task_id).is_some()
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
            "fire_strategy=until_success requires an existing linked_task_id (task '{id}' not found)"
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// v2: `{trigger: {kind, ...}}` on each row, with `Once` variant.
    /// v1 rows are transparently upgraded on load via `ScheduleRaw::From`.
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
                if !sched.enabled {
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
                sched.enabled = false;
                sched.updated_at = now.to_rfc3339();
                if at_utc < cutoff {
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

/// Flip a schedule's `enabled` to false. Used by the daemon after a
/// one-shot fires so the row stays in the store for audit but will not
/// retrigger.
pub fn set_enabled(home: &Path, schedule_id: &str, enabled: bool) {
    let sid = schedule_id.to_string();
    persist_or_log!(
        crate::store::mutate_versioned(&store_path(home), |store: &mut ScheduleStore| {
            if let Some(sched) = store.schedules.iter_mut().find(|s| s.id == sid) {
                sched.enabled = enabled;
                sched.updated_at = chrono::Utc::now().to_rfc3339();
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
/// surviving instance. Idempotent: an already-disabled schedule is left
/// untouched (no duplicate `orphaned` marker), so a boot-time re-sweep doesn't
/// grow `run_history` unboundedly. Returns the number of schedules newly
/// orphaned.
pub fn orphan_schedules_for_target(home: &Path, deleted_target: &str) -> usize {
    let target = deleted_target.to_string();
    let mut orphaned = 0usize;
    persist_or_log!(
        crate::store::mutate_versioned(&store_path(home), |store: &mut ScheduleStore| {
            let now = chrono::Utc::now().to_rfc3339();
            for sched in store.schedules.iter_mut() {
                if sched.target != target || !sched.enabled {
                    continue;
                }
                sched.enabled = false;
                sched.updated_at = now.clone();
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

pub fn create(home: &Path, instance_name: &str, args: &Value) -> Value {
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
        target: args["instance"]
            .as_str()
            .unwrap_or(instance_name)
            .to_string(),
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
    };
    match crate::store::mutate_versioned(&store_path(home), |store: &mut ScheduleStore| {
        store.schedules.push(schedule);
        Ok(())
    }) {
        Ok(()) => serde_json::json!({"id": id, "status": "created"}),
        Err(e) => serde_json::json!({"error": format!("{e}")}),
    }
}

pub fn list(home: &Path, args: &Value) -> Value {
    let store = load(home);
    let target_filter = args["instance"].as_str();
    let filtered: Vec<_> = store
        .schedules
        .iter()
        .filter(|s| target_filter.is_none_or(|t| s.target == t))
        .collect();
    serde_json::json!({"schedules": filtered})
}

pub fn update(home: &Path, args: &Value) -> Value {
    let id = match args["id"].as_str() {
        Some(i) => i.to_string(),
        None => return serde_json::json!({"error": "missing 'id'"}),
    };

    // Pre-validate trigger change (if any) outside the store lock so
    // errors return without touching the file.
    let has_cron = args.get("cron").and_then(|v| v.as_str()).is_some();
    let has_run_at = args.get("run_at").and_then(|v| v.as_str()).is_some();
    if has_cron && has_run_at {
        return serde_json::json!({"error": "'cron' and 'run_at' are mutually exclusive"});
    }

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
                schedule.enabled = e;
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

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

    /// #1608: create a REAL task on the event-sourced board (so
    /// `tasks::load_by_id` finds it), NOT a `tasks/<id>.json` file. The old
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
            },
        )
        .expect("seed real task");
        // Sanity: the authoritative lookup the fix relies on must now resolve.
        assert!(
            crate::tasks::load_by_id(home, id).is_some(),
            "seeded task '{id}' must be visible to tasks::load_by_id"
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
        assert!(
            validate_fire_strategy(&home, FireStrategy::UntilSuccess, Some("t-ghost")).is_err()
        );
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
}
