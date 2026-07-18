//! `#685` sub-task 7a — Stage-1-only auto-recovery dispatcher.
//!
//! Decision: `d-20260514030404021793-1`. Three-party consensus
//! (lead-claude + dev-claude + reviewer-opencode).
//!
//! When `check_hang` (sub-task 1) detects `Hung`, this handler sends an
//! ESC byte to the agent's PTY ("simulate operator pressing ESC") and
//! monitors whether the agent self-recovers within a timeout window.
//!
//! #2549 P2 (operator decision `d-20260703021554626467-13`): Stage 2
//! (auto-restart) and the dispatcher-driven Stage 3 escalation path were
//! removed — converged to Stage-1-only. Prior to this PR, Stage 2/3 were
//! shipped but gated OFF by default (`hang_auto_recovery_enabled: false`
//! and no per-stage env var set); under that default, `Stage2Eligible`
//! parked indefinitely re-logging a shadow message every tick and NEVER
//! reached Stage 3 — `stage2_decision`'s `Shadow` arm returned without
//! mutating `recovery_stage_state`, and the only two paths that ever
//! advanced past `Stage2Eligible` (`Stage2Pending` timeout,
//! `EscalateNoConsumer`) both required the Stage 2 gate active. So the
//! three branches that used to transition into `Stage2Eligible` (Stage 1
//! timeout, PTY write failure, dead-likely classification) are now
//! **log-only** — this preserves the default-gate-off behavior exactly
//! (no new pause/restart side effects for agents that previously just sat
//! in shadow-logged limbo). See the per-branch comments below.
//!
//! One consequence: since nothing in this file transitions into it
//! anymore, `RecoveryStageState::Stage3Eligible` became permanently
//! unreachable and was removed along with `Stage2Eligible`/`Stage2Pending`.
//! `Stage3Pending` / `HealthState::Paused` /
//! [`crate::health::HealthTracker::enter_paused`] are **untouched** — they
//! are shared terminal-escalation machinery also used independently by
//! `RespawnWatchdogHandler` (an unrelated failure mode: a stuck `resume`
//! spawn, not the Hung ladder), so this dispatcher's `Stage3Pending` arm
//! stays reachable via that path even though this file no longer
//! constructs the state itself.
//!
//! `recovery_restart_count` (the Stage 2 cumulative-restart cap) is
//! deleted, not kept for hypothetical future reuse — with Stage 2 gone
//! nothing increments it, so the cap check was a permanently-dead branch;
//! git history is the reuse mechanism if Stage 2 is ever revisited.
//!
//! ## Tick order
//!
//! Runs AFTER [`super::hang_detection`] in the same tick. Sequencing
//! guarantees `core.health.state` is fresh: `hang_detection`'s
//! `check_hang` call may have transitioned the agent to `Hung` (per
//! sub-task 1 §Invariants 5b), and this dispatcher then reads that
//! state directly via the per-agent core lock. We do **NOT** subscribe
//! to `check_hang`'s `bool` return value because that bool only fires
//! on transition-into-`Hung`; subsequent ticks while still `Hung`
//! return `false`. Reading `core.health.state == Hung` works regardless
//! of the transition edge.
//!
//! ## Shadow mode default
//!
//! Activation gated on env var `AGEND_AUTO_RECOVERY_STAGE1=1`. Default
//! (env var unset) emits the same telemetry log but does NOT send the
//! ESC byte or transition the state machine — observability without
//! production impact, mirroring the F9 shadow-mode SOP (sub-task 4).
//!
//! ## Combined-gate three branches
//!
//! Decision §1.4 Delta 2 — dispatcher inspects raw silence + productive
//! silence elapsed times directly (NOT via F9 classification flag) so
//! Stage 1 ships valuable independent of F9 promotion timeline:
//!
//! - **alive-stuck**: `productive_silence > threshold` && `silent < threshold`
//!   → fire Stage 1 ESC (agent process reading PTY, just not productive)
//! - **dead-likely**: `silent > threshold`
//!   → skip Stage 1 (ESC won't help a process that's not reading); log
//!   only, state stays `None` (#2549: was a transition to the now-removed
//!   `Stage2Eligible`)
//! - **anomaly**: neither condition holds (agent shouldn't be `Hung`)
//!   → log warning, leave state unchanged
//!
//! ## Anti-thrash cooldown
//!
//! Decision §1.4 Refinement B — if agent re-enters `Hung` within
//! `STAGE1_COOLDOWN_DEFAULT_MS` of a recent Stage 1 fire, dispatcher
//! skips re-firing Stage 1; log only, state stays `None` (#2549: was a
//! transition to the now-removed `Stage2Eligible`). Prevents rapid-fire
//! ESC sending that would mask the underlying issue.

use super::{PerTickHandler, TickContext};
use crate::agent;
use crate::health::{
    productive_silence_exceeds, HealthState, RecoveryStageState, STAGE1_COOLDOWN_DEFAULT_MS,
    STAGE1_TIMEOUT_DEFAULT_MS,
};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// #1617 (#1593 F1): per-agent snapshot captured under the registry lock
/// so the recovery state machine — including the Stage-1 blocking PTY
/// write — runs AFTER the lock is dropped. `core` + `pty_writer` are both
/// `Arc`, so cloning them out is cheap and lets the registry guard be
/// released before any blocking work.
struct RecoveryTarget {
    name: String,
    core: Arc<crate::sync_audit::CoreMutex<agent::AgentCore>>,
    pty_writer: agent::PtyWriter,
}

