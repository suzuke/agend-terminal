//! Issue #651 — `waiting_on` stale detection.
//!
//! Daemon periodically scans all agents with an active `waiting_on`
//! condition. When `waiting_on_since` exceeds 15 minutes, emits an
//! inbox alert to the agent itself AND its team orchestrator (if any).
//!
//! Uses the standard tracker pattern (tick_count + TICKS_PER_SCAN
//! throttle) consistent with `idle_watchdog`, `anti_stall`, etc.

use std::collections::HashMap;
use std::path::Path;

/// Stale threshold: 15 minutes in seconds.
const STALE_THRESHOLD_SECS: i64 = 15 * 60;

/// Re-alert suppression: 30 minutes between repeated alerts for the
/// same agent.
const REALERT_INTERVAL_SECS: i64 = 30 * 60;

/// Scan throttle: 30 ticks × 10s = 5 min cadence (matches other
/// watchdogs).
const TICKS_PER_SCAN: u64 = 30;

pub(crate) struct WaitingOnStaleTracker {
    /// Cadence gate — throttles scans to once per [`TICKS_PER_SCAN`]
    /// supervisor ticks (fire-on-Nth).
    gate: crate::daemon::cadence_gate::CadenceGate,
    /// agent → last alert timestamp (dedup guard).
    last_alerted_at: HashMap<String, chrono::DateTime<chrono::Utc>>,
    /// #1739 boot-seed latch. The first scan after a fresh daemon start seeds
    /// `last_alerted_at` with currently-stale waiters (stamped now) WITHOUT
    /// emitting, so a restart doesn't re-alert conditions the operator already
    /// saw. Only waiters newly stale after boot (or past REALERT_INTERVAL) emit.
    seeded: bool,
}

impl Default for WaitingOnStaleTracker {
    fn default() -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new_interval(TICKS_PER_SCAN),
            last_alerted_at: HashMap::new(),
            seeded: false,
        }
    }
}

impl WaitingOnStaleTracker {
    pub(crate) fn maybe_scan(&mut self, home: &Path) -> bool {
        if !self.gate.fire() {
            return false;
        }
        let seeding = !self.seeded;
        self.seeded = true;
        scan_and_emit(home, &mut self.last_alerted_at, seeding);
        true
    }
}

/// Scan all metadata files for stale `waiting_on` conditions and emit
/// alerts. Exposed `pub(crate)` for unit tests.
pub(crate) fn scan_and_emit(
    home: &Path,
    last_alerted: &mut HashMap<String, chrono::DateTime<chrono::Utc>>,
    seeding: bool,
) {
    let now = chrono::Utc::now();
    let meta_dir = home.join("metadata");
    let Ok(entries) = std::fs::read_dir(&meta_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Some(agent) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(meta) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };
        let Some(condition) = meta.get("waiting_on").and_then(|v| v.as_str()) else {
            continue;
        };
        if condition.is_empty() {
            continue;
        }
        let Some(since_str) = meta.get("waiting_on_since").and_then(|v| v.as_str()) else {
            continue;
        };
        let Ok(since) = chrono::DateTime::parse_from_rfc3339(since_str) else {
            continue;
        };
        let elapsed_secs = now.signed_duration_since(since).num_seconds();
        if elapsed_secs < STALE_THRESHOLD_SECS {
            continue;
        }
        // Dedup: skip if already alerted within REALERT_INTERVAL_SECS
        if let Some(prev) = last_alerted.get(agent) {
            if now.signed_duration_since(*prev).num_seconds() < REALERT_INTERVAL_SECS {
                continue;
            }
        }
        let elapsed_min = elapsed_secs / 60;
        // #1739 boot-seed: on the first scan, record the stale waiter without
        // emitting (treated as already-known across the restart). The dedup
        // insert below still runs so later scans suppress it.
        if !seeding {
            // #event-bus Step 2 (legacy-zero): the bus is the sole delivery path.
            crate::daemon::event_bus::global().emit(
                home,
                crate::daemon::event_bus::EventKind::WaitingOnStale {
                    agent: agent.to_string(),
                    condition: condition.to_string(),
                    elapsed_min,
                },
            );
        }
        last_alerted.insert(agent.to_string(), now);
    }
}

/// #event-bus pattern #4: the stale-waiting notification text. Shared by the
/// legacy direct deliver AND the event-bus subscriber so both rebuild the
/// BYTE-IDENTICAL text.
fn waiting_on_stale_text(agent: &str, condition: &str, elapsed_min: i64) -> String {
    format!(
        "[waiting_on_stale] {agent}: waiting on \"{condition}\" for {elapsed_min}m\n\n\
         ⚠ Action checklist:\n\
         1. Re-evaluate if blocker is resolved\n\
         2. If resolved → clear waiting_on, resume work\n\
         3. If still blocked → escalate to lead with status update"
    )
}

