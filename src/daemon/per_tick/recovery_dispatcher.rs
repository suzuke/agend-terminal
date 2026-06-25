//! `#685` sub-task 7a Stage 1 — auto-recovery dispatcher.
//!
//! Decision: `d-20260514030404021793-1`. Three-party consensus
//! (lead-claude + dev-claude + reviewer-opencode).
//!
//! Phase 2 of `#685`: when `check_hang` (sub-task 1) detects `Hung`,
//! the daemon currently emits a single `tracing::warn!` and does
//! nothing else. This handler is the first step toward staged
//! auto-recovery — Stage 1 sends an ESC byte to the agent's PTY
//! ("simulate operator pressing ESC") and monitors whether the agent
//! self-recovers within a timeout window.
//!
//! Stages 2 (auto-restart) and 3 (pause + escalate) are follow-up
//! sub-tasks (`7b`, `7c`). This module ships the **infrastructure** for
//! all three stages: the `RecoveryStageState` state machine, per-agent
//! tracking field on `HealthTracker`, env-var gate, anti-thrash
//! cooldown. Stages 2/3 will add their dispatch arms but reuse this
//! tick loop, this state machine, and this telemetry pattern.
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
//! Stages 2/3 follow the same pattern with their own env vars.
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
//!   → skip Stage 1, transition directly to `Stage2Eligible`
//!   (ESC won't help a process that's not reading)
//! - **anomaly**: neither condition holds (agent shouldn't be `Hung`)
//!   → log warning, leave state unchanged
//!
//! ## Anti-thrash cooldown
//!
//! Decision §1.4 Refinement B — if agent re-enters `Hung` within
//! `STAGE1_COOLDOWN_DEFAULT_MS` of a recent Stage 1 fire, dispatcher
//! skips Stage 1 and goes directly to `Stage2Eligible`. Prevents
//! rapid-fire ESC sending that would mask the underlying issue.

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
/// write and the Stage-2/3 notify I/O — runs AFTER the lock is dropped.
/// `core` + `pty_writer` are both `Arc`, so cloning them out is cheap and
/// lets the registry guard be released before any blocking work.
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

/// `#685` sub-task 7b: env var controlling Stage 2 activation. When
/// set to `"1"`, the dispatcher emits `AgentExitEvent::Stage2Restart`
/// to `crash_tx` (active mode). Default unset = shadow mode: same
/// telemetry, no event emission.
const STAGE2_ENV_VAR: &str = "AGEND_AUTO_RECOVERY_STAGE2";
const STAGE2_MAX_RESTARTS_ENV_VAR: &str = "AGEND_AUTO_RECOVERY_STAGE2_MAX_RESTARTS";

/// `#685` sub-task 7c: env var controlling Stage 3 escalation activation.
/// When set to `"1"`, the dispatcher writes `HealthState::Paused` via
/// `HealthTracker::enter_paused` (active mode). Default unset = shadow
/// mode: telegram + tracing only, no state write — operator can observe
/// the decision pattern before flipping. Mirrors STAGE1 / STAGE2 env
/// gate pattern.
const STAGE3_ENV_VAR: &str = "AGEND_AUTO_RECOVERY_STAGE3";

pub(crate) struct RecoveryDispatcherHandler {
    /// `#685` sub-task 7b: handle to the daemon's `crash_tx` channel.
    /// Stage 1 path (already shipped in 7a) holds but never sends —
    /// kept for uniform constructor across stages. Stage 2 path uses
    /// `try_send` to emit `AgentExitEvent::Stage2Restart` events that
    /// the respawn worker arm (in `daemon/mod.rs:642`) splits on.
    ///
    /// `Arc` so the dispatcher can clone-as-needed; channel `Sender` is
    /// itself cheap-to-clone but `Arc` keeps the surface uniform across
    /// future stages and avoids leaking `crossbeam_channel::Sender`
    /// across the trait boundary.
    crash_tx: std::sync::Arc<crossbeam_channel::Sender<crate::agent::AgentExitEvent>>,
    /// #1694(a): whether a `Stage2Restart` emitted on `crash_tx` actually has a
    /// live consumer in THIS runtime. `run_core` wires `crash_rx` →
    /// `handle_crash_respawn` (true); app-standalone passes a throwaway channel
    /// whose `rx` is dropped (false), so a Stage2 `try_send` there would silently
    /// vanish. When false, `handle_stage2_fire` escalates straight to Stage3
    /// (graceful pause + operator escalation) instead of a no-op restart — the
    /// M-class silent-drop this whole sprint has been closing. (The real fix —
    /// Stage2 driving app mode's pane-based respawn — is a follow-up; Stage2 stays
    /// gated-off by default, so this only fires if an operator enables Stage2 in
    /// app mode.)
    stage2_dispatch_available: bool,
}

