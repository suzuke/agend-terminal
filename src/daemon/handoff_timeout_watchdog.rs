//! #1491(B) next_after_ci handoff-timeout watchdog.
//!
//! When CI passes on a watched branch, the poller hands the PR off to the
//! reviewer by enqueuing a `[ci-ready-for-action]` message (see
//! `ci_watch::poller::make_ci_ready_for_action_msg`). If the reviewer is stuck
//! or offline that handoff can sit unclaimed indefinitely — last night a PR
//! sat for an hour because the reviewer never picked it up and nothing
//! escalated.
//!
//! RCA note: the handoff is *already recorded* — it's the inbox message itself,
//! carrying its send time (`timestamp`) and the `repo@branch` correlation. So
//! this watchdog needs no new tracking store: it simply looks for a
//! `ci-ready-for-action` message that is still UNREAD after the timeout, and
//! escalates to the reviewer's team lead so they can re-route or nudge.
//!
//! Detection ONLY — like the inbox-stuck watchdog (#1491A) it never reassigns
//! automatically; the lead decides.

use std::collections::HashMap;
use std::path::Path;

/// A handoff unread for at least this long (minutes) is escalated.
const HANDOFF_TIMEOUT_MINS: i64 = 10;
/// Don't re-escalate the same (target, handoff) more often than this.
const REALERT_AFTER_MINS: i64 = 30;
/// Fallback recipient when the target isn't in any team.
const FALLBACK_RECIPIENT: &str = "lead";
/// Inbox kind of a CI handoff message.
const HANDOFF_KIND: &str = "ci-ready-for-action";