/// #event-bus pattern #4: deliver the stale-waiting alert to the agent itself +
/// the team orchestrator (if any). Shared by the legacy path AND the subscriber
/// ([`handle_event`]), so the two are byte-identical by construction.
fn deliver_stale_alert(home: &Path, agent: &str, condition: &str, elapsed_min: i64) {
    let text = waiting_on_stale_text(agent, condition, elapsed_min);
    // Alert the agent itself
    emit_to(home, agent, "waiting_on_stale", &text, Some(agent));
    // Alert team orchestrator (if any)
    if let Some(team) = crate::teams::find_team_for(home, agent) {
        if let Some(ref orch) = team.orchestrator {
            if orch != agent {
                emit_to(home, orch, "waiting_on_stale", &text, Some(agent));
            }
        }
    }
}

/// #event-bus pattern #4: subscriber — rebuild the alert from the event.
fn handle_event(event: &crate::daemon::event_bus::Event) -> bool {
    if let crate::daemon::event_bus::EventKind::WaitingOnStale {
        agent,
        condition,
        elapsed_min,
    } = &event.kind
    {
        deliver_stale_alert(&event.home, agent, condition, *elapsed_min);
        true
    } else {
        false
    }
}

/// #event-bus pattern #4: register the delivery subscriber at daemon startup.
/// Home-agnostic — the home travels on each event. Wired beside the other
/// patterns in `daemon::mod`.
pub fn register_subscriber() {
    crate::daemon::event_bus::global().subscribe(handle_event);
}

