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
    pub(crate) fn maybe_scan(&mut self, home: &Path, live: &HashMap<String, String>) -> bool {
        if !self.gate.fire() {
            return false;
        }
        let seeding = !self.seeded;
        self.seeded = true;
        scan_and_emit(home, &mut self.last_alerted_at, seeding, live);
        true
    }

    /// CR-2026-06-14: prune the dedup map to currently-active agents, mirroring
    /// `ConflictNotifyTracker::retain_active` (the #1923 leak class). Without
    /// this, `last_alerted_at` grows one permanent entry per agent that ever
    /// went stale, and a same-name redeploy inherits a stale dedup timestamp
    /// that can false-suppress a real alert. Driven each tick from
    /// `WaitingOnStaleHandler::run`.
    pub(crate) fn retain_active(&mut self, active: &HashMap<String, String>) {
        self.last_alerted_at
            .retain(|stem, _| active.contains_key(stem));
    }
}

/// Scan all metadata files for stale `waiting_on` conditions and emit
/// alerts. Exposed `pub(crate)` for unit tests.
pub(crate) fn scan_and_emit(
    home: &Path,
    last_alerted: &mut HashMap<String, chrono::DateTime<chrono::Utc>>,
    seeding: bool,
    live: &HashMap<String, String>,
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
        // Deliver by NAME, not by stem. `inbox::notify_system` →
        // `inbox_path_resolved` → `fleet::resolve_uuid(home, name)` maps a
        // configured instance onto its `inbox/<uuid>.jsonl`, so a raw id stem
        // can land in the right FILE by coincidence — but it is not the
        // canonical recipient: `teams::find_team_for` matches membership by
        // name, so the orchestrator copy is lost, and the alert text and
        // correlation carry a UUID instead of the agent's name. A stem absent
        // from the map has no live instance behind it, so there is no
        // recipient at all — skip it. The metadata file is left alone;
        // reaping it is not this handler's concern.
        let Some(recipient) = live.get(agent) else {
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
                    agent: recipient.clone(),
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

    /// The stem -> name map a direct `scan_and_emit` call runs under. These
    /// unit tests drive the scan below the handler, so they state the map the
    /// handler would have built for them.
    fn live_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(stem, name)| ((*stem).to_string(), (*name).to_string()))
            .collect()
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
        scan_and_emit(
            &home,
            &mut last_alerted,
            false,
            &live_map(&[("dev-1", "dev-1")]),
        );

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
        scan_and_emit(
            &home,
            &mut last_alerted,
            true,
            &live_map(&[("dev-bs", "dev-bs")]),
        );
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
        scan_and_emit(
            &home,
            &mut last_alerted,
            false,
            &live_map(&[("dev-bs", "dev-bs")]),
        );
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
        scan_and_emit(
            &home,
            &mut last_alerted,
            false,
            &live_map(&[("dev-2", "dev-2")]),
        );

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
        scan_and_emit(
            &home,
            &mut last_alerted,
            false,
            &live_map(&[("dev-3", "dev-3")]),
        );
        assert!(last_alerted.contains_key("dev-3"));

        let count_lines = || {
            std::fs::read_to_string(home.join("inbox").join("dev-3.jsonl"))
                .unwrap_or_default()
                .lines()
                .count()
        };
        let first_count = count_lines();

        scan_and_emit(
            &home,
            &mut last_alerted,
            false,
            &live_map(&[("dev-3", "dev-3")]),
        );
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
            assert!(!tracker.maybe_scan(&home, &live_map(&[])));
        }
        assert!(tracker.maybe_scan(&home, &live_map(&[])));
        assert!(!tracker.maybe_scan(&home, &live_map(&[])));
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
        scan_and_emit(
            &home,
            &mut last_alerted,
            false,
            &live_map(&[("dev-gateoff", "dev-gateoff")]),
        );
        assert!(
            !drained_payloads(&home, "dev-gateoff").is_empty(),
            "#event-bus Option A: gate-off must deliver via the legacy path (no regression)"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    // ── Stem key-space: re-alert loop + delivery routing ────────────────
    //
    // Dedup is keyed by metadata FILE STEM, which is an `InstanceId` once an
    // instance carries an id in fleet.yaml. Pruning that key space against
    // names alone dropped every id-stemmed entry one tick after a scan wrote
    // it, so `REALERT_INTERVAL_SECS` was unreachable and the waiter re-alerted
    // every cadence (six 72–76-day-old files did exactly that in production).
    //
    // These drive the REAL per-tick handler, not `scan_and_emit` directly, so
    // the prune sits in the loop as it does in the daemon. No sleeps: the
    // cadence is a tick counter, not wall clock.

    use crate::agent::{AgentRegistry, ExternalRegistry};
    use crate::daemon::per_tick::supervisor_trackers::WaitingOnStaleHandler;
    use crate::daemon::per_tick::{PerTickHandler, TickContext};
    use parking_lot::Mutex;
    use std::sync::Arc;

    /// An abandoned instance's metadata stem: a well-formed `InstanceId` that
    /// is simply absent from the registry.
    const GHOST_STEM: &str = "8f9cf087-1d98-4139-9372-0292c0164094";
    const LIVE_NAME: &str = "dev-live";

    fn stale_since(mins: i64) -> String {
        (chrono::Utc::now() - chrono::Duration::minutes(mins)).to_rfc3339()
    }

    /// Alerts actually delivered to `recipient`, read through the production
    /// drain path so the name→id inbox resolution is the real one rather than
    /// a re-implementation. Drains, so call once per recipient per assertion.
    fn alert_rows(home: &Path, recipient: &str) -> usize {
        crate::inbox::drain(home, recipient)
            .into_iter()
            .filter(|m| m.kind.as_deref() == Some("waiting_on_stale"))
            .count()
    }

    /// Drive `cadences` whole scan cadences through the real handler, with the
    /// per-tick prune running on every tick (that prune is half of the defect).
    fn run_cadences(handler: &WaitingOnStaleHandler, ctx: &TickContext<'_>, cadences: usize) {
        for _ in 0..(cadences * TICKS_PER_SCAN as usize) {
            handler.run(ctx);
        }
    }

    /// Minimal fleet.yaml. A configured `id` is what makes an instance's
    /// metadata stem id-shaped (`agent_ops::metadata_path_resolved`); without
    /// one the stem is the name. `teams` is appended verbatim.
    fn write_fleet(home: &Path, instances: &[(&str, &str)], teams: &str) {
        let mut y = String::new();
        if !instances.is_empty() {
            y.push_str("instances:\n");
            for (name, id) in instances {
                y.push_str(&format!("  {name}:\n    id: {id}\n"));
            }
        }
        y.push_str(teams);
        std::fs::write(home.join("fleet.yaml"), y).unwrap();
    }

    /// One live agent. Returns its `InstanceId`, which is ALSO its metadata
    /// stem in production — the distinction this defect turns on.
    fn registry_with_live() -> (AgentRegistry, crate::types::InstanceId) {
        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let id = crate::types::InstanceId::new();
        crate::agent::lock_registry(&registry)
            .insert(id, crate::agent::mk_test_handle(LIVE_NAME, id));
        (registry, id)
    }

    /// RED: an abandoned stem is seeded by the #1739 boot latch and must then
    /// stay silent for at least `REALERT_INTERVAL_SECS`. Today the per-tick
    /// prune drops its dedup entry, so every 5-minute scan re-emits it.
    #[test]
    fn ghost_metadata_must_not_realert_after_boot_seed() {
        let home = tmp_home("ghost-realert");
        std::fs::create_dir_all(home.join("inbox")).unwrap();
        write_metadata(&home, GHOST_STEM, "operator direction", &stale_since(20));

        let (registry, _id) = registry_with_live();
        let externals: ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let configs = Arc::new(Mutex::new(HashMap::new()));
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };

        // Cadence 1 = #1739 boot seed (records, must not emit). Cadences 2 and
        // 3 are normal scans, both well inside REALERT_INTERVAL_SECS.
        run_cadences(&WaitingOnStaleHandler::new(), &ctx, 3);

        let rows = alert_rows(&home, GHOST_STEM);
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(
            rows, 0,
            "a stem seeded by the boot latch must not re-alert within \
             REALERT_INTERVAL_SECS; the per-tick \
             `retain_active(&live_agent_names(..))` prune compares NAMES against \
             id-shaped metadata stems, so the dedup entry is dropped every tick \
             and each scan re-emits (observed: {rows})"
        );
    }

    /// A LIVE agent whose metadata stem is its `InstanceId` (the production
    /// shape) must alert exactly once, be suppressed for the rest of
    /// `REALERT_INTERVAL_SECS`, and be delivered by NAME.
    ///
    /// The recipient half is the point. A configured instance's inbox IS
    /// `inbox/<uuid>.jsonl` (`inbox_path_resolved` →
    /// `fleet::resolve_uuid(home, name)`), so emitting the raw stem is not
    /// caught by the agent's own row count — it is caught by the ORCHESTRATOR
    /// row, because `teams::find_team_for` matches membership by name and a
    /// UUID matches no team. Hence the assertion covers both recipients. It is
    /// also the over-fix lock: filtering stems by live agent NAMES alone
    /// silences the live agent entirely and both counts read 0.
    #[test]
    fn live_agent_id_stem_alerts_once_to_the_agent_and_orchestrator() {
        let home = tmp_home("live-id-stem");
        std::fs::create_dir_all(home.join("inbox")).unwrap();

        let (registry, id) = registry_with_live();
        // Configured id ⇒ id-shaped stem; team wiring so the orchestrator copy
        // travels the real subscriber path (`teams::find_team_for`).
        write_fleet(
            &home,
            &[(LIVE_NAME, &id.full())],
            &format!(
                "teams:\n  ops:\n    members: [{LIVE_NAME}, lead-x]\n    orchestrator: lead-x\n"
            ),
        );
        let externals: ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let configs = Arc::new(Mutex::new(HashMap::new()));
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };
        let handler = WaitingOnStaleHandler::new();
        let live_stem = id.full();

        // Boot seed with nothing stale yet, so the live agent going stale
        // afterwards is a genuine new alert rather than restart backlog.
        run_cadences(&handler, &ctx, 1);
        write_metadata(&home, &live_stem, "review from reviewer", &stale_since(20));
        // Cadence 2 alerts; cadence 3 must be suppressed by REALERT.
        run_cadences(&handler, &ctx, 2);

        let observed = (alert_rows(&home, LIVE_NAME), alert_rows(&home, "lead-x"));
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(
            observed,
            (1, 1),
            "id-stemmed metadata must reach the agent and its team orchestrator \
             exactly once — emitting the raw stem instead of the name would \
             reach neither; observed (agent, orchestrator) = {observed:?}"
        );
    }

    /// Control (passes today, must keep passing): the #1739 boot latch
    /// swallows the first cadence for every stem, live or abandoned — so the
    /// repeats the RED above counts provably come from later scans, not boot.
    #[test]
    fn boot_seed_cadence_emits_for_neither_stem() {
        let home = tmp_home("boot-latch-both");
        std::fs::create_dir_all(home.join("inbox")).unwrap();

        let (registry, id) = registry_with_live();
        let live_stem = id.full();
        write_fleet(&home, &[(LIVE_NAME, &live_stem)], "");
        write_metadata(&home, GHOST_STEM, "operator direction", &stale_since(20));
        write_metadata(&home, &live_stem, "review from reviewer", &stale_since(20));

        let externals: ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let configs = Arc::new(Mutex::new(HashMap::new()));
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };

        run_cadences(&WaitingOnStaleHandler::new(), &ctx, 1);

        let (ghost_rows, live_rows) =
            (alert_rows(&home, GHOST_STEM), alert_rows(&home, &live_stem));
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(
            (ghost_rows, live_rows),
            (0, 0),
            "#1739 boot seed must record pre-existing stale waiters without \
             emitting, for both an abandoned and a live stem"
        );
    }

    /// Control (passes before and after the id-key correction): an instance
    /// with no id in fleet.yaml keeps a NAME-stemmed metadata file
    /// (`metadata_path_resolved`'s legacy fallback), and its dedup entry must
    /// survive the prune too. Retaining only the registry's `InstanceId` keys
    /// would start re-alerting these every scan — this pins that both stem
    /// shapes stay retained.
    #[test]
    fn legacy_name_stem_alerts_once_then_stays_suppressed() {
        let home = tmp_home("legacy-name-stem");
        std::fs::create_dir_all(home.join("inbox")).unwrap();

        let (registry, _id) = registry_with_live();
        let externals: ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let configs = Arc::new(Mutex::new(HashMap::new()));
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };
        let handler = WaitingOnStaleHandler::new();

        run_cadences(&handler, &ctx, 1);
        write_metadata(&home, LIVE_NAME, "review from reviewer", &stale_since(20));
        run_cadences(&handler, &ctx, 2);

        let rows = alert_rows(&home, LIVE_NAME);
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(
            rows, 1,
            "a live agent's legacy name-stemmed metadata must alert once then \
             stay suppressed (observed: {rows})"
        );
    }

    /// A hyphenated UUID is a legal instance name, so agent A may be NAMED
    /// exactly agent B's `InstanceId`. The snapshot inserts name aliases first
    /// and id entries second, so B's own `metadata/<B_id>.json` deterministically
    /// reaches B and team-B, never A.
    #[test]
    fn id_stem_colliding_with_another_agents_name_routes_to_the_id_owner() {
        let home = tmp_home("stem-collision");
        std::fs::create_dir_all(home.join("inbox")).unwrap();

        let id_b = crate::types::InstanceId::new();
        let colliding_name = id_b.full(); // agent A is NAMED B's InstanceId
        let id_a = crate::types::InstanceId::new();

        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut reg = crate::agent::lock_registry(&registry);
            reg.insert(id_a, crate::agent::mk_test_handle(&colliding_name, id_a));
            reg.insert(id_b, crate::agent::mk_test_handle("dev-b", id_b));
        }
        // Both configured, so both stems are id-shaped; distinct teams so a
        // misroute shows up in the escalation too.
        write_fleet(
            &home,
            &[(&colliding_name, &id_a.full()), ("dev-b", &id_b.full())],
            &format!(
                "teams:\n  team-a:\n    members: [{colliding_name}, lead-a]\n    \
                 orchestrator: lead-a\n  team-b:\n    members: [dev-b, lead-b]\n    \
                 orchestrator: lead-b\n"
            ),
        );

        let externals: ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let configs = Arc::new(Mutex::new(HashMap::new()));
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };
        let handler = WaitingOnStaleHandler::new();

        run_cadences(&handler, &ctx, 1);
        write_metadata(
            &home,
            &colliding_name,
            "review from reviewer",
            &stale_since(20),
        );
        run_cadences(&handler, &ctx, 2);

        let observed = (
            alert_rows(&home, "dev-b"),
            alert_rows(&home, "lead-b"),
            alert_rows(&home, &colliding_name),
            alert_rows(&home, "lead-a"),
        );
        // `colliding_name` is A's NAME, so the drain above resolves A's own
        // inbox, not B's id file.
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(
            observed,
            (1, 1, 0, 0),
            "an id-shaped stem must reach its id owner B and team-B exactly \
             once, never the agent merely NAMED that UUID; observed \
             (b, lead_b, a, lead_a) = {observed:?}"
        );
    }

    /// The absent-owner half of the same collision: B has LEFT the registry but
    /// its `metadata/<B_id>.json` remains, while live A is NAMED `B_UUID`. The
    /// orphan must be skipped, never revived as A's alert — resolving each live
    /// agent's single authoritative stem is what makes that fail closed, since
    /// A's own stem is its configured id, not its name.
    #[test]
    fn orphaned_id_stem_never_falls_back_to_a_live_agent_named_that_uuid() {
        let home = tmp_home("orphan-no-fallback");
        std::fs::create_dir_all(home.join("inbox")).unwrap();

        let departed_b = crate::types::InstanceId::new(); // B: no registry entry
        let colliding_name = departed_b.full(); // A is NAMED B's id
        let id_a = crate::types::InstanceId::new();

        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        crate::agent::lock_registry(&registry)
            .insert(id_a, crate::agent::mk_test_handle(&colliding_name, id_a));
        write_fleet(
            &home,
            &[(&colliding_name, &id_a.full())],
            &format!(
                "teams:\n  team-a:\n    members: [{colliding_name}, lead-a]\n    \
                 orchestrator: lead-a\n"
            ),
        );

        let externals: ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let configs = Arc::new(Mutex::new(HashMap::new()));
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };
        let handler = WaitingOnStaleHandler::new();

        // Boot/seed BEFORE the orphan exists, so a later delivery cannot be
        // excused as restart backlog.
        run_cadences(&handler, &ctx, 1);
        write_metadata(
            &home,
            &colliding_name,
            "review from reviewer",
            &stale_since(20),
        );
        run_cadences(&handler, &ctx, 2);

        let observed = (
            alert_rows(&home, &colliding_name),
            alert_rows(&home, "lead-a"),
        );
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(
            observed,
            (0, 0),
            "a departed instance's orphaned id-stemmed metadata must be skipped, \
             not delivered to the live agent that merely happens to be NAMED \
             that UUID; observed (agent_a, lead_a) = {observed:?}"
        );
    }
}