/// Scan every fleet instance for `ci-ready-for-action` handoffs it received but
/// never read, and escalate timed-out ones to the target's team lead.
/// `last_escalated` (keyed by `(target, correlation)`) is owned by the caller
/// so dedup survives across ticks; `now` is injected for deterministic tests.
pub(crate) fn scan_and_emit(
    home: &Path,
    now: &chrono::DateTime<chrono::Utc>,
    last_escalated: &mut HashMap<(String, String), chrono::DateTime<chrono::Utc>>,
) {
    let Ok(fleet) = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)) else {
        return;
    };
    for target in fleet.instances.keys() {
        for (correlation, sent_at) in crate::inbox::unread_of_kind(home, target, HANDOFF_KIND) {
            let age_min = now.signed_duration_since(sent_at).num_minutes();
            if age_min < HANDOFF_TIMEOUT_MINS {
                continue;
            }
            let corr = correlation.unwrap_or_else(|| "<unknown>".to_string());
            let key = (target.clone(), corr.clone());
            if let Some(prev) = last_escalated.get(&key) {
                if now.signed_duration_since(*prev).num_minutes() < REALERT_AFTER_MINS {
                    continue;
                }
            }
            let recipient = crate::fleet::team_orchestrator_for(home, target)
                .unwrap_or_else(|| FALLBACK_RECIPIENT.to_string());
            if recipient == *target {
                continue;
            }
            let text = format!(
                "[handoff_timeout_watchdog] the next_after_ci handoff to '{target}' for {corr} \
                 has been unclaimed for {age_min}min — CI passed and a [ci-ready-for-action] \
                 message was sent, but '{target}' still hasn't read it. The reviewer may be \
                 stuck/offline; consider re-routing the review to another reviewer or nudging it."
            );
            if let Err(e) = crate::inbox::notify_system(
                home,
                &recipient,
                "system:handoff_timeout_watchdog",
                "handoff_timeout_watchdog",
                text,
                Some(&corr),
                None,
            ) {
                tracing::warn!(%target, %recipient, error = %e, "handoff_timeout_watchdog: notify failed");
                continue;
            }
            tracing::info!(
                %target,
                %recipient,
                correlation = %corr,
                age_min,
                "#1491 handoff_timeout_watchdog: escalated an unclaimed CI handoff to the lead"
            );
            last_escalated.insert(key, *now);
        }
    }
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
            "agend-1491-handoff-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_fleet(home: &Path) {
        std::fs::write(
            crate::fleet::fleet_yaml_path(home),
            "instances:\n  reviewer:\n    backend: claude\n  lead:\n    backend: claude\n\
             teams:\n  t:\n    members: [reviewer, lead]\n    orchestrator: lead\n",
        )
        .unwrap();
    }

    /// Seed a `ci-ready-for-action` handoff in `target`'s inbox, stamped
    /// `age_min` minutes ago, optionally already read.
    fn seed_handoff(home: &Path, target: &str, corr: &str, age_min: i64, read: bool) {
        std::fs::create_dir_all(home.join("inbox")).unwrap();
        let mut msg = crate::inbox::InboxMessage::new_system(
            "system:ci",
            HANDOFF_KIND,
            format!("[ci-ready-for-action] {corr}: CI passed, your turn."),
        )
        .with_correlation_id(corr);
        msg.timestamp = (chrono::Utc::now() - chrono::Duration::minutes(age_min)).to_rfc3339();
        if read {
            msg.read_at = Some(chrono::Utc::now().to_rfc3339());
        }
        crate::inbox::enqueue(home, target, msg).unwrap();
    }

    #[test]
    fn escalates_unread_handoff_past_timeout() {
        let home = tmp_home("escalate");
        write_fleet(&home);
        seed_handoff(&home, "reviewer", "o/r@feat", 15, false);
        let mut last = HashMap::new();
        scan_and_emit(&home, &chrono::Utc::now(), &mut last);
        let msgs = crate::inbox::drain(&home, "lead");
        assert!(
            msgs.iter()
                .any(|m| m.text.contains("handoff_timeout_watchdog")
                    && m.text.contains("reviewer")
                    && m.text.contains("o/r@feat")),
            "lead must be escalated about the unclaimed handoff: {:?}",
            msgs.iter().map(|m| &m.text).collect::<Vec<_>>()
        );
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn no_escalation_for_fresh_or_read_handoff() {
        // Fresh (< 10min) → no escalation.
        let home = tmp_home("fresh");
        write_fleet(&home);
        seed_handoff(&home, "reviewer", "o/r@feat", 3, false);
        scan_and_emit(&home, &chrono::Utc::now(), &mut HashMap::new());
        assert!(
            crate::inbox::drain(&home, "lead").is_empty(),
            "a fresh handoff must not escalate"
        );
        // Old but already READ → reviewer acted → no escalation.
        let home2 = tmp_home("read");
        write_fleet(&home2);
        seed_handoff(&home2, "reviewer", "o/r@feat", 30, true);
        scan_and_emit(&home2, &chrono::Utc::now(), &mut HashMap::new());
        assert!(
            crate::inbox::drain(&home2, "lead").is_empty(),
            "a read handoff means the reviewer acted — no escalation"
        );
        std::fs::remove_dir_all(home).ok();
        std::fs::remove_dir_all(home2).ok();
    }

    #[test]
    fn dedup_suppresses_reescalation_within_window() {
        let home = tmp_home("dedup");
        write_fleet(&home);
        seed_handoff(&home, "reviewer", "o/r@feat", 15, false);
        let now = chrono::Utc::now();
        let mut last = HashMap::new();
        scan_and_emit(&home, &now, &mut last);
        assert_eq!(
            crate::inbox::drain(&home, "lead").len(),
            1,
            "first escalation"
        );
        // Re-seed (drain cleared inbox) and scan again soon — dedup suppresses.
        seed_handoff(&home, "reviewer", "o/r@feat", 15, false);
        scan_and_emit(&home, &(now + chrono::Duration::minutes(5)), &mut last);
        assert!(
            crate::inbox::drain(&home, "lead").is_empty(),
            "re-escalation within the dedup window must be suppressed"
        );
        std::fs::remove_dir_all(home).ok();
    }
}