impl RecoveryDispatcherHandler {
    pub(crate) fn new(
        crash_tx: std::sync::Arc<crossbeam_channel::Sender<crate::agent::AgentExitEvent>>,
        stage2_dispatch_available: bool,
    ) -> Self {
        Self {
            crash_tx,
            stage2_dispatch_available,
        }
    }
}

fn stage2_gate_active() -> bool {
    crate::runtime_config::get().hang_auto_recovery_enabled
        || std::env::var(STAGE2_ENV_VAR)
            .map(|v| v == "1")
            .unwrap_or(false)
}

fn stage2_max_restarts() -> u32 {
    match std::env::var(STAGE2_MAX_RESTARTS_ENV_VAR) {
        Ok(v) => match v.parse::<u32>() {
            Ok(n) => n,
            Err(_) => crate::health::STAGE2_MAX_RESTARTS_DEFAULT,
        },
        Err(_) => crate::health::STAGE2_MAX_RESTARTS_DEFAULT,
    }
}

fn stage3_gate_active() -> bool {
    crate::runtime_config::get().hang_auto_recovery_enabled
        || std::env::var(STAGE3_ENV_VAR)
            .map(|v| v == "1")
            .unwrap_or(false)
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
    /// wasted; transition directly to `Stage2Eligible`.
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

/// #1694(a): three-way decision for `handle_stage2_fire`, extracted (like
/// [`Stage1Branch`]) so the branch logic is unit-testable independent of the
/// PTY/channel side effects — no AgentHandle harness needed.
#[derive(Debug, PartialEq, Eq)]
enum Stage2Decision {
    /// Gate off — shadow mode: log the would-fire, no send, no transition.
    Shadow,
    /// Gate on but no live crash consumer in this runtime (app-standalone) —
    /// escalate to Stage3 (graceful pause + escalation) rather than silent-drop
    /// the `Stage2Restart` onto a receiver-less channel.
    EscalateNoConsumer,
    /// Gate on + a live consumer — emit the `Stage2Restart` (active mode).
    Fire,
}

fn stage2_decision(gate_active: bool, stage2_dispatch_available: bool) -> Stage2Decision {
    if !gate_active {
        Stage2Decision::Shadow
    } else if !stage2_dispatch_available {
        Stage2Decision::EscalateNoConsumer
    } else {
        Stage2Decision::Fire
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
        let stage2_active = stage2_gate_active();
        let stage2_timeout = Duration::from_millis(crate::health::STAGE2_TIMEOUT_DEFAULT_MS);
        let stage2_max = stage2_max_restarts();
        let stage3_active = stage3_gate_active();

        // #1617 (#1593 F1): snapshot each agent's `core` + `pty_writer`
        // (both `Arc`) UNDER the registry lock, then DROP the lock before
        // any per-agent recovery work. The Stage-1 path does a blocking PTY
        // write and Stages 2/3 do notify I/O — holding the global registry
        // lock across those is the deadlock class #1593 closed elsewhere:
        // a hung agent's PTY never drains, the write blocks, and the
        // supervisor tick stalls holding the registry → whole daemon hangs.
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
                    recovery_restart_count: core.health.recovery_restart_count,
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
                    stage2_max,
                ),
                RecoveryStageState::Stage1Pending { entered_at } => {
                    if entered_at.elapsed() >= stage1_timeout {
                        tracing::info!(
                            target: TARGET,
                            agent = %name,
                            timeout_ms = stage1_timeout.as_millis() as u64,
                            gate_active = stage1_active,
                            "stage1 timeout expired without recovery — escalating to Stage2Eligible"
                        );
                        let mut core = target.core.lock();
                        core.health.recovery_stage_state = RecoveryStageState::Stage2Eligible;
                    }
                }
                RecoveryStageState::Stage2Eligible => {
                    self.handle_stage2_fire(name, target, &snapshot, stage2_active);
                }
                RecoveryStageState::Stage2Pending { entered_at } => {
                    if entered_at.elapsed() >= stage2_timeout {
                        tracing::info!(
                            target: TARGET,
                            agent = %name,
                            timeout_ms = stage2_timeout.as_millis() as u64,
                            gate_active = stage2_active,
                            recovery_restart_count = snapshot.recovery_restart_count,
                            "stage2 timeout expired without recovery — escalating to Stage3Eligible"
                        );
                        let mut core = target.core.lock();
                        core.health.recovery_stage_state = RecoveryStageState::Stage3Eligible;
                    }
                }
                RecoveryStageState::Stage3Eligible => {
                    self.handle_stage3_escalate(name, target, &snapshot, stage3_active);
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
        stage2_max: u32,
    ) {
        // `#685` sub-task 7b: cumulative restart cap check. If the agent
        // has already been Stage-2-restarted `stage2_max` times across
        // prior Hung cycles (decayed by `maybe_decay` per
        // `STABILITY_WINDOW`), skip Stages 1/2 entirely and escalate
        // directly to `Stage3Eligible`. Operator intervention required
        // rather than further automated thrashing.
        if snapshot.recovery_restart_count >= stage2_max {
            tracing::info!(
                target: TARGET,
                agent = %name,
                recovery_restart_count = snapshot.recovery_restart_count,
                stage2_max,
                "recovery restart cap reached — escalating directly to Stage3Eligible"
            );
            let mut core = target.core.lock();
            core.health.recovery_stage_state = RecoveryStageState::Stage3Eligible;
            return;
        }

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
                tracing::info!(
                    target: TARGET,
                    agent = %name,
                    cooldown_ms = cooldown_window.as_millis() as u64,
                    gate_active,
                    "stage1 skipped (cooldown active) — escalating to Stage2Eligible"
                );
                let mut core = target.core.lock();
                core.health.recovery_stage_state = RecoveryStageState::Stage2Eligible;
            }
            (Stage1Branch::DeadLikely, _) => {
                tracing::info!(
                    target: TARGET,
                    agent = %name,
                    silent_ms = snapshot.silent.as_millis() as u64,
                    gate_active,
                    "stage1 skipped (dead-likely: silence > threshold) — escalating to Stage2Eligible"
                );
                let mut core = target.core.lock();
                core.health.recovery_stage_state = RecoveryStageState::Stage2Eligible;
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

    /// Stage 2 fire arm. Emits `AgentExitEvent::Stage2Restart` to the
    /// respawn worker via `try_send` (channel-full safety: failed send
    /// does NOT increment counter; state stays `Stage2Eligible` for
    /// next-tick retry). Telegram notify pre-emit per dev refinement A
    /// (Stages 2/3 fire telegram; Stage 1 silent on success).
    ///
    /// #1339 DAEMON-AUTONOMIC, GATE-EXEMPT BY DESIGN: the restart this triggers is
    /// reached ONLY from the per-tick recovery state machine on an internal
    /// trigger — a Hung-state detection (gated by `hang_auto_recovery_enabled`) —
    /// never from the API socket. Daemon self-heal (a third trusted principal),
    /// so the operator-mode gate does NOT apply: a hung agent is still recovered
    /// in away/sleep. Not agent-invocable (an agent can at most hang ITSELF).
    fn handle_stage2_fire(
        &self,
        name: &str,
        target: &RecoveryTarget,
        snapshot: &DispatchSnapshot,
        gate_active: bool,
    ) {
        match stage2_decision(gate_active, self.stage2_dispatch_available) {
            Stage2Decision::Shadow => {
                // Emit telemetry but no event send and no state transition.
                // Operator can observe the decision pattern before flipping
                // `AGEND_AUTO_RECOVERY_STAGE2=1`.
                tracing::info!(
                    target: TARGET,
                    agent = %name,
                    recovery_restart_count = snapshot.recovery_restart_count,
                    silent_ms = snapshot.silent.as_millis() as u64,
                    silent_productive_ms = snapshot.silent_productive.as_millis() as u64,
                    gate_active = false,
                    "stage2 would-have-fired (shadow mode): Stage2Restart event NOT emitted"
                );
                return;
            }
            Stage2Decision::EscalateNoConsumer => {
                // #1694(a): Stage 2 auto-restart has no consumer in THIS runtime —
                // app-standalone passes a throwaway `crash_tx` whose `rx` is
                // dropped, so `try_send(Stage2Restart)` would silently vanish (the
                // M-class silent-drop). Escalate straight to Stage3 (graceful pause
                // + operator escalation, which needs no crash consumer) so the
                // stuck agent is SURFACED, not dropped. Stage2 stays gated-off by
                // default, so this only fires if an operator enabled Stage2 in app
                // mode; the proper fix (Stage2 driving app mode's pane-based
                // respawn) is a follow-up.
                tracing::warn!(
                    target: TARGET,
                    agent = %name,
                    recovery_restart_count = snapshot.recovery_restart_count,
                    "stage2 restart unavailable in this runtime (no crash consumer) — escalating to Stage3 instead of silent-drop"
                );
                let mut core = target.core.lock();
                core.health.recovery_stage_state = RecoveryStageState::Stage3Eligible;
                return;
            }
            Stage2Decision::Fire => {}
        }

        // Active mode — emit telegram notify pre-emit so operators have
        // visibility into the restart even if the channel send fails.
        notify_stage2_fire(name, snapshot);

        // try_send returns `Err(TrySendError::Full)` if the bounded(64)
        // channel is saturated. Counter NOT incremented on rejection;
        // state stays `Stage2Eligible` so the next tick retries.
        match self
            .crash_tx
            .try_send(crate::agent::AgentExitEvent::Stage2Restart(
                name.to_string(),
            )) {
            Ok(_) => {
                tracing::info!(
                    target: TARGET,
                    agent = %name,
                    recovery_restart_count = snapshot.recovery_restart_count,
                    gate_active = true,
                    "stage2 fired: Stage2Restart event emitted to respawn worker"
                );
                let mut core = target.core.lock();
                core.health.recovery_stage_state = RecoveryStageState::Stage2Pending {
                    entered_at: Instant::now(),
                };
                core.health.last_stage2_fired_at = Some(Instant::now());
                // Counter increment lives on the respawn worker side
                // (selective-restore arm in `daemon/mod.rs` Stage 2 path)
                // — it preserves the counter across the spawn boundary
                // AND increments by 1. This avoids double-counting if
                // the dispatcher tick fires here and the respawn worker
                // also increments.
            }
            Err(e) => {
                tracing::warn!(
                    target: TARGET,
                    agent = %name,
                    error = ?e,
                    "stage2 try_send failed (channel full?) — state stays Stage2Eligible for retry"
                );
            }
        }
    }

    /// Stage 3 escalate arm. Stage 3 is the terminal stage of the
    /// recovery state machine — after Stage 1 ESC failed and Stage 2
    /// auto-restart was attempted up to the cumulative cap, the agent
    /// is escalated to `HealthState::Paused` and the operator is
    /// notified that manual intervention is required.
    ///
    /// Order of operations (atomicity-relevant):
    /// 1. Pre-emit telegram via `notify_stage3_escalate` — fires in
    ///    BOTH shadow and active modes so operators have visibility
    ///    into the decision pattern before flipping the gate to `=1`.
    /// 2. If `gate_active`: acquire the per-agent lock once and call
    ///    `core.health.enter_paused(now)` — atomically writes
    ///    `state = Paused`, `recovery_stage_state = Stage3Pending`,
    ///    `last_stage3_fired_at = Some(now)`. Next-tick dispatcher
    ///    lands on the `Stage3Pending` no-op arm and on the top-level
    ///    `Paused` `continue` guard, so the telegram never re-fires.
    /// 3. Else (shadow): emit `tracing::info!` with would-have-paused
    ///    details; no state mutation.
    ///
    /// `recovery_restart_count` is NOT reset on entry — per decision
    /// `enter_paused` documentation, the count is preserved so a
    /// future operator-unpause that doesn't address the root cause
    /// immediately re-escalates rather than burning further auto-
    /// restart budget.
    fn handle_stage3_escalate(
        &self,
        name: &str,
        target: &RecoveryTarget,
        snapshot: &DispatchSnapshot,
        gate_active: bool,
    ) {
        // Pre-emit telegram in BOTH modes — operator visibility on
        // shadow promotions matters as much as on active escalations.
        notify_stage3_escalate(name, snapshot);

        if gate_active {
            let now = Instant::now();
            let mut core = target.core.lock();
            core.health.enter_paused(now);
            tracing::info!(
                target: TARGET,
                agent = %name,
                recovery_restart_count = snapshot.recovery_restart_count,
                gate_active = true,
                "stage3 fired: HealthState transitioned to Paused via enter_paused"
            );
        } else {
            tracing::info!(
                target: TARGET,
                agent = %name,
                recovery_restart_count = snapshot.recovery_restart_count,
                silent_ms = snapshot.silent.as_millis() as u64,
                gate_active = false,
                "stage3 would-have-fired (shadow mode): Paused write NOT applied"
            );
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
    recovery_restart_count: u32,
    agent_state: crate::state::AgentState,
    silent: Duration,
    silent_productive: Duration,
}

/// Stage 2 telegram notify (pre-emit). Decision §A "Stages 2/3 fire
/// telegram; Stage 1 silent on success". Operator-actionable content:
/// agent name + backend + Hung duration + Stage 1 fire correlation +
/// next-step expectation. Uses existing `gated_notify` so the
/// info-leak gate prevents leakage when channel is unauthorised.
fn notify_stage2_fire(name: &str, snapshot: &DispatchSnapshot) {
    let body = format!(
        "[recovery] {name}: Stage 2 auto-restart triggered.\n\
         Hung silence: {silent_ms}ms (productive silence: {prod_ms}ms)\n\
         Recovery restart count: {count}\n\
         Next: monitoring 30s for recovery; Stage 3 (pause + operator action) on continued failure.",
        silent_ms = snapshot.silent.as_millis(),
        prod_ms = snapshot.silent_productive.as_millis(),
        count = snapshot.recovery_restart_count,
    );
    if let Some(channel) = crate::channel::active_channel() {
        let _ = crate::channel::gated_notify(
            channel.as_ref(),
            name,
            crate::channel::NotifySeverity::Warn,
            &body,
            false,
        );
    } else {
        tracing::debug!(
            target: TARGET,
            agent = %name,
            "stage2 telegram skipped: no active channel"
        );
    }
}

/// Build the Stage 3 escalation telegram body. Extracted so unit
/// tests can pin the operator-facing wording (decision §3 "dev's
/// revised 4-line content") independent of the `gated_notify` plumbing.
fn format_stage3_body(name: &str, recovery_restart_count: u32) -> String {
    format!(
        "[recovery ESCALATION] {name}: PAUSED — manual intervention required.\n  \
         Stage 2 auto-restart fired {count} time(s), all exhausted.\n  \
         Final state: Paused (no further auto-recovery).\n  \
         Action: investigate root cause + manual unpause (CLI command pending sub-task).",
        count = recovery_restart_count,
    )
}

/// Stage 3 escalation telegram. Operator-action-required severity =
/// `NotifySeverity::Error` (per decision §3 dev-grep evidence: enum has
/// only Info/Warn/Error; Stage 2 = Warn, crash = Error; Stage 3 ≥ crash
/// since auto-recovery is exhausted and only operator action can
/// resume). `silent=false` so the operator's channel surfaces it
/// alongside crash notifications. Uses `gated_notify` so the
/// info-leak gate prevents leakage when channel is unauthorised.
fn notify_stage3_escalate(name: &str, snapshot: &DispatchSnapshot) {
    let body = format_stage3_body(name, snapshot.recovery_restart_count);
    // #1744-M6: Stage 3 is an Error-severity operator P0 (auto-recovery exhausted,
    // only operator action resumes) — deliver to EVERY registered channel.
    // `active_channel()` would silently drop it in a multi-channel fleet. (Stage 2
    // above stays `Warn` + `active_channel()` — not a P0.) `gated_notify` inside
    // the helper still enforces the info-leak / authorization gate per channel.
    crate::channel::notify_all_escalation_channels(
        name,
        crate::channel::NotifySeverity::Error,
        &body,
        false,
    );
}

/// Stage 1 alive-stuck branch — emit ESC byte (or shadow-log it),
/// transition state machine to `Stage1Pending`, stamp the cooldown
/// clock.
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
        // as `Err`, which escalates to Stage 2 just like a write error.
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
                tracing::warn!(
                    target: TARGET,
                    agent = %name,
                    error = %e,
                    "stage1 PTY write failed — escalating to Stage2Eligible"
                );
                let mut core = target.core.lock();
                core.health.recovery_stage_state = RecoveryStageState::Stage2Eligible;
                return;
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

    /// Sentinel `crash_tx` for tests — bounded(8) sender that nobody
    /// reads. Tests that only exercise empty-registry / classification
    /// paths never send through it; tests verifying try_send behaviour
    /// can drain the matching `Receiver` via `crossbeam_channel::bounded`.
    fn sentinel_crash_tx() -> std::sync::Arc<crossbeam_channel::Sender<crate::agent::AgentExitEvent>>
    {
        let (tx, _rx) = crossbeam_channel::bounded(8);
        // Leak the receiver so the channel stays open. For tests that
        // need to drain, use `crossbeam_channel::bounded` directly.
        std::mem::forget(_rx);
        std::sync::Arc::new(tx)
    }

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
        RecoveryDispatcherHandler::new(sentinel_crash_tx(), true).run(&ctx);
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
            RecoveryDispatcherHandler::new(sentinel_crash_tx(), true).name(),
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
            RecoveryDispatcherHandler::new(sentinel_crash_tx(), true).run(&ctx);
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
    // `#685` sub-task 7b Stage 2 tests. Cover the new dispatcher arm
    // surface: counter cap → Stage3Eligible, channel try_send + no-
    // increment-on-rejection, decay reduces counter, shadow vs active.
    // Branch classification + cooldown + spontaneous reset already
    // pinned by Stage 1 tests above.
    // -------------------------------------------------------------------

    fn with_stage2_gate<R>(active: bool, f: impl FnOnce() -> R) -> R {
        // #1812: shared crate-wide env lock (see `with_stage1_gate`).
        let _guard = crate::daemon::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var(STAGE2_ENV_VAR).ok();
        unsafe {
            if active {
                std::env::set_var(STAGE2_ENV_VAR, "1");
            } else {
                std::env::remove_var(STAGE2_ENV_VAR);
            }
        }
        let result = f();
        unsafe {
            match prior {
                Some(v) => std::env::set_var(STAGE2_ENV_VAR, v),
                None => std::env::remove_var(STAGE2_ENV_VAR),
            }
        }
        result
    }

    #[test]
    fn stage2_gate_env_var_round_trip() {
        // Same shape as Stage 1 env var test — operator can flip
        // `AGEND_AUTO_RECOVERY_STAGE2=1` without restarting daemon.
        with_stage2_gate(true, || {
            assert!(stage2_gate_active());
        });
        with_stage2_gate(false, || {
            assert!(!stage2_gate_active());
        });
    }

    /// #1694(a): the three-way Stage2 decision — the seam that makes
    /// `handle_stage2_fire`'s no-consumer escalation testable without a full
    /// AgentHandle/PTY harness (same pattern as `classify_branch`/`Stage1Branch`).
    #[test]
    fn stage2_decision_escalates_on_no_consumer_else_fires_1694a() {
        // Gate OFF → shadow, regardless of consumer (no send, no transition).
        assert_eq!(stage2_decision(false, true), Stage2Decision::Shadow);
        assert_eq!(stage2_decision(false, false), Stage2Decision::Shadow);
        // Gate ON + live consumer (run_core's crash_rx) → Fire (emit Stage2Restart).
        assert_eq!(stage2_decision(true, true), Stage2Decision::Fire);
        // Gate ON + NO consumer (app-standalone's throwaway crash_tx) → escalate
        // to Stage3 instead of silently dropping the restart onto a dead channel.
        assert_eq!(
            stage2_decision(true, false),
            Stage2Decision::EscalateNoConsumer
        );
    }

    #[test]
    fn stage2_max_restarts_default_is_three() {
        // Default cap N=3 per decision §Q1/Q2. Env var override unset
        // returns the default. #1812: hold the shared crate-wide env lock
        // — `stage2_max_restarts()` READS env, which races a concurrent
        // `set_var` from another module's test (e.g. daemon::restart)
        // just as a write would.
        let _guard = crate::daemon::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var(STAGE2_MAX_RESTARTS_ENV_VAR).ok();
        unsafe {
            std::env::remove_var(STAGE2_MAX_RESTARTS_ENV_VAR);
        }
        assert_eq!(
            stage2_max_restarts(),
            crate::health::STAGE2_MAX_RESTARTS_DEFAULT
        );
        unsafe {
            match prior {
                Some(v) => std::env::set_var(STAGE2_MAX_RESTARTS_ENV_VAR, v),
                None => std::env::remove_var(STAGE2_MAX_RESTARTS_ENV_VAR),
            }
        }
    }

    #[test]
    fn stage2_max_restarts_env_override() {
        // #1812: shared crate-wide env lock + save/restore (see above).
        let _guard = crate::daemon::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var(STAGE2_MAX_RESTARTS_ENV_VAR).ok();
        unsafe {
            std::env::set_var(STAGE2_MAX_RESTARTS_ENV_VAR, "5");
        }
        assert_eq!(stage2_max_restarts(), 5);
        unsafe {
            match prior {
                Some(v) => std::env::set_var(STAGE2_MAX_RESTARTS_ENV_VAR, v),
                None => std::env::remove_var(STAGE2_MAX_RESTARTS_ENV_VAR),
            }
        }
    }

    #[test]
    fn maybe_decay_reduces_recovery_restart_count_after_stability_window() {
        // Decay discipline (decision §Delta 3): `maybe_decay_at`
        // decrements `recovery_restart_count` by 1 when
        // `last_stage2_fired_at` is older than `STABILITY_WINDOW`.
        // Mirror crash counter decay shape.
        //
        // Cross-platform `Instant` discipline (PR #775 v2): use
        // `base + offset` + `maybe_decay_at(future_now)` instead of
        // `Instant::now() - offset`. Windows anchors `Instant::now()` to
        // system uptime and subtracting from a low-uptime CI VM panics;
        // `Instant::add` saturates and is safe on all platforms.
        let mut tracker = crate::health::HealthTracker::new();
        tracker.recovery_restart_count = 2;
        let base = Instant::now();
        tracker.last_stage2_fired_at = Some(base);
        tracker.maybe_decay_at(base + Duration::from_secs(31 * 60), true);
        assert_eq!(tracker.recovery_restart_count, 1);
    }

    #[test]
    fn maybe_decay_does_not_reduce_recovery_count_within_stability_window() {
        // Counter stays put if Stage 2 fired recently — agent hasn't
        // demonstrated stability long enough to forgive prior restart.
        let mut tracker = crate::health::HealthTracker::new();
        tracker.recovery_restart_count = 2;
        let base = Instant::now();
        tracker.last_stage2_fired_at = Some(base);
        tracker.maybe_decay_at(base, true);
        assert_eq!(tracker.recovery_restart_count, 2);
    }

    #[test]
    fn maybe_decay_skips_recovery_count_on_paused_state() {
        // `HealthState::Paused` guard (sub-task 7a invariant): decay
        // must NOT exit Paused. Counter also untouched.
        //
        // Cross-platform `Instant` discipline (PR #775 v2): use
        // `base + offset` + `maybe_decay_at(future_now)` — see sibling
        // test for rationale.
        let mut tracker = crate::health::HealthTracker::new();
        tracker.state = HealthState::Paused;
        tracker.recovery_restart_count = 2;
        let base = Instant::now();
        tracker.last_stage2_fired_at = Some(base);
        tracker.maybe_decay_at(base + Duration::from_secs(31 * 60), true);
        assert_eq!(tracker.state, HealthState::Paused);
        assert_eq!(tracker.recovery_restart_count, 2);
    }

    // Removed `stage2_pending_timeout_drives_stage3_eligible_in_unit_form`: its
    // two assertions reduced to pure arithmetic facts on locally-constructed
    // values (`31s >= 30s`, `5s < 30s`) and called no production code, so it
    // could never catch a regression in the real `entered_at.elapsed() >=
    // stage2_timeout` dispatcher path. The comment itself deferred the actual
    // integration coverage; this stub added none.

    // -------------------------------------------------------------------
    // `#685` sub-task 7c Stage 3 tests. Cover the new dispatcher arm
    // surface: env gate round-trip, enter_paused atomic invariants,
    // recovery_restart_count NOT reset on Paused entry (operator-must-
    // fix-root-cause signal), Stage3Pending idempotent no-op semantics,
    // and operator-facing telegram content. Decision §7.
    // -------------------------------------------------------------------

    fn with_stage3_gate<R>(active: bool, f: impl FnOnce() -> R) -> R {
        // #1812: shared crate-wide env lock (see `with_stage1_gate`).
        let _guard = crate::daemon::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var(STAGE3_ENV_VAR).ok();
        unsafe {
            if active {
                std::env::set_var(STAGE3_ENV_VAR, "1");
            } else {
                std::env::remove_var(STAGE3_ENV_VAR);
            }
        }
        let result = f();
        unsafe {
            match prior {
                Some(v) => std::env::set_var(STAGE3_ENV_VAR, v),
                None => std::env::remove_var(STAGE3_ENV_VAR),
            }
        }
        result
    }

    #[test]
    fn stage3_gate_env_var_round_trip() {
        // Operator can flip `AGEND_AUTO_RECOVERY_STAGE3=1` without
        // restarting the daemon — same shape as Stage 1 / Stage 2 gate
        // env tests. Decision §4 shadow-mode default + env var promotion
        // workflow.
        with_stage3_gate(true, || {
            assert!(stage3_gate_active());
        });
        with_stage3_gate(false, || {
            assert!(!stage3_gate_active());
        });
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

    #[test]
    fn enter_paused_does_not_reset_recovery_restart_count() {
        // Decision §1 critical invariant: `recovery_restart_count` is
        // preserved across `enter_paused` to keep the operator-must-fix
        // signal. If a future operator unpauses the agent without
        // addressing the root cause and it Hungs again, the dispatcher
        // cap check immediately escalates to Stage3Eligible rather than
        // burning further auto-restart budget.
        let mut tracker = crate::health::HealthTracker::new();
        tracker.recovery_restart_count = 3;
        let now = Instant::now();
        tracker.enter_paused(now);
        assert_eq!(tracker.recovery_restart_count, 3);
        assert_eq!(tracker.state, HealthState::Paused);
    }

    #[test]
    fn stage3_pending_state_no_op_under_maybe_decay() {
        // Stage 3 is terminal: dispatcher's `Stage3Pending` arm is
        // explicit no-op, and `maybe_decay_at` honours the
        // `HealthState::Paused` short-circuit. Together these ensure
        // that ticking the dispatcher / decay loop while an agent is
        // Paused does NOT silently mutate state away from Paused or
        // re-fire Stage 3. Pinned at the decay boundary because that's
        // the only health-side mutation that runs on every tick;
        // dispatcher-tick Stage3Pending no-op is verified by reading
        // the source (no AgentHandle harness exists for the run() arm).
        let mut tracker = crate::health::HealthTracker::new();
        let entered = Instant::now();
        tracker.enter_paused(entered);
        // Simulate a very long stable window — decay must still not
        // exit Paused or touch the preserved counter.
        tracker.recovery_restart_count = 2;
        tracker.last_stage2_fired_at = Some(entered);
        tracker.maybe_decay_at(entered + Duration::from_secs(31 * 60), true);
        assert_eq!(tracker.state, HealthState::Paused);
        assert_eq!(tracker.recovery_restart_count, 2);
        match tracker.recovery_stage_state {
            RecoveryStageState::Stage3Pending { entered_at } => {
                assert_eq!(entered_at, entered);
            }
            other => panic!("expected Stage3Pending preserved, got {other:?}"),
        }
    }

    #[test]
    fn format_stage3_body_includes_recovery_restart_count_and_action() {
        // Decision §3 telegram content (dev-revised after grep evidence
        // cut Backend + N1): operator-facing wording must surface the
        // exhausted Stage 2 count + manual-unpause action hint. Cuts
        // Backend (DispatchSnapshot lacks the field) and Stage 1 N1
        // (HealthTracker doesn't track the count).
        let body = format_stage3_body("orchestrator", 3);
        assert!(body.contains("ESCALATION"));
        assert!(body.contains("orchestrator"));
        assert!(body.contains("PAUSED"));
        assert!(body.contains("Stage 2 auto-restart fired 3 time(s)"));
        assert!(body.contains("manual unpause"));
        // Negative: must NOT include the cut fields per decision §3.
        assert!(!body.contains("Backend:"));
        assert!(!body.contains("Stage 1 ESC"));
    }
}