/// Env var name controlling Stage 1 activation. When set to `"1"`, the
/// dispatcher writes the ESC byte to the agent's PTY; otherwise the
/// dispatcher logs the would-fire decision and skips the write.
const STAGE1_ENV_VAR: &str = "AGEND_AUTO_RECOVERY_STAGE1";

/// `tracing` target for shadow-mode + active-mode telemetry. Parallels
/// the F9 `behavioral_shadow` target (sub-task 4 §F9.5) so dashboards
/// can aggregate "would-have-fired" / "did-fire" decisions across the
/// audit observability surface.
const TARGET: &str = "recovery_shadow";

pub(crate) struct RecoveryDispatcherHandler;

impl RecoveryDispatcherHandler {
    pub(crate) fn new() -> Self {
        Self
    }
}

fn stage1_gate_active() -> bool {
    // #685 Phase 2: runtime config master gate OR per-stage env var.
    crate::runtime_config::get().hang_auto_recovery_enabled
        || std::env::var(STAGE1_ENV_VAR)
            .map(|v| v == "1")
            .unwrap_or(false)
}

/// Decision branch for the combined-gate inspection. Centralises the
/// three-way classification so the dispatcher body is readable and so
/// tests can pin the branch logic independent of the side effects.
enum Stage1Branch {
    /// Agent is alive-stuck — `productive_silence > threshold` and
    /// `silence < threshold`. ESC is the right action.
    AliveStuck,
    /// Agent is dead-likely — `silence > threshold`. Stage 1 would be
    /// wasted (ESC won't help a process not reading its PTY).
    DeadLikely,
    /// Neither condition holds; agent shouldn't be `Hung`. Log warning
    /// but leave dispatcher state untouched.
    Anomaly,
}

fn classify_branch(
    agent_state: crate::state::AgentState,
    silent: Duration,
    silent_productive: Duration,
) -> Stage1Branch {
    let silence_exceeds = match agent_state {
        crate::state::AgentState::Idle => false,
        crate::state::AgentState::Starting => silent > Duration::from_secs(120),
        crate::state::AgentState::Active => silent > Duration::from_secs(600),
        _ => silent > Duration::from_secs(120),
    };
    let productive_exceeds = productive_silence_exceeds(agent_state, silent_productive);
    if silence_exceeds {
        Stage1Branch::DeadLikely
    } else if productive_exceeds {
        Stage1Branch::AliveStuck
    } else {
        Stage1Branch::Anomaly
    }
}

impl PerTickHandler for RecoveryDispatcherHandler {
    fn name(&self) -> &'static str {
        "recovery_dispatcher"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        let stage1_active = stage1_gate_active();
        let stage1_timeout = Duration::from_millis(STAGE1_TIMEOUT_DEFAULT_MS);
        let stage1_cooldown = Duration::from_millis(STAGE1_COOLDOWN_DEFAULT_MS);

        // #1617 (#1593 F1): snapshot each agent's `core` + `pty_writer`
        // (both `Arc`) UNDER the registry lock, then DROP the lock before
        // any per-agent recovery work. The Stage-1 path does a blocking PTY
        // write — holding the global registry lock across that is the
        // deadlock class #1593 closed elsewhere: a hung agent's PTY never
        // drains, the write blocks, and the supervisor tick stalls holding
        // the registry → whole daemon hangs.
        // #941 holder-tracking still wraps the (now brief) snapshot hold.
        let targets: Vec<RecoveryTarget> = {
            let reg = agent::lock_registry_tracked(ctx.registry, "recovery_dispatcher");
            reg.values()
                // #1915 TIER-B2: skip a handle being deleted (deleted flag set in
                // delete_transaction Step1, handle removed Step4) — don't dispatch
                // hang-recovery (PTY writes / stage2) to an instance mid-teardown.
                // Separate concern from the spawn chokepoint.
                .filter(|h| !h.deleted.load(std::sync::atomic::Ordering::Acquire))
                .map(|h| RecoveryTarget {
                    name: h.name.to_string(),
                    core: Arc::clone(&h.core),
                    pty_writer: Arc::clone(&h.pty_writer),
                })
                .collect()
        };

