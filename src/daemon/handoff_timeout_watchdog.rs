//! #1491(B) next_after_ci handoff-timeout watchdog.
//!
//! When CI passes on a watched branch, the poller hands the PR off to the
//! reviewer by enqueuing a `[ci-ready-for-action]` message (see
//! `ci_watch::poller::make_ci_ready_for_action_msg`). If the reviewer is stuck
//! or offline that handoff can sit unclaimed indefinitely — last night a PR
//! sat for an hour because the reviewer never picked it up and nothing
//! escalated.
//!
//! #1888 phase-2 (decouple from read-state): this watchdog ORIGINALLY scanned
//! the inbox message itself (`unread_of_kind`) — but ANY inbox drain marks the
//! handoff read (the drain is kind-blind), so the watchdog went blind the
//! moment the reviewer ran a routine pending check. Production confirmed the
//! coupling (14 `#1888-ciready-read` @ 2-7s : 0 `#1888-renudge-decision` in a
//! full day — the 2-min window never once opened). The scan source is now the
//! `ci_handoff_track` sidecar: recorded when the poller enqueues the handoff,
//! resolved on an explicit RESOLUTION signal (the reviewer's report for that
//! correlation / a PR terminal state / the target claiming the branch / the
//! 24h backstop) — never on read.
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

/// #35896-11 ⑥: parse a track's persisted throttle stamp (RFC3339) into a UTC
/// instant. `None`/unparseable → `None` = "never" (fires now) — the pre-⑥
/// behavior, so a corrupt/missing stamp degrades to "slightly noisier", never
/// "obligation lost".
fn parse_stamp(s: &Option<String>) -> Option<chrono::DateTime<chrono::Utc>> {
    s.as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&chrono::Utc))
}

/// #26795: if `correlation`'s (`owner/repo@branch`) PR is already known to be
/// merge-blocked (REJECTED verdict or Draft) per the CACHED `pr_state`
/// snapshot, resolve its `ci_handoff_track` entry and report `true`. Read-only
/// from `ci_watch`'s perspective — this never touches the watch registry or
/// makes a GitHub call, only reads whatever `pr_state::record_verdict` last
/// wrote when a reviewer's report was processed. Returns `false` (no-op) if
/// no `pr_state` file exists yet for this correlation, or its verdict isn't
/// blocking.
fn resolve_if_merge_blocked(home: &Path, correlation: &str) -> bool {
    let Some((repo, branch)) = correlation.split_once('@') else {
        return false;
    };
    let blocked = crate::daemon::pr_state::with_pr_state(home, repo, branch, |s| {
        crate::daemon::pr_state::is_ci_ready_merge_blocked(s)
    })
    .ok()
    .flatten()
    .unwrap_or(false);
    if blocked {
        tracing::info!(
            tag = "#26795-track-merge-blocked",
            %correlation,
            "ci-handoff track resolved — pr_state cache shows REJECTED/Draft; \
             re-nudging further would be pure noise"
        );
        crate::daemon::ci_handoff_track::resolve_by_correlation(
            home,
            correlation,
            "pr_merge_blocked_inline",
        );
    }
    blocked
}

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
        |target, msg| {
            // #2729 real operator-P0 route. The return is the number of REGISTERED
            // escalation channels the alert was DISPATCHED to — a route count, NOT a
            // delivery receipt (the channel layer may still suppress/authorize per its
            // own rules; `notify_all_escalation_channels` reports `channels.len()`).
            crate::channel::notify_all_escalation_channels(
                target,
                crate::channel::NotifySeverity::Error,
                msg,
                false,
            )
        },
    );
}

