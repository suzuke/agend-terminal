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

        let reg = agent::lock_registry(registry);
        // #1441: registry is UUID-keyed; resolve target name via fleet.yaml.
        let target_id = crate::fleet::resolve_uuid(home, target);
        // The inbox fallback below does self-IPC (`enqueue_with_idle_hint` →
        // `api::call` over the loopback socket). It MUST run outside the
        // registry-lock window: the API handler servicing the self-call needs
        // the same lock, so calling it here deadlocks the daemon tick. Record
        // the intent under the lock; execute the enqueue after `drop(reg)`.
        let mut deferred_inbox = false;
        let status = if fire.missed {
            // Daemon was down through the one-shot instant — don't silently
            // inject a stale message (could be a morning "stand-up" from
            // three days ago). Just mark it missed so the user can see it
            // in run_history, and let the auto-disable below retire it.
            "missed"
        } else if let Some(handle) = target_id.and_then(|id| reg.get(&id)) {
            match agent::inject_to_agent(handle, message.as_bytes(), false) {
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
        drop(reg);

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
}
