//! #2604: offline-target unread-obligation escalation. When a message with a
//! real obligation (query / open task) is sent to an instance that is OFFLINE
//! (not in the live registry) — or never existed — it sits actionable-unread
//! until `sweep_expired`'s 30-day `UNREAD_TTL_DAYS` backstop SILENTLY drops it
//! (storage.rs). poll-reminder only nudges the target's OWN pane (useless when
//! the target is gone); nothing surfaces the pending loss to an operator.
//!
//! This watchdog closes that gap: per tick (60-cadence, co-located with the
//! inbox-maintenance sweep it races against) it scans every inbox file, and for
//! each agent that is (a) offline and (b) has an obligation older than the
//! threshold, pages ALL operator escalation channels
//! ([`crate::channel::notify_all_escalation_channels`]) + writes an event-log
//! row. Fire-and-forget rows (report / update / ci-watch / poll, post-#2636)
//! are NOT obligations — they carry no waiting work, so they never trip the P0
//! (their count rides along in the alert body only, as context).
//!
//! Dedup: per-agent, count-keyed (mirrors poll-reminder's
//! `should_notify_and_record`) — a fixed obligation backlog pages ONCE; a NEW
//! obligation piling on (count change) re-pages; draining to zero (agent
//! returned) or the backlog dropping below threshold resets the latch so a
//! later re-accumulation can page again. Latch entries for inboxes that
//! vanished (swept / deleted) are pruned each run so the map can't grow
//! unbounded.

use super::{PerTickHandler, TickContext};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};

/// Default obligation-age threshold before an offline target is escalated.
/// Must stay well below `UNREAD_TTL_DAYS` (30) so the operator is paged with
/// runway to act before the silent sweep. Override:
/// `AGEND_OFFLINE_UNREAD_ALERT_DAYS`.
const DEFAULT_ALERT_DAYS: i64 = 7;
/// The inbox unread sweep TTL this watchdog races (storage.rs `UNREAD_TTL_DAYS`),
/// surfaced in the alert body so the operator knows the drop-dead window.
const UNREAD_TTL_DAYS: i64 = 30;

fn alert_threshold_days() -> i64 {
    std::env::var("AGEND_OFFLINE_UNREAD_ALERT_DAYS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|d| *d > 0)
        .unwrap_or(DEFAULT_ALERT_DAYS)
}

/// Pure dedup decision, isolated for unit testing. `last_escalated` = the count
/// last paged at (`None` = never / reset). Fire when the count is non-zero AND
/// differs from the last escalated count (first crossing, or a genuinely
/// larger/smaller backlog). A zero count resets the latch to `None` so a future
/// re-accumulation pages again. Mutates `last_escalated` to the new state.
fn decide(last_escalated: &mut Option<usize>, obligation_count: usize) -> bool {
    if obligation_count == 0 {
        *last_escalated = None;
        return false;
    }
    if *last_escalated == Some(obligation_count) {
        return false;
    }
    *last_escalated = Some(obligation_count);
    true
}

/// Classify an offline target for the alert wording. An agent absent from the
/// registry is OFFLINE if still declared in fleet.yaml, NONEXISTENT if declared
/// nowhere (a message addressed to a name that never was).
fn describe_target(name: &str, fleet_instances: &HashSet<String>) -> &'static str {
    if fleet_instances.contains(name) {
        "offline"
    } else {
        "nonexistent"
    }
}

pub(crate) struct OfflineUnreadAlertHandler {
    gate: crate::daemon::cadence_gate::CadenceGate,
    /// Per-agent last-escalated obligation count (dedup latch). Presence = last
    /// escalated count; absence = never escalated / reset.
    latch: Mutex<HashMap<String, usize>>,
}

impl OfflineUnreadAlertHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new(every_n_ticks),
            latch: Mutex::new(HashMap::new()),
        }
    }
}

impl PerTickHandler for OfflineUnreadAlertHandler {
    fn name(&self) -> &'static str {
        "offline_unread_alert"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if !self.gate.fire() {
            return;
        }

        // Phase 1 (locked, cheap): snapshot the ONLINE agent names — an agent in
        // the live registry is not offline, so it's excluded here and its own
        // poll-reminder handles its unread. Drop the lock before any inbox /
        // task-board IO (#1617 fleet-stall class).
        let online: HashSet<String> = {
            let reg = crate::agent::lock_registry(ctx.registry);
            reg.values().map(|h| h.name.as_str().to_string()).collect()
        };

