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
        }
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

    let _ = crate::store::mutate_versioned(&store_path(home), |store: &mut ScheduleStore| {
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
    });
    to_replay
}

/// Flip a schedule's `enabled` to false. Used by the daemon after a
/// one-shot fires so the row stays in the store for audit but will not
/// retrigger.
pub fn set_enabled(home: &Path, schedule_id: &str, enabled: bool) {
    let sid = schedule_id.to_string();
    let _ = crate::store::mutate_versioned(&store_path(home), |store: &mut ScheduleStore| {
        if let Some(sched) = store.schedules.iter_mut().find(|s| s.id == sid) {
            sched.enabled = enabled;
            sched.updated_at = chrono::Utc::now().to_rfc3339();
        }
        Ok(())
    });
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
    let now = chrono::Utc::now().to_rfc3339();
    let id = format!("s-{}", &now[..19].replace([':', '-', 'T'], ""));
    let schedule = Schedule {
        id: id.clone(),
        trigger,
        message: message.to_string(),
        target: args["target"].as_str().unwrap_or(instance_name).to_string(),
        label: args["label"].as_str().map(String::from),
        timezone,
        enabled: true,
        created_by: instance_name.to_string(),
        created_at: now.clone(),
        updated_at: now,
        run_history: Vec::new(),
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
    let target_filter = args["target"].as_str();
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
    let new_target = args["target"].as_str().map(String::from);
    let new_label = args["label"].as_str().map(String::from);
    let new_tz = args["timezone"].as_str().map(String::from);
    let new_enabled = args["enabled"].as_bool();
    let new_cron = args["cron"].as_str().map(String::from);
    let new_run_at = args["run_at"].as_str().map(String::from);

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
            &serde_json::json!({"cron": "0 9 * * *", "message": "m1", "target": "agent1"}),
        );
        create(
            &home,
            "a",
            &serde_json::json!({"cron": "0 10 * * *", "message": "m2", "target": "agent2"}),
        );

        let listed = list(&home, &serde_json::json!({"target": "agent1"}));
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
        assert_eq!(replayed.len(), 1, "missed one-shot within 24h must be replayed");
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
        assert!(replayed.is_empty(), "stale one-shot (>24h) must NOT be replayed");

        // Schedule must still be disabled
        let listed = list(&home, &serde_json::json!({}));
        assert_eq!(listed["schedules"][0]["enabled"], false);
        // run_history should record stale_dropped
        let history = listed["schedules"][0]["run_history"].as_array().expect("arr");
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
                crate::inbox::InboxMessage {
                    schema_version: 0,
                    id: None,
                    from: "system:schedule".to_string(),
                    text: sched.message.clone(),
                    kind: Some("schedule_replay".to_string()),
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    read_at: None,
                },
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
        let history = listed["schedules"][0]["run_history"].as_array().expect("arr");
        assert_eq!(history[0]["status"], "replayed");

        std::fs::remove_dir_all(&home).ok();
    }
}
