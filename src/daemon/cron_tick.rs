use crate::agent::{self, AgentRegistry};
use crate::schedules::Trigger;
use std::path::Path;

/// Check schedules and inject messages for due ones.
///
/// Two trigger kinds are handled:
/// - `Cron` — fires every time the cron expression lands inside the window
///   `(last_check, now]`, evaluated in the schedule's declared timezone.
/// - `Once` — fires exactly once when its absolute `at` instant falls into
///   the window; after firing (or being detected as missed because the
///   daemon was down through `at`), the schedule is auto-disabled so it
///   never triggers again.
pub fn check_schedules(home: &Path, registry: &AgentRegistry) {
    use cron::Schedule;
    use std::str::FromStr;

    // Typed load normalises legacy v1 rows (top-level `cron` field) into
    // `Trigger::Cron` via `ScheduleRaw::From`, so this tick works against
    // both old and new files without a separate migration pass.
    let store = crate::schedules::load(home);
    if store.schedules.is_empty() {
        return;
    }

    let now_utc = chrono::Utc::now();
    let last_check_path = home.join(".schedule_last_check");
    let last_check_utc = std::fs::read_to_string(&last_check_path)
        .ok()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s.trim()).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .unwrap_or_else(|| now_utc - chrono::Duration::seconds(10));

    let mut any_triggered = false;
    for sched in &store.schedules {
        if !sched.enabled {
            continue;
        }

        // Resolve target timezone once — used by Cron dispatch and by
        // log/error messages. `Once` stores an absolute RFC 3339 instant
        // so it doesn't need tz to fire, but keeping one tz variable here
        // keeps the two branches symmetrical.
        let tz_name: &str = if sched.timezone.is_empty() {
            crate::schedules::detect_timezone()
        } else {
            sched.timezone.as_str()
        };
        let tz: chrono_tz::Tz = match tz_name.parse() {
            Ok(t) => t,
            Err(_) => {
                tracing::error!(
                    schedule = %sched.id,
                    timezone = tz_name,
                    "unknown timezone, skipping schedule"
                );
                continue;
            }
        };

        // Decide whether this schedule is due and whether firing it
        // consumes it (one-shot auto-disable). The outcome is a small
        // struct so the firing code below stays unified across kinds.
        let fire = match &sched.trigger {
            Trigger::Cron { expr } => {
                let full = if expr.split_whitespace().count() == 5 {
                    format!("0 {expr}")
                } else {
                    expr.clone()
                };
                match Schedule::from_str(&full) {
                    Ok(s) if is_due_in_tz(&s, tz, last_check_utc, now_utc) => Some(FireDecision {
                        one_shot: false,
                        missed: false,
                    }),
                    Ok(_) => None,
                    Err(e) => {
                        tracing::warn!(cron = %expr, error = %e, "invalid cron");
                        None
                    }
                }
            }
            Trigger::Once { at } => classify_once(at, last_check_utc, now_utc),
        };
        let Some(fire) = fire else { continue };

        // #1521: UntilSuccess fire-strategy — suppress re-fires once the linked
        // task is `done` for the current calendar day (schedule tz). The status
        // read mirrors dispatch_idle's lock-free, fail-open pattern. (`missed`
        // one-shots fall through to their normal missed-handling below.)
        if sched.fire_strategy == crate::schedules::FireStrategy::UntilSuccess && !fire.missed {
            let today = now_utc.with_timezone(&tz).format("%Y-%m-%d").to_string();
            if sched.last_success_date.as_deref() == Some(today.as_str()) {
                continue; // already succeeded today → suppress until tomorrow
            }
            match linked_task_gate(home, sched.linked_task_id.as_deref()) {
                TaskGate::Done => {
                    // First success today → stamp + suppress rest of day.
                    crate::schedules::mark_success_today(home, sched.id.as_str(), &today);
                    continue;
                }
                TaskGate::Missing => {
                    // Broken link — don't nag forever; disable + audit (NOT a
                    // silent degrade to Always).
                    tracing::warn!(
                        schedule = %sched.id,
                        task = ?sched.linked_task_id,
                        "#1521: until_success linked task missing — disabling schedule"
                    );
                    crate::schedules::record_run(home, sched.id.as_str(), "target_task_missing");
                    crate::schedules::set_enabled(home, sched.id.as_str(), false);
                    continue;
                }
                // Pending (task exists, not done) OR ReadError (transient/
                // malformed → fail-open) → fall through and fire the reminder.
                TaskGate::Pending | TaskGate::ReadError => {}
            }
        }

        let (sched_id, target) = (sched.id.as_str(), sched.target.as_str());
        let message = sched.message.as_str();
        let label = sched.label.as_deref().unwrap_or("(unnamed)");

        tracing::info!(label, target, message, "schedule triggered");
        crate::event_log::log(
            home,
            "schedule_trigger",
            target,
            &format!("{label}: {message}"),
        );

        // #1441: registry is UUID-keyed; resolve target name via fleet.yaml.
        let target_id = crate::fleet::resolve_uuid(home, target);
        // #1530/F1: snapshot the inject target under the registry lock, then
        // RELEASE the lock before the (up to 5s + payload-scaled) blocking PTY
        // write — the registry must not be held across inject. The inbox
        // fallback below also does self-IPC (`enqueue_with_idle_hint` →
        // `api::call` loopback), which the API handler can't service while we
        // hold the registry — so it too runs only after the lock is released.
        let inject_snap = {
            let reg = agent::lock_registry(registry);
            target_id
                .and_then(|id| reg.get(&id))
                .map(|h| (agent::InjectTarget::from_handle(h), h.name.to_string()))
        };
        let mut deferred_inbox = false;
        let status = if fire.missed {
            // Daemon was down through the one-shot instant — don't silently
            // inject a stale message (could be a morning "stand-up" from
            // three days ago). Just mark it missed so the user can see it
            // in run_history, and let the auto-disable below retire it.
            "missed"
        } else if let Some((tgt, name)) = inject_snap.as_ref() {
            match agent::inject_with_target_gated(tgt, name, message.as_bytes(), false) {
                Ok(()) => "ok",
                Err(e) => {
                    tracing::warn!(error = %e, "schedule inject failed");
                    "inject_failed"
                }
            }
        } else if crate::fleet::instance_is_known(home, target) {
            // Known fleet instance that simply isn't running right now. Defer
            // the self-IPC enqueue past the lock (see the `deferred_inbox`
            // note above) so it lands in the inbox for next time it checks.
            deferred_inbox = true;
            "ok_inbox"
        } else {
            // #1488 fail-safe: the target is NOT a known fleet instance —
            // a deleted/renamed/typo'd target (the #1441 routing change made
            // these fall through to the inbox fallback). Enqueuing would
            // create an orphan inbox nobody drains and feed the dangerous
            // self-IPC fallback that triggered the morning deadlock. Skip +
            // warn instead; the schedule row stays (cascade/boot-sweep
            // disables it), so this only fires until cleanup catches up.
            tracing::warn!(
                schedule = %sched_id,
                target,
                "#1488: schedule target is not a known instance — skipping fire (orphaned target)"
            );
            "skipped_unknown_target"
        };

        if deferred_inbox {
            let _ = crate::inbox::enqueue_with_idle_hint(
                home,
                target,
                crate::inbox::InboxMessage::new_system("system:schedule", "schedule", message),
            );
        }

        crate::schedules::record_run(home, sched_id, status);
        if fire.one_shot {
            // Even on inject_failed we disable: one-shots are not retry-
            // safe (the window already rolled forward). The user can
            // re-create with a new run_at if they want another attempt.
            crate::schedules::set_enabled(home, sched_id, false);
        }
        any_triggered = true;
    }

    if any_triggered || now_utc.signed_duration_since(last_check_utc).num_seconds() >= 10 {
        let _ = crate::store::atomic_write(&last_check_path, now_utc.to_rfc3339().as_bytes());
    }
}