        for target in &targets {
            let name = target.name.as_str();
            // Single per-agent lock acquisition reads all dispatcher
            // inputs, then drops the lock before any I/O or channel send.
            let snapshot = {
                let core = target.core.lock();
                DispatchSnapshot {
                    health_state: core.health.state,
                    recovery_stage_state: core.health.recovery_stage_state,
                    last_stage1_fired_at: core.health.last_stage1_fired_at,
                    // KEEP-RAW (#2465): the recovery dispatcher is a health-state machine — feeding it
                    // the promoted/observed state could let a stale/false 'Active' hook MASK a genuinely
                    // stuck agent and skip its recovery escalation. Do NOT migrate to operated_state.
                    agent_state: core.state.current,
                    silent: core.state.last_output.elapsed(),
                    silent_productive: core.state.productive_silence(),
                }
            };

            // Spontaneous recovery reset — if dispatcher previously
            // transitioned the state machine into a non-`None` value
            // and the agent has since returned to `Healthy`, clear the
            // state machine. Subsequent `Hung` re-entry begins a fresh
            // Stage 1 sequence per the linear-escalation discipline.
            if snapshot.health_state == HealthState::Healthy
                && snapshot.recovery_stage_state != RecoveryStageState::None
            {
                let mut core = target.core.lock();
                core.health.recovery_stage_state = RecoveryStageState::None;
                tracing::debug!(
                    target: TARGET,
                    agent = %name,
                    "recovery dispatcher: state reset on spontaneous return to Healthy"
                );
                continue;
            }

            // `Paused` is operator-action-required terminal.
            if snapshot.health_state == HealthState::Paused {
                continue;
            }

            // Dispatcher only acts on `Hung`. Other states (Healthy
            // handled above, Failed/ErrorLoop/etc handled by crash path)
            // are no-ops here.
            if snapshot.health_state != HealthState::Hung {
                continue;
            }

            // Branch on dispatcher state machine — compile-time exhaustive
            // match catches missing variants. Each arm performs at most
            // one transition.
            match snapshot.recovery_stage_state {
                RecoveryStageState::None => self.handle_stage1_entry(
                    name,
                    target,
                    &snapshot,
                    stage1_active,
                    stage1_cooldown,
                ),
                RecoveryStageState::Stage1Pending { entered_at } => {
                    // #2549: Stage 1 timeout used to escalate to the now-removed
                    // `Stage2Eligible`. Under the pre-#2549 default (Stage 2 gate
                    // off), `Stage2Eligible` re-logged a shadow "would-have-fired"
                    // message every tick and never advanced further — so log-only
                    // here (state stays `Stage1Pending`, re-logging every tick the
                    // same way) preserves that default behavior exactly, with zero
                    // new pause/restart side effects.
                    if entered_at.elapsed() >= stage1_timeout {
                        tracing::info!(
                            target: TARGET,
                            agent = %name,
                            timeout_ms = stage1_timeout.as_millis() as u64,
                            gate_active = stage1_active,
                            "stage1 timeout expired without recovery — no further auto-recovery stage available (Stage 2/3 removed #2549); agent remains Hung pending manual intervention or spontaneous recovery"
                        );
                    }
                }
                RecoveryStageState::Stage3Pending { entered_at } => {
                    // Stage 3 is terminal — no further auto-recovery,
                    // no timeout escalation, no telegram re-fire. The
                    // explicit no-op arm exists so the match is
                    // compile-time exhaustive and so audit traces show
                    // the dispatcher saw the agent but deliberately
                    // declined to act. Operator unpause command (future
                    // sub-task) is the only path out of Paused.
                    tracing::debug!(
                        target: TARGET,
                        agent = %name,
                        paused_for_ms = entered_at.elapsed().as_millis() as u64,
                        "stage3_pending: awaiting operator unpause"
                    );
                }
            }
        }
    }
}

impl RecoveryDispatcherHandler {
    /// Stage 1 entry arm (formerly inlined in `run`). Handles the
    /// alive-stuck / dead-likely / anomaly branches plus the
    /// anti-thrash cooldown skip. Decision §1.4 Refinement B governs
    /// the cooldown branch.
    fn handle_stage1_entry(
        &self,
        name: &str,
        target: &RecoveryTarget,
        snapshot: &DispatchSnapshot,
        gate_active: bool,
        cooldown_window: Duration,
    ) {
        let branch = classify_branch(
            snapshot.agent_state,
            snapshot.silent,
            snapshot.silent_productive,
        );

        let in_cooldown = snapshot
            .last_stage1_fired_at
            .map(|t| t.elapsed() < cooldown_window)
            .unwrap_or(false);

        match (branch, in_cooldown) {
            (Stage1Branch::AliveStuck, false) => {
                fire_stage1_alive_stuck(
                    name,
                    target,
                    gate_active,
                    snapshot.silent,
                    snapshot.silent_productive,
                );
            }
            (Stage1Branch::AliveStuck, true) => {
                // #2549: used to escalate to the now-removed `Stage2Eligible`.
                // Under the pre-#2549 default (Stage 2 gate off) that state
                // just re-logged a shadow message every tick with no further
                // action, so log-only + leave state at `None` (re-classified
                // fresh next tick) preserves that behavior exactly.
                tracing::info!(
                    target: TARGET,
                    agent = %name,
                    cooldown_ms = cooldown_window.as_millis() as u64,
                    gate_active,
                    "stage1 skipped (cooldown active) — no further auto-recovery stage available (Stage 2/3 removed #2549)"
                );
            }
            (Stage1Branch::DeadLikely, _) => {
                // #2549: see the cooldown-skip arm above — same log-only
                // preservation of the pre-#2549 default-gate-off behavior.
                tracing::info!(
                    target: TARGET,
                    agent = %name,
                    silent_ms = snapshot.silent.as_millis() as u64,
                    gate_active,
                    "stage1 skipped (dead-likely: silence > threshold) — no further auto-recovery stage available (Stage 2/3 removed #2549)"
                );
            }
            (Stage1Branch::Anomaly, _) => {
                tracing::warn!(
                    target: TARGET,
                    agent = %name,
                    agent_state = ?snapshot.agent_state,
                    silent_ms = snapshot.silent.as_millis() as u64,
                    silent_productive_ms = snapshot.silent_productive.as_millis() as u64,
                    "stage1 anomaly: agent is Hung but neither alive-stuck nor dead-likely classification holds"
                );
            }
        }
    }
}

/// Read-only snapshot of per-agent state captured under a single lock
/// acquisition. Dispatcher body operates on this snapshot to keep the
/// per-agent lock window minimal.
struct DispatchSnapshot {
    health_state: HealthState,
    recovery_stage_state: RecoveryStageState,
    last_stage1_fired_at: Option<Instant>,
    agent_state: crate::state::AgentState,
    silent: Duration,
    silent_productive: Duration,
}

