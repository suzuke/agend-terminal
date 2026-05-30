//! #1491(A) inbox-stuck watchdog.
//!
//! Detects an agent that is RECEIVING messages but not DRAINING its inbox.
//! This is orthogonal to the idle watchdog: idle triggers on *output silence*
//! (the agent stopped producing), while this triggers on *inbox accumulation*
//! (messages pile up unread regardless of what the agent is doing). The gap it
//! closes is real — a reviewer that keeps looping/producing output but never
//! reads its inbox sat on unread review handoffs all night, undetected by the
//! idle watchdog because it was never "silent".
//!
//! Detection ONLY — it never auto-restarts the agent. The cause may be
//! transient (a rate-limit pause or an auto-compact that self-heals), so the
//! lead is notified to decide whether to nudge / restart or wait it out.

use std::collections::HashMap;
use std::path::Path;

/// Minimum unread inbox messages before an agent is a stuck candidate. A
/// single transient message doesn't qualify — we want genuine accumulation.
const MIN_UNREAD: usize = 3;
/// The oldest unread message must be at least this old (minutes) before we
/// alert. Generous so an agent legitimately heads-down on a long task isn't
/// flagged for not checking its inbox for a few minutes.
const STUCK_AFTER_MINS: i64 = 30;
/// Re-alert dedup window (minutes): don't renotify the lead more often than
/// this for the same still-stuck agent.
const REALERT_AFTER_MINS: i64 = 60;
/// Fallback alert recipient when the stuck agent isn't in any team (so no
/// orchestrator can be resolved). Matches the idle watchdog's default.
const FALLBACK_RECIPIENT: &str = "lead";

/// Scan every fleet instance and alert the lead about any that is sitting on a
/// pile of unread inbox messages. `last_alerted` is owned by the caller (the
/// per-tick handler) so dedup state survives across ticks; `now` is injected
/// for deterministic tests.
pub(crate) fn scan_and_emit(
    home: &Path,
    now: &chrono::DateTime<chrono::Utc>,
    last_alerted: &mut HashMap<String, chrono::DateTime<chrono::Utc>>,
) {
    let Ok(fleet) = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)) else {
        return;
    };
    for agent in fleet.instances.keys() {
        let (unread, oldest) = crate::inbox::unread_count(home, agent);
        if unread < MIN_UNREAD {
            continue;
        }
        let Some(oldest) = oldest else { continue };
        let age_min = now.signed_duration_since(oldest).num_minutes();
        if age_min < STUCK_AFTER_MINS {
            continue;
        }
        // Dedup: skip if we already alerted about this agent recently.
        if let Some(prev) = last_alerted.get(agent) {
            if now.signed_duration_since(*prev).num_minutes() < REALERT_AFTER_MINS {
                continue;
            }
        }
        // Notify the agent's team orchestrator (the lead). Never notify the
        // stuck agent about itself — it can't act on an alert it isn't reading.
        let recipient =
            orchestrator_for(&fleet, agent).unwrap_or_else(|| FALLBACK_RECIPIENT.to_string());
        if recipient == *agent {
            continue;
        }
        let text = format!(
            "[inbox_stuck_watchdog] agent '{agent}' has {unread} unread inbox messages, \
             oldest {age_min}min old (thresholds: {MIN_UNREAD} msgs / {STUCK_AFTER_MINS}min). \
             It appears to be receiving but not draining its inbox — stuck, distinct from idle \
             (output-silence). NOT auto-restarting: this may be a transient stall (rate-limit / \
             auto-compact that self-heals) or a genuine wedge. Please check and nudge/restart if needed."
        );
        if let Err(e) = crate::inbox::notify_system(
            home,
            &recipient,
            "system:inbox_stuck_watchdog",
            "inbox_stuck_watchdog",
            text,
            Some(agent),
            None,
        ) {
            tracing::warn!(%agent, %recipient, error = %e, "inbox_stuck_watchdog: notify failed");
            continue;
        }
        tracing::info!(
            %agent,
            %recipient,
            unread,
            age_min,
            "#1491 inbox_stuck_watchdog: alerted lead about a stuck agent"
        );
        last_alerted.insert(agent.clone(), *now);
    }
}