fn emit_to(home: &Path, recipient: &str, kind: &str, text: &str, correlation_agent: Option<&str>) {
    let source = format!("system:{kind}");
    if let Err(e) = crate::inbox::notify_system(
        home,
        recipient,
        &source,
        kind,
        text,
        correlation_agent,
        None,
    ) {
        tracing::warn!(error = %e, recipient, kind, "waiting_on_stale: enqueue failed");
    } else {
        tracing::info!(
            recipient,
            agent = correlation_agent.unwrap_or(""),
            "waiting_on_stale: emitted alert"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("agend-test-waiting-stale-{tag}-{id}"))
    }

    fn write_metadata(home: &Path, agent: &str, waiting_on: &str, since: &str) {
        let dir = home.join("metadata");
        std::fs::create_dir_all(&dir).unwrap();
        let meta = serde_json::json!({
            "waiting_on": waiting_on,
            "waiting_on_since": since,
        });
        std::fs::write(
            dir.join(format!("{agent}.json")),
            serde_json::to_string_pretty(&meta).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn detects_stale_waiting_on() {
        let home = tmp_home("detect");
        let since = (chrono::Utc::now() - chrono::Duration::minutes(20)).to_rfc3339();
        write_metadata(&home, "dev-1", "review from reviewer", &since);
        std::fs::create_dir_all(home.join("inbox")).unwrap();

        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted, false);

        assert!(last_alerted.contains_key("dev-1"));
        let inbox_file = home.join("inbox").join("dev-1.jsonl");
        assert!(inbox_file.exists(), "inbox file should exist");
        let content = std::fs::read_to_string(&inbox_file).unwrap();
        assert!(content.contains("waiting_on_stale"));
        assert!(content.contains("review from reviewer"));

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn boot_seed_suppresses_existing_stale_then_no_reburst() {
        // #1739: the first scan after a fresh daemon start seeds an
        // already-stale waiter into the dedup WITHOUT emitting (restart should
        // not re-alert backlog the operator saw before), and a subsequent scan
        // does not re-burst it.
        let home = tmp_home("bootseed");
        let since = (chrono::Utc::now() - chrono::Duration::minutes(20)).to_rfc3339();
        write_metadata(&home, "dev-bs", "review from reviewer", &since);
        std::fs::create_dir_all(home.join("inbox")).unwrap();

        let mut last_alerted = HashMap::new();
        // seeding scan: record the existing stale waiter, but do NOT emit.
        scan_and_emit(&home, &mut last_alerted, true);
        assert!(
            last_alerted.contains_key("dev-bs"),
            "boot-seed must record the existing stale waiter in the dedup"
        );
        assert!(
            !home.join("inbox").join("dev-bs.jsonl").exists(),
            "boot-seed must NOT emit for restart-existing backlog (negative-probe: \
             removing the `if !seeding` gate makes this fire)"
        );
        // next normal scan: the seeded waiter stays suppressed (no boot-burst).
        scan_and_emit(&home, &mut last_alerted, false);
        assert!(
            !home.join("inbox").join("dev-bs.jsonl").exists(),
            "seeded waiter must remain suppressed on the next scan within REALERT"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn skips_fresh_waiting_on() {
        let home = tmp_home("fresh");
        let since = (chrono::Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
        write_metadata(&home, "dev-2", "CI result", &since);

        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted, false);

        assert!(!last_alerted.contains_key("dev-2"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn dedup_suppresses_repeated_alert() {
        let home = tmp_home("dedup");
        let since = (chrono::Utc::now() - chrono::Duration::minutes(20)).to_rfc3339();
        write_metadata(&home, "dev-3", "task from lead", &since);
        std::fs::create_dir_all(home.join("inbox")).unwrap();

        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted, false);
        assert!(last_alerted.contains_key("dev-3"));

        let count_lines = || {
            std::fs::read_to_string(home.join("inbox").join("dev-3.jsonl"))
                .unwrap_or_default()
                .lines()
                .count()
        };
        let first_count = count_lines();

        scan_and_emit(&home, &mut last_alerted, false);
        assert_eq!(
            count_lines(),
            first_count,
            "dedup should suppress second alert"
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn tracker_throttles_scans() {
        let home = tmp_home("throttle");
        let mut tracker = WaitingOnStaleTracker::default();
        for _ in 0..29 {
            assert!(!tracker.maybe_scan(&home));
        }
        assert!(tracker.maybe_scan(&home));
        assert!(!tracker.maybe_scan(&home));
        let _ = std::fs::remove_dir_all(&home);
    }

    fn drained_payloads(
        home: &Path,
        recipient: &str,
    ) -> Vec<(String, Option<String>, String, Option<String>)> {
        crate::inbox::drain(home, recipient)
            .into_iter()
            .map(|m| (m.from, m.kind, m.text, m.correlation_id))
            .collect()
    }

    /// #event-bus pattern #4 PARITY (gate-ON): the bus `emit`→subscriber path
    /// delivers payloads byte-identical (from/kind/text/correlation) to the legacy
    /// direct enqueue. Exercises the REAL bus emit→fan-out→subscriber wiring. No
    /// `env_lock`: the recipients (agent + team orchestrator) are data/file-derived,
    /// not env-derived, so there is no process-global env race.
    #[test]
    fn gate_on_emit_subscriber_matches_legacy_direct_enqueue() {
        let (agent, condition, elapsed_min) = ("dev-parity", "review from reviewer", 20_i64);

        // Legacy direct deliver (the gate-OFF path).
        let home_legacy = tmp_home("parity-legacy");
        std::fs::create_dir_all(home_legacy.join("inbox")).unwrap();
        deliver_stale_alert(&home_legacy, agent, condition, elapsed_min);

        // Bus emit→subscriber (the gate-ON path) — real fan-out via a test bus.
        let home_bus = tmp_home("parity-bus");
        std::fs::create_dir_all(home_bus.join("inbox")).unwrap();
        let bus = crate::daemon::event_bus::EventBus::new();
        bus.subscribe(handle_event);
        bus.emit(
            &home_bus,
            crate::daemon::event_bus::EventKind::WaitingOnStale {
                agent: agent.to_string(),
                condition: condition.to_string(),
                elapsed_min,
            },
        );

        let legacy = drained_payloads(&home_legacy, agent);
        let viabus = drained_payloads(&home_bus, agent);
        assert_eq!(
            legacy, viabus,
            "emit→subscriber payload must equal legacy direct enqueue"
        );
        assert!(!legacy.is_empty(), "the agent must be alerted");
        let _ = std::fs::remove_dir_all(&home_legacy);
        let _ = std::fs::remove_dir_all(&home_bus);
    }

    /// #event-bus Step 2 (legacy-zero): the scan emits to the global bus; the
    /// registered subscriber delivers to the agent + orchestrator at the event's
    /// home (this test's home).
    #[test]
    fn scan_delivers_via_bus() {
        let home = tmp_home("via-bus");
        std::fs::create_dir_all(home.join("inbox")).unwrap();
        let since = (chrono::Utc::now() - chrono::Duration::minutes(20)).to_rfc3339();
        write_metadata(&home, "dev-gateoff", "blocker", &since);
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted, false);
        assert!(
            !drained_payloads(&home, "dev-gateoff").is_empty(),
            "#event-bus Option A: gate-off must deliver via the legacy path (no regression)"
        );
        let _ = std::fs::remove_dir_all(&home);
    }
}