/// Test-seam variant: `renudge` is the per-target re-nudge emitter and
/// `page_operator` is the self-orchestrator operator-P0 emitter (real paths are the
/// PTY inject / the channel fan-out; tests pass capturing closures so no
/// process-global state or daemon loopback is needed). `page_operator(target, msg)`
/// returns the number of REGISTERED channel routes the alert was dispatched to (NOT
/// a delivery receipt). The busy/interval/escalation GATING lives here (not in the
/// emitters) so a test driving the real entry can assert exactly when each fires.
pub(crate) fn scan_and_emit_with<F, G>(
    home: &Path,
    now: &chrono::DateTime<chrono::Utc>,
    last_escalated: &mut HashMap<(String, String), chrono::DateTime<chrono::Utc>>,
    last_renudged: &mut HashMap<(String, String), chrono::DateTime<chrono::Utc>>,
    mut renudge: F,
    mut page_operator: G,
) where
    F: FnMut(&str, usize),
    G: FnMut(&str, &str) -> usize,
{
    // #1888 phase-2: backstop sweep BEFORE the scan — an expired track must
    // neither re-nudge nor escalate this tick.
    let _ = crate::daemon::ci_handoff_track::sweep_expired(home, now);
    let tracks = crate::daemon::ci_handoff_track::list(home);
    // #1598-mirror: collect every (target, correlation) key still backed by a
    // pending TRACK this tick, so the trailing `retain` can reap
    // `last_escalated` / `last_renudged` entries whose repo@branch handoff is no
    // longer active (resolved/expired). Without it the maps grow one entry per
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
    {
        for (path, track) in &tracks {
            let target = &track.target;
            // Orphan guard: the target left the fleet — nothing to nudge; the
            // 24h sweep reaps the file.
            if !crate::fleet::instance_is_known(home, target) {
                continue;
            }
            // sweep_expired (above) already deleted unparseable sent_at tracks;
            // a race re-creating one just waits a tick.
            let Ok(sent_at) = chrono::DateTime::parse_from_rfc3339(&track.sent_at) else {
                continue;
            };
            let sent_at = sent_at.with_timezone(&chrono::Utc);
            // #1870-H3: clamp so a backward clock skew (future `sent_at`) can't
            // make the age negative → re-nudge / escalation silently stop firing.
            let age_min = crate::daemon::utils::elapsed_since(*now, sent_at).num_minutes();
            let corr = track.correlation.clone();
            let key = (target.clone(), corr.clone());
            active.insert(key.clone());

            // #26795: a PR already known to be merge-blocked (REJECTED verdict /
            // Draft) is unactionable by the target no matter how many times we
            // re-nudge — resolve right here instead of relying on the SEPARATE
            // `pr_state::scanner` tick to (maybe) reach the same conclusion.
            // That tick's own resolve path, like `resolve_head_advanced` below,
            // depends on an active `ci_watch` continuing to refresh state for
            // this branch; once polling stops (superseded / no active watch —
            // the exact orphan-track class this fixes), neither ever fires
            // again except the 24h backstop. This check reads the CACHED
            // `pr_state` snapshot instead — written the moment a reviewer's
            // report is processed (`pr_state::record_verdict`), independent of
            // `ci_watch` state entirely. No GitHub call, no new dependency.
            if resolve_if_merge_blocked(home, &corr) {
                continue;
            }

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
            // #1888: record WHY this PENDING handoff did / didn't get re-nudged
            // this tick (tag kept from the phase-1 instrument for log
            // continuity). Phase-2: the loop now iterates TRACKS, so a handoff
            // that was merely marked read still shows up here — `track_pending`
            // replaces the old `unread_found` (which production proved was
            // always extinguished within seconds).
            // #35896-11 ⑥: the effective "last re-nudged" is the in-mem throttle map,
            // OR — when the map lacks the key (a daemon RESTART cleared it) — the
            // track's PERSISTED `last_renudged_at`, so a boot doesn't reset the
            // throttle and re-nudge a burst for every live handoff. In-mem stays
            // primary (fast); the persisted value is the restart-survival fallback
            // (they're stamped together, so in-mem present ⟹ equals the persisted).
            //
            // #2729: resolve the escalation recipient + a "recently DISPATCHED a
            // self-orch operator page" throttle flag up front. A self-orchestrator's
            // own handoff has no peer to relay, so once an operator P0 has been
            // DISPATCHED to at least one registered channel route (→ the durable
            // `last_escalated` stamp — a route/throttle fact, NOT a delivery receipt),
            // stop the 2-min re-nudge storm until the REALERT window elapses. Peer
            // tracks are unaffected.
            let recipient = crate::fleet::team_orchestrator_for(home, target)
                .unwrap_or_else(|| FALLBACK_RECIPIENT.to_string());
            let is_self_orch = recipient == *target;
            let escalated_recently = last_escalated
                .get(&key)
                .copied()
                .or_else(|| parse_stamp(&track.last_escalated_at))
                .is_some_and(|prev| {
                    now.signed_duration_since(prev).num_minutes() < REALERT_AFTER_MINS
                });
            let self_orch_dispatched_recently = is_self_orch && escalated_recently;
            let busy = crate::snapshot::agent_is_busy(home, target);
            let effective_last_renudged = last_renudged
                .get(&key)
                .copied()
                .or_else(|| parse_stamp(&track.last_renudged_at));
            let renudge_due = effective_last_renudged.is_none_or(|prev| {
                now.signed_duration_since(prev).num_minutes() >= RENUDGE_INTERVAL_MINS
            });
            // info!-level so it lands in the production daemon.log (default filter
            // is `agend_terminal=info`); bounded — at most once per UNREAD ci-handoff
            // per tick, and a handoff stops being scanned the moment it's read.
            tracing::info!(
                tag = "#1888-renudge-decision",
                agent = %target,
                correlation = %corr,
                track_pending = true,
                age_min,
                agent_is_busy = busy,
                renudge_due,
                age_ok = age_min >= RENUDGE_AFTER_MINS,
                self_orch_dispatched_recently,
                will_fire = age_min >= RENUDGE_AFTER_MINS && !busy && renudge_due && !self_orch_dispatched_recently,
                "ci-handoff re-nudge decision"
            );
            if age_min >= RENUDGE_AFTER_MINS
                && !busy
                && renudge_due
                && !self_orch_dispatched_recently
            {
                // Stamp EVERY due key (per-key cross-tick interval honesty — so a
                // collapsed key isn't treated as never-nudged next scan, which would
                // let the target re-fire inside the interval).
                last_renudged.insert(key.clone(), *now);
                // #35896-11 ⑥: persist to the durable track so a restart doesn't reset
                // this throttle (the burst fix). Per-key, matching the in-mem stamp
                // above (the actual inject collapses to once-per-target below).
                crate::daemon::ci_handoff_track::stamp_throttle(
                    home, path, target, &corr, now, true, false,
                );
                // ...but inject at most ONCE per target per scan.
                if renudged_this_scan.insert(target.clone()) {
                    // #1888 phase-2: the pointer count is the target's PENDING TRACKS —
                    // the handoff message itself may already be read (that no longer
                    // stops the re-nudge), so the unread count would say 0 and mislead.
                    let pending = tracks.iter().filter(|(_, t)| &t.target == target).count();
                    renudge(target, pending);
                }
            }

            // (2) Escalation to the target's lead (detection only; unchanged).
            if age_min < HANDOFF_TIMEOUT_MINS {
                continue;
            }
            // Dedup: don't re-alert inside the REALERT window. `escalated_recently`
            // was computed above from the in-mem map + the persisted restart-survival
            // fallback (byte-identical source to the pre-#2729 inline check).
            if escalated_recently {
                continue;
            }
            if is_self_orch {
                // #2729: `next_after_ci` IS the team orchestrator — no peer to relay.
                // Dispatch an operator P0 (via the `page_operator` seam) exactly like
                // #1701's self-orch Hung path, instead of the pre-#2729 silent
                // `continue`. `dispatched` is the number of REGISTERED channel routes
                // the alert was handed to — NOT a delivery receipt. Stamp / suppress
                // ONLY when at least one route was registered; zero registered routes
                // must record nothing and suppress nothing, so the retry (re-nudge +
                // re-escalation) continues and a channel registering later still pages.
                let text = format!(
                    "🛑 {target} (team orchestrator) has an unclaimed CI handoff for \
                     {corr} for {age_min}min and is its own orchestrator — no peer can \
                     relay. Manual intervention likely (check the pane / interrupt / re-prime)."
                );
                let dispatched = page_operator(target, &text);
                if dispatched > 0 {
                    tracing::info!(
                        %target, %corr, age_min, channel_routes = dispatched,
                        "#2729 handoff_timeout_watchdog: dispatched self-orchestrator operator page to registered channel route(s)"
                    );
                    last_escalated.insert(key, *now);
                    crate::daemon::ci_handoff_track::stamp_throttle(
                        home, path, target, &corr, now, false, true,
                    );
                } else {
                    tracing::warn!(
                        %target, %corr,
                        "#2729 handoff_timeout_watchdog: no escalation channel registered — self-orch operator page not dispatched, will retry"
                    );
                }
                continue;
            }
            // #1923 G11: the recipient is the hardcoded `FALLBACK_RECIPIENT`
            // ("lead") when `team_orchestrator_for` finds no team orchestrator. If
            // no instance named "lead" exists in this fleet, the escalation would
            // be written to a GHOST inbox (silently lost). Skip + log instead of
            // escalating into the void.
            if !crate::fleet::instance_is_known(home, &recipient) {
                tracing::warn!(
                    %target, %recipient, %corr,
                    "#1923 G11: handoff-timeout escalation recipient not in fleet — skipping (would be a ghost inbox)"
                );
                continue;
            }
            let text = format!(
                "[handoff_timeout_watchdog] the next_after_ci handoff to '{target}' for {corr} \
                 has been unclaimed for {age_min}min — CI passed and a [ci-ready-for-action] \
                 message was sent, but '{target}' still hasn't picked it up (no review report / \
                 branch claim / PR terminal for that correlation). The reviewer may be \
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
            // #35896-11 ⑥: persist the escalation throttle so a restart doesn't
            // re-escalate this handoff inside its REALERT window.
            crate::daemon::ci_handoff_track::stamp_throttle(
                home, path, target, &corr, now, false, true,
            );
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
        // #1888 phase-2 production parity: the poller records a TRACK alongside
        // the enqueue — the watchdog scans tracks, not inbox read-state, so
        // `read` no longer extinguishes the handoff (that's the fix).
        crate::daemon::ci_handoff_track::record(
            home,
            target,
            corr,
            &(chrono::Utc::now() - chrono::Duration::minutes(age_min)).to_rfc3339(),
            None, // #2008: head-awareness not exercised by the watchdog tests
            None,
        );
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
                output_silent_secs: 0,
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
        // Default operator-page emitter: no route registered (returns 0), for the
        // re-nudge/escalation tests that don't exercise the self-orch page.
        run_watchdog_pageable(home, now, last_escalated, last_renudged, |_, _| 0)
    }

    /// Like `run_watchdog` but with an injectable operator-page emitter
    /// (`(target, msg) -> dispatched-route-count`), so the #2729 self-orch tests
    /// drive the REAL state machine deterministically — no process-global channel
    /// registry, no shared mutable state, so they stay isolated under parallelism.
    fn run_watchdog_pageable(
        home: &Path,
        now: &chrono::DateTime<chrono::Utc>,
        last_escalated: &mut HashMap<(String, String), chrono::DateTime<chrono::Utc>>,
        last_renudged: &mut HashMap<(String, String), chrono::DateTime<chrono::Utc>>,
        page_operator: impl FnMut(&str, &str) -> usize,
    ) -> Vec<String> {
        let mut nudged = Vec::new();
        scan_and_emit_with(
            home,
            now,
            last_escalated,
            last_renudged,
            |t, _| nudged.push(t.to_string()),
            page_operator,
        );
        nudged
    }

    /// §3.9 #1888 (instrument-only): the re-nudge decision is OBSERVABLE (the
    /// `#1888-renudge-decision` trace fires for an unread handoff) while the
    /// surrounding behavior is byte-identical (an unread, past-threshold, idle
    /// handoff still re-nudges).
    #[test]
    #[tracing_test::traced_test]
    fn renudge_decision_is_instrumented_1888() {
        let home = tmp_home("1888-renudge-trace");
        write_fleet(&home);
        seed_handoff(&home, "reviewer", "owner/repo@br", 5, false); // unread, 5min old
        seed_snapshot(&home, "reviewer", "idle"); // not busy
        let nudged = run_watchdog(
            &home,
            &chrono::Utc::now(),
            &mut HashMap::new(),
            &mut HashMap::new(),
        );
        assert!(
            logs_contain("#1888-renudge-decision"),
            "the re-nudge decision must be traced"
        );
        assert!(
            nudged.iter().any(|t| t == "reviewer"),
            "behavior unchanged: an unread idle past-threshold handoff still re-nudges"
        );
    }

    /// §3.9 #1888 (instrument-only): a `ci-ready-for-action` handoff transitioning
    /// on a drain is OBSERVABLE (the `#1888-ciready-read` trace fires) while the
    /// drain behavior is preserved (the message is returned). #2299: the first
    /// drain now marks it `delivering` (in-flight), not `read` — the trace tag is
    /// unchanged, so the instrumentation still fires.
    #[test]
    #[tracing_test::traced_test]
    fn ciready_read_on_drain_is_instrumented_1888() {
        let home = tmp_home("1888-ciready-read-trace");
        seed_handoff(&home, "reviewer", "owner/repo@br", 1, false); // unread ci-ready
        let drained = crate::inbox::drain(&home, "reviewer");
        assert!(
            drained
                .iter()
                .any(|m| m.kind.as_deref() == Some("ci-ready-for-action")
                    && m.delivering_at.is_some()),
            "#2299: drain returns the handoff, now marked delivering (not yet processed)"
        );
        assert!(
            logs_contain("#1888-ciready-read"),
            "the ci-ready transition-on-drain must be traced"
        );
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

    /// #1923 G11: when the escalation recipient resolves to the hardcoded
    /// `FALLBACK_RECIPIENT` ("lead") — no team orchestrator for the target — but
    /// NO instance named "lead" exists in the fleet, the watchdog must SKIP the
    /// escalation, not write it to a ghost inbox.
    #[test]
    fn no_escalation_to_ghost_fallback_recipient_1923_g11() {
        let home = tmp_home("g11-ghost");
        // Fleet has the target but no team orchestrator and no "lead" → the
        // recipient resolves to the "lead" fallback, which does not exist.
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  reviewer:\n    backend: claude\n",
        )
        .unwrap();
        seed_handoff(&home, "reviewer", "o/r@feat", 15, false);
        run_watchdog(
            &home,
            &chrono::Utc::now(),
            &mut HashMap::new(),
            &mut HashMap::new(),
        );
        assert!(
            crate::inbox::drain(&home, "lead").is_empty(),
            "#1923 G11: escalation must be skipped when the fallback recipient \
             'lead' is absent from the fleet (no ghost inbox)"
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
        // #1888 phase-2: old and already READ but NOT resolved → still
        // escalates (read used to blind the watchdog — that was the bug).
        let home2 = tmp_home("read");
        write_fleet(&home2);
        seed_handoff(&home2, "reviewer", "o/r@feat", 30, true);
        run_watchdog(
            &home2,
            &chrono::Utc::now(),
            &mut HashMap::new(),
            &mut HashMap::new(),
        );
        assert_eq!(
            crate::inbox::drain(&home2, "lead").len(),
            1,
            "#1888: a read-but-unresolved handoff must STILL escalate"
        );
        // RESOLVED (reviewer reported the correlation) → no escalation.
        let home3 = tmp_home("resolved");
        write_fleet(&home3);
        seed_handoff(&home3, "reviewer", "o/r@feat", 30, true);
        crate::daemon::ci_handoff_track::resolve_by_correlation(&home3, "o/r@feat", "test");
        run_watchdog(
            &home3,
            &chrono::Utc::now(),
            &mut HashMap::new(),
            &mut HashMap::new(),
        );
        assert!(
            crate::inbox::drain(&home3, "lead").is_empty(),
            "#1888: a RESOLVED handoff must not escalate"
        );
        std::fs::remove_dir_all(home).ok();
        std::fs::remove_dir_all(home2).ok();
        std::fs::remove_dir_all(home3).ok();
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

    /// #1888 phase-2: the 24h backstop — a track whose resolution signal never
    /// arrives is swept at scan start (WARN) and neither re-nudges nor
    /// escalates. Guarantees "never re-nudge forever".
    #[test]
    fn backstop_expired_track_swept_no_nudge_no_escalation_1888() {
        let home = tmp_home("backstop");
        write_fleet(&home);
        seed_snapshot(&home, "reviewer", "idle");
        crate::daemon::ci_handoff_track::record(
            &home,
            "reviewer",
            "o/r@abandoned",
            &(chrono::Utc::now() - chrono::Duration::hours(25)).to_rfc3339(),
            None,
            None,
        );
        let nudged = run_watchdog(
            &home,
            &chrono::Utc::now(),
            &mut HashMap::new(),
            &mut HashMap::new(),
        );
        assert!(
            !nudged.contains(&"reviewer".to_string()),
            "#1888: an expired track must not re-nudge"
        );
        assert!(
            crate::inbox::drain(&home, "lead").is_empty(),
            "#1888: an expired track must not escalate"
        );
        assert!(
            crate::daemon::ci_handoff_track::list(&home).is_empty(),
            "#1888: the expired track is swept"
        );
        std::fs::remove_dir_all(home).ok();
    }

    /// #35896-11 ⑥: the RE-NUDGE throttle is PERSISTED on the track, so a daemon
    /// RESTART (empty in-mem map) does NOT re-nudge a burst — yet a genuinely-due
    /// handoff (persisted stamp older than the interval) STILL fires (the
    /// over-suppression counter-example). Pre-⑥ the empty in-mem map alone drove
    /// the decision, so Case 1 would burst-fire on restart.
    #[test]
    fn persisted_renudge_throttle_survives_restart_without_over_suppression_35896_11() {
        let home = tmp_home("35896-persist-renudge");
        write_fleet(&home);
        seed_snapshot(&home, "reviewer", "idle");
        let now = chrono::Utc::now();
        // Live handoff old enough to be renudge-eligible (5min >= RENUDGE_AFTER_MINS).
        seed_handoff(&home, "reviewer", "o/r@b", 5, false);
        let track_path = crate::daemon::ci_handoff_track::list(&home)[0].0.clone();

        // Case 1 (burst prevention): a RECENT persisted stamp (30s ago, inside the
        // 2min interval) must suppress the re-nudge on a RESTART (empty in-mem maps).
        crate::daemon::ci_handoff_track::stamp_throttle(
            &home,
            &track_path,
            "reviewer",
            "o/r@b",
            &(now - chrono::Duration::seconds(30)),
            true,
            false,
        );
        let nudged = run_watchdog(&home, &now, &mut HashMap::new(), &mut HashMap::new());
        assert!(
            nudged.is_empty(),
            "post-restart, a recently-persisted renudge throttle must suppress the boot burst: {nudged:?}"
        );

        // Case 2 (no over-suppression): an OLD persisted stamp (5min ago, past the
        // interval) must STILL re-nudge on restart.
        crate::daemon::ci_handoff_track::stamp_throttle(
            &home,
            &track_path,
            "reviewer",
            "o/r@b",
            &(now - chrono::Duration::minutes(5)),
            true,
            false,
        );
        let nudged2 = run_watchdog(&home, &now, &mut HashMap::new(), &mut HashMap::new());
        assert_eq!(
            nudged2,
            vec!["reviewer".to_string()],
            "a persisted throttle older than the interval must not over-suppress a due re-nudge"
        );
        std::fs::remove_dir_all(home).ok();
    }

    /// #35896-11 ⑥ (lead acceptance #2): the ESCALATION throttle is likewise
    /// persisted — a RESTART must not re-escalate a timed-out handoff inside its
    /// REALERT window, but MUST still escalate once that window has elapsed (no
    /// over-suppression of a legitimately-due escalation). Target seeded BUSY so
    /// the re-nudge path is gated off and only the escalation is exercised.
    #[test]
    fn persisted_escalation_throttle_survives_restart_without_over_suppression_35896_11() {
        let home = tmp_home("35896-persist-escalate");
        write_fleet(&home);
        seed_snapshot(&home, "reviewer", "thinking"); // busy → no re-nudge noise
        let now = chrono::Utc::now();
        // Handoff past the escalation timeout (40min >= HANDOFF_TIMEOUT_MINS=10).
        seed_handoff(&home, "reviewer", "o/r@b", 40, false);
        let track_path = crate::daemon::ci_handoff_track::list(&home)[0].0.clone();

        // Case 1 (burst prevention): a RECENT persisted escalation stamp (5min ago,
        // inside REALERT_AFTER_MINS=30) must suppress re-escalation on a restart.
        crate::daemon::ci_handoff_track::stamp_throttle(
            &home,
            &track_path,
            "reviewer",
            "o/r@b",
            &(now - chrono::Duration::minutes(5)),
            false,
            true,
        );
        run_watchdog(&home, &now, &mut HashMap::new(), &mut HashMap::new());
        assert!(
            crate::inbox::drain(&home, "lead").is_empty(),
            "post-restart, a recently-persisted escalation throttle must suppress the boot re-escalation"
        );

        // Case 2 (no over-suppression): an OLD persisted stamp (35min ago, past
        // REALERT) must STILL escalate on restart.
        crate::daemon::ci_handoff_track::stamp_throttle(
            &home,
            &track_path,
            "reviewer",
            "o/r@b",
            &(now - chrono::Duration::minutes(35)),
            false,
            true,
        );
        run_watchdog(&home, &now, &mut HashMap::new(), &mut HashMap::new());
        assert_eq!(
            crate::inbox::drain(&home, "lead").len(),
            1,
            "a persisted escalation throttle older than REALERT must not over-suppress a due escalation"
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
        // Reviewer RESOLVES it (verdict report / claim / PR terminal) → the
        // track is gone. The next scan must reap the now-stale
        // (reviewer, o/r@gone) entry rather than accumulating one per dead
        // branch forever (the leak). (#1888 phase-2: a mere READ no longer
        // ends the handoff — resolution does.)
        crate::daemon::ci_handoff_track::resolve_by_correlation(&home, "o/r@gone", "test");
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

        seed_snapshot(&home, "reviewer", "active"); // busy → skip
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
    fn renudge_interval_gated_read_does_not_stop_resolution_does_1888() {
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

        // #1888 phase-2 CORE regression (read-then-not-act, the stranded-PR
        // case): the reviewer DRAINS its inbox (marks the handoff read) but
        // does NOT act — the re-nudge must keep firing. Pre-fix this went
        // permanently silent (production: 14 reads @ 2-7s, 0 decisions).
        crate::inbox::drain(&home, "reviewer");
        let after_read = run_watchdog(
            &home,
            &(now + chrono::Duration::minutes(10)),
            &mut HashMap::new(),
            &mut renudged,
        );
        assert!(
            after_read.contains(&"reviewer".to_string()),
            "#1888: a read-but-unresolved handoff must STILL be re-nudged"
        );

        // RESOLUTION (verdict report / branch claim / PR terminal) stops the
        // re-nudge and the dedup entry is reaped — never re-nudge forever.
        crate::daemon::ci_handoff_track::resolve_by_correlation(&home, "o/r@feat", "test");
        let after_resolve = run_watchdog(
            &home,
            &(now + chrono::Duration::minutes(20)),
            &mut HashMap::new(),
            &mut renudged,
        );
        assert!(
            !after_resolve.contains(&"reviewer".to_string()),
            "#1888: a resolved handoff must not be re-nudged"
        );
        assert!(
            renudged.is_empty(),
            "last_renudged must be reaped once the handoff is resolved: {:?}",
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

    /// Write a `pr_state` file with `verdict_state = Rejected` for `repo@branch`
    /// — mirrors what `pr_state::mod::record_verdict` writes the moment a
    /// reviewer's REJECTED report is processed, independent of whether any
    /// `ci_watch` is currently active for that branch (the write itself doesn't
    /// touch the watch registry at all).
    fn seed_rejected_pr_state(home: &Path, repo: &str, branch: &str, head_sha: &str) {
        use crate::daemon::pr_state::{
            CiState, DraftState, MergeState, PrState, ReviewClass, VerdictState,
        };
        let now = chrono::Utc::now().to_rfc3339();
        crate::daemon::pr_state::with_pr_state_or_create(
            home,
            repo,
            branch,
            || PrState {
                repo: repo.to_string(),
                pr_number: 1,
                branch: branch.to_string(),
                head_sha: head_sha.to_string(),
                pr_author: "dev".to_string(),
                subscribers: vec!["reviewer".to_string()],
                ci_state: CiState::Pending,
                verdict_state: VerdictState::None,
                merge_state: MergeState::NotReady,
                draft_state: DraftState::Ready,
                review_class: ReviewClass::Single,
                ready_emitted_for_sha: None,
                diagnostic_emitted_for_sha: None,
                auto_armed: false,
                auto_armed_for_sha: None,
                auto_armed_at: None,
                last_gh_poll_at: None,
                gh_poll_failures: 0,
                last_gh_state: None,
                closed_unmerged_pending: false,
                freshness_checked_head_sha: None,
                freshness_checked_base_sha: None,
                freshness_checked_at: None,
                freshness_behind_by: None,
                freshness_error: false,
                freshness_retry_after: None,
                observed_head_sha: None,
                observed_base_sha: None,
                observed_at: None,
                observed_error: false,
                reserved_assignments: Vec::new(),
                created_at: now.clone(),
                updated_at: now,
            },
            |s| {
                s.verdict_state = VerdictState::Rejected {
                    reviewer: "reviewer".to_string(),
                    reviewed_head: head_sha.to_string(),
                    reason: None,
                };
            },
        )
        .unwrap();
    }

    /// #26795 RED baseline (Phase 1 RCA — REJECTED-head orphan track class):
    /// a `ci-ready-for-action` track whose PR was REJECTED (verdict already
    /// recorded in the pr_state cache, independent of any active `ci_watch`)
    /// must not keep re-nudging — the outcome is already known and unactionable
    /// by the target. Pre-fix, the watchdog has no idea the PR is
    /// merge-blocked (it only scans `ci_handoff_track`, never cross-checks
    /// `pr_state`), so this currently FAILS (renudge fires anyway) — this is
    /// the RED baseline; the fix makes it pass without touching the
    /// `RENUDGE_AFTER_MINS`/`RENUDGE_INTERVAL_MINS` gating itself.
    #[test]
    fn renudge_skips_when_pr_state_shows_merge_blocked() {
        let home = tmp_home("merge-blocked");
        write_fleet(&home);
        seed_handoff(&home, "reviewer", "o/r@feat", 15, false);
        seed_snapshot(&home, "reviewer", "idle");
        seed_rejected_pr_state(&home, "o/r", "feat", "sha-A");

        let nudged = run_watchdog(
            &home,
            &chrono::Utc::now(),
            &mut HashMap::new(),
            &mut HashMap::new(),
        );
        assert!(
            !nudged.contains(&"reviewer".to_string()),
            "a REJECTED (merge-blocked) PR's ci-ready handoff must not keep \
             re-nudging — the pr_state cache already shows the outcome, \
             independent of ci_watch state: {nudged:?}"
        );
        std::fs::remove_dir_all(home).ok();
    }

    // ── #2729: self-orchestrator handoff-timeout operator escalation ─────────

    /// Fleet where `solo` is its OWN team orchestrator
    /// (`team_orchestrator_for(solo) == solo`), so the watchdog's
    /// `recipient == target` self-orchestrator branch is exercised.
    fn write_self_orch_fleet(home: &Path) {
        std::fs::write(
            crate::fleet::fleet_yaml_path(home),
            "instances:\n  solo:\n    backend: claude\n\
             teams:\n  s:\n    members: [solo]\n    orchestrator: solo\n",
        )
        .unwrap();
    }

    /// #2729: a timed-out handoff whose target is its own orchestrator has no peer
    /// to relay — it must DISPATCH an operator page exactly once, persist the
    /// escalation stamp, and stop the 2-min renudge storm once dispatched.
    #[test]
    fn self_orch_timed_out_handoff_pages_operator_once_and_stops_renudge() {
        let home = tmp_home("2729-selforch-page");
        write_self_orch_fleet(&home);
        seed_snapshot(&home, "solo", "idle"); // idle → renudge-eligible absent suppression
        seed_handoff(&home, "solo", "o/r@feat", 11, false); // past HANDOFF_TIMEOUT_MINS(10)
        let now = chrono::Utc::now();
        let mut last = HashMap::new();
        let pages = std::cell::Cell::new(0usize);

        run_watchdog_pageable(&home, &now, &mut last, &mut HashMap::new(), |_t, _m| {
            pages.set(pages.get() + 1);
            1 // one registered channel route
        });
        assert_eq!(
            pages.get(),
            1,
            "a timed-out self-orchestrator handoff must dispatch the operator page once"
        );
        let track = crate::daemon::ci_handoff_track::list(&home)[0].1.clone();
        assert!(
            track.last_escalated_at.is_some(),
            "the self-orch escalation must persist a last_escalated_at stamp on the track"
        );

        // Second tick within REALERT_AFTER_MINS: no re-dispatch, and the renudge stops.
        let nudged = run_watchdog_pageable(
            &home,
            &(now + chrono::Duration::minutes(3)),
            &mut last,
            &mut HashMap::new(),
            |_t, _m| {
                pages.set(pages.get() + 1);
                1
            },
        );
        assert_eq!(
            pages.get(),
            1,
            "re-dispatch within the REALERT window must be deduped"
        );
        assert!(
            !nudged.contains(&"solo".to_string()),
            "once the operator is paged, the 2-min renudge storm must stop (self-orch): {nudged:?}"
        );
        std::fs::remove_dir_all(home).ok();
    }

    /// #2729 review boundary: ZERO dispatched routes must NOT stamp nor suppress —
    /// nothing was dispatched, so the renudge/retry continues and no escalation stamp
    /// is recorded (a route registering later still pages).
    #[test]
    fn self_orch_page_without_route_does_not_stamp_or_suppress_renudge() {
        let home = tmp_home("2729-selforch-noroute");
        write_self_orch_fleet(&home);
        seed_snapshot(&home, "solo", "idle");
        seed_handoff(&home, "solo", "o/r@feat", 11, false);
        let mut last = HashMap::new();
        let pages = std::cell::Cell::new(0usize);
        let nudged = run_watchdog_pageable(
            &home,
            &chrono::Utc::now(),
            &mut last,
            &mut HashMap::new(),
            |_t, _m| {
                pages.set(pages.get() + 1);
                0 // no route registered
            },
        );

        assert_eq!(pages.get(), 1, "the operator page is still attempted");
        let track = crate::daemon::ci_handoff_track::list(&home)[0].1.clone();
        assert!(
            track.last_escalated_at.is_none(),
            "zero dispatched routes must NOT persist an escalation stamp"
        );
        assert!(
            !last.contains_key(&("solo".to_string(), "o/r@feat".to_string())),
            "zero dispatched routes must NOT record an in-mem escalation"
        );
        assert!(
            nudged.contains(&"solo".to_string()),
            "with zero routes dispatched, the renudge/retry must continue: {nudged:?}"
        );
        std::fs::remove_dir_all(home).ok();
    }

    /// #2729: once the REALERT window elapses, a still-unresolved self-orch handoff
    /// re-alerts the operator exactly once more.
    #[test]
    fn self_orch_re_alerts_after_realert_window() {
        let home = tmp_home("2729-selforch-realert");
        write_self_orch_fleet(&home);
        seed_snapshot(&home, "solo", "thinking"); // busy → isolate escalation from renudge
        seed_handoff(&home, "solo", "o/r@feat", 11, false);
        let now = chrono::Utc::now();
        let mut last = HashMap::new();
        let pages = std::cell::Cell::new(0usize);

        run_watchdog_pageable(&home, &now, &mut last, &mut HashMap::new(), |_t, _m| {
            pages.set(pages.get() + 1);
            1
        });
        assert_eq!(pages.get(), 1, "first operator page");
        run_watchdog_pageable(
            &home,
            &(now + chrono::Duration::minutes(REALERT_AFTER_MINS + 1)),
            &mut last,
            &mut HashMap::new(),
            |_t, _m| {
                pages.set(pages.get() + 1);
                1
            },
        );
        assert_eq!(
            pages.get(),
            2,
            "a self-orch page must re-alert once REALERT_AFTER_MINS has elapsed"
        );
        std::fs::remove_dir_all(home).ok();
    }

    /// #2729 pre-timeout preservation: a fresh (< HANDOFF_TIMEOUT_MINS) self-orch
    /// handoff must not dispatch a page.
    #[test]
    fn self_orch_fresh_handoff_does_not_page() {
        let home = tmp_home("2729-selforch-fresh");
        write_self_orch_fleet(&home);
        seed_snapshot(&home, "solo", "idle");
        seed_handoff(&home, "solo", "o/r@feat", 3, false); // < HANDOFF_TIMEOUT_MINS(10)
        let pages = std::cell::Cell::new(0usize);
        run_watchdog_pageable(
            &home,
            &chrono::Utc::now(),
            &mut HashMap::new(),
            &mut HashMap::new(),
            |_t, _m| {
                pages.set(pages.get() + 1);
                1
            },
        );
        assert_eq!(
            pages.get(),
            0,
            "a fresh (<10min) self-orch handoff must not dispatch a page"
        );
        std::fs::remove_dir_all(home).ok();
    }

    /// #2729 peer preservation: a PEER escalation (recipient != target) must go to
    /// the lead INBOX and never dispatch an operator page.
    #[test]
    fn peer_escalation_does_not_dispatch_operator_page() {
        let home = tmp_home("2729-peer-preserved");
        write_fleet(&home); // reviewer's orchestrator = lead (peer)
        seed_handoff(&home, "reviewer", "o/r@feat", 15, false);
        let pages = std::cell::Cell::new(0usize);
        run_watchdog_pageable(
            &home,
            &chrono::Utc::now(),
            &mut HashMap::new(),
            &mut HashMap::new(),
            |_t, _m| {
                pages.set(pages.get() + 1);
                1
            },
        );
        assert_eq!(
            pages.get(),
            0,
            "a peer escalation must NOT dispatch an operator page (it goes to the lead inbox)"
        );
        assert_eq!(
            crate::inbox::drain(&home, "lead").len(),
            1,
            "peer escalation to the lead inbox is preserved"
        );
        std::fs::remove_dir_all(home).ok();
    }
}
