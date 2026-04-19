use crate::agent::{self, AgentRegistry};
use std::path::Path;

/// Check cron schedules and inject messages for due ones.
pub fn check_schedules(home: &Path, registry: &AgentRegistry) {
    use cron::Schedule;
    use std::str::FromStr;

    let store: serde_json::Value = match std::fs::read_to_string(home.join("schedules.json"))
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
    {
        Some(v) => v,
        None => return,
    };
    let schedules = match store["schedules"].as_array() {
        Some(s) => s,
        None => return,
    };

    let now_utc = chrono::Utc::now();
    let last_check_path = home.join(".schedule_last_check");
    let last_check_utc = std::fs::read_to_string(&last_check_path)
        .ok()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s.trim()).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .unwrap_or_else(|| now_utc - chrono::Duration::seconds(10));

    let mut any_triggered = false;
    for sched in schedules {
        if !sched["enabled"].as_bool().unwrap_or(true) {
            continue;
        }
        let cron_expr = match sched["cron"].as_str() {
            Some(c) => c,
            None => continue,
        };
        let full_expr = if cron_expr.split_whitespace().count() == 5 {
            format!("0 {cron_expr}")
        } else {
            cron_expr.to_string()
        };

        let schedule = match Schedule::from_str(&full_expr) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(cron = cron_expr, error = %e, "invalid cron");
                continue;
            }
        };

        // Evaluate the cron expression in the schedule's declared timezone
        // (falls back to detected local TZ, then UTC). Previously we always
        // compared against UTC, so "0 9 * * *" fired at 9 AM UTC regardless
        // of the user's intent, and a DST transition in the user's region
        // silently shifted the trigger by 1h. A tz-aware DateTime lets
        // chrono resolve the wall-clock → instant conversion correctly.
        // Explicit `match` rather than `unwrap_or_else(detect_timezone)` so
        // the &str borrowed from `store` (JSON Value) doesn't need to unify
        // with detect_timezone's &'static str — Rust would otherwise infer
        // `'static` for the Value borrow and blow up lifetimes.
        let tz_name: &str = match sched["timezone"].as_str().filter(|s| !s.is_empty()) {
            Some(s) => s,
            None => crate::schedules::detect_timezone(),
        };
        let tz: chrono_tz::Tz = match tz_name.parse() {
            Ok(t) => t,
            Err(_) => {
                tracing::warn!(
                    cron = cron_expr,
                    timezone = tz_name,
                    "unknown timezone, falling back to UTC"
                );
                chrono_tz::UTC
            }
        };
        if !is_due_in_tz(&schedule, tz, last_check_utc, now_utc) {
            continue;
        }

        let (sched_id, target) = (
            sched["id"].as_str().unwrap_or(""),
            sched["target"].as_str().unwrap_or(""),
        );
        let (message, label) = (
            sched["message"].as_str().unwrap_or(""),
            sched["label"].as_str().unwrap_or("(unnamed)"),
        );

        tracing::info!(label, target, message, "schedule triggered");
        crate::event_log::log(
            home,
            "schedule_trigger",
            target,
            &format!("{label}: {message}"),
        );

        let reg = agent::lock_registry(registry);
        let status = if let Some(handle) = reg.get(target) {
            match agent::inject_to_agent(handle, message.as_bytes()) {
                Ok(()) => "ok",
                Err(e) => {
                    tracing::warn!(error = %e, "schedule inject failed");
                    "inject_failed"
                }
            }
        } else {
            let _ = crate::inbox::enqueue(
                home,
                target,
                crate::inbox::InboxMessage {
                    from: "system:schedule".to_string(),
                    text: message.to_string(),
                    kind: Some("schedule".to_string()),
                    timestamp: now_utc.to_rfc3339(),
                },
            );
            "ok_inbox"
        };
        drop(reg);
        if !sched_id.is_empty() {
            crate::schedules::record_run(home, sched_id, status);
        }
        any_triggered = true;
    }

    if any_triggered || now_utc.signed_duration_since(last_check_utc).num_seconds() >= 10 {
        let _ = std::fs::write(&last_check_path, now_utc.to_rfc3339());
    }
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
    fn unknown_timezone_name_falls_back_to_utc() {
        // `check_schedules` uses `tz_name.parse::<chrono_tz::Tz>().unwrap_or(chrono_tz::UTC)`
        // — lock in that contract so a future rewrite can't silently start panicking
        // or treating the fallback as a different zone.
        let resolved: chrono_tz::Tz = "Not/A_Real_Zone"
            .parse::<chrono_tz::Tz>()
            .unwrap_or(chrono_tz::UTC);
        assert_eq!(resolved, chrono_tz::UTC);
        // And the empty string also cleanly falls back.
        let resolved: chrono_tz::Tz = "".parse::<chrono_tz::Tz>().unwrap_or(chrono_tz::UTC);
        assert_eq!(resolved, chrono_tz::UTC);
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
}
