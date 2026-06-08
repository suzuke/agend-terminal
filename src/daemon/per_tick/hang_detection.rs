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
        let (hung_now, newly_hung, left_hung): (
            Vec<String>,
            std::collections::HashSet<String>,
            Vec<String>,
        ) = {
            let reg = agent::lock_registry_tracked(ctx.registry, "hang_detection");
            let mut hung = Vec::new();
            let mut newly = std::collections::HashSet::new();
            let mut left = Vec::new();
            for handle in reg.values() {
                let name = handle.name.as_str();
                let mut core = handle.core.lock();
                core.health.maybe_decay();
                let was_hung = core.health.state == crate::health::HealthState::Hung;
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
                    hung.push(name.to_string());
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

        // Phase 2/3 (#1701 Hung half): a self-orchestrator stuck Hung past the
        // confirm-window has no peer to relay — escalate operator P0, INDEPENDENT
        // of `hang_auto_recovery_enabled` (a leaderless team must page even in
        // recovery shadow mode). `self_orch_status` is a teams-file read (done
        // lock-free); the confirm-window + cooldown gate (`hung_escalation_due`)
        // then runs under a brief per-candidate re-lock and stamps, so a
        // persisting Hung pages at most once per cooldown. Self-orch Hung is rare,
        // so the re-lock cost is negligible.
        // #1744-M7: fail-closed — escalate on `Yes` OR `Unknown` (teams config
        // unreadable). A no-peer Hung P0 is high-cost to miss, so an indeterminate
        // read errs toward paging; only a determinate `No` skips.
        for name in hung_now {
            if crate::teams::self_orch_status(ctx.home, &name) == crate::teams::SelfOrchStatus::No {
                continue;
            }
            // Re-lock briefly: run the confirm-window + cooldown gate (which
            // stamps the hung cooldown on fire) and snapshot the escalation state
            // for persistence — both under one lock so the snapshot reflects the
            // gate's stamp.
            let (due, snapshot) = {
                let reg = agent::lock_registry(ctx.registry);
                reg.values()
                    .find(|h| h.name.as_str() == name)
                    .map(|h| {
                        let mut core = h.core.lock();
                        let due = core.health.hung_escalation_due(HUNG_ESCALATE_AFTER);
                        (due, Some(core.health.escalation_snapshot()))
                    })
                    .unwrap_or((false, None))
            };
            if due {
                notify_self_orch_hung(&name);
            }
            // #1744-H2: persist on first Hung detection (anchor `hung_since`) and
            // whenever the escalation fires (stamp the hung cooldown) — both must
            // survive a restart. Between those the snapshot is unchanged, so this
            // does not write every tick a self-orch stays Hung.
            if let Some(snapshot) = snapshot {
                if newly_hung.contains(&name) || due {
                    crate::daemon::escalation_persist::persist(ctx.home, &name, &snapshot);
                }
            }
        }

        // #1744-H2 (codex HIGH): a self-orchestrator that just LEFT Hung had its
        // `hung_since` cleared in memory by `check_hang` — persist that cleared
        // snapshot so a restart does not rehydrate the stale anchor. Only when a
        // store entry already exists (it escalated / was tracked while Hung):
        // skip otherwise so a never-persisted agent that merely flickered through
        // Hung doesn't spawn a store entry. The snapshot's `hung_since` is now
        // None; its crash budget / cooldowns (which DO matter) are preserved.
        for name in left_hung {
            persist_or_clear_left_hung_anchor(ctx, &name);
        }
    }
}

/// #1744-H2 + #1870-H2: persist a left-Hung self-orchestrator's cleared anchor,
/// or clear it in place if the agent has already left the registry. Extracted
/// from the `left_hung` loop so the absent-agent (TOCTOU) branch is testable
/// through a real entry.
///
/// - `self_orch_status == No` → peer-relayable, not our concern → skip.
/// - no persisted store entry → never tracked while Hung → skip (don't spawn one).
/// - agent in registry → persist its escalation snapshot (`hung_since` is now
///   `None`; crash budget / cooldowns preserved). (#1744-H2)
/// - agent ABSENT from the registry (unregistered between the `load_for` check
///   and this read — the #1870-H2 TOCTOU) → no snapshot to persist, so clear
///   `hung_since` in place; pre-fix this skipped and LEFT a stale anchor that a
///   restart rehydrated into a false Hung P0.
fn persist_or_clear_left_hung_anchor(ctx: &TickContext<'_>, name: &str) {
    // #1744-M7: `Yes`|`Unknown` proceed (clearing on an indeterminate read is
    // harmless + correct); only a determinate `No` skips.
    if crate::teams::self_orch_status(ctx.home, name) == crate::teams::SelfOrchStatus::No {
        return;
    }
    if crate::daemon::escalation_persist::load_for(ctx.home, name).is_none() {
        return;
    }
    let snapshot = {
        let reg = agent::lock_registry(ctx.registry);
        reg.values()
            .find(|h| h.name.as_str() == name)
            .map(|h| h.core.lock().health.escalation_snapshot())
    };
    match snapshot {
        Some(snapshot) => {
            crate::daemon::escalation_persist::persist(ctx.home, name, &snapshot);
            tracing::debug!(agent = %name, "#1744-H2: persisted cleared hung anchor on Hung exit");
        }
        None => {
            crate::daemon::escalation_persist::clear_hung_since(ctx.home, name);
            tracing::debug!(
                agent = %name,
                "#1870-H2: cleared hung anchor for agent absent from registry on Hung exit"
            );
        }
    }
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

    /// §3.9 #1870-H2 (real branch): a self-orch with a persisted hung record that
    /// has LEFT the registry (the TOCTOU end-state — unregistered between the
    /// `load_for` check and the registry read) gets its `hung_since` cleared in
    /// place, NOT skipped. Pre-fix this skipped → a restart rehydrated the stale
    /// anchor → false Hung P0. No teams file → `self_orch_status` = Unknown →
    /// proceeds (#1744-M7).
    #[test]
    fn left_hung_anchor_cleared_when_agent_absent_from_registry_1870_h2() {
        let home = std::env::temp_dir().join(format!(
            "agend-h2-absent-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).ok();
        // Make orch-1 a self-orchestrator (orchestrator of its own team) so the
        // #1744-M7 gate proceeds (a non-self-orch is peer-relayable → skipped).
        crate::teams::create(
            &home,
            &serde_json::json!({"name": "t", "members": ["orch-1"], "orchestrator": "orch-1"}),
        );
        // Persisted hung record (the agent was tracked while Hung).
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

        // Empty registry = the agent is absent (it unregistered).
        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let externals: ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let configs = Arc::new(Mutex::new(HashMap::new()));
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };

        persist_or_clear_left_hung_anchor(&ctx, "orch-1");

        assert!(
            crate::daemon::escalation_persist::load_for(&home, "orch-1")
                .unwrap()
                .hung_since_epoch_ms
                .is_none(),
            "#1870-H2: an absent agent's stale hung anchor must be CLEARED, not left for restart rehydrate"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
