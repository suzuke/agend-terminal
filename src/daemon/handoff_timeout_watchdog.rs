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
/// #1859 Fix A: a handoff unread for at least this long (minutes) is RE-NUDGED to
/// the target itself (daemon-side redelivery — earlier + cheaper than the 10-min
/// lead escalation, and the only recovery when `next_after_ci` IS the orchestrator).
const RENUDGE_AFTER_MINS: i64 = 2;
/// #1859 Fix A: don't re-nudge the same (target, handoff) more often than this
/// (anti-storm). Combined with the idle gate, a busy/working target is retried at
/// most once per interval and a target that has read the handoff stops entirely.
const RENUDGE_INTERVAL_MINS: i64 = 2;
/// Fallback recipient when the target isn't in any team.
const FALLBACK_RECIPIENT: &str = "lead";
/// Inbox kind of a CI handoff message.
const HANDOFF_KIND: &str = "ci-ready-for-action";

/// Scan every fleet instance for `ci-ready-for-action` handoffs it received but
/// never read; (1) #1859: RE-NUDGE the target itself (daemon-side redelivery) and
/// (2) escalate timed-out ones to the target's team lead. `last_escalated` /
/// `last_renudged` (keyed by `(target, correlation)`) are owned by the caller so
/// dedup survives across ticks; `now` is injected for deterministic tests.
pub(crate) fn scan_and_emit(
    home: &Path,
    now: &chrono::DateTime<chrono::Utc>,
    last_escalated: &mut HashMap<(String, String), chrono::DateTime<chrono::Utc>>,
    last_renudged: &mut HashMap<(String, String), chrono::DateTime<chrono::Utc>>,
) {
    scan_and_emit_with(
        home,
        now,
        last_escalated,
        last_renudged,
        |target, unread| {
            crate::inbox::notify::renudge_actionable_unread(home, target, HANDOFF_KIND, unread);
        },
    );
}

