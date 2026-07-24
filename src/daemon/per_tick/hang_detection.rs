//! Hang detection + health decay: walks the agent registry every tick,
//! decays each agent's health, classifies hangs, logs warnings on the
//! transition. Extracted verbatim from `src/daemon/mod.rs:591-626`
//! (pre-T-B4) — same iteration order, same lock-acquisition chain,
//! same `tracing::warn!` field names.
//!
//! **Cohort note** (T-B4): this handler MUTATES `core.health` (via
//! `maybe_decay` + the implicit `check_hang` side-effects on transition
//! tracking), and is followed in the same tick by [`super::watchdog`]
//! which also mutates `core.health` (BlockedReason classification). The
//! two handlers are extracted together so the same-tick mutation
//! sequence stays contained in a single PR — splitting would route the
//! sequence across module boundaries with no compile-time signal that
//! the ordering matters.

use super::{PerTickHandler, TickContext};
use crate::agent;
use std::sync::Arc;

pub(crate) struct HangDetectionHandler;

impl HangDetectionHandler {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl PerTickHandler for HangDetectionHandler {
    fn name(&self) -> &'static str {
        "hang_detection"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        // Lock-acquisition order per docs/DAEMON-LOCK-ORDERING.md:
        // registry (L0, root) → per-agent core (L1) → heartbeat_pair
        // (L3 leaf, acquired+released synchronously by `snapshot_for`).
        // Rule 3: the leaf lock is never held while acquiring another
        // lock — `snapshot_for` returns a copy and drops its pair guard
        // before `check_hang` runs.
        // #941: use the holder-tracking wrapper so the periodic
        // ThreadDumpHandler can surface "hang_detection wedged" if this
        // handler ever blocks the main loop (the H1 hypothesis from
        // #932 RCA).
        // Phase 1 (under the registry lock): decay + classify hangs, and collect
        // the names currently in Hung. NO teams-file read here — that runs
        // lock-free in phase 2 (#1530 / DAEMON-LOCK-ORDERING: no file IO under
        // the registry lock).
        // `hung_now`: every agent currently Hung. `newly_hung`: the subset whose
        // `check_hang` returned true THIS tick (first detection — `hung_since`
        // was just anchored). #1744-H2 persists a self-orch's anchor on entry
        // (not only on escalation) so a restart in the first confirm-window
        // doesn't reset it.
        // `hung_now`: every agent currently Hung. `newly_hung`: entered this tick.
        // `left_hung`: was Hung before this tick's `check_hang` and is no longer —
        // a recovery/exit that cleared `hung_since` in memory; #1744-H2 must
        // persist that CLEAR (else a restart rehydrates the stale anchor and the
        // next unrelated Hung re-entry's `get_or_insert` keeps it → false
        // immediate escalation. codex catch).
        // #2944: Phase 1 retains the `Arc<CoreMutex<AgentCore>>` for each
        // hung agent so Phase 2 can use them directly without re-locking
        // the registry.  `left_hung` collects names only — Phase 3 uses
        // the narrow `clear_hung_since` (no full snapshot needed).
        type CoreArc = Arc<crate::sync_audit::CoreMutex<crate::agent::AgentCore>>;
        #[allow(clippy::type_complexity)]
        let (hung_now, newly_hung, left_hung): (
            Vec<(String, CoreArc)>,
            std::collections::HashSet<String>,
            Vec<String>,
        ) = {
            let reg = agent::lock_registry_tracked(ctx.registry, "hang_detection");
            let mut hung = Vec::new();
            let mut newly = std::collections::HashSet::new();
            let mut left = Vec::new();
            for handle in reg.values() {
                let name = handle.name.as_str();
                let process_alive = handle
                    .child
                    .lock()
                    .process_id()
                    .map(crate::process::is_pid_alive)
                    .unwrap_or(false);
                let mut core = handle.core.lock();
                core.health.maybe_decay(process_alive);
                let was_hung = core.health.state == crate::health::HealthState::Hung;
                // KEEP-RAW (#2465): health/recovery must see the raw screen state — feeding the
                // promoted/observed state could let a stale/false 'Active' hook MASK a genuinely
                // stuck agent (the inverse of the SRL false-idle bug). Do NOT migrate to operated_state.
                let agent_state = core.state.current;
                let silent = core.state.last_output.elapsed();
                // F9 (#685 sub-task 4): productive-silence reads the new
                // `last_productive_output` field which is bumped only when
                // `infer_productivity` returns a Productive signal. Default
                // shadow-mode in `check_hang` gates classification on
                // `AGEND_PRODUCTIVE_GATE=1`.
                let silent_productive = core.state.productive_silence();
                let pair = crate::daemon::heartbeat_pair::snapshot_for(name);
                let just_detected = core.health.check_hang(
                    agent_state,
                    silent,
                    silent_productive,
                    pair.last_input_at_ms,
                    pair.heartbeat_at_ms,
                );
                if just_detected {
                    tracing::warn!(
                        agent = %name,
                        state = agent_state.display_name(),
                        silent = ?silent,
                        "hang detected"
                    );
                }
                let now_hung = core.health.state == crate::health::HealthState::Hung;
                if now_hung {
                    hung.push((name.to_string(), Arc::clone(&handle.core)));
                    if just_detected {
                        newly.insert(name.to_string());
                    }
                } else if was_hung {
                    // Hung → not-Hung this tick: `check_hang` cleared `hung_since`.
                    left.push(name.to_string());
                }
            }
            (hung, newly, left)
        };