/// The orchestrator of the first team that lists `agent` as a member.
fn orchestrator_for(fleet: &crate::fleet::FleetConfig, agent: &str) -> Option<String> {
    fleet
        .teams
        .values()
        .find(|t| t.members.iter().any(|m| m == agent))
        .and_then(|t| t.orchestrator.clone())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static C: AtomicU32 = AtomicU32::new(0);
        let id = C.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-1491-stuck-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_fleet(home: &Path) {
        // A team so the orchestrator (lead) is resolvable for the alert.
        std::fs::write(
            crate::fleet::fleet_yaml_path(home),
            "instances:\n  worker:\n    backend: claude\n  lead:\n    backend: claude\n\
             teams:\n  t:\n    members: [worker, lead]\n    orchestrator: lead\n",
        )
        .unwrap();
    }

    /// Seed `n` unread inbox messages for `agent`, the oldest stamped
    /// `oldest_age_min` minutes ago. `enqueue` preserves `msg.timestamp` and
    /// leaves `read_at = None`, so `unread_count` sees these as unread.
    fn seed_unread(home: &Path, agent: &str, n: usize, oldest_age_min: i64) {
        std::fs::create_dir_all(home.join("inbox")).unwrap();
        for i in 0..n {
            // Spread timestamps so the FIRST one is the oldest.
            let age = (oldest_age_min - i as i64).max(0);
            let mut msg =
                crate::inbox::InboxMessage::new_system("system:test", "task", format!("m{i}"));
            msg.timestamp = (chrono::Utc::now() - chrono::Duration::minutes(age)).to_rfc3339();
            crate::inbox::enqueue(home, agent, msg).unwrap();
        }
    }

    #[test]
    fn alerts_lead_when_unread_pile_is_old_enough() {
        let home = tmp_home("alert");
        write_fleet(&home);
        seed_unread(&home, "worker", 4, 45);
        let now = chrono::Utc::now();
        let mut last = HashMap::new();
        scan_and_emit(&home, &now, &mut last);
        // The orchestrator "lead" must have received an inbox_stuck alert.
        let msgs = crate::inbox::drain(&home, "lead");
        assert!(
            msgs.iter()
                .any(|m| m.text.contains("inbox_stuck_watchdog") && m.text.contains("worker")),
            "lead must be alerted about the stuck worker: {:?}",
            msgs.iter().map(|m| &m.text).collect::<Vec<_>>()
        );
        assert!(
            last.contains_key("worker"),
            "dedup state must record the alert"
        );
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn no_alert_when_below_count_or_age_threshold() {
        let home = tmp_home("below");
        write_fleet(&home);
        // Enough age but too few messages.
        seed_unread(&home, "worker", 1, 45);
        let now = chrono::Utc::now();
        let mut last = HashMap::new();
        scan_and_emit(&home, &now, &mut last);
        assert!(
            crate::inbox::drain(&home, "lead").is_empty(),
            "1 unread message must not trigger an alert"
        );
        // Enough messages but too fresh.
        let home2 = tmp_home("fresh");
        write_fleet(&home2);
        seed_unread(&home2, "worker", 5, 5);
        scan_and_emit(&home2, &chrono::Utc::now(), &mut HashMap::new());
        assert!(
            crate::inbox::drain(&home2, "lead").is_empty(),
            "a fresh pile (5min) must not trigger an alert"
        );
        std::fs::remove_dir_all(home).ok();
        std::fs::remove_dir_all(home2).ok();
    }

    #[test]
    fn dedup_suppresses_realert_within_window() {
        let home = tmp_home("dedup");
        write_fleet(&home);
        seed_unread(&home, "worker", 4, 45);
        let now = chrono::Utc::now();
        let mut last = HashMap::new();
        scan_and_emit(&home, &now, &mut last);
        assert_eq!(
            crate::inbox::drain(&home, "lead").len(),
            1,
            "first alert fires"
        );
        // Re-seed (drain cleared the inbox) and scan again immediately —
        // dedup must suppress a second alert.
        seed_unread(&home, "worker", 4, 45);
        scan_and_emit(&home, &(now + chrono::Duration::minutes(5)), &mut last);
        assert!(
            crate::inbox::drain(&home, "lead").is_empty(),
            "re-alert within the dedup window must be suppressed"
        );
        std::fs::remove_dir_all(home).ok();
    }
}