/// Test-seam variant: `renudge` is the per-target re-nudge emitter (real path is
/// the direct PTY inject; tests pass a capturing closure so the daemon API
/// loopback isn't required). The busy/interval GATING lives here (not in the
/// emitter) so a test driving the real `scan_and_emit_with` entry can assert
/// exactly when a re-nudge fires.
pub(crate) fn scan_and_emit_with<F>(
    home: &Path,
    now: &chrono::DateTime<chrono::Utc>,
    last_escalated: &mut HashMap<(String, String), chrono::DateTime<chrono::Utc>>,
    last_renudged: &mut HashMap<(String, String), chrono::DateTime<chrono::Utc>>,
    mut renudge: F,
) where
    F: FnMut(&str, usize),
{
    let Ok(fleet) = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)) else {
        return;
    };
    // #1598-mirror: collect every (target, correlation) key still backed by a
    // pending unread handoff this tick, so the trailing `retain` can reap
    // `last_escalated` / `last_renudged` entries whose repo@branch handoff is no
    // longer active (read/resolved). Without it the maps grow one entry per
    // (reviewer, branch) forever — a slow leak, since `.get`/`.insert` were the
    // only ops and the dedup windows suppress but never evict.
    let mut active: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    // #1859 reviewer-2: collapse same-scan re-nudge fan-out to TARGET granularity.
    // The re-nudge pointer is target-level (handoff-agnostic — it just wakes the
    // agent to drain its inbox), so a target holding K≥2 simultaneously-unread
    // handoffs needs ONE inject, not K. Without this the per-`(target,corr)`
    // interval gate fires once per handoff in a single scan, and since the
    // busy-gate reads a per-tick snapshot (constant within the scan) injects #2..K
    // lose busy protection — a storm exactly on the headline beneficiary (a
    // multi-branch orchestrator-as-next_after_ci). The per-key `last_renudged`
    // still governs the cross-tick interval.
    let mut renudged_this_scan: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    for target in fleet.instances.keys() {
        for (correlation, sent_at) in crate::inbox::unread_of_kind(home, target, HANDOFF_KIND) {
            let age_min = now.signed_duration_since(sent_at).num_minutes();
            let corr = correlation.unwrap_or_else(|| "<unknown>".to_string());
            let key = (target.clone(), corr.clone());
            active.insert(key.clone());

            // (1) #1859 Fix A: daemon-side RE-NUDGE of the target itself. The
            // poller's actionable `[ci-ready-for-action]` wake can be deferred
            // (mid-token guard) into the `notification_queue`, whose only flush is
            // the TUI loop — so a busy target's wake strands with no daemon-side
            // redelivery (Scenario A). Re-fire it here, but ONLY when the target
            // is idle (busy → skip; THIS watchdog tick is the retry loop, so no
            // mid-token corruption and no queueing), and at most once per
            // `RENUDGE_INTERVAL_MINS` (anti-storm). Stops automatically once the
            // target reads the handoff (drops from the unread scan → reaped). This
            // also covers the orchestrator-as-`next_after_ci` case below, where
            // there is no lead to escalate to.
            if age_min >= RENUDGE_AFTER_MINS && !crate::snapshot::agent_is_busy(home, target) {
                let due = last_renudged.get(&key).is_none_or(|prev| {
                    now.signed_duration_since(*prev).num_minutes() >= RENUDGE_INTERVAL_MINS
                });
                if due {
                    // Stamp EVERY due key (per-key cross-tick interval honesty —
                    // so a collapsed key isn't treated as never-nudged next scan,
                    // which would let the target re-fire inside the interval).
                    last_renudged.insert(key.clone(), *now);
                    // ...but inject at most ONCE per target per scan.
                    if renudged_this_scan.insert(target.clone()) {
                        let (unread, _) = crate::inbox::unread_count(home, target);
                        renudge(target, unread);
                    }
                }
            }

            // (2) Escalation to the target's lead (detection only; unchanged).
            if age_min < HANDOFF_TIMEOUT_MINS {
                continue;
            }
            if let Some(prev) = last_escalated.get(&key) {
                if now.signed_duration_since(*prev).num_minutes() < REALERT_AFTER_MINS {
                    continue;
                }
            }
            let recipient = crate::fleet::team_orchestrator_for(home, target)
                .unwrap_or_else(|| FALLBACK_RECIPIENT.to_string());
            if recipient == *target {
                // #1859: `next_after_ci` IS the team orchestrator — there is no
                // higher authority to escalate to. This is no longer a silent
                // total-skip (the Scenario A bug): the re-nudge above already
                // redelivers the handoff to the orchestrator itself.
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
    // Reap dedup entries for handoffs that are no longer pending (read/resolved
    // → absent from this tick's unread scan), bounding the maps to the set of
    // currently-active handoffs.
    last_escalated.retain(|k, _| active.contains(k));
    last_renudged.retain(|k, _| active.contains(k));
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

    /// Seed the per-tick snapshot so `agent_is_busy(home, agent)` is
    /// controllable (`thinking`/`tool_use` = busy; anything else = idle).
    fn seed_snapshot(home: &Path, agent: &str, state: &str) {
        crate::snapshot::save(
            home,
            &[crate::snapshot::AgentSnapshot {
                name: agent.to_string(),
                backend_command: String::new(),
                args: vec![],
                working_dir: None,
                submit_key: String::new(),
                health_state: String::new(),
                agent_state: state.to_string(),
                silent_secs: 0,
            }],
        );
    }

    /// Real watchdog entry with the re-nudge emitter CAPTURED (so the daemon API
    /// loopback isn't needed). Returns the targets that were re-nudged this scan.
    fn run_watchdog(
        home: &Path,
        now: &chrono::DateTime<chrono::Utc>,
        last_escalated: &mut HashMap<(String, String), chrono::DateTime<chrono::Utc>>,
        last_renudged: &mut HashMap<(String, String), chrono::DateTime<chrono::Utc>>,
    ) -> Vec<String> {
        let mut nudged = Vec::new();
        scan_and_emit_with(home, now, last_escalated, last_renudged, |t, _| {
            nudged.push(t.to_string())
        });
        nudged
    }

    #[test]
    fn escalates_unread_handoff_past_timeout() {
        let home = tmp_home("escalate");
        write_fleet(&home);
        seed_handoff(&home, "reviewer", "o/r@feat", 15, false);
        let mut last = HashMap::new();
        run_watchdog(&home, &chrono::Utc::now(), &mut last, &mut HashMap::new());
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
        run_watchdog(
            &home,
            &chrono::Utc::now(),
            &mut HashMap::new(),
            &mut HashMap::new(),
        );
        assert!(
            crate::inbox::drain(&home, "lead").is_empty(),
            "a fresh handoff must not escalate"
        );
        // Old but already READ → reviewer acted → no escalation.
        let home2 = tmp_home("read");
        write_fleet(&home2);
        seed_handoff(&home2, "reviewer", "o/r@feat", 30, true);
        run_watchdog(
            &home2,
            &chrono::Utc::now(),
            &mut HashMap::new(),
            &mut HashMap::new(),
        );
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
        run_watchdog(&home, &now, &mut last, &mut HashMap::new());
        assert_eq!(
            crate::inbox::drain(&home, "lead").len(),
            1,
            "first escalation"
        );
        // Re-seed (drain cleared inbox) and scan again soon — dedup suppresses.
        seed_handoff(&home, "reviewer", "o/r@feat", 15, false);
        run_watchdog(
            &home,
            &(now + chrono::Duration::minutes(5)),
            &mut last,
            &mut HashMap::new(),
        );
        assert!(
            crate::inbox::drain(&home, "lead").is_empty(),
            "re-escalation within the dedup window must be suppressed"
        );
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn stale_entry_reaped_when_handoff_no_longer_pending() {
        let home = tmp_home("reap");
        write_fleet(&home);
        // Old unread handoff → first scan escalates and records a dedup entry.
        seed_handoff(&home, "reviewer", "o/r@gone", 15, false);
        let now = chrono::Utc::now();
        let mut last = HashMap::new();
        run_watchdog(&home, &now, &mut last, &mut HashMap::new());
        assert!(
            last.contains_key(&("reviewer".to_string(), "o/r@gone".to_string())),
            "first scan must record a dedup entry for the escalated handoff"
        );
        // Reviewer reads/resolves it → the handoff leaves the unread scan. The
        // next scan must reap the now-stale (reviewer, o/r@gone) entry rather
        // than accumulating one per dead branch forever (the leak).
        crate::inbox::drain(&home, "reviewer");
        run_watchdog(
            &home,
            &(now + chrono::Duration::minutes(1)),
            &mut last,
            &mut HashMap::new(),
        );
        assert!(
            last.is_empty(),
            "a dedup entry whose handoff is no longer pending must be reaped, not leaked: {:?}",
            last.keys().collect::<Vec<_>>()
        );
        std::fs::remove_dir_all(home).ok();
    }

    /// #1859 §3.9 (a): when `next_after_ci` IS the team orchestrator, the old
    /// `recipient == target` branch silently skipped the WHOLE handoff (no
    /// escalation AND no re-nudge) — the Scenario A hole. Now the orchestrator's
    /// own unread handoff is RE-NUDGED (and correctly NOT self-escalated).
    #[test]
    fn orchestrator_as_next_after_ci_is_renudged_not_silently_skipped() {
        let home = tmp_home("orch-renudge");
        write_fleet(&home); // orchestrator = lead
        seed_handoff(&home, "lead", "o/r@feat", 15, false);
        seed_snapshot(&home, "lead", "idle");
        let nudged = run_watchdog(
            &home,
            &chrono::Utc::now(),
            &mut HashMap::new(),
            &mut HashMap::new(),
        );
        assert!(
            nudged.contains(&"lead".to_string()),
            "orchestrator-as-next_after_ci must be re-nudged, not silently skipped: {nudged:?}"
        );
        // No self-escalation (no higher authority above the orchestrator).
        assert!(
            crate::inbox::drain(&home, "lead")
                .iter()
                .all(|m| !m.text.contains("handoff_timeout_watchdog")),
            "orchestrator must not be escalated about its own handoff"
        );
        std::fs::remove_dir_all(home).ok();
    }

    /// #1859 §3.9 (b): a BUSY (mid-token) target is NOT re-nudged (the watchdog
    /// tick is the retry loop — no PTY corruption, no queueing); the same target,
    /// once idle, gets a daemon-side re-nudge WITHOUT relying on the TUI flush.
    #[test]
    fn busy_target_skipped_idle_target_renudged() {
        let home = tmp_home("busy-idle");
        write_fleet(&home);
        seed_handoff(&home, "reviewer", "o/r@feat", 15, false);

        seed_snapshot(&home, "reviewer", "tool_use"); // busy → skip
        let nudged = run_watchdog(
            &home,
            &chrono::Utc::now(),
            &mut HashMap::new(),
            &mut HashMap::new(),
        );
        assert!(
            !nudged.contains(&"reviewer".to_string()),
            "a busy (tool_use) target must not be re-nudged mid-token: {nudged:?}"
        );

        seed_snapshot(&home, "reviewer", "idle"); // idle → deliver
        let nudged = run_watchdog(
            &home,
            &chrono::Utc::now(),
            &mut HashMap::new(),
            &mut HashMap::new(),
        );
        assert!(
            nudged.contains(&"reviewer".to_string()),
            "an idle target with an unread handoff must be re-nudged daemon-side: {nudged:?}"
        );
        std::fs::remove_dir_all(home).ok();
    }

    /// #1859 anti-storm: within `RENUDGE_INTERVAL_MINS` a second scan must NOT
    /// re-nudge again (idempotent via `last_renudged`); once the target reads the
    /// handoff it stops AND the dedup entry is reaped (no leak).
    #[test]
    fn renudge_is_interval_gated_and_stops_when_read() {
        let home = tmp_home("renudge-interval");
        write_fleet(&home);
        seed_handoff(&home, "reviewer", "o/r@feat", 15, false);
        seed_snapshot(&home, "reviewer", "idle");
        let now = chrono::Utc::now();
        let mut renudged = HashMap::new();

        let first = run_watchdog(&home, &now, &mut HashMap::new(), &mut renudged);
        assert!(
            first.contains(&"reviewer".to_string()),
            "first re-nudge must fire"
        );

        // Within the interval → suppressed.
        let soon = run_watchdog(
            &home,
            &(now + chrono::Duration::minutes(1)),
            &mut HashMap::new(),
            &mut renudged,
        );
        assert!(
            !soon.contains(&"reviewer".to_string()),
            "a re-nudge within RENUDGE_INTERVAL_MINS must be suppressed (anti-storm): {soon:?}"
        );

        // Reviewer reads it → no more re-nudge, and the dedup entry is reaped.
        crate::inbox::drain(&home, "reviewer");
        let after_read = run_watchdog(
            &home,
            &(now + chrono::Duration::minutes(10)),
            &mut HashMap::new(),
            &mut renudged,
        );
        assert!(
            !after_read.contains(&"reviewer".to_string()),
            "a read handoff must not be re-nudged"
        );
        assert!(
            renudged.is_empty(),
            "last_renudged must be reaped once the handoff is read: {:?}",
            renudged.keys().collect::<Vec<_>>()
        );
        std::fs::remove_dir_all(home).ok();
    }

    /// #1859 reviewer-2 (lens 5): a target holding K≥2 simultaneously-unread
    /// ci-ready handoffs (distinct corr) must get AT MOST ONE re-nudge per scan
    /// — the wake pointer is target-level. Pre-fix the per-`(target,corr)` gate
    /// fired once per handoff (K injects; #2..K bypassed the per-tick busy
    /// snapshot). This is the storm-on-the-orchestrator case.
    #[test]
    fn multiple_unread_handoffs_collapse_to_one_renudge_per_scan() {
        let home = tmp_home("multi-handoff");
        write_fleet(&home);
        seed_handoff(&home, "reviewer", "o/r@feat-a", 15, false);
        seed_handoff(&home, "reviewer", "o/r@feat-b", 15, false);
        seed_snapshot(&home, "reviewer", "idle");
        let nudged = run_watchdog(
            &home,
            &chrono::Utc::now(),
            &mut HashMap::new(),
            &mut HashMap::new(),
        );
        let count = nudged.iter().filter(|t| t.as_str() == "reviewer").count();
        assert_eq!(
            count, 1,
            "2 unread handoffs for one idle target must collapse to a single re-nudge: {nudged:?}"
        );
        std::fs::remove_dir_all(home).ok();
    }
}
