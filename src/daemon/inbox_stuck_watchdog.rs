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
pub(crate) const FALLBACK_RECIPIENT: &str = "lead";

/// Scan every fleet instance and alert the lead about any that is sitting on a
/// pile of unread inbox messages. `last_alerted` is owned by the caller (the
/// per-tick handler) so dedup state survives across ticks; `now` is injected
/// for deterministic tests.
#[cfg(test)]
pub(crate) fn scan_and_emit(
    home: &Path,
    now: &chrono::DateTime<chrono::Utc>,
    last_alerted: &mut HashMap<String, chrono::DateTime<chrono::Utc>>,
) {
    scan_and_emit_with_blocked(home, now, last_alerted, &HashMap::new(), &HashMap::new());
}

/// Scan with the per-tick live usage/quota snapshot. The snapshot is produced
/// under registry/core locks by [`InboxStuckHandler`] and is intentionally
/// consumed only after those locks are dropped, because this function reads
/// fleet and inbox files.
///
/// `mcp_refusals` (t-20260902154222470714-82348-82) is the same-pass snapshot
/// of `health.last_mcp_refusal`. When the stuck agent has evidence, the alert
/// text gains a sentence naming it — the incident that motivated this had the
/// fleet learn about a 15h total MCP refusal only through THIS watchdog, whose
/// text said nothing about why. With no evidence the message is byte-identical
/// to what it has always been.
pub(crate) fn scan_and_emit_with_blocked(
    home: &Path,
    now: &chrono::DateTime<chrono::Utc>,
    last_alerted: &mut HashMap<String, chrono::DateTime<chrono::Utc>>,
    usage_blocked: &HashMap<String, Option<String>>,
    mcp_refusals: &HashMap<String, crate::health::McpRefusalEvidence>,
) {
    // RED scaffolding: parameter accepted, not yet read (see RED commit body).
    let _ = mcp_refusals;
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
        if let Some(unlock_at) = usage_blocked.get(agent) {
            // A usage-limit notice is the acknowledgeable signal. If the
            // supervisor already recorded one for this episode, or this path
            // successfully delivers the readable fallback, the redundant
            // inbox-stuck alert is suppressed. Failed fallback delivery leaves
            // the ordinary alert path active rather than silently muting it.
            if ensure_readable_usage_notice(
                home,
                &fleet,
                agent,
                unlock_at.as_deref(),
                usage_blocked,
                *now,
            ) {
                continue;
            }
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
        // Ghost-inbox guard (t-20260724035332273132-42380-3): a recipient with
        // no instance (a team-less fleet's `lead` fallback, or a team whose
        // orchestrator was removed) would accumulate alerts nobody drains —
        // the single largest source in the archived ghost inbox (68/101).
        // `fleet` is already loaded here, so check it directly; dedup state is
        // deliberately NOT stamped (nothing was delivered).
        if !fleet.instances.contains_key(&recipient) {
            static WARNED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                tracing::warn!(
                    %agent,
                    %recipient,
                    "inbox_stuck alert dropped — recipient has no fleet.yaml \
                     instance (ghost-inbox guard)"
                );
            }
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

/// Prove or establish a readable UsageLimit notice before repeat suppression.
/// The existing usage-limit ledger is only a notice ledger: it never decides
/// whether `agent` is currently blocked (that authority is the live snapshot).
fn ensure_readable_usage_notice(
    home: &Path,
    fleet: &crate::fleet::FleetConfig,
    agent: &str,
    unlock_at: Option<&str>,
    usage_blocked: &HashMap<String, Option<String>>,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    if crate::daemon::supervisor::usage_limit_notify_suppressed(home, agent, unlock_at, now) {
        return true;
    }
    let recipient = orchestrator_for(fleet, agent)
        .filter(|orch| orch != agent)
        .filter(|orch| fleet.instances.contains_key(orch))
        .filter(|orch| !usage_blocked.contains_key(orch))
        .or_else(|| {
            (fleet.instances.contains_key("general")
                && !usage_blocked.contains_key("general")
                && agent != "general")
                .then(|| "general".to_string())
        })
        .or_else(|| {
            (fleet.instances.contains_key(FALLBACK_RECIPIENT)
                && !usage_blocked.contains_key(FALLBACK_RECIPIENT)
                && agent != FALLBACK_RECIPIENT)
                .then(|| FALLBACK_RECIPIENT.to_string())
        });
    let Some(recipient) = recipient else {
        return false;
    };
    let text = format!(
        "[usage_limit_watchdog] agent '{agent}' is in a live UsageLimit/QuotaExceeded episode. \
         The inbox-stuck repeat alert is suppressed while this episode remains live; \
         wait for reset or switch backend."
    );
    if let Err(error) = crate::inbox::notify_system(
        home,
        &recipient,
        "system:usage_limit_watchdog",
        "usage_limit_watchdog",
        text,
        Some(agent),
        None,
    ) {
        tracing::warn!(%agent, %recipient, %error, "usage_limit_watchdog: readable fallback failed");
        return false;
    }
    crate::daemon::supervisor::record_usage_limit_notified(home, agent, unlock_at, now)
}

/// The orchestrator of the first team that lists `agent` as a member.
pub(crate) fn orchestrator_for(fleet: &crate::fleet::FleetConfig, agent: &str) -> Option<String> {
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
    fn failed_usage_limit_fallback_keeps_ordinary_alert_active() {
        let home = tmp_home("usage-fallback-failure");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  worker:\n    backend: claude\n  lead:\n    backend: claude\n  general:\n    backend: claude\n\
             teams:\n  t:\n    members: [worker, lead]\n    orchestrator: lead\n",
        )
        .unwrap();
        seed_unread(&home, "worker", 4, 45);

        // Make only the readable UsageLimit fallback recipient fail. The
        // ordinary inbox-stuck alert still targets the team lead, whose inbox
        // remains writable. This is the same resolved-path failure shape used
        // by the messaging failure tests and works on all platforms.
        let fallback_path = crate::inbox::storage::inbox_path_resolved(&home, "general");
        std::fs::create_dir_all(&fallback_path).unwrap();

        let mut usage_blocked = HashMap::new();
        usage_blocked.insert("worker".to_string(), None);
        usage_blocked.insert("lead".to_string(), None);
        let mut last = HashMap::new();
        scan_and_emit_with_blocked(
            &home,
            &chrono::Utc::now(),
            &mut last,
            &usage_blocked,
            &HashMap::new(),
        );

        let lead_messages = crate::inbox::drain(&home, "lead");
        assert!(
            lead_messages
                .iter()
                .any(|message| message.kind.as_deref() == Some("inbox_stuck_watchdog")),
            "fallback delivery failure must preserve the ordinary alert: {lead_messages:?}"
        );
        assert!(
            last.contains_key("worker"),
            "ordinary alert delivery must stamp its own re-alert dedup state"
        );
        assert!(
            !crate::daemon::supervisor::usage_limit_notify_path(&home).exists(),
            "failed fallback must not mark the usage-limit notice ledger"
        );
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn no_readable_usage_fallback_recipient_keeps_ordinary_alert_active() {
        let home = tmp_home("usage-no-recipient");
        // Every present readable-notice candidate is usage-blocked, but the
        // ordinary team orchestrator remains present and must still receive
        // the inbox-stuck alert.
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            r#"instances:
  worker:
    backend: claude
  lead:
    backend: claude
  general:
    backend: claude
teams:
  t:
    members: [worker, lead]
    orchestrator: lead
"#,
        )
        .unwrap();
        seed_unread(&home, "worker", 4, 45);

        let mut usage_blocked = HashMap::new();
        usage_blocked.insert("worker".to_string(), None);
        usage_blocked.insert("lead".to_string(), None);
        usage_blocked.insert("general".to_string(), None);
        let mut last = HashMap::new();
        scan_and_emit_with_blocked(
            &home,
            &chrono::Utc::now(),
            &mut last,
            &usage_blocked,
            &HashMap::new(),
        );

        let lead_messages = crate::inbox::drain(&home, "lead");
        assert!(
            lead_messages
                .iter()
                .any(|message| message.kind.as_deref() == Some("inbox_stuck_watchdog")),
            "no readable fallback recipient must preserve the ordinary alert: {lead_messages:?}"
        );
        assert!(
            last.contains_key("worker"),
            "ordinary alert delivery must stamp its own re-alert dedup state"
        );
        assert!(
            !crate::daemon::supervisor::usage_limit_notify_path(&home).exists(),
            "without a readable fallback recipient the usage-limit notice must not be marked"
        );
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn usage_limit_ledger_write_failure_keeps_ordinary_alert_active() {
        let home = tmp_home("usage-ledger-failure");
        write_fleet(&home);
        seed_unread(&home, "worker", 4, 45);

        let ledger_path = crate::daemon::supervisor::usage_limit_notify_path(&home);
        crate::store::fail_next_atomic_write_for_test(&ledger_path);
        let mut usage_blocked = HashMap::new();
        usage_blocked.insert("worker".to_string(), None);
        let mut last = HashMap::new();
        scan_and_emit_with_blocked(
            &home,
            &chrono::Utc::now(),
            &mut last,
            &usage_blocked,
            &HashMap::new(),
        );

        let lead_messages = crate::inbox::drain(&home, "lead");
        assert!(
            lead_messages
                .iter()
                .any(|message| message.kind.as_deref() == Some("usage_limit_watchdog")),
            "the readable fallback must be delivered before ledger persistence fails: {lead_messages:?}"
        );
        assert!(
            lead_messages
                .iter()
                .any(|message| message.kind.as_deref() == Some("inbox_stuck_watchdog")),
            "ledger persistence failure must preserve the ordinary alert: {lead_messages:?}"
        );
        assert!(
            last.contains_key("worker"),
            "ordinary alert delivery must stamp its own re-alert dedup state"
        );
        assert!(
            !ledger_path.exists(),
            "failed ledger persistence must not create false durable suppression proof"
        );
        std::fs::remove_dir_all(home).ok();
    }

    /// Ghost-inbox guard rollout (t-20260724035332273132-42380-3): a stuck
    /// alert must not be enqueued to a recipient with no fleet.yaml instance —
    /// pre-fix the team-less `FALLBACK_RECIPIENT` ("lead") grew
    /// `~/.agend/inbox/lead.jsonl` forever (68 of the 101 archived ghost
    /// entries on the reporting fleet were inbox_stuck alerts).
    #[test]
    fn skips_alert_when_fallback_recipient_has_no_instance() {
        let home = tmp_home("ghost-fallback");
        // No teams and no `lead` instance → the fallback recipient is a ghost.
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  worker:\n    backend: claude\n",
        )
        .unwrap();
        seed_unread(&home, "worker", 4, 45);
        let now = chrono::Utc::now();
        let mut last = HashMap::new();
        scan_and_emit(&home, &now, &mut last);
        assert!(
            crate::inbox::drain(&home, "lead").is_empty(),
            "lead has no fleet.yaml instance — alert must be dropped (ghost-inbox guard)"
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

    /// t-20260902154222470714-82348-82 (g): when the stuck agent carries
    /// codex MCP-refusal evidence, the alert names it — timestamp and pane
    /// line — so the orchestrator sees WHY the agent went quiet instead of
    /// only THAT it did. Without evidence the text stays byte-identical to
    /// today's (pinned here as "the evidence text is exactly the old text
    /// plus one appended sentence").
    #[test]
    fn stuck_alert_appends_mcp_refusal_evidence_when_present() {
        let line = "  \u{2514} MCP tool call requires approval, but approval policy is never";
        let at = chrono::DateTime::parse_from_rfc3339("2026-09-02T10:11:12+00:00")
            .unwrap()
            .with_timezone(&chrono::Utc);

        // Baseline: no evidence → the historical message.
        let base_home = tmp_home("refusal-none");
        write_fleet(&base_home);
        seed_unread(&base_home, "worker", 4, 45);
        let mut last = HashMap::new();
        scan_and_emit_with_blocked(
            &base_home,
            &chrono::Utc::now(),
            &mut last,
            &HashMap::new(),
            &HashMap::new(),
        );
        let base = crate::inbox::drain(&base_home, "lead")
            .into_iter()
            .map(|m| m.text)
            .find(|t| t.contains("[inbox_stuck_watchdog]"))
            .expect("baseline alert must fire");
        assert!(
            !base.contains("Last MCP-refusal evidence:"),
            "no evidence → no evidence sentence: {base}"
        );

        // With evidence for the stuck agent.
        let home = tmp_home("refusal-some");
        write_fleet(&home);
        seed_unread(&home, "worker", 4, 45);
        let mut refusals = HashMap::new();
        refusals.insert(
            "worker".to_string(),
            crate::health::McpRefusalEvidence {
                at,
                line: line.to_string(),
            },
        );
        let mut last = HashMap::new();
        scan_and_emit_with_blocked(
            &home,
            &chrono::Utc::now(),
            &mut last,
            &HashMap::new(),
            &refusals,
        );
        let text = crate::inbox::drain(&home, "lead")
            .into_iter()
            .map(|m| m.text)
            .find(|t| t.contains("[inbox_stuck_watchdog]"))
            .expect("evidence alert must fire");
        assert!(
            text.contains("Last MCP-refusal evidence:"),
            "evidence sentence must be appended: {text}"
        );
        assert!(
            text.contains(&at.to_rfc3339()),
            "evidence timestamp must be RFC3339: {text}"
        );
        assert!(
            text.contains(line),
            "evidence pane line must appear: {text}"
        );
        assert!(
            text.starts_with(&base),
            "the evidence text must be the UNCHANGED message plus an appended \
             sentence (byte-identical prefix)"
        );

        std::fs::remove_dir_all(base_home).ok();
        std::fs::remove_dir_all(home).ok();
    }
}
