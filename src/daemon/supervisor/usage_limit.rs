//! Usage-limit notify-dedup persistence (split from supervisor.rs).

use std::time::Duration;

/// #1894: null-unlock_at usage_limit re-notify window. When the pane had no
/// parseable "try again at <time>" at detection (e.g. it was showing
/// poll-reminders), `parse_unlock_at` records `unlock_at: null` and the dedup
/// falls back to a TIMESTAMP cooldown. #1861/#1864 used the 60s `NOTIFY_COOLDOWN`
/// here, which restarts hours apart trivially exceed → the operator was re-paged
/// on every boot for the SAME ongoing limit. A real usage-limit episode lasts
/// hours-to-days, so suppress for a long window: a multi-day limit re-notifies at
/// most ~once/day instead of once/restart. Parseable-unlock_at suppression
/// (#1864) is unchanged, and a missing/corrupt record still FAILS OPEN (notify).
const NULL_UNLOCK_NOTIFY_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

/// #1861: persisted usage_limit notify dedup record (one per member). A daemon
/// restart wipes the in-mem `NotifyTrack` cooldown, so without persistence the
/// operator is re-notified of the SAME usage limit on every restart (the backend
/// boots `Starting` → re-detects UsageLimit).
#[derive(serde::Serialize, serde::Deserialize, Default)]
pub(crate) struct UsageNotifyRecord {
    /// Parsed "HH:MM" unlock string at notify time (None if the pane had no
    /// parseable reset time).
    unlock_at: Option<String>,
    /// When we notified (rfc3339 UTC) — anchors the unlock deadline + the
    /// null-unlock fallback cooldown.
    notified_at: String,
}

pub(crate) fn usage_limit_notify_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join("usage_limit_notify.json")
}

/// #1906: drop one agent's usage-limit notify-dedup entry on delete, so a
/// same-name redeploy does NOT inherit stale suppression and silently eat its
/// first real usage_limit notify (until the #1894/#1895 stale-unlock window).
/// Mirrors `escalation_persist::remove` (#1680 stale-state-on-redeploy class).
/// Locked RMW via `with_json_state`; no-op when the store is absent.
pub(crate) fn remove_usage_limit_notify(home: &std::path::Path, name: &str) {
    let path = usage_limit_notify_path(home);
    if !path.exists() {
        return;
    }
    let _ = crate::store::with_json_state::<
        std::collections::HashMap<String, UsageNotifyRecord>,
        _,
        _,
    >(&path, |map| {
        map.remove(name);
    });
}

/// #1906: does the usage-limit notify-dedup store still hold `name`? For the
/// `full_delete_instance` residual audit (this store was a teardown blind spot).
pub(crate) fn usage_limit_notify_has(home: &std::path::Path, name: &str) -> bool {
    std::fs::read_to_string(usage_limit_notify_path(home))
        .ok()
        .and_then(|s| {
            serde_json::from_str::<std::collections::HashMap<String, UsageNotifyRecord>>(&s).ok()
        })
        .is_some_and(|m| m.contains_key(name))
}

/// The UTC instant an `HH:MM` unlock window elapses, anchored to `notified_at`
/// (the next occurrence of HH:MM at-or-after the notify, treated as UTC since the
/// pane renders e.g. "Resets at 15:14 UTC"). `None` if unparseable.
pub(crate) fn unlock_deadline(
    hhmm: &str,
    notified_at: chrono::DateTime<chrono::Utc>,
) -> Option<chrono::DateTime<chrono::Utc>> {
    use chrono::TimeZone;
    let (h, m) = hhmm.split_once(':')?;
    let h: u32 = h.trim().parse().ok()?;
    let m: u32 = m.trim().parse().ok()?;
    let naive = notified_at.date_naive().and_hms_opt(h, m, 0)?;
    let candidate = chrono::Utc.from_utc_datetime(&naive);
    Some(if candidate >= notified_at {
        candidate
    } else {
        candidate + chrono::Duration::days(1)
    })
}

