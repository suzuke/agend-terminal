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
        let now_local = now_utc.with_timezone(&tz);
        let last_check_local = last_check_utc.with_timezone(&tz);
        if !schedule
            .after(&last_check_local)
            .take(1)
            .any(|next| next <= now_local)
        {
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