        // fleet.yaml membership distinguishes "offline (declared, not running)"
        // from "nonexistent (addressed name that never was)" — both escalate,
        // only the wording differs.
        let fleet_instances: HashSet<String> =
            crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(ctx.home))
                .map(|f| f.instances.keys().cloned().collect())
                .unwrap_or_default();

        let threshold_days = alert_threshold_days();
        let threshold = chrono::Duration::days(threshold_days);
        let now = chrono::Utc::now();

        // Phase 2 (unlocked): scan every inbox file. `seen` collects the live
        // inbox agent-name set for the latch prune below.
        let mut seen: HashSet<String> = HashSet::new();
        let mut latch = self.latch.lock();
        for name in crate::inbox::inbox_agent_names(ctx.home) {
            seen.insert(name.clone());
            // Online agents are not our concern (poll-reminder owns them); keep
            // no latch entry so a later going-offline starts clean.
            if online.contains(&name) {
                latch.remove(&name);
                continue;
            }
            let summary: crate::inbox::UnreadObligationSummary =
                crate::inbox::unread_obligation_summary(ctx.home, &name);
            // Only obligations older than the threshold are escalation-worthy.
            let over_threshold = summary
                .oldest_obligation
                .is_some_and(|ts| now.signed_duration_since(ts) >= threshold);
            let escalate_count = if over_threshold {
                summary.obligation_count
            } else {
                0
            };
            // Latch presence = last escalated count; absence = never / reset.
            let mut last = latch.get(&name).copied();
            let fire = decide(&mut last, escalate_count);
            match last {
                Some(c) => {
                    latch.insert(name.clone(), c);
                }
                None => {
                    latch.remove(&name);
                }
            }
            if !fire {
                continue;
            }
            let oldest_age_days = summary
                .oldest_obligation
                .map(|ts| now.signed_duration_since(ts).num_days())
                .unwrap_or(0);
            let target_kind = describe_target(&name, &fleet_instances);
            let msg = format!(
                "[offline-unread] {target_kind} target '{name}' has {escalate_count} \
                 unhandled obligation(s) (oldest {oldest_age_days}d, threshold \
                 {threshold_days}d) sitting unread — the target is not running, so \
                 poll-reminder cannot reach it. These will be SILENTLY dropped by \
                 the {UNREAD_TTL_DAYS}-day inbox sweep unless the target is \
                 restarted (or the sender reassigns). ({} total unread incl. \
                 fire-and-forget, context only.)",
                summary.raw_unread_total,
            );
            let dispatched = crate::channel::notify_all_escalation_channels(
                &name,
                crate::channel::NotifySeverity::Error,
                &msg,
                false,
            );
            crate::event_log::log(ctx.home, "offline_unread_alert", &name, &msg);
            tracing::info!(
                agent = %name,
                target_kind,
                obligations = escalate_count,
                oldest_age_days,
                channels = dispatched,
                "offline_unread_alert: escalated offline target's unread obligation backlog"
            );
        }
        // Prune latch entries whose inbox no longer exists (swept / deleted), so
        // the map tracks only currently-present inboxes (cleanup-on-delete).
        latch.retain(|name, _| seen.contains(name));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn decide_fires_once_then_dedups_same_count() {
        let mut latch = None;
        assert!(decide(&mut latch, 3), "first non-zero count fires");
        assert!(!decide(&mut latch, 3), "same count dedups");
        assert!(!decide(&mut latch, 3), "still dedups");
    }

    #[test]
    fn decide_refires_on_count_change() {
        let mut latch = None;
        assert!(decide(&mut latch, 2));
        assert!(decide(&mut latch, 4), "a larger backlog re-pages");
        assert!(
            decide(&mut latch, 1),
            "a smaller (but non-zero) backlog re-pages"
        );
    }

    #[test]
    fn decide_zero_count_resets_latch_and_never_fires() {
        let mut latch = Some(5);
        assert!(!decide(&mut latch, 0), "zero count never fires");
        assert_eq!(latch, None, "zero resets the latch");
        // After reset, a re-accumulation to the SAME prior count pages again.
        assert!(
            decide(&mut latch, 5),
            "re-accumulation after reset re-pages"
        );
    }

    #[test]
    fn decide_below_threshold_zero_behaves_like_drained() {
        // The handler passes escalate_count=0 when under the age threshold; that
        // must behave exactly like a drained backlog (reset, no page).
        let mut latch = Some(3);
        assert!(!decide(&mut latch, 0));
        assert_eq!(latch, None);
    }

    #[test]
    fn describe_target_distinguishes_offline_from_nonexistent() {
        let mut fleet = HashSet::new();
        fleet.insert("declared".to_string());
        assert_eq!(describe_target("declared", &fleet), "offline");
        assert_eq!(describe_target("ghost", &fleet), "nonexistent");
    }

    #[test]
    #[serial(offline_unread_env)]
    fn threshold_env_override_positive_only() {
        std::env::remove_var("AGEND_OFFLINE_UNREAD_ALERT_DAYS");
        assert_eq!(alert_threshold_days(), DEFAULT_ALERT_DAYS);
        // A zero / negative override is rejected (falls back to default) so the
        // watchdog can't be silently turned into paging on every fresh unread.
        std::env::set_var("AGEND_OFFLINE_UNREAD_ALERT_DAYS", "0");
        assert_eq!(alert_threshold_days(), DEFAULT_ALERT_DAYS);
        std::env::set_var("AGEND_OFFLINE_UNREAD_ALERT_DAYS", "14");
        assert_eq!(alert_threshold_days(), 14);
        std::env::remove_var("AGEND_OFFLINE_UNREAD_ALERT_DAYS");
    }

    // ── run() integration (REAL entry point) ──

    use parking_lot::Mutex as PLMutex;
    use std::path::Path;
    use std::sync::Arc;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-offline-alert-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn seed_obligation(home: &Path, agent: &str, id: &str, days_ago: i64) {
        let ts = (chrono::Utc::now() - chrono::Duration::days(days_ago)).to_rfc3339();
        crate::inbox::enqueue(
            home,
            agent,
            crate::inbox::InboxMessage {
                schema_version: 1,
                id: Some(id.to_string()),
                from: "lead".to_string(),
                kind: Some("query".to_string()),
                timestamp: ts,
                text: "please reply".to_string(),
                ..Default::default()
            },
        )
        .unwrap();
    }

    fn empty_ctx_parts() -> (
        crate::agent::AgentRegistry,
        crate::agent::ExternalRegistry,
        Arc<PLMutex<HashMap<String, crate::daemon::AgentConfig>>>,
    ) {
        (
            Arc::new(PLMutex::new(HashMap::new())),
            Arc::new(PLMutex::new(HashMap::new())),
            Arc::new(PLMutex::new(HashMap::new())),
        )
    }

    fn event_log(home: &Path) -> String {
        std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default()
    }

    /// An OFFLINE agent (empty registry) with an obligation older than the
    /// threshold escalates: an `offline_unread_alert` event-log row naming the
    /// agent is written (the operator-visible artifact; the channel fan-out is a
    /// no-op with no channel registered, which is fine — the row still lands).
    #[test]
    #[serial(offline_unread_env)]
    fn offline_over_threshold_escalates_2604() {
        let home = tmp_home("over");
        seed_obligation(&home, "gone", "q-old", 10); // 10d > 7d default
        let (registry, externals, configs) = empty_ctx_parts();
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };
        OfflineUnreadAlertHandler::new(1).run(&ctx);
        let log = event_log(&home);
        assert!(
            log.contains("offline_unread_alert") && log.contains("gone"),
            "offline over-threshold obligation must write an escalation event-log row: {log}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// An obligation YOUNGER than the threshold does not escalate (the operator
    /// still has runway before the 30-day sweep).
    #[test]
    #[serial(offline_unread_env)]
    fn offline_under_threshold_does_not_escalate_2604() {
        let home = tmp_home("under");
        seed_obligation(&home, "gone", "q-fresh", 1); // 1d < 7d default
        let (registry, externals, configs) = empty_ctx_parts();
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };
        OfflineUnreadAlertHandler::new(1).run(&ctx);
        assert!(
            !event_log(&home).contains("offline_unread_alert"),
            "a fresh obligation (under threshold) must NOT escalate"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// An ONLINE agent (present in the registry) is skipped even with an old
    /// obligation — its own poll-reminder owns it; the offline watchdog must not
    /// double-page. Reverse-regression for the registry-membership skip.
    #[test]
    #[serial(offline_unread_env)]
    fn online_agent_with_old_obligation_skipped_2604() {
        let home = tmp_home("online");
        seed_obligation(&home, "alive", "q-old", 10);
        let (registry, externals, configs) = empty_ctx_parts();
        let (handle, _reader) = crate::daemon::per_tick::mock_live_agent_no_context("alive");
        registry.lock().insert(handle.id, handle);
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };
        OfflineUnreadAlertHandler::new(1).run(&ctx);
        assert!(
            !event_log(&home).contains("offline_unread_alert"),
            "an ONLINE agent must be skipped (poll-reminder owns it), no offline escalation"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Dedup across ticks: the same backlog pages ONCE, not every tick.
    #[test]
    #[serial(offline_unread_env)]
    fn repeated_run_dedups_same_backlog_2604() {
        let home = tmp_home("dedup");
        seed_obligation(&home, "gone", "q-old", 10);
        let (registry, externals, configs) = empty_ctx_parts();
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };
        let h = OfflineUnreadAlertHandler::new(1);
        h.run(&ctx);
        let rows_after_first = event_log(&home).matches("offline_unread_alert").count();
        h.run(&ctx);
        let rows_after_second = event_log(&home).matches("offline_unread_alert").count();
        assert_eq!(rows_after_first, 1, "first run escalates once");
        assert_eq!(
            rows_after_second, 1,
            "second run with the SAME backlog must be deduped (no new row)"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