/// Outcome of deciding that a schedule is due. `one_shot` means "auto-
/// disable after firing"; `missed` means "the firing instant was before
/// last_check — record as missed rather than injecting a stale message".
struct FireDecision {
    one_shot: bool,
    missed: bool,
}

/// #1521: the linked task's gate state for an `UntilSuccess` schedule.
enum TaskGate {
    /// Current status is `done` → suppress (and stamp the success day).
    Done,
    /// Task exists but isn't done → fire the reminder.
    Pending,
    /// No task file (deleted/never-existed) → disable the schedule.
    Missing,
    /// File present but unreadable / malformed status → fail-open (fire).
    ReadError,
}

/// #1521: read a linked task's current status the same lock-free, fail-open way
/// the dispatch-idle watchdog does (`tasks/{id}.json` direct read; the board
/// writes per-task files). Current-status gate (not ever-done), so a reopened
/// task resumes reminders. `NotFound` is `Missing` (a removed/typo'd link);
/// any other I/O or parse error is `ReadError` → fail-open so a transient
/// hiccup never silently drops the reminder.
fn linked_task_gate(home: &Path, task_id: Option<&str>) -> TaskGate {
    let Some(id) = task_id.filter(|s| !s.is_empty()) else {
        // UntilSuccess is validated to have a task at create/update, so a
        // None here means the link was lost — treat as Missing (disable).
        return TaskGate::Missing;
    };
    let path = home.join("tasks").join(format!("{id}.json"));
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return TaskGate::Missing,
        Err(_) => return TaskGate::ReadError,
    };
    match serde_json::from_str::<serde_json::Value>(&content) {
        Ok(v) => match v.get("status").and_then(|s| s.as_str()) {
            Some("done") => TaskGate::Done,
            Some(_) => TaskGate::Pending,
            None => TaskGate::ReadError,
        },
        Err(_) => TaskGate::ReadError,
    }
}