        // Common case: no hung or left-hung agents → skip the fleet load entirely.
        if hung_now.is_empty() && left_hung.is_empty() {
            return;
        }

        // Phase 2/3: shared fleet snapshot — one load per tick instead of
        // O(hung) self_orch_status calls (each of which deep-clones the config).
        // Uses `try_load_fleet` semantics: missing fleet.yaml → determinate No
        // (no teams configured), file-exists-but-unreadable → Unknown → escalate
        // (#1744-M7 fail-closed).
        let fleet_result = crate::teams::try_load_fleet(ctx.home);
        let self_orch = |name: &str| -> crate::teams::SelfOrchStatus {
            match &fleet_result {
                Ok(fleet) => {
                    match crate::teams::find_team_for_in(fleet, name).and_then(|t| t.orchestrator) {
                        Some(orch) if orch == name => crate::teams::SelfOrchStatus::Yes,
                        _ => crate::teams::SelfOrchStatus::No,
                    }
                }
                Err(_) => crate::teams::SelfOrchStatus::Unknown,
            }
        };

        // Phase 2 (#1701 Hung half): escalate self-orchestrators stuck past the
        // confirm-window. Uses stored Arc<CoreMutex> from Phase 1 — no registry
        // re-lock. #2944: lock_registry eliminated from this loop.
        for (name, core_arc) in &hung_now {
            if self_orch(name) == crate::teams::SelfOrchStatus::No {
                continue;
            }
            let (due, snapshot) = {
                let mut core = core_arc.lock();
                let due = core.health.hung_escalation_due(HUNG_ESCALATE_AFTER);
                (due, core.health.escalation_snapshot())
            };
            if due {
                notify_self_orch_hung(name);
            }
            if newly_hung.contains(name) || due {
                crate::daemon::escalation_persist::persist(ctx.home, name, &snapshot);
            }
        }

        // Phase 3 (#1744-H2): clear persisted hung anchor for left-Hung
        // self-orchestrators.  Uses the narrow `clear_hung_since` — no full
        // snapshot persist, no registry re-lock.
        for name in &left_hung {
            clear_left_hung_anchor(ctx, name, &self_orch);
        }
    }
}