/// #2127 Phase 1: time until `name`'s usage-limit window unlocks, per the
/// persisted notify record. `None` when there is no record, no parseable
/// `unlock_at`, or an unparseable `notified_at` — the reclaim caller then falls
/// back to a conservative long default (a missing reset time is treated as a long
/// block). A past deadline clamps to `Duration::ZERO`. Lock-free read; reuses the
/// same record + `unlock_deadline` math as the notify-suppression path so the two
/// agree on what "this window" means.
pub(crate) fn usage_limit_remaining(
    home: &std::path::Path,
    name: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<std::time::Duration> {
    let map: std::collections::HashMap<String, UsageNotifyRecord> =
        std::fs::read_to_string(usage_limit_notify_path(home))
            .ok()
            .and_then(|c| serde_json::from_str(&c).ok())?;
    let rec = map.get(name)?;
    let unlock_at = rec.unlock_at.as_deref()?;
    let notified_at = chrono::DateTime::parse_from_rfc3339(&rec.notified_at)
        .ok()?
        .with_timezone(&chrono::Utc);
    let deadline = unlock_deadline(unlock_at, notified_at)?;
    Some(
        (deadline - now)
            .to_std()
            .unwrap_or(std::time::Duration::ZERO),
    )
}

/// True ⇒ suppress this usage_limit notify (already notified for the same still-
/// open window). Lock-free FAIL-OPEN read: a missing/corrupt record ⇒ NOT
/// suppressed (notify), so a real new limit is never silently swallowed.
pub(crate) fn usage_limit_notify_suppressed(
    home: &std::path::Path,
    name: &str,
    unlock_at: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    let map: std::collections::HashMap<String, UsageNotifyRecord> =
        std::fs::read_to_string(usage_limit_notify_path(home))
            .ok()
            .and_then(|c| serde_json::from_str(&c).ok())
            .unwrap_or_default();
    let Some(rec) = map.get(name) else {
        return false;
    };
    let Ok(notified_at) = chrono::DateTime::parse_from_rfc3339(&rec.notified_at) else {
        return false;
    };
    let notified_at = notified_at.with_timezone(&chrono::Utc);
    match unlock_at {
        // Different unlock window string ⇒ a NEW limit ⇒ notify.
        Some(u) if rec.unlock_at.as_deref() != Some(u) => false,
        // Same window: suppress until its deadline passes (then the limit reset →
        // notify again). Unparseable deadline ⇒ conservatively suppress (an
        // identical string is a strong same-window signal).
        Some(u) => unlock_deadline(u, notified_at).is_none_or(|deadline| now < deadline),
        // No parseable reset time ⇒ persisted-timestamp window (NOT the in-session
        // Instant cooldown a restart wipes). #1894: use the long
        // `NULL_UNLOCK_NOTIFY_WINDOW` (24h) instead of the 60s `NOTIFY_COOLDOWN`,
        // so restarts WITHIN the same ongoing usage-limit episode (which the
        // operator hit repeatedly) stay silent. A genuinely-new episode > the
        // window later re-notifies.
        None => {
            now.signed_duration_since(notified_at)
                < chrono::Duration::from_std(NULL_UNLOCK_NOTIFY_WINDOW)
                    .unwrap_or_else(|_| chrono::Duration::hours(24))
        }
    }
}

/// Persist that we notified `name` for `unlock_at` at `now` (locked RMW).
pub(crate) fn record_usage_limit_notified(
    home: &std::path::Path,
    name: &str,
    unlock_at: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    let record = UsageNotifyRecord {
        unlock_at: unlock_at.map(String::from),
        notified_at: now.to_rfc3339(),
    };
    match crate::store::with_json_state_or_create(
        &usage_limit_notify_path(home),
        std::collections::HashMap::<String, UsageNotifyRecord>::new,
        |map| {
            map.insert(name.to_string(), record);
        },
    ) {
        Ok(()) => true,
        Err(error) => {
            tracing::error!(%error, agent = %name, "usage_limit notify ledger write failed");
            false
        }
    }
}