/// Classify a `Once` trigger against the current window.
/// - `at` inside `(last_check, now]` → fire normally (`one_shot=true`).
/// - `at` before `last_check` → missed (one_shot=true, missed=true).
/// - `at` after `now` → not due yet.
/// - `at` unparseable → warn and treat as not due; the schedule sticks
///   around so the user can fix it via `update_schedule`.
fn classify_once(
    at: &str,
    last_check_utc: chrono::DateTime<chrono::Utc>,
    now_utc: chrono::DateTime<chrono::Utc>,
) -> Option<FireDecision> {
    let at_utc = match chrono::DateTime::parse_from_rfc3339(at) {
        Ok(dt) => dt.with_timezone(&chrono::Utc),
        Err(e) => {
            tracing::warn!(run_at = %at, error = %e, "invalid one-shot run_at");
            return None;
        }
    };
    if at_utc > now_utc {
        return None;
    }
    if at_utc <= last_check_utc {
        return Some(FireDecision {
            one_shot: true,
            missed: true,
        });
    }
    Some(FireDecision {
        one_shot: true,
        missed: false,
    })
}

/// Return true if the cron `schedule` would fire at least once in the
/// half-open window `(last_check_utc, now_utc]` when evaluated in `tz`.
///
/// Extracted from `check_schedules` so DST/tz behaviour is unit-testable
/// without a real registry or filesystem. Caller is responsible for logging
/// `invalid cron` / `unknown timezone` warnings before reaching this helper.
pub(crate) fn is_due_in_tz(
    schedule: &cron::Schedule,
    tz: chrono_tz::Tz,
    last_check_utc: chrono::DateTime<chrono::Utc>,
    now_utc: chrono::DateTime<chrono::Utc>,
) -> bool {
    let now_local = now_utc.with_timezone(&tz);
    let last_check_local = last_check_utc.with_timezone(&tz);
    schedule
        .after(&last_check_local)
        .take(1)
        .any(|next| next <= now_local)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use cron::Schedule;
    use std::str::FromStr;

    fn cron(expr: &str) -> Schedule {
        // Match `check_schedules` normalisation: 5-field → prepend seconds.
        let full = if expr.split_whitespace().count() == 5 {
            format!("0 {expr}")
        } else {
            expr.to_string()
        };
        Schedule::from_str(&full).expect("parse cron")
    }

    #[test]
    fn utc_fires_when_cron_hour_crossed() {
        // "0 9 * * *" — daily at 09:00. Window straddles 09:00 UTC.
        let schedule = cron("0 9 * * *");
        let last = Utc.with_ymd_and_hms(2026, 4, 19, 8, 59, 55).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 4, 19, 9, 0, 5).unwrap();
        assert!(is_due_in_tz(&schedule, chrono_tz::UTC, last, now));
    }

    #[test]
    fn utc_does_not_fire_before_cron_hour() {
        let schedule = cron("0 9 * * *");
        let last = Utc.with_ymd_and_hms(2026, 4, 19, 8, 58, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 4, 19, 8, 59, 0).unwrap();
        assert!(!is_due_in_tz(&schedule, chrono_tz::UTC, last, now));
    }

    #[test]
    fn taipei_fires_at_local_9am_which_is_0100_utc() {
        // Asia/Taipei = UTC+8, no DST. "0 9 * * *" local == 01:00 UTC.
        let schedule = cron("0 9 * * *");
        let last = Utc.with_ymd_and_hms(2026, 4, 19, 0, 59, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 4, 19, 1, 1, 0).unwrap();
        assert!(is_due_in_tz(&schedule, chrono_tz::Asia::Taipei, last, now));
    }

    #[test]
    fn taipei_does_not_fire_at_0900_utc() {
        // In Taipei, 09:00 UTC is already 17:00 local — past the "0 9 * * *" hour.
        // A narrow UTC window around 09:00 UTC should NOT trigger the local-9am job.
        let schedule = cron("0 9 * * *");
        let last = Utc.with_ymd_and_hms(2026, 4, 19, 8, 59, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 4, 19, 9, 1, 0).unwrap();
        assert!(!is_due_in_tz(&schedule, chrono_tz::Asia::Taipei, last, now));
    }

    #[test]
    fn la_dst_spring_forward_fires_at_pdt_wall_clock() {
        // US DST forward: 2026-03-08 at 02:00 local becomes 03:00 PDT.
        // A "0 9 * * *" schedule should still fire at 09:00 PDT on that day,
        // which is 16:00 UTC (PDT = UTC-7), not 17:00 UTC (PST = UTC-8).
        let schedule = cron("0 9 * * *");
        let last = Utc.with_ymd_and_hms(2026, 3, 8, 15, 59, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 3, 8, 16, 1, 0).unwrap();
        assert!(is_due_in_tz(
            &schedule,
            chrono_tz::America::Los_Angeles,
            last,
            now
        ));
        // At 17:00 UTC on the same day, the PDT-aware cron has already fired,
        // so a window sliding one hour later should NOT trigger again.
        let last = Utc.with_ymd_and_hms(2026, 3, 8, 16, 59, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 3, 8, 17, 1, 0).unwrap();
        assert!(!is_due_in_tz(
            &schedule,
            chrono_tz::America::Los_Angeles,
            last,
            now
        ));
    }

    #[test]
    fn la_dst_fall_back_fires_once_at_pst_wall_clock() {
        // US DST backward: 2026-11-01 at 02:00 local rewinds to 01:00 PST.
        // "0 9 * * *" on that day should fire once, at 09:00 PST = 17:00 UTC.
        let schedule = cron("0 9 * * *");
        let last = Utc.with_ymd_and_hms(2026, 11, 1, 16, 59, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 11, 1, 17, 1, 0).unwrap();
        assert!(is_due_in_tz(
            &schedule,
            chrono_tz::America::Los_Angeles,
            last,
            now
        ));
        // The previous-day window at 16:00 UTC (which would be PDT) must not
        // trigger — that instant is 09:00 PDT, but the day rolled to PST at
        // 02:00 local, i.e. 09:00 UTC on 2026-11-01.
        let last = Utc.with_ymd_and_hms(2026, 11, 1, 15, 59, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 11, 1, 16, 1, 0).unwrap();
        assert!(!is_due_in_tz(
            &schedule,
            chrono_tz::America::Los_Angeles,
            last,
            now
        ));
    }

    #[test]
    fn six_field_cron_is_accepted() {
        // 6-field cron (with seconds) should parse without the auto-prepend.
        let schedule = cron("30 0 9 * * *"); // 09:00:30 daily
        let last = Utc.with_ymd_and_hms(2026, 4, 19, 9, 0, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 4, 19, 9, 1, 0).unwrap();
        assert!(is_due_in_tz(&schedule, chrono_tz::UTC, last, now));
    }

    #[test]
    fn unknown_timezone_name_is_rejected() {
        // Invalid timezone names must fail to parse so check_schedules
        // skips them rather than silently falling back to UTC.
        assert!("Not/A_Real_Zone".parse::<chrono_tz::Tz>().is_err());
        assert!("".parse::<chrono_tz::Tz>().is_err());
    }

    #[test]
    fn window_too_narrow_does_not_fire_even_at_boundary() {
        // If neither endpoint crosses the scheduled instant, no fire.
        let schedule = cron("0 9 * * *");
        let last = Utc.with_ymd_and_hms(2026, 4, 19, 9, 0, 1).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 4, 19, 9, 0, 2).unwrap();
        // 09:00:00 already passed before `last`, next trigger tomorrow.
        assert!(!is_due_in_tz(&schedule, chrono_tz::UTC, last, now));
    }

    // --- One-shot classification ---

    #[test]
    fn once_fires_when_at_inside_window() {
        let last = Utc.with_ymd_and_hms(2026, 4, 20, 14, 29, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 4, 20, 14, 31, 0).unwrap();
        let at = "2026-04-20T14:30:00+00:00";
        let fire = classify_once(at, last, now).expect("fire");
        assert!(fire.one_shot);
        assert!(!fire.missed);
    }

    #[test]
    fn once_missed_when_at_before_last_check() {
        let last = Utc.with_ymd_and_hms(2026, 4, 20, 15, 0, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 4, 20, 15, 1, 0).unwrap();
        let at = "2026-04-20T14:30:00+00:00";
        let fire = classify_once(at, last, now).expect("fire");
        assert!(fire.one_shot);
        assert!(fire.missed);
    }

    #[test]
    fn once_skipped_when_at_in_future() {
        let last = Utc.with_ymd_and_hms(2026, 4, 20, 14, 0, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 4, 20, 14, 5, 0).unwrap();
        let at = "2026-04-20T14:30:00+00:00";
        assert!(classify_once(at, last, now).is_none());
    }

    #[test]
    fn once_at_offset_zone_resolves_correctly() {
        // run_at stored as +08:00; matching UTC instant is 8h earlier.
        let last = Utc.with_ymd_and_hms(2026, 4, 20, 6, 29, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 4, 20, 6, 31, 0).unwrap();
        let at = "2026-04-20T14:30:00+08:00"; // = 06:30 UTC
        let fire = classify_once(at, last, now).expect("fire");
        assert!(!fire.missed);
    }

    #[test]
    fn once_unparseable_at_does_not_fire() {
        let last = Utc.with_ymd_and_hms(2026, 4, 20, 14, 0, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 4, 20, 15, 0, 0).unwrap();
        assert!(classify_once("not a date", last, now).is_none());
    }

    // ── #1488 fail-safe: don't fire a schedule whose target is a ghost ──

    fn cron_tmp_home(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static C: AtomicU32 = AtomicU32::new(0);
        let id = C.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-1488-cron-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn empty_registry() -> crate::agent::AgentRegistry {
        std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()))
    }

    fn seed_oneshot(home: &std::path::Path, target: &str) {
        let at = (chrono::Utc::now() - chrono::Duration::seconds(2)).to_rfc3339();
        let store = serde_json::json!({
            "schema_version": 2,
            "schedules": [{
                "id": "s-1488", "message": "ping", "target": target,
                "trigger": {"kind": "once", "at": at}, "enabled": true,
                "timezone": "UTC", "label": "t",
                "created_at": at, "updated_at": at, "run_history": []
            }]
        });
        std::fs::write(
            home.join("schedules.json"),
            serde_json::to_string_pretty(&store).unwrap(),
        )
        .unwrap();
    }

    fn last_status(home: &std::path::Path) -> String {
        crate::schedules::load(home).schedules[0]
            .run_history
            .last()
            .map(|r| r.status.clone())
            .unwrap_or_default()
    }

    #[test]
    fn schedule_targeting_unknown_instance_is_skipped_not_enqueued() {
        let home = cron_tmp_home("ghost");
        // No fleet.yaml → "ghost" is not a known instance.
        seed_oneshot(&home, "ghost");
        check_schedules(&home, &empty_registry());
        assert_eq!(
            last_status(&home),
            "skipped_unknown_target",
            "fire to a ghost target must be skipped"
        );
        assert!(
            !home.join("inbox").join("ghost.jsonl").exists(),
            "no orphan inbox file may be created for a ghost target"
        );
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn schedule_targeting_known_offline_instance_enqueues_to_inbox() {
        let home = cron_tmp_home("known");
        // fleet.yaml declares "offline" (present but not in the registry).
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  offline:\n    backend: claude\n",
        )
        .unwrap();
        seed_oneshot(&home, "offline");
        check_schedules(&home, &empty_registry());
        assert_eq!(
            last_status(&home),
            "ok_inbox",
            "a known but offline target must still get an inbox enqueue"
        );
        std::fs::remove_dir_all(home).ok();
    }

    // ── #1521: UntilSuccess fire-strategy ──

    fn seed_task(home: &std::path::Path, id: &str, status: &str) {
        let dir = home.join("tasks");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{id}.json")),
            serde_json::json!({"id": id, "status": status}).to_string(),
        )
        .unwrap();
    }

    /// Seed an enabled `* * * * *` (always-due) cron schedule targeting a known
    /// offline instance (so a fire lands as "ok_inbox"), with #1521 fields.
    fn seed_until_success_cron(
        home: &std::path::Path,
        linked_task_id: &str,
        last_success_date: Option<&str>,
    ) {
        std::fs::write(
            crate::fleet::fleet_yaml_path(home),
            "instances:\n  offline:\n    backend: claude\n",
        )
        .unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        let mut row = serde_json::json!({
            "id": "s-1521", "message": "remind", "target": "offline",
            // 6-field every-second cron → reliably due in any check window.
            "trigger": {"kind": "cron", "expr": "* * * * * *"}, "enabled": true,
            "timezone": "UTC", "label": "r",
            "created_at": now, "updated_at": now, "run_history": [],
            "fire_strategy": "until_success", "linked_task_id": linked_task_id,
        });
        if let Some(d) = last_success_date {
            row["last_success_date"] = serde_json::json!(d);
        }
        let store = serde_json::json!({"schema_version": 2, "schedules": [row]});
        std::fs::write(
            home.join("schedules.json"),
            serde_json::to_string_pretty(&store).unwrap(),
        )
        .unwrap();
    }

    fn today_utc() -> String {
        chrono::Utc::now().format("%Y-%m-%d").to_string()
    }

    fn sched0(home: &std::path::Path) -> crate::schedules::Schedule {
        crate::schedules::load(home)
            .schedules
            .into_iter()
            .next()
            .unwrap()
    }

    #[test]
    fn until_success_done_suppresses_and_stamps() {
        let home = cron_tmp_home("us-done");
        seed_until_success_cron(&home, "t-done", None);
        seed_task(&home, "t-done", "done");
        check_schedules(&home, &empty_registry());
        let s = sched0(&home);
        assert!(s.run_history.is_empty(), "done task → no fire");
        assert_eq!(
            s.last_success_date.as_deref(),
            Some(today_utc().as_str()),
            "first success today must be stamped"
        );
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn until_success_pending_fires() {
        let home = cron_tmp_home("us-pending");
        seed_until_success_cron(&home, "t-open", None);
        seed_task(&home, "t-open", "in_progress");
        check_schedules(&home, &empty_registry());
        assert_eq!(
            last_status(&home),
            "ok_inbox",
            "unfinished task → fire reminder"
        );
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn until_success_missing_task_disables_schedule() {
        let home = cron_tmp_home("us-missing");
        seed_until_success_cron(&home, "t-gone", None); // no task file seeded
        check_schedules(&home, &empty_registry());
        let s = sched0(&home);
        assert_eq!(last_status(&home), "target_task_missing");
        assert!(!s.enabled, "missing linked task must disable the schedule");
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn until_success_already_succeeded_today_suppresses_without_reading_task() {
        let home = cron_tmp_home("us-today");
        // last_success_date == today + NO task file: must still suppress (the
        // same-day short-circuit fires before any task read).
        seed_until_success_cron(&home, "t-x", Some(&today_utc()));
        check_schedules(&home, &empty_registry());
        assert!(
            sched0(&home).run_history.is_empty(),
            "already-succeeded-today → suppress"
        );
        assert!(
            sched0(&home).enabled,
            "must not disable on the same-day short-circuit"
        );
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn until_success_cross_day_refires_and_reopen_resumes() {
        let home = cron_tmp_home("us-crossday");
        // Succeeded yesterday; today the task is reopened (in_progress) → fire.
        seed_until_success_cron(&home, "t-reopen", Some("2020-01-01"));
        seed_task(&home, "t-reopen", "in_progress");
        check_schedules(&home, &empty_registry());
        assert_eq!(
            last_status(&home),
            "ok_inbox",
            "stale (yesterday) success must not suppress today; reopened task re-fires"
        );
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn linked_task_gate_classifies_status() {
        let home = cron_tmp_home("gate");
        seed_task(&home, "d", "done");
        seed_task(&home, "p", "open");
        std::fs::write(home.join("tasks").join("bad.json"), "{not json").unwrap();
        assert!(matches!(linked_task_gate(&home, Some("d")), TaskGate::Done));
        assert!(matches!(
            linked_task_gate(&home, Some("p")),
            TaskGate::Pending
        ));
        assert!(matches!(
            linked_task_gate(&home, Some("nope")),
            TaskGate::Missing
        ));
        assert!(matches!(
            linked_task_gate(&home, Some("bad")),
            TaskGate::ReadError
        ));
        assert!(matches!(linked_task_gate(&home, None), TaskGate::Missing));
        std::fs::remove_dir_all(home).ok();
    }
}