/// #1744-H2 + #1870-H2: clear the persisted `hung_since` anchor for a
/// left-Hung self-orchestrator. Uses the narrow `clear_hung_since` so only
/// the anchor is cleared — crash budget / cooldowns are preserved and no
/// stale full snapshot is written under a potentially reused name.
/// #2944: uses a shared fleet closure — no per-agent fleet load.
fn clear_left_hung_anchor(
    ctx: &TickContext<'_>,
    name: &str,
    self_orch: &dyn Fn(&str) -> crate::teams::SelfOrchStatus,
) {
    if self_orch(name) == crate::teams::SelfOrchStatus::No {
        return;
    }
    if crate::daemon::escalation_persist::load_for(ctx.home, name).is_none() {
        return;
    }
    crate::daemon::escalation_persist::clear_hung_since(ctx.home, name);
    tracing::debug!(agent = %name, "#1744-H2: cleared hung anchor on Hung exit");
}

/// #1701: how long a self-orchestrator must stay Hung before its hang escalates
/// to the operator — a confirm-window FP-filter on top of `check_hang`'s
/// Hung/IdleLong split (which already excludes the 04:00 idle false-alarm). 60s
/// is conservative: a real hang pages within a minute (orchestrator recovery is
/// not millisecond-critical), while transient residual FPs (F39 stale-Thinking
/// scrollback, F10 1-byte-output exit, E1 keystroke-draining) don't survive it.
/// TODO: revisit if #685 enables the F9 productive-path by default (it shifts
/// Hung sensitivity) — see docs/HUNG-STATE-TRANSITIONS.md.
const HUNG_ESCALATE_AFTER: std::time::Duration = std::time::Duration::from_secs(60);