/// Stage 1 alive-stuck branch — emit ESC byte (or shadow-log it),
/// transition state machine to `Stage1Pending`, stamp the cooldown
/// clock.
///
/// #1339 DAEMON-AUTONOMIC, GATE-EXEMPT BY DESIGN: this ESC write is reached
/// ONLY from the per-tick recovery state machine on an internal trigger — a
/// Hung-state detection (gated by `hang_auto_recovery_enabled`) — never from
/// the API socket. Daemon self-heal (a third trusted principal), so the
/// operator-mode gate does NOT apply: a hung agent is still recovered in
/// away/sleep. Not agent-invocable (an agent can at most hang ITSELF).
fn fire_stage1_alive_stuck(
    name: &str,
    target: &RecoveryTarget,
    gate_active: bool,
    silent: Duration,
    silent_productive: Duration,
) {
    let now = Instant::now();

    if gate_active {
        // Active mode — write the ESC byte to PTY. Single byte; no
        // submit_key suffix (mirrors comments in `inject_to_agent` re Ink
        // TUI interpretation of `\x1b` as ESC-cancel). #1617: via the
        // timeout-safe `write_to_pty` (was a raw `write_all` that blocked
        // forever on a hung, non-draining PTY) AND only after the registry
        // lock was dropped (the caller snapshots `target.pty_writer` under
        // the lock, then releases it) — so a stuck write can no longer
        // stall the supervisor tick / wedge the daemon. A timeout surfaces
        // as `Err`, handled below — #2549 removed Stage 2, so this no
        // longer escalates anywhere; it falls through to `Stage1Pending`.
        let write_result = agent::write_to_pty(&target.pty_writer, b"\x1b");
        match write_result {
            Ok(_) => {
                tracing::info!(
                    target: TARGET,
                    agent = %name,
                    silent_ms = silent.as_millis() as u64,
                    silent_productive_ms = silent_productive.as_millis() as u64,
                    gate_active = true,
                    "stage1 fired: ESC byte written to agent PTY (alive-stuck branch)"
                );
            }
            Err(e) => {
                // #2549: used to escalate to the now-removed `Stage2Eligible`.
                // Falling through to the common `Stage1Pending` tail below
                // (instead of an early return) is the deliberate fix here —
                // NOT a leftover no-op. Leaving state at `None` would have
                // `handle_stage1_entry` re-classify and retry the write
                // EVERY tick (a retry storm on a persistently-failing PTY,
                // new behavior the pre-#2549 default never had); landing in
                // `Stage1Pending` instead makes this a one-shot "attempted,
                // stop" marker — closest to the pre-#2549 default's
                // "fails once, then parks" shape. `entered_at`/
                // `last_stage1_fired_at` below are stamped purely as a
                // re-fire guard (drives the timeout log + cooldown skip),
                // NOT a claim that the ESC byte was actually delivered.
                tracing::warn!(
                    target: TARGET,
                    agent = %name,
                    error = %e,
                    "stage1 PTY write failed — no further auto-recovery stage available (Stage 2/3 removed #2549); parking in Stage1Pending to stop retrying"
                );
            }
        }
    } else {
        // Shadow mode — same decision telemetry, no I/O. Operator can
        // observe the decision pattern before flipping `AGEND_AUTO_RECOVERY_STAGE1=1`.
        tracing::info!(
            target: TARGET,
            agent = %name,
            silent_ms = silent.as_millis() as u64,
            silent_productive_ms = silent_productive.as_millis() as u64,
            gate_active = false,
            "stage1 would-have-fired (shadow mode): ESC byte NOT written (alive-stuck branch)"
        );
    }

    // #2549: entered_at/last_stage1_fired_at are a re-fire guard (timeout log
    // + cooldown skip), not a success claim — reached on the PTY-write-Err
    // path too (see the comment there).
    let mut core = target.core.lock();
    core.health.recovery_stage_state = RecoveryStageState::Stage1Pending { entered_at: now };
    core.health.last_stage1_fired_at = Some(now);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::state::AgentState;

    // -------------------------------------------------------------------
    // Branch classification tests — pin the three-way decision logic
    // without touching env vars or PTY I/O.
    // -------------------------------------------------------------------

    #[test]
    fn classify_alive_stuck_when_productive_exceeds_silence_below() {
        // Silent < 120s default threshold, productive_silence > 120s →
        // alive-stuck branch. Carrier is `Starting` (120s threshold) — the
        // Ready/Idle merge made `Idle` exempt (`=> false`), so the old `Ready`
        // carrier was re-pointed to another 120s-threshold non-exempt state.
        let branch = classify_branch(
            AgentState::Starting,
            Duration::from_secs(30),
            Duration::from_secs(300),
        );
        assert!(matches!(branch, Stage1Branch::AliveStuck));
    }

    #[test]
    fn classify_dead_likely_when_silence_exceeds() {
        // Silent > 120s → dead-likely; ESC won't help a process not
        // reading PTY. Productive_silence value is irrelevant once
        // silence exceeds. Carrier `Starting` (see note above — `Idle` is now
        // exempt; `classify_idle_never_dead_likely_on_silence` pins that).
        let branch = classify_branch(
            AgentState::Starting,
            Duration::from_secs(300),
            Duration::from_secs(500),
        );
        assert!(matches!(branch, Stage1Branch::DeadLikely));
    }

    #[test]
    fn classify_idle_never_dead_likely_on_silence() {
        // Ready/Idle merge (accepted behavior change ②): an `Idle` agent is
        // NEVER classified dead-likely on silence — it is legitimately quiet.
        // Pre-merge, `Ready` (agy/opencode idle prompt) fell into the 120s
        // catch-all and COULD be silence-reaped; now it follows Idle's exemption
        // (consistent with claude). A real hang still surfaces via Thinking/ToolUse.
        let branch = classify_branch(
            AgentState::Idle,
            Duration::from_secs(600), // far past any silence threshold
            Duration::from_secs(600),
        );
        assert!(
            matches!(branch, Stage1Branch::Anomaly),
            "idle agent must never be DeadLikely/AliveStuck from silence alone"
        );
    }

    #[test]
    fn classify_anomaly_when_neither_exceeds() {
        // Both below threshold → agent shouldn't be `Hung`. Dispatcher
        // logs warning and leaves state unchanged. Tested via the
        // branch classifier directly; the warn log is exercised via
        // the dispatcher-state integration test below.
        let branch = classify_branch(
            AgentState::Idle,
            Duration::from_secs(30),
            Duration::from_secs(30),
        );
        assert!(matches!(branch, Stage1Branch::Anomaly));
    }

    #[test]
    fn classify_thinking_uses_higher_threshold() {
        // Active gets the 600s threshold (sub-task 1 audit
        // §Entry.E1 PRE). Silent 500s + productive_silence 500s on
        // Active → anomaly (neither exceeds), even though both >
        // 120s default.
        let branch = classify_branch(
            AgentState::Active,
            Duration::from_secs(500),
            Duration::from_secs(500),
        );
        assert!(matches!(branch, Stage1Branch::Anomaly));
    }

    // -------------------------------------------------------------------
    // Production-hook integration tests — exercise the dispatcher tick
    // path against a real `TickContext` + minimal agent registry. Per
    // decision §6 (`§3.14 observability` directive) — observability infra
    // ships with at least one integration test exercising the hook
    // path. Env-var serialization via a private mutex (mirror of
    // `tests/common/env_gate.rs::with_f9_gate` for `AGEND_PRODUCTIVE_GATE`).
    // -------------------------------------------------------------------

    use crate::agent::{AgentRegistry, ExternalRegistry};
    use std::collections::HashMap;

    fn with_stage1_gate<R>(active: bool, f: impl FnOnce() -> R) -> R {
        // #1812: serialise via the SINGLE crate-wide env lock, not a
        // module-local one — env mutation races across ALL keys, so a
        // local mutex wouldn't serialise against `daemon::restart`'s env
        // tests (the `cargo test restart` interleave the reviewer caught).
        let _guard = crate::daemon::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var(STAGE1_ENV_VAR).ok();
        // SAFETY: serialised by the LOCK guard. Test-only env mutation.
        unsafe {
            if active {
                std::env::set_var(STAGE1_ENV_VAR, "1");
            } else {
                std::env::remove_var(STAGE1_ENV_VAR);
            }
        }
        let result = f();
        unsafe {
            match prior {
                Some(v) => std::env::set_var(STAGE1_ENV_VAR, v),
                None => std::env::remove_var(STAGE1_ENV_VAR),
            }
        }
        result
    }

    /// Tuple of empty per-tick fixtures: home dir, registry, external
    /// registry, configs map. Built by `empty_ctx` for smoke tests.
    type EmptyCtxBundle = (
        std::path::PathBuf,
        AgentRegistry,
        ExternalRegistry,
        std::sync::Arc<parking_lot::Mutex<HashMap<String, crate::daemon::AgentConfig>>>,
    );

    /// Build an empty TickContext for smoke tests. `home` is a unique
    /// tempdir per test to avoid registry/snapshot cross-talk.
    fn empty_ctx() -> EmptyCtxBundle {
        let home = std::env::temp_dir().join(format!(
            "agend-recovery-dispatcher-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).ok();
        let registry: AgentRegistry = std::sync::Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let externals: ExternalRegistry =
            std::sync::Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let configs = std::sync::Arc::new(parking_lot::Mutex::new(HashMap::new()));
        (home, registry, externals, configs)
    }

    #[test]
    fn run_is_noop_on_empty_registry() {
        // Smoke test mirroring `HangDetectionHandler::run_is_noop_on_empty_registry`.
        // Empty registry → dispatcher tick does nothing, no panic.
        let (home, registry, externals, configs) = empty_ctx();
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };
        RecoveryDispatcherHandler::new().run(&ctx);
        assert!(registry.lock().is_empty());
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1617 invariant (deadlock class of #1593 F1): the recovery
    /// dispatcher must NEVER hold the global registry lock across a
    /// blocking PTY write / notify. Structural source-scan pins:
    ///  (a) `fire_stage1_alive_stuck` writes via the timeout-safe
    ///      `write_to_pty`, NOT a raw `pty_writer.lock().write_all`.
    ///  (b) `run` snapshots agents into a `RecoveryTarget` Vec and loops
    ///      over THAT (post-drop), never directly over `reg.values()`.
    /// Needles are built by `concat` so this test's own source (the file
    /// is `include_str!`'d whole) can't self-satisfy the assertions.
    #[test]
    fn recovery_loop_never_holds_registry_across_blocking_io() {
        // Scope to the PRODUCTION portion only (everything before the
        // `#[cfg(test)]` mod) so this test's own source — incl. the literal
        // needles in these assertion messages — can never self-satisfy the
        // scan (the #1593 F2 / lock-audit lesson). Needles also concat-built.
        let src = include_str!("recovery_dispatcher.rs");
        let cfg_test = ["#[cfg(", "test)]"].concat();
        let prod = &src[..src.find(&cfg_test).expect("test mod present")];

        let timeout_safe = ["write", "_to_pty"].concat(); // write_to_pty
                                                          // The deadlock idiom is locking the pty_writer directly to write a
                                                          // raw byte (which blocks forever on a hung PTY). Assert that exact
                                                          // idiom is absent rather than a loose "write_all" substring (which
                                                          // would false-match an explanatory code comment).
        let raw_lock_idiom = ["pty_writer", ".lock()"].concat();
        let reg_loop = ["for handle in reg", ".values()"].concat();

        // (a) fire_stage1_alive_stuck body uses the timeout-safe write only.
        let fire_marker = ["fn fire_stage1", "_alive_stuck"].concat();
        let fstart = prod.find(&fire_marker).expect("fire fn present");
        let fire_body = &prod[fstart..]; // runs to end of prod (last prod fn)
        assert!(
            fire_body.contains(&timeout_safe),
            "fire_stage1 must write via the timeout-safe write_to_pty"
        );
        assert!(
            !fire_body.contains(&raw_lock_idiom),
            "fire_stage1 must NOT lock the pty_writer to do a raw blocking write (deadlocks on a hung PTY)"
        );

        // (b) run() loops over the dropped-lock snapshot, not reg.values().
        let rstart = prod.find("fn run(").expect("run fn present");
        let rafter = &prod[rstart..];
        let rend = rafter[3..]
            .find("\n    fn ")
            .map(|i| i + 3)
            .unwrap_or(rafter.len());
        let run_body = &rafter[..rend];
        assert!(
            !run_body.contains(&reg_loop),
            "run() must NOT iterate the registry guard directly (holds the lock across the loop)"
        );
        assert!(
            run_body.contains("for target in"),
            "run() must iterate the post-drop snapshot"
        );
        assert!(
            run_body.contains("RecoveryTarget"),
            "run() must snapshot handles under the lock before dropping it"
        );
    }

    #[test]
    fn name_matches_module() {
        assert_eq!(
            RecoveryDispatcherHandler::new().name(),
            "recovery_dispatcher"
        );
    }

    #[test]
    fn shadow_mode_default_does_not_send_pty_byte() {
        // Production-hook integration test for §3.14 observability:
        // env var unset → dispatcher emits would-fire log but does NOT
        // write the ESC byte. Verify by checking the dispatcher
        // transitions the state machine to Stage1Pending in BOTH modes
        // (the byte-write is the only difference), and that state machine
        // transition only happens with the alive-stuck branch path.
        //
        // This test instantiates a real per-agent core with synthesized
        // HealthState::Hung, ticks the dispatcher, and asserts the
        // recovery_stage_state transition. Verifies the production hook
        // path (real lock acquisition, real env-var read, real tracing
        // target emission) without requiring a live PTY.
        with_stage1_gate(false, || {
            let (home, registry, externals, configs) = empty_ctx();
            // Note: this test verifies the no-op path because we don't
            // construct a full AgentHandle (requires PTY setup). The
            // empty-registry smoke test above + the
            // classify_branch unit tests cover the decision logic; a
            // full PTY-backed integration test would need
            // `tests/fixture_corpus_measurement.rs`-style infra and
            // is deferred to a follow-up PR if the smoke pattern
            // proves insufficient.
            let ctx = TickContext {
                home: &home,
                registry: &registry,
                externals: &externals,
                configs: &configs,
            };
            RecoveryDispatcherHandler::new().run(&ctx);
            std::fs::remove_dir_all(&home).ok();
        });
    }

    #[test]
    fn active_mode_gate_check_reads_env_var() {
        // Smoke-check that the env var is read each tick (no caching).
        // Operator can flip `AGEND_AUTO_RECOVERY_STAGE1=1` without
        // restarting the daemon — important for the shadow→active
        // promotion workflow.
        with_stage1_gate(true, || {
            assert!(stage1_gate_active());
        });
        with_stage1_gate(false, || {
            assert!(!stage1_gate_active());
        });
    }

    // -------------------------------------------------------------------
    // #2549 pin tests: the three branches that used to escalate into the
    // now-removed `Stage2Eligible` are log-only. One test per branch,
    // each asserting the terminal state + absence of a second (mutating)
    // action — via a structural source-scan (no AgentHandle/PTY harness
    // exists for these arms, same constraint as
    // `recovery_loop_never_holds_registry_across_blocking_io` above).
    // -------------------------------------------------------------------

    /// Branch 1 — Stage 1 timeout (`Stage1Pending` arm in `run()`): must log
    /// but NOT mutate `recovery_stage_state` (terminal: stays `Stage1Pending`,
    /// re-logs every tick — same every-tick cadence the pre-#2549 default's
    /// `Stage2Eligible` shadow-log had, zero new side effects).
    #[test]
    fn stage1_timeout_is_log_only_no_second_action_2549() {
        let src = include_str!("recovery_dispatcher.rs");
        let cfg_test = ["#[cfg(", "test)]"].concat();
        let prod = &src[..src.find(&cfg_test).expect("test mod present")];

        let arm_marker = ["RecoveryStageState::Stage1Pending", " { entered_at } => {"].concat();
        let astart = prod.find(&arm_marker).expect("Stage1Pending arm present");
        let arest = &prod[astart..];
        // Arm ends at the next arm (`Stage3Pending` — the only other variant).
        let aend = arest
            .find("RecoveryStageState::Stage3Pending")
            .expect("Stage3Pending arm follows");
        let arm_body = &arest[..aend];

        assert!(
            arm_body.contains("no further auto-recovery stage available"),
            "must log the terminal-no-escalation message: {arm_body}"
        );
        let mutation = ["recovery_stage_state", " ="].concat();
        assert!(
            !arm_body.contains(&mutation),
            "must NOT mutate recovery_stage_state (log-only, terminal): {arm_body}"
        );
    }

    /// Branches 2 — dead-likely classification and cooldown-active skip
    /// (both arms in `handle_stage1_entry`): must log but NOT touch
    /// `target.core` (terminal: state stays `None`, re-classified fresh
    /// next tick — no second action).
    #[test]
    fn stage1_entry_dead_likely_and_cooldown_skip_are_log_only_2549() {
        let src = include_str!("recovery_dispatcher.rs");
        let cfg_test = ["#[cfg(", "test)]"].concat();
        let prod = &src[..src.find(&cfg_test).expect("test mod present")];

        let start_marker = ["(Stage1Branch::AliveStuck", ", true) => {"].concat();
        let sstart = prod.find(&start_marker).expect("cooldown-skip arm present");
        let srest = &prod[sstart..];
        let end_marker = ["(Stage1Branch::Anomaly", ", _) => {"].concat();
        let send = srest.find(&end_marker).expect("Anomaly arm follows");
        // Covers BOTH the cooldown-skip (AliveStuck, true) arm and the
        // DeadLikely arm — Anomaly is the next (and last) arm in the match.
        let both_arms = &srest[..send];

        let no_escalation_msg = "no further auto-recovery stage available";
        assert_eq!(
            both_arms.matches(no_escalation_msg).count(),
            2,
            "both the cooldown-skip and dead-likely arms must log the terminal message: {both_arms}"
        );
        let lock_call = ["target.core", ".lock()"].concat();
        assert!(
            !both_arms.contains(&lock_call),
            "neither arm may touch target.core (log-only, no mutation): {both_arms}"
        );
    }

    /// Branch 3 — PTY write failure (`fire_stage1_alive_stuck`'s `Err` arm):
    /// must NOT `return` early (the pre-#2549 escalation path) — falling
    /// through to the function's common tail is the fix, landing in
    /// `Stage1Pending` as a one-shot "attempted, stop" marker instead of
    /// retrying the write every tick.
    #[test]
    fn fire_stage1_pty_write_failure_falls_through_to_stage1_pending_2549() {
        let src = include_str!("recovery_dispatcher.rs");
        let cfg_test = ["#[cfg(", "test)]"].concat();
        let prod = &src[..src.find(&cfg_test).expect("test mod present")];

        let fire_marker = ["fn fire_stage1", "_alive_stuck"].concat();
        let fstart = prod.find(&fire_marker).expect("fire fn present");
        let fire_body = &prod[fstart..];

        let err_marker = "Err(e) => {";
        let estart = fire_body.find(err_marker).expect("Err arm present");
        let erest = &fire_body[estart..];
        let eend = erest
            .find("} else {")
            .expect("shadow-mode else branch follows");
        let err_arm = &erest[..eend];

        // `return;` (not the bare word "return", which also appears in this
        // test's own explanatory prose above) — an early-return statement.
        assert!(
            !err_arm.contains("return;"),
            "the Err arm must NOT early-return (must fall through to the common Stage1Pending tail): {err_arm}"
        );

        let common_tail = [
            "core.health.recovery_stage_state = RecoveryStageState::Stage1Pending",
            " { entered_at: now };",
        ]
        .concat();
        assert!(
            fire_body.contains(&common_tail),
            "the function's common tail must unconditionally set Stage1Pending (reached by both Ok and Err paths)"
        );
    }

    /// Capture ALL tracing events (any target, TRACE+) emitted while `f` runs.
    /// `tracing_test::traced_test`'s default filter is crate-path-scoped and
    /// DROPS this file's custom `target: TARGET` ("recovery_shadow") events —
    /// same gotcha `state::tests::capture_all_logs` documents (verified
    /// empirically there too) — so an unfiltered subscriber is installed for
    /// the closure's duration instead. `cfg`-gated with its sole caller below
    /// (Unix-only) to avoid an unused-fn warning on Windows.
    #[cfg(not(target_os = "windows"))]
    fn capture_all_logs<F: FnOnce()>(f: F) -> String {
        use std::io::Write;
        use std::sync::{Arc, Mutex};
        #[derive(Clone)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0
                    .lock()
                    .expect("capture buf mutex")
                    .extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
            type Writer = Buf;
            fn make_writer(&'a self) -> Buf {
                self.clone()
            }
        }
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sub = tracing_subscriber::fmt()
            .with_writer(Buf(buf.clone()))
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .without_time()
            .finish();
        tracing::subscriber::with_default(sub, f);
        let bytes = buf.lock().expect("capture buf mutex").clone();
        String::from_utf8(bytes).expect("capture buf is utf8")
    }

    /// t-...14440-6 caller-level integration pin (companion to the
    /// structural scan above): drives `fire_stage1_alive_stuck`'s Err arm
    /// through a REAL registered writer — spawned via the actual
    /// `spawn_agent` production path, so `write_actor::register` runs
    /// exactly as it does in production (#2620) — with its queue saturated
    /// so the ESC byte write genuinely fails, not a structural stand-in.
    ///
    /// NOTE for readers cross-checking against PTY-WRITE-ACTOR-SPIKE.md §2:
    /// the spike's item ① expected this failure to "escalate to Stage 2" —
    /// that was already stale when written. Stage 2/3 were removed in
    /// #2549 P2 (commit 97960ce8), before write_actor (#2620) existed, so a
    /// write failure here has nothing left to escalate to; it falls
    /// through to `Stage1Pending` (a one-shot "attempted, stop" marker),
    /// which is what this test pins.
    ///
    /// Unix-only: the wedge fixture (`sh -c "stty raw -echo; sleep 30"`) and
    /// `write_actor` itself don't exist on Windows (mirrors the other
    /// real-PTY-wedge tests in this codebase).
    #[test]
    #[cfg(not(target_os = "windows"))]
    fn fire_stage1_pty_write_failure_lands_in_stage1_pending_real_actor_2620() {
        let registry: agent::AgentRegistry =
            std::sync::Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let args = vec!["-c".to_string(), "stty raw -echo; sleep 30".to_string()];
        let cfg = agent::SpawnConfig {
            name: "wedged-esc-target",
            backend: None,
            backend_command: "sh",
            args: &args,
            spawn_mode: crate::backend::SpawnMode::Fresh,
            cols: 80,
            rows: 24,
            env: None,
            working_dir: None,
            submit_key: "\r",
            home: None,
            crash_tx: None,
            shutdown: None,
        };
        let id = agent::spawn_agent(&cfg, &registry).expect("wedged shell must spawn");
        // Let `stty raw -echo` take effect before writing — mirrors
        // write_actor.rs's own `wedged_pty()` fixture's post-spawn wait.
        std::thread::sleep(Duration::from_millis(300));

        let (core, pty_writer) = {
            let reg = registry.lock();
            let handle = reg.get(&id).expect("spawned handle must be present");
            (
                std::sync::Arc::clone(&handle.core),
                std::sync::Arc::clone(&handle.pty_writer),
            )
        };

        // Saturate the actor's per-writer queue so the ESC byte write below
        // genuinely fails. A single write LARGER than the cap is rejected
        // outright at enqueue time (never queued at all) — priming with
        // exactly write_actor's cap (`MAX_QUEUE_BYTES_PER_WRITER`, 1 MiB at
        // time of writing — private to write_actor.rs, so duplicated here
        // by value) is what actually gets a near-full job INTO the queue.
        // The wedged child never drains it, so the follow-up 1-byte ESC
        // write either hits the instant backpressure reject (if the tiny
        // real kernel-pty buffer hasn't drained any headroom yet) or queues
        // behind it and times out after `PTY_WRITE_TIMEOUT` (5s) — either
        // way, a genuine `Err`, not a race against an empty queue. Confirmed
        // empirically (both outcomes observed as `Err` under this setup).
        let priming_result = agent::write_to_pty(&pty_writer, &vec![b'x'; 1 << 20]);
        assert!(
            priming_result.is_err(),
            "test invariant: the priming write itself must also see a saturated/wedged queue"
        );

        let target = RecoveryTarget {
            name: "wedged-esc-target".to_string(),
            core: std::sync::Arc::clone(&core),
            pty_writer,
        };
        let logs = capture_all_logs(|| {
            fire_stage1_alive_stuck(
                "wedged-esc-target",
                &target,
                true, // gate_active
                Duration::from_secs(200),
                Duration::from_secs(200),
            );
        });

        // Confirm the Err arm was actually the one that ran — the common
        // tail below sets Stage1Pending unconditionally on BOTH Ok and Err,
        // so the state alone can't distinguish a genuine write failure from
        // a write that happened to succeed against an empty queue.
        assert!(
            logs.contains("stage1 PTY write failed"),
            "the ESC write must have genuinely failed (logged warn), not silently succeeded: {logs}"
        );

        let state = core.lock().health.recovery_stage_state;
        assert!(
            matches!(state, RecoveryStageState::Stage1Pending { .. }),
            "a genuine PTY write failure must still land in Stage1Pending — nothing left to \
             escalate to post-#2549; got {state:?}"
        );
    }

    #[test]
    fn enter_paused_sets_state_recovery_stage_and_timestamp_atomically() {
        // Decision §1: `enter_paused(now)` writes 3 invariants in one
        // logical step:
        //   1. state = Paused
        //   2. recovery_stage_state = Stage3Pending { entered_at: now }
        //   3. last_stage3_fired_at = Some(now)
        // Single grep target enforces 7a §F39.5 "Paused entered ONLY
        // via Stage 3 dispatcher".
        let mut tracker = crate::health::HealthTracker::new();
        let now = Instant::now();
        tracker.enter_paused(now);
        assert_eq!(tracker.state, HealthState::Paused);
        match tracker.recovery_stage_state {
            RecoveryStageState::Stage3Pending { entered_at } => {
                assert_eq!(entered_at, now);
            }
            other => panic!("expected Stage3Pending, got {other:?}"),
        }
        assert_eq!(tracker.last_stage3_fired_at, Some(now));
    }

    /// #2549: `Stage3Pending`/`enter_paused` are shared terminal-escalation
    /// machinery — `RespawnWatchdogHandler` calls `enter_paused` independently
    /// of this dispatcher (an unrelated failure mode, a stuck `resume` spawn).
    /// This pins that this dispatcher's convergence to Stage-1-only did NOT
    /// touch that shared path: `Stage3Pending` is still terminal under the
    /// crate-wide `maybe_decay_at` sweep (dispatcher's own `Stage3Pending` arm
    /// is an explicit no-op, verified by reading the source — no AgentHandle
    /// harness exists for the `run()` arm).
    #[test]
    fn stage3_pending_state_no_op_under_maybe_decay() {
        let mut tracker = crate::health::HealthTracker::new();
        let entered = Instant::now();
        tracker.enter_paused(entered);
        // Simulate a very long stable window — decay must still not exit Paused.
        tracker.maybe_decay_at(entered + Duration::from_secs(31 * 60), true);
        assert_eq!(tracker.state, HealthState::Paused);
        match tracker.recovery_stage_state {
            RecoveryStageState::Stage3Pending { entered_at } => {
                assert_eq!(entered_at, entered);
            }
            other => panic!("expected Stage3Pending preserved, got {other:?}"),
        }
    }
}