/// #1701: page the operator that a self-orchestrator is hung. Mirrors
/// `crash_respawn::notify_self_orch_crash` — same `gated_notify(Error)`
/// Sleep-penetrating path (#1595/#1717). Channel + name only (no registry).
fn notify_self_orch_hung(name: &str) {
    tracing::warn!(
        agent = %name,
        "#1701: self-orchestrator hung past confirm-window — escalating P0 to operator"
    );
    let msg = format!(
        "🛑 {name} (team orchestrator) has been HUNG ≥{}s — no peer can relay this and \
         the team is stalled until it recovers. Manual intervention likely (check the \
         pane / interrupt / re-prime).",
        HUNG_ESCALATE_AFTER.as_secs()
    );
    // #1744-M6: every registered channel (multi-channel-safe P0).
    crate::channel::notify_all_escalation_channels(
        name,
        crate::channel::NotifySeverity::Error,
        &msg,
        false,
    );
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::agent::{AgentRegistry, ExternalRegistry};
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::sync::Arc;

    /// Smoke test: empty registry → no-op. The interesting integration
    /// paths (hang threshold tripping, heartbeat_pair freshness, health
    /// decay) are covered by the existing tests in `crate::health` and
    /// `daemon::supervisor`; this PR is pure relocation so we only need
    /// to prove `run()` doesn't panic on the empty case.
    #[test]
    fn run_is_noop_on_empty_registry() {
        let home = std::env::temp_dir().join(format!(
            "agend-hang-handler-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).ok();
        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let externals: ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let configs = Arc::new(Mutex::new(HashMap::new()));
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };

        HangDetectionHandler::new().run(&ctx);

        assert!(registry.lock().is_empty());
        std::fs::remove_dir_all(&home).ok();
    }

    /// Name pin — used by future Vec<Box<dyn PerTickHandler>> aggregator
    /// for tracing spans / diagnostic dumps.
    #[test]
    fn name_matches_module() {
        assert_eq!(HangDetectionHandler::new().name(), "hang_detection");
    }

    /// #2944: Phase 2 must use the stored `Arc<CoreMutex<AgentCore>>` from
    /// Phase 1 — no per-candidate registry re-lock.
    #[test]
    fn phase2_does_not_relock_registry_per_hung_agent() {
        let src = include_str!("../per_tick/hang_detection.rs");
        let phase2_marker = "Phase 2/3";
        let phase2_start = src
            .find(phase2_marker)
            .expect("Phase 2/3 comment must exist");
        // Scan only up to the test module boundary so the assertion
        // string itself doesn't self-match.
        let test_mod = "#[cfg(test)]";
        let phase2_src = &src[phase2_start..];
        let prod_only = match phase2_src.find(test_mod) {
            Some(i) => &phase2_src[..i],
            None => phase2_src,
        };
        // Build the needle from parts to avoid self-matching.
        let needle = ["lock_registry", "("].concat();
        assert!(
            !prod_only.contains(&needle),
            "#2944: Phase 2/3 production code must not re-lock the registry"
        );
    }

    /// #1870-H2 + #2944: a self-orch that left Hung gets its stale anchor
    /// cleared via the narrow `clear_hung_since` path — no full snapshot
    /// persist, no registry re-lock.
    #[test]
    fn left_hung_anchor_cleared_narrow_1870_h2() {
        let home = std::env::temp_dir().join(format!(
            "agend-h2-narrow-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).ok();
        crate::teams::create(
            &home,
            &serde_json::json!({"name": "t", "members": ["orch-1"], "orchestrator": "orch-1"}),
        );
        crate::daemon::escalation_persist::persist(
            &home,
            "orch-1",
            &crate::health::PersistedEscalation {
                hung_since_epoch_ms: Some(1000),
                ..Default::default()
            },
        );
        assert!(
            crate::daemon::escalation_persist::load_for(&home, "orch-1")
                .unwrap()
                .hung_since_epoch_ms
                .is_some(),
            "precondition: a stale anchor is persisted"
        );

        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let externals: ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let configs = Arc::new(Mutex::new(HashMap::new()));
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };

        let self_orch = |_: &str| crate::teams::SelfOrchStatus::Yes;
        clear_left_hung_anchor(&ctx, "orch-1", &self_orch);

        assert!(
            crate::daemon::escalation_persist::load_for(&home, "orch-1")
                .unwrap()
                .hung_since_epoch_ms
                .is_none(),
            "#1870-H2: narrow clear must remove the stale hung anchor"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Regression through `HangDetectionHandler::run`: a left-Hung agent on a
    /// system with NO fleet.yaml must NOT have its escalation store modified.
    /// Missing fleet.yaml is a determinate `No` (no teams configured), not
    /// `Unknown` — the `self_orch` gate skips non-self-orchestrators.
    ///
    /// RED proof: on the defective `load_arc` wiring, missing fleet.yaml
    /// → Err → `Unknown` ≠ `No` → Phase 3 proceeds → `clear_hung_since`
    /// fires → assertion fails.
    #[test]
    fn no_fleet_yaml_left_hung_agent_escalation_store_unchanged() {
        let home = std::env::temp_dir().join(format!(
            "agend-nofleet-run-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).ok();
        assert!(
            !crate::fleet::fleet_yaml_path(&home).exists(),
            "precondition: no fleet.yaml"
        );

        let id = crate::types::InstanceId::new();
        let agent_name = format!("nofleet-test-{}", id);
        let handle = crate::agent::mk_test_handle(&agent_name, id);
        handle.core.lock().health.state = crate::health::HealthState::Hung;

        crate::daemon::escalation_persist::persist(
            &home,
            &agent_name,
            &crate::health::PersistedEscalation {
                hung_since_epoch_ms: Some(1000),
                ..Default::default()
            },
        );
        assert!(
            crate::daemon::escalation_persist::load_for(&home, &agent_name)
                .unwrap()
                .hung_since_epoch_ms
                .is_some(),
            "precondition: escalation store has hung_since"
        );

        let mut reg_map = HashMap::new();
        reg_map.insert(id, handle);
        let registry: AgentRegistry = Arc::new(Mutex::new(reg_map));
        let externals: ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let configs = Arc::new(Mutex::new(HashMap::new()));
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };

        HangDetectionHandler::new().run(&ctx);

        assert!(
            crate::daemon::escalation_persist::load_for(&home, &agent_name)
                .unwrap()
                .hung_since_epoch_ms
                .is_some(),
            "missing fleet.yaml → No → Phase 3 must skip; escalation store unchanged"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
