//! Health monitoring: auto-respawn, backoff, hang detection, error loop.
//!
//! Two-layer state:
//! - AgentState: instant PTY output detection (Thinking, Idle, RateLimit...)
//! - HealthState: cumulative lifecycle (Healthy, Recovering, Unstable, Failed...)

use crate::state::AgentState;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

const CRASH_WINDOW: Duration = Duration::from_secs(600); // 10 minutes
const NOTIFY_COOLDOWN: Duration = Duration::from_secs(300); // 5 min between same notifications
const DEFAULT_MAX_RETRIES: u32 = 5;
const BACKOFF_BASE: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(300);
const STABILITY_WINDOW: Duration = Duration::from_secs(1800); // 30 min stable → decay
/// Silence threshold for AwaitingOperator detection. Agent in `Starting`
/// with no stdout for this long is likely blocked on an interactive startup
/// prompt (trust dialog, codex update menu before banner, auth confirmation).
///
/// Only `Starting` is considered: once the agent transitions to `Ready` we
/// trust the detection and any further silence is either legitimate idle or
/// a real hang — the latter is handled by `check_hang` with much higher
/// thresholds (120s+). Flagging Ready-with-short-silence produced false
/// positives for agents in the middle of tool execution that simply had a
/// few seconds of quiet between bursts of output.
///
/// Threshold chosen to be a true last-resort fallback: structurally
/// recognizable prompts (y/n, press enter, etc. — see
/// `state::is_generic_startup_prompt`, plus the backend-specific
/// `InteractivePrompt` patterns) fire immediately on detection, so the
/// silence window only matters for prompts whose text we can't pattern
/// match. 30s is long enough that CLIs with slow splash screens or token
/// loading don't falsely trip, and still well under the 120s
/// `check_hang` threshold.
const AWAITING_OP_SILENCE: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)] // ErrorLoop constructed by record_error() in health monitoring
pub enum HealthState {
    Healthy,
    Recovering,
    Unstable,
    Failed,
    Hung,
    ErrorLoop,
    /// Sprint 24 P1 (F-NEW-DAEMON-HEALTH-CLASSIFIER-1): agent is silent
    /// past the hang threshold but **no input is pending past last
    /// response** — typically `Ready` state waiting for next dispatch.
    /// Cron escalation chains (`interrupt` / `replace`)
    /// MUST NOT trigger on this state; only `Hung` is escalation-worthy.
    /// Closes the operator 04:00 UTC false-alarm pattern where impl-1's
    /// 30-min idle-waiting was mis-classified as `Hung`.
    IdleLong,
    /// `#685` sub-task 7a Stage 1 recovery (decision
    /// `d-20260514030404021793-1`): agent escalated through Stage 3 of
    /// the auto-recovery dispatcher — Stage 1 ESC failed, Stage 2
    /// auto-restart failed N times. Operator action required to
    /// unpause (separate sub-task). Distinct from `Failed` (which =
    /// crash counter exhausted): same "operator must intervene"
    /// terminal status but different trigger.
    ///
    /// Guards: `check_hang` short-circuits on `Paused` (returns
    /// `false` — no auto-recovery dispatcher work); `maybe_decay` does
    /// NOT touch `Paused`; entered ONLY via Stage 3 dispatcher.
    /// See `docs/RECOVERY-STAGES.md` §RS for the lifecycle.
    Paused,
}

impl HealthState {
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Recovering => "recovering",
            Self::Unstable => "unstable",
            Self::Failed => "failed",
            Self::Hung => "hung",
            Self::ErrorLoop => "error_loop",
            Self::IdleLong => "idle_long",
            Self::Paused => "paused",
        }
    }
}

/// `#685` sub-task 7a: per-agent recovery dispatcher state machine for
/// the auto-recovery ladder. Carried inside `HealthTracker` so the
/// dispatcher can read both `HealthState` and stage progression in one
/// per-tick lock acquisition. Compile-time exhaustive match in the
/// dispatcher catches missing-state bugs.
///
/// **Spontaneous recovery reset** (decision §5 Refinement): when
/// `health.state` transitions back to `Healthy` (either Stage success
/// or external recovery), the dispatcher resets `recovery_stage_state`
/// to `None`. Subsequent `Hung` re-entry begins a fresh sequence —
/// linear escalation rule restarts from Stage 1.
///
/// **Future variant**: a 7th `Disabling { until_operator_unpauses }`
/// variant is intentionally NOT added in Phase 1 — Stage 3 + Paused
/// HealthState already covers the operator-action-required terminal
/// case. Recorded here as a comment-only note for future sub-tasks
/// that add operator-unpause command surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Stage2Pending / Stage3Eligible / Stage3Pending: emitted by sub-tasks 7b / 7c
pub enum RecoveryStageState {
    /// No recovery in progress. Default.
    None,
    /// Stage 1 ESC dispatched (or shadow-logged); monitoring for recovery.
    /// `entered_at` drives the Stage 1 timeout window.
    Stage1Pending { entered_at: Instant },
    /// Stage 1 timed out (or skipped via dead-likely branch). Dispatcher
    /// will fire Stage 2 on the next tick once 7b ships. Phase 1 logs
    /// the eligibility transition and stops here.
    Stage2Eligible,
    /// Stage 2 restart in progress; monitoring for recovery. (Phase 1
    /// stub — emitted by 7b when Stage 2 ships.)
    Stage2Pending { entered_at: Instant },
    /// Stage 2 timed out. Dispatcher will fire Stage 3 on next tick.
    Stage3Eligible,
    /// Stage 3 dispatched (HealthState transitioned to Paused).
    /// Awaiting operator unpause action. `entered_at` is the
    /// `Instant` passed to [`HealthTracker::enter_paused`] and is the
    /// same value stamped into [`HealthTracker::last_stage3_fired_at`]
    /// — kept on the variant so dispatcher tick-time debug logs can
    /// report Paused-since duration without reaching back into
    /// `HealthTracker` (parallel `Stage1Pending` / `Stage2Pending`).
    Stage3Pending { entered_at: Instant },
}

/// Stage 1 default timeout — ESC dispatched, dispatcher waits this long
/// before declaring failure and transitioning to `Stage2Eligible`.
/// Decision §1.4 Delta 1: 10s default (reviewer recommendation: ESC
/// delivery latency = PTY write + agent process scheduling + Ink TUI
/// state reset; under load, 5s false-positives Stage 2 escalation).
/// Operator override via env var `AGEND_AUTO_RECOVERY_STAGE1_TIMEOUT_MS`.
pub const STAGE1_TIMEOUT_DEFAULT_MS: u64 = 10_000;

/// Stage 1 default cooldown — if agent re-enters Hung within this window
/// after a recent Stage 1 fire, dispatcher skips Stage 1 (goes directly
/// to `Stage2Eligible`). Prevents rapid-fire ESC sending that masks
/// underlying issues. Decision §1.4 Refinement B.
/// Operator override via env var `AGEND_AUTO_RECOVERY_STAGE1_COOLDOWN_MS`.
pub const STAGE1_COOLDOWN_DEFAULT_MS: u64 = 60_000;

/// Stage 2 default backoff — sleep before `spawn_agent` re-runs in the
/// respawn worker's Stage 2 arm. Decision §1.4 Delta 2: 1s default
/// (defensive padding against tight-loop on transient spawn errors —
/// transient filesystem / network / PTY allocation failures), with env
/// var override for operators who observe unnecessary latency.
/// Operator override via env var `AGEND_AUTO_RECOVERY_STAGE2_BACKOFF_MS`.
pub const STAGE2_BACKOFF_DEFAULT_MS: u64 = 1_000;

/// Stage 2 default monitoring window — how long the dispatcher waits in
/// `Stage2Pending` for the agent to settle on `Healthy` before
/// classifying Stage 2 as failed and escalating to `Stage3Eligible`.
/// Mirrors the 30s window already documented in `docs/RECOVERY-STAGES.md`.
/// Operator override via env var `AGEND_AUTO_RECOVERY_STAGE2_TIMEOUT_MS`.
pub const STAGE2_TIMEOUT_DEFAULT_MS: u64 = 30_000;

/// Stage 2 cumulative retry cap — when `recovery_restart_count` reaches
/// this number across Hung cycles, the dispatcher skips Stages 1/2 on
/// the next Hung and escalates directly to `Stage3Eligible`. Decision
/// §Q1/Q2: N=3 default mirrors the issue body's "fails N times → Stage 3"
/// language with a conservative restart budget before operator intervention.
/// Operator override via env var `AGEND_AUTO_RECOVERY_STAGE2_MAX_RESTARTS`.
pub const STAGE2_MAX_RESTARTS_DEFAULT: u32 = 3;

/// Returns `true` when `silent_productive` (silence since last productive
/// output) exceeds the per-`AgentState` threshold. Mirrors the
/// `silence_exceeds_threshold` pattern at the top of `check_hang`.
/// Extracted per decision §1.4 Delta 2 (Option a: DRY, single source of
/// truth — recovery dispatcher reads this directly without
/// re-implementing the threshold mapping).
pub fn productive_silence_exceeds(agent_state: AgentState, silent_productive: Duration) -> bool {
    match agent_state {
        AgentState::Idle => false,
        AgentState::Starting => silent_productive > Duration::from_secs(120),
        AgentState::Thinking | AgentState::ToolUse => silent_productive > Duration::from_secs(600),
        _ => silent_productive > Duration::from_secs(120),
    }
}

/// Why an agent is blocked. Used to prevent `check_hang` from
/// misdiagnosing expected waits as hangs (race mutex).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum BlockedReason {
    Hang,
    RateLimit {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_after_secs: Option<u64>,
    },
    QuotaExceeded,
    AwaitingOperator,
    PermissionPrompt,
    Crash,
}

/// Tracks health for one agent.
#[derive(Clone)]
#[allow(dead_code)] // error_events, last_output, record_error, reset: reserved for daemon health monitoring
pub struct HealthTracker {
    pub state: HealthState,
    pub crash_times: VecDeque<Instant>,
    pub total_crashes: u32,
    max_retries: u32,
    pub last_notification: Option<Instant>,
    error_events: VecDeque<(Instant, AgentState)>,
    pub last_output: Instant,
    pub current_reason: Option<BlockedReason>,
    /// `#685` sub-task 7a: per-agent recovery dispatcher state machine.
    /// Mutated only by `src/daemon/per_tick/recovery_dispatcher.rs`;
    /// `health.rs` ships it as a field to keep all per-agent state in
    /// one struct (single lock acquisition for dispatcher tick).
    pub recovery_stage_state: RecoveryStageState,
    /// `#685` sub-task 7a: timestamp of last Stage 1 ESC fire (or
    /// shadow-log emission). Used by the cooldown guard — if agent
    /// re-enters `Hung` within `STAGE1_COOLDOWN_DEFAULT_MS` of this
    /// timestamp, dispatcher skips Stage 1 and escalates directly to
    /// `Stage2Eligible`. `None` means Stage 1 has never fired for this
    /// agent in the current daemon run.
    pub last_stage1_fired_at: Option<Instant>,
    /// `#685` sub-task 7b: cumulative Stage 2 auto-restart count across
    /// Hung cycles. Increments on each Stage 2 fire that the respawn
    /// worker successfully consumes (channel `try_send` `Ok`). When
    /// count reaches `STAGE2_MAX_RESTARTS_DEFAULT`, the dispatcher
    /// skips Stages 1/2 on the next Hung cycle and escalates directly
    /// to `Stage3Eligible` — operator must intervene rather than the
    /// daemon repeatedly thrashing the agent.
    ///
    /// Decays via `maybe_decay`: 1 unit per `STABILITY_WINDOW` (30 min)
    /// of last-crash stability. Mirrors `total_crashes` decay
    /// discipline so an agent that recovers and stays stable does not
    /// carry recovery-restart attribution forever.
    ///
    /// Preserved selectively across the Stage 2 respawn boundary in
    /// `daemon/mod.rs` Stage 2 variant arm — fresh `HealthTracker` on
    /// spawn re-applies this counter (plus crash_times + total_crashes,
    /// plus last_notification) so the cap survives the restart that
    /// the counter itself drove.
    pub recovery_restart_count: u32,
    /// `#685` sub-task 7b: timestamp of last Stage 2 auto-restart fire.
    /// Used to drive `recovery_restart_count` decay alongside
    /// `crash_times` — `maybe_decay` checks the most recent of the two
    /// when deciding whether the agent has been stable long enough to
    /// decrement the recovery counter. `None` means Stage 2 has never
    /// fired for this agent in the current daemon run.
    pub last_stage2_fired_at: Option<Instant>,
    /// `#685` sub-task 7c: timestamp of last Stage 3 escalation (the
    /// transition into `HealthState::Paused`). Reserved for the future
    /// operator-unpause sub-task — it will read this to display "Paused
    /// since {duration}" in the unpause UI and may extend
    /// [`Self::maybe_decay_at`] to honour a Paused decay window. Stage 3
    /// is terminal in Phase 2, so nothing in 7c reads the field; carry
    /// `#[allow(dead_code)]` until the unpause sub-task lands.
    #[allow(dead_code)] // reserved for unpause sub-task (Stage 3 decay)
    pub(crate) last_stage3_fired_at: Option<Instant>,
}

impl HealthTracker {
    pub fn new() -> Self {
        Self {
            state: HealthState::Healthy,
            crash_times: VecDeque::new(),
            total_crashes: 0,
            max_retries: DEFAULT_MAX_RETRIES,
            last_notification: None,
            error_events: VecDeque::new(),
            last_output: Instant::now(),
            current_reason: None,
            recovery_stage_state: RecoveryStageState::None,
            last_stage1_fired_at: None,
            recovery_restart_count: 0,
            last_stage2_fired_at: None,
            last_stage3_fired_at: None,
        }
    }

    /// `#685` sub-task 7c: atomic transition into `HealthState::Paused`
    /// for Stage 3 escalation. Encapsulates the three invariants that
    /// the dispatcher's Stage 3 arm must apply together so the §F39.5
    /// rule "Paused entered ONLY via Stage 3 dispatcher" has a single
    /// grep target — `enter_paused` is the sole writer of
    /// `HealthState::Paused` in the codebase.
    ///
    /// Invariants written in one logical step (caller holds the per-
    /// agent lock for the duration):
    /// 1. `state` → `HealthState::Paused` (terminal — no further
    ///    auto-recovery; check_hang short-circuits and maybe_decay
    ///    no-touches Paused per 7a guards).
    /// 2. `recovery_stage_state` → `Stage3Pending { entered_at: now }`
    ///    so the dispatcher's next tick lands on the idempotent no-op
    ///    arm rather than re-firing Stage 3.
    /// 3. `last_stage3_fired_at` → `Some(now)` for the future operator-
    ///    unpause sub-task's UX (Paused-since-{duration}).
    ///
    /// **`recovery_restart_count` is NOT reset** — preserves the
    /// operator-must-fix-root-cause signal across a future manual
    /// unpause. If the post-unpause agent Hungs again without the root
    /// cause being addressed, the cap check in
    /// [`crate::daemon::per_tick::recovery_dispatcher`] immediately
    /// re-escalates to Stage3Eligible rather than burning further
    /// auto-restart budget.
    ///
    /// DI-friendly signature (parallels [`Self::maybe_decay_at`]) so
    /// tests can supply a deterministic `now`. Production callers
    /// always pass `Instant::now()`.
    pub fn enter_paused(&mut self, now: Instant) {
        self.state = HealthState::Paused;
        self.recovery_stage_state = RecoveryStageState::Stage3Pending { entered_at: now };
        self.last_stage3_fired_at = Some(now);
    }

    /// Record a crash event. Returns (should_respawn, respawn_delay, should_notify).
    pub fn record_crash(&mut self) -> (bool, Duration, bool) {
        let now = Instant::now();
        self.crash_times.push_back(now);
        self.total_crashes += 1;

        // Clean old crashes outside window
        while let Some(front) = self.crash_times.front() {
            if now.duration_since(*front) > CRASH_WINDOW {
                self.crash_times.pop_front();
            } else {
                break;
            }
        }

        let recent = self.crash_times.len();
        let delay = self.backoff_delay();

        // Check max retries
        if self.total_crashes >= self.max_retries {
            self.state = HealthState::Failed;
            return (false, Duration::ZERO, true); // Don't respawn, do notify
        }

        let should_notify = recent >= 2 && self.should_notify();

        if recent >= 3 {
            self.state = HealthState::Unstable;
        } else if recent >= 1 {
            self.state = HealthState::Recovering;
        }

        if should_notify {
            self.last_notification = Some(now);
        }

        (true, delay, should_notify)
    }

    /// Mark successful respawn.
    pub fn respawn_ok(&mut self) {
        if self.state == HealthState::Recovering {
            self.state = HealthState::Healthy;
        }
        // Unstable stays until crash window clears
    }

    /// Calculate exponential backoff delay.
    fn backoff_delay(&self) -> Duration {
        if self.total_crashes == 0 {
            return BACKOFF_BASE;
        }
        let exp = (self.total_crashes - 1).min(10);
        let delay = BACKOFF_BASE.mul_f64(2.0_f64.powi(exp as i32));
        delay.min(BACKOFF_MAX)
    }

    /// Check if we should send a notification (rate limiting).
    fn should_notify(&self) -> bool {
        match self.last_notification {
            Some(last) => last.elapsed() >= NOTIFY_COOLDOWN,
            None => true,
        }
    }

    /// Check whether agent is stalled on an interactive startup prompt.
    /// Pure predicate — no state mutation.
    ///
    /// Fires only when the agent is in `Starting` AND has been silent past
    /// the threshold. Once the agent reaches `Ready` we trust the backend
    /// and any further quiet period is handled by `check_hang` with a much
    /// higher threshold — flagging short Ready silences produced false
    /// positives for agents in the middle of legitimate tool execution.
    pub fn check_awaiting_operator(&self, agent_state: AgentState, silent: Duration) -> bool {
        silent > AWAITING_OP_SILENCE && matches!(agent_state, AgentState::Starting)
    }

    /// Check for hang based on agent state and output timeout.
    ///
    /// Takes `silent` as a plain `Duration` (rather than `Instant::elapsed()`
    /// internally) so tests can construct arbitrary durations without
    /// overflowing on platforms where `Instant` is boot-anchored (Windows).
    ///
    /// **Sprint 24 P1 (F-NEW-DAEMON-HEALTH-CLASSIFIER-1)** added the
    /// `last_input_at_ms` + `last_heartbeat_at_ms` parameters from the
    /// per-instance [`crate::daemon::heartbeat_pair::HeartbeatPair`]
    /// snapshot to discriminate "idle waiting (no input pending)" from
    /// "hung unresponsive (input pending past last response)". Pass `0`
    /// for both in test contexts where the caller only wants legacy
    /// silence-based hang detection (back-compat with existing tests).
    ///
    /// Returns `true` ONLY when transitioning **into** [`HealthState::Hung`]
    /// (the escalation-worthy state). [`HealthState::IdleLong`] transitions
    /// return `false` so cron escalation consumers (interrupt
    /// / replace) keep their existing semantics — they only act on `Hung`.
    ///
    /// Mutator monopoly: [`Self::maybe_decay`] does NOT touch
    /// [`HealthState::Hung`]; all Hung mutations are inside this function
    /// (entries below, exit at the silence-drops branch). See
    /// `docs/HUNG-STATE-TRANSITIONS.md §Invariants`.
    ///
    /// **F9 (#685 sub-task 4)**: `silent_productive` is the dual-path
    /// supplement signal (silence-since-last-productive-output vs the
    /// existing `silent` = silence-since-any-output). When the env var
    /// `AGEND_PRODUCTIVE_GATE=1` is set, the productive-silence path can
    /// trigger Hung classification independently of the silent path —
    /// catching the F9 grey failure where 1-byte spinner output keeps
    /// `silent` below threshold while no productive work happens. Default
    /// (env var unset) behavior is shadow-mode: telemetry collected, no
    /// classification change. See `docs/F9-PRODUCTIVE-OUTPUT-GATE.md` §F9.5.
    pub fn check_hang(
        &mut self,
        agent_state: AgentState,
        silent: Duration,
        silent_productive: Duration,
        last_input_at_ms: u64,
        last_heartbeat_at_ms: u64,
    ) -> bool {
        // `#685` sub-task 7a guard: `Paused` is operator-action-required
        // terminal state — auto-recovery dispatcher already escalated
        // through Stage 3 and stopped. `check_hang` must NOT mutate state
        // back to `Hung` or trigger further dispatcher work. Return false
        // immediately so the upstream `tracing::warn!` at the hang-detection
        // tick site is suppressed too (operator already alerted via Stage 3
        // telegram notify; further warns would be noise).
        if self.state == HealthState::Paused {
            return false;
        }

        // Race mutex: skip hang check when agent is blocked for a known
        // reason that legitimately suppresses output.
        if let Some(ref reason) = self.current_reason {
            if matches!(
                reason,
                BlockedReason::RateLimit { .. }
                    | BlockedReason::QuotaExceeded
                    | BlockedReason::AwaitingOperator
            ) {
                return false;
            }
        }

        let silence_exceeds_threshold = productive_silence_exceeds(agent_state, silent);

        // F9 (#685 sub-task 4): productive-silence threshold mirrors silent
        // thresholds. Active only when `AGEND_PRODUCTIVE_GATE=1` is set;
        // telemetry fires regardless for fixture-corpus measurement.
        let productive_exceeds = productive_silence_exceeds(agent_state, silent_productive);
        let f9_gate_active = std::env::var("AGEND_PRODUCTIVE_GATE")
            .map(|v| v == "1")
            .unwrap_or(false);
        // Shadow-mode telemetry: fires when the productive path would
        // independently flag Hung but the silent path does not. Lets the
        // fixture corpus measure F9 FP rate without affecting prod behavior
        // until the env var flips to active.
        if productive_exceeds && !silence_exceeds_threshold {
            tracing::debug!(
                target: "behavioral_shadow",
                silent_secs = silent.as_secs(),
                silent_productive_secs = silent_productive.as_secs(),
                agent_state = ?agent_state,
                active = f9_gate_active,
                "F9 dual-path candidate: silent_productive exceeded without silent"
            );
        }
        // Dual-path: Hung classification fires if either path exceeded,
        // gated on env var for productive path until promoted to default.
        let any_path_exceeds = silence_exceeds_threshold || (f9_gate_active && productive_exceeds);

        if !any_path_exceeds {
            // Hung Exit (X1) / IdleLong Exit (X1): silence dropped below threshold
            // PRE: state in {Hung, IdleLong}, !silence_exceeds_threshold
            // POST: state = Healthy, check_hang returns false
            // FP vector: F10 — any 1 byte of output (spinner tick, log line) flips
            //   Hung → Healthy without productive-work evidence.
            // FN vector: indirect via FP (stale "Healthy" hides genuine stuck agent).
            // See docs/HUNG-STATE-TRANSITIONS.md §Exit.X1
            if matches!(self.state, HealthState::Hung | HealthState::IdleLong) {
                self.state = HealthState::Healthy;
            }
            return false;
        }

        // Sprint 24 P1 discriminator: input pending past last response
        // (heartbeat) → real Hung. Otherwise (no input pending OR input
        // already responded to) → IdleLong (no escalation).
        //
        // Grace window prevents flapping when input arrives 1-tick before
        // the heartbeat write completes. 5s mirrors typical MCP roundtrip
        // upper-bound for a non-busy agent.
        const INPUT_RESPONSE_GRACE_MS: u64 = 5_000;
        let input_pending_past_response = last_input_at_ms > 0
            && last_input_at_ms > last_heartbeat_at_ms.saturating_add(INPUT_RESPONSE_GRACE_MS);

        if input_pending_past_response {
            // Real hung: input was delivered but agent has not responded
            // (no MCP call to refresh heartbeat). Operator-facing log
            // includes delta diagnostic per self-diagnostic pattern (PR #241
            // praise pattern transfer).
            let delta_ms = last_input_at_ms.saturating_sub(last_heartbeat_at_ms);
            tracing::warn!(
                last_input_at_ms,
                last_heartbeat_at_ms,
                input_response_delta_ms = delta_ms,
                silent_secs = silent.as_secs(),
                agent_state = ?agent_state,
                "agent classified Hung — input pending {delta_ms}ms past last heartbeat (escalation-worthy)"
            );
            // Hung Entry (E1): input pending past heartbeat_deadline
            // PRE: !blocked-reason race mutex, silence > threshold,
            //   last_input_at_ms > last_heartbeat_at_ms + 5s grace,
            //   state != Hung
            // POST: state = Hung, check_hang returns true (first detection only)
            // FP vector: operator typed input but agent is genuinely producing
            //   keystrokes draining through MCP; bounded by heartbeat refresh.
            // FN vector: F9 grey failure — 1-byte spinner output resets silent
            //   timer in StateTracker; never crosses threshold.
            // See docs/HUNG-STATE-TRANSITIONS.md §Entry.E1
            if self.state != HealthState::Hung {
                self.state = HealthState::Hung;
                return true; // First hang detection — caller escalates
            }
            return false;
        }

        // No input pending past response → idle waiting (operator 04:00
        // UTC false-alarm pattern). Mark IdleLong so consumers can
        // distinguish from Hung but cron escalation MUST NOT trigger.
        //
        // Sprint 24 P2 F1 cross-check: heartbeat fresh but PTY silent →
        // agent is calling MCP tools (refreshing heartbeat) without
        // producing PTY output. Indicates a stuck agent in a tight MCP
        // loop — operator should be notified for diagnosis.
        let heartbeat_age_ms =
            crate::daemon::heartbeat_pair::now_ms().saturating_sub(last_heartbeat_at_ms);
        let heartbeat_fresh =
            last_heartbeat_at_ms > 0 && heartbeat_age_ms < silent.as_millis() as u64;
        if heartbeat_fresh {
            let delta_ms = silent.as_millis() as u64;
            tracing::warn!(
                last_heartbeat_at_ms,
                heartbeat_age_ms,
                silent_ms = delta_ms,
                agent_state = ?agent_state,
                "agent classified Hung — heartbeat fresh but PTY silent (F1 cross-check)"
            );
            // Hung Entry (E2): heartbeat fresh but PTY silent (F1 cross-check)
            // PRE: !blocked-reason race mutex, silence > threshold,
            //   !input_pending_past_response (E1 did not fire),
            //   last_heartbeat_at_ms > 0 AND heartbeat_age_ms < silent.as_millis(),
            //   state != Hung
            // POST: state = Hung, check_hang returns true (first detection only)
            // FP vector: F39 — stale AgentState::Thinking pattern in vterm scrollback;
            //   bounded by LATCHED_STATE_EXPIRY (30s) but not perfectly.
            // FN vector: F9 grey failure — same shape as §Entry.E1.
            // See docs/HUNG-STATE-TRANSITIONS.md §Entry.E2
            if self.state != HealthState::Hung {
                self.state = HealthState::Hung;
                return true;
            }
            return false;
        }

        // IdleLong Entry (E1): silent past threshold, no input pending
        // PRE: !blocked-reason race mutex, silence > threshold,
        //   !input_pending_past_response, !heartbeat_fresh, state != IdleLong
        // POST: state = IdleLong, check_hang returns false (escalation
        //   consumers act only on Hung per rustdoc contract above)
        // FP vector: genuinely idle agent waiting for next operator prompt
        //   (04:00 UTC false-alarm pattern that motivated splitting Hung/IdleLong)
        // FN vector: F9 grey failure — same shape as §Entry.E1.
        // See docs/HUNG-STATE-TRANSITIONS.md §IdleLong.Entry.E1
        if self.state != HealthState::IdleLong {
            tracing::debug!(
                last_input_at_ms,
                last_heartbeat_at_ms,
                silent_secs = silent.as_secs(),
                agent_state = ?agent_state,
                "agent classified IdleLong — silent past threshold but no input pending (no escalation)"
            );
            self.state = HealthState::IdleLong;
        }
        false
    }

    /// Set the current blocked reason. Prevents `check_hang` from
    /// misdiagnosing expected waits as hangs.
    #[allow(dead_code)] // stacking dep: wired by S2-T2/S2-T3/S2-T4
    pub fn set_blocked_reason(&mut self, reason: BlockedReason) {
        self.current_reason = Some(reason);
    }

    /// Clear the current blocked reason, resuming normal hang detection.
    #[allow(dead_code)] // stacking dep: wired by S2-T2 MCP clear_blocked_reason tool
    pub fn clear_blocked_reason(&mut self) {
        self.current_reason = None;
    }

    /// Record an error state. Returns true if error loop detected (3x in 10min).
    #[allow(dead_code)] // wired by daemon health monitoring; used in tests
    pub fn record_error(&mut self, state: AgentState) -> bool {
        let now = Instant::now();
        self.error_events.push_back((now, state));

        // Clean old events
        while let Some((t, _)) = self.error_events.front() {
            if now.duration_since(*t) > CRASH_WINDOW {
                self.error_events.pop_front();
            } else {
                break;
            }
        }

        let count = self
            .error_events
            .iter()
            .filter(|(_, s)| *s == state)
            .count();

        if count >= 3 {
            self.state = HealthState::ErrorLoop;
            true
        } else {
            false
        }
    }

    /// Get crash reason string for inject hint.
    pub fn crash_reason(&self) -> &'static str {
        match self.state {
            HealthState::Recovering => "crash",
            HealthState::Unstable => "repeated crashes",
            HealthState::Failed => "too many crashes",
            HealthState::ErrorLoop => "error loop",
            _ => "unknown",
        }
    }

    /// Decay total_crashes if stable for STABILITY_WINDOW.
    /// Call periodically from daemon main loop.
    ///
    /// `#685` sub-task 7a guard: `Paused` is operator-action-required —
    /// crash decay must NOT exit `Paused` (only operator unpause can).
    /// `Paused` is reachable only via Stage 3 dispatcher, never via
    /// crash counter or decay paths.
    pub fn maybe_decay(&mut self) {
        self.maybe_decay_at(Instant::now());
    }

    /// Test-injection variant of [`Self::maybe_decay`] — accepts a
    /// caller-supplied `now` so tests can simulate elapsed time without
    /// constructing backdated `Instant` values via subtraction.
    ///
    /// **Why this exists**: Windows `Instant::now()` is anchored to system
    /// uptime via `QueryPerformanceCounter`. On a fresh CI VM with low
    /// uptime, `Instant::now() - Duration::from_secs(30 * 60)` underflows
    /// and panics. Tests instead use `base + offset` (cross-platform safe;
    /// `Instant::add` saturates to `Instant::MAX` on all platforms) and
    /// pass the resulting future-Instant as the `now` argument. Internal
    /// elapsed checks use `now.saturating_duration_since(t)` defensively
    /// against clock skew.
    ///
    /// Production callers should always use [`Self::maybe_decay`] which
    /// passes `Instant::now()` — zero behaviour change. Sub-task 7b PR
    /// #775 v2 hot-fix.
    pub(crate) fn maybe_decay_at(&mut self, now: Instant) {
        if self.state == HealthState::Paused {
            return;
        }
        // `#685` sub-task 7b: decay recovery_restart_count via the same
        // STABILITY_WINDOW discipline as crash decay. Independent of
        // crash counter — if agent went through Stage 2 restart without
        // crashing, last_stage2_fired_at drives the decay clock alone.
        // Mirrors decision §Delta 3: long-stability decay (NOT
        // reset-on-Healthy, which oscillates too aggressively).
        if self.recovery_restart_count > 0 {
            let last_stage2_idle = self
                .last_stage2_fired_at
                .map(|t| now.saturating_duration_since(t) >= STABILITY_WINDOW)
                .unwrap_or(false);
            if last_stage2_idle {
                self.recovery_restart_count = self.recovery_restart_count.saturating_sub(1);
            }
        }
        if self.total_crashes == 0 {
            return;
        }
        let last_crash = match self.crash_times.back() {
            Some(t) => *t,
            None => return,
        };
        if now.saturating_duration_since(last_crash) >= STABILITY_WINDOW {
            self.total_crashes = self.total_crashes.saturating_sub(1);
            if self.total_crashes == 0 {
                self.crash_times.clear();
            }
            // Recover from Failed/Unstable if crashes decayed enough.
            // Known limitation: Failed → Recovering with a dead process
            // leaves the agent in Recovering until operator manual restart
            // or #685 Stage 2 auto-restart fires. record_crash returned
            // (false, _, _) when entering Failed, so the process is gone.
            if self.total_crashes < DEFAULT_MAX_RETRIES && self.state == HealthState::Failed {
                self.state = HealthState::Recovering;
            }
            if self.total_crashes < 3
                && matches!(self.state, HealthState::Unstable | HealthState::Recovering)
            {
                self.state = HealthState::Healthy;
            }
        }
    }

    /// Reset health state (e.g., after manual restart).
    #[allow(dead_code)] // used in tests; available for manual restart path
    pub fn reset(&mut self) {
        self.state = HealthState::Healthy;
        self.crash_times.clear();
        self.total_crashes = 0;
        self.error_events.clear();
        self.current_reason = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_first_crash_silent() {
        let mut h = HealthTracker::new();
        let (respawn, _delay, notify) = h.record_crash();
        assert!(respawn);
        assert!(!notify); // 1st crash = silent
        assert_eq!(h.state, HealthState::Recovering);
    }

    #[test]
    fn test_second_crash_notifies() {
        let mut h = HealthTracker::new();
        h.record_crash();
        let (respawn, _delay, notify) = h.record_crash();
        assert!(respawn);
        assert!(notify); // 2nd crash = notify
    }

    #[test]
    fn test_unstable_after_three() {
        let mut h = HealthTracker::new();
        h.record_crash();
        h.record_crash();
        h.record_crash();
        assert_eq!(h.state, HealthState::Unstable);
    }

    #[test]
    fn test_failed_after_max_retries() {
        let mut h = HealthTracker::new();
        for _ in 0..5 {
            h.record_crash();
        }
        assert_eq!(h.state, HealthState::Failed);
        let (respawn, _, _) = h.record_crash();
        assert!(!respawn); // Failed state = no more respawn
    }

    #[test]
    fn test_backoff_exponential() {
        let mut h = HealthTracker::new();
        h.record_crash();
        assert_eq!(h.backoff_delay(), Duration::from_secs(5));
        h.record_crash();
        assert_eq!(h.backoff_delay(), Duration::from_secs(10));
        h.record_crash();
        assert_eq!(h.backoff_delay(), Duration::from_secs(20));
        h.record_crash();
        assert_eq!(h.backoff_delay(), Duration::from_secs(40));
    }

    #[test]
    fn test_error_loop() {
        let mut h = HealthTracker::new();
        assert!(!h.record_error(AgentState::RateLimit));
        assert!(!h.record_error(AgentState::RateLimit));
        assert!(h.record_error(AgentState::RateLimit)); // 3rd = loop
        assert_eq!(h.state, HealthState::ErrorLoop);
    }

    #[test]
    fn test_hang_idle_exempt() {
        let mut h = HealthTracker::new();
        assert!(!h.check_hang(
            AgentState::Idle,
            Duration::from_secs(300),
            Duration::from_secs(0),
            1_000_000,
            0
        ));
        // Idle never hangs
    }

    #[test]
    fn test_hang_thinking_long_timeout() {
        let mut h = HealthTracker::new();
        assert!(!h.check_hang(
            AgentState::Thinking,
            Duration::from_secs(100),
            Duration::from_secs(0),
            1_000_000,
            0
        )); // 100s < 600s
        assert!(h.check_hang(
            AgentState::Thinking,
            Duration::from_secs(700),
            Duration::from_secs(0),
            1_000_000,
            0
        ));
        // 700s > 600s
    }

    #[test]
    fn test_awaiting_operator_starting_silence() {
        let h = HealthTracker::new();
        // Starting + 29s silence → under threshold (slow splash/token load)
        assert!(!h.check_awaiting_operator(AgentState::Starting, Duration::from_secs(29)));
        // Starting + 31s silence → flagged
        assert!(h.check_awaiting_operator(AgentState::Starting, Duration::from_secs(31)));
    }

    #[test]
    fn test_awaiting_operator_non_starting_exempt() {
        // Only Starting triggers. Ready and all other states are exempt
        // regardless of silence — Ready silence is handled by `check_hang`
        // with much higher thresholds so legitimate pauses between tool
        // bursts don't produce false positives.
        let h = HealthTracker::new();
        for s in [
            AgentState::Ready,
            AgentState::Idle,
            AgentState::Thinking,
            AgentState::ToolUse,
            AgentState::InteractivePrompt,
            AgentState::PermissionPrompt,
            AgentState::Hang,
            AgentState::AwaitingOperator,
            AgentState::Crashed,
        ] {
            assert!(
                !h.check_awaiting_operator(s, Duration::from_secs(60)),
                "state {:?} should not trigger awaiting_operator",
                s
            );
        }
    }

    #[test]
    fn test_notification_rate_limit() {
        let mut h = HealthTracker::new();
        h.record_crash();
        let (_, _, notify1) = h.record_crash();
        assert!(notify1); // First notification

        let (_, _, notify2) = h.record_crash();
        assert!(!notify2); // Rate limited (< 5 min)
    }

    #[test]
    fn test_respawn_ok_recovers() {
        let mut h = HealthTracker::new();
        h.record_crash();
        assert_eq!(h.state, HealthState::Recovering);
        h.respawn_ok();
        assert_eq!(h.state, HealthState::Healthy);
    }

    #[test]
    fn test_clone_preserves_crash_history() {
        let mut h = HealthTracker::new();
        h.record_crash();
        h.record_crash();
        assert_eq!(h.total_crashes, 2);

        // Simulate respawn: clone old tracker, call respawn_ok
        let mut h2 = h.clone();
        h2.respawn_ok();
        assert_eq!(h2.total_crashes, 2); // History preserved

        // 3rd crash on cloned tracker should see recent=3
        let (_, _, notify) = h2.record_crash();
        // notify is false: 2nd crash already set last_notification and cooldown (5 min) hasn't elapsed
        assert!(!notify);
        assert_eq!(h2.state, HealthState::Unstable);
    }

    #[test]
    fn test_maybe_decay() {
        let mut h = HealthTracker::new();
        h.record_crash();
        h.record_crash();
        assert_eq!(h.total_crashes, 2);
        // Decay won't trigger immediately (need 30 min)
        h.maybe_decay();
        assert_eq!(h.total_crashes, 2);
    }

    #[test]
    fn test_check_hang_skipped_when_rate_limited() {
        let mut h = HealthTracker::new();
        h.set_blocked_reason(BlockedReason::RateLimit {
            retry_after_secs: Some(60),
        });
        // Thinking + 700s silence would normally trigger hang
        assert!(!h.check_hang(
            AgentState::Thinking,
            Duration::from_secs(700),
            Duration::from_secs(0),
            1_000_000,
            0
        ));
        assert_ne!(h.state, HealthState::Hung);

        // Also test QuotaExceeded and AwaitingOperator
        h.clear_blocked_reason();
        h.set_blocked_reason(BlockedReason::QuotaExceeded);
        assert!(!h.check_hang(
            AgentState::Thinking,
            Duration::from_secs(700),
            Duration::from_secs(0),
            1_000_000,
            0
        ));

        h.clear_blocked_reason();
        h.set_blocked_reason(BlockedReason::AwaitingOperator);
        assert!(!h.check_hang(
            AgentState::Thinking,
            Duration::from_secs(700),
            Duration::from_secs(0),
            1_000_000,
            0
        ));

        // PermissionPrompt does NOT suppress hang check
        h.clear_blocked_reason();
        h.set_blocked_reason(BlockedReason::PermissionPrompt);
        assert!(h.check_hang(
            AgentState::Thinking,
            Duration::from_secs(700),
            Duration::from_secs(0),
            1_000_000,
            0
        ));
    }

    #[test]
    fn test_clear_blocked_reason_resumes_hang_check() {
        let mut h = HealthTracker::new();
        h.set_blocked_reason(BlockedReason::RateLimit {
            retry_after_secs: None,
        });
        assert!(!h.check_hang(
            AgentState::Thinking,
            Duration::from_secs(700),
            Duration::from_secs(0),
            1_000_000,
            0
        ));

        h.clear_blocked_reason();
        assert!(h.check_hang(
            AgentState::Thinking,
            Duration::from_secs(700),
            Duration::from_secs(0),
            1_000_000,
            0
        ));
        assert_eq!(h.state, HealthState::Hung);
    }

    #[test]
    fn test_blocked_reason_serde() {
        let cases = vec![
            BlockedReason::Hang,
            BlockedReason::RateLimit {
                retry_after_secs: Some(60),
            },
            BlockedReason::RateLimit {
                retry_after_secs: None,
            },
            BlockedReason::QuotaExceeded,
            BlockedReason::AwaitingOperator,
            BlockedReason::PermissionPrompt,
            BlockedReason::Crash,
        ];
        for reason in cases {
            let json = serde_json::to_string(&reason).expect("serialize");
            let parsed: BlockedReason = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(parsed, reason, "round-trip failed for {json}");
        }
    }

    #[test]
    fn test_watchdog_dry_run_logs_but_no_state_change() {
        // Simulate dry-run: classify returns Some(RateLimit), but we only log, don't set.
        let mut h = HealthTracker::new();
        let backend = crate::backend::Backend::ClaudeCode;
        // #1125 M4: updated to use canonical for_backend pattern
        let output = "Server is temporarily limiting requests";
        let reason = crate::state::classify_pty_output(&backend, output);
        assert!(reason.is_some(), "should classify as blocked");

        // Dry-run: do NOT call set_blocked_reason
        // (in production, daemon checks AGEND_WATCHDOG_DRY_RUN)
        assert!(
            h.current_reason.is_none(),
            "dry-run must not mutate health state"
        );

        // check_hang should still fire (no reason set)
        assert!(h.check_hang(
            AgentState::Thinking,
            Duration::from_secs(700),
            Duration::from_secs(0),
            1_000_000,
            0
        ));
    }

    #[test]
    fn test_watchdog_live_sets_reason() {
        // Simulate live mode: classify returns Some, set_blocked_reason called.
        let mut h = HealthTracker::new();
        let backend = crate::backend::Backend::KiroCli;
        let output = "ThrottlingError: Too Many Requests";
        if let Some(reason) = crate::state::classify_pty_output(&backend, output) {
            h.set_blocked_reason(reason);
        }
        assert!(
            matches!(h.current_reason, Some(BlockedReason::RateLimit { .. })),
            "live mode must set current_reason, got: {:?}",
            h.current_reason
        );
        // check_hang should be suppressed
        assert!(!h.check_hang(
            AgentState::Thinking,
            Duration::from_secs(700),
            Duration::from_secs(0),
            1_000_000,
            0
        ));
    }

    #[test]
    fn test_watchdog_ignores_classify_none() {
        // Healthy output → classify returns None → no change.
        let mut h = HealthTracker::new();
        let backend = crate::backend::Backend::ClaudeCode;
        let output = "Thinking about your request...";
        let reason = crate::state::classify_pty_output(&backend, output);
        assert!(reason.is_none(), "healthy output should not classify");
        assert!(h.current_reason.is_none());
        // check_hang still works normally
        assert!(h.check_hang(
            AgentState::Thinking,
            Duration::from_secs(700),
            Duration::from_secs(0),
            1_000_000,
            0
        ));
    }

    #[test]
    fn test_reset_clears_current_reason() {
        let mut h = HealthTracker::new();
        h.set_blocked_reason(BlockedReason::QuotaExceeded);
        assert!(h.current_reason.is_some());
        h.reset();
        assert!(
            h.current_reason.is_none(),
            "reset must clear current_reason"
        );
    }

    // Sprint 24 P1 (F-NEW-DAEMON-HEALTH-CLASSIFIER-1) — IdleLong vs Hung
    // discriminator tests. Closes operator 04:00 UTC false-alarm pattern.

    #[test]
    fn classifier_returns_hung_when_input_pending_past_response() {
        // Real hung: input delivered at T+5s, agent has not responded
        // (heartbeat still at T+0). Silence exceeds threshold. Classifier
        // must return true (escalation-worthy) and set state = Hung.
        let mut h = HealthTracker::new();
        // last_input_at_ms past last_heartbeat_at_ms by > 5s grace.
        let result = h.check_hang(
            AgentState::Ready,
            Duration::from_secs(180), // > 120s threshold
            Duration::from_secs(0),   // F9: productive-silence — recent
            10_000,                   // input delivered at T+10s
            0,                        // no heartbeat (or T-0)
        );
        assert!(result, "input pending past response → Hung, return true");
        assert_eq!(h.state, HealthState::Hung);
    }

    #[test]
    fn classifier_returns_idle_long_when_no_input_pending() {
        // Operator 04:00 UTC pattern: agent silent past threshold but NO
        // input was delivered (last_input_at_ms == 0). Classifier must
        // mark IdleLong (no escalation) and return false.
        let mut h = HealthTracker::new();
        let result = h.check_hang(
            AgentState::Ready,
            Duration::from_secs(180), // > 120s threshold
            Duration::from_secs(0),   // F9: productive-silence — recent
            0,                        // no input ever delivered
            5_000,                    // heartbeat at T+5s (some past activity)
        );
        assert!(!result, "no input pending → IdleLong, no escalation");
        assert_eq!(
            h.state,
            HealthState::IdleLong,
            "must be IdleLong, NOT Hung — operator 04:00 UTC false-alarm pattern"
        );
    }

    #[test]
    fn classifier_returns_idle_long_when_input_already_responded_to() {
        // Input delivered at T+0, agent responded at T+8s (heartbeat
        // refreshed). Silence then accrues. Last_input < last_heartbeat
        // → no input pending → IdleLong (NOT Hung).
        let mut h = HealthTracker::new();
        let result = h.check_hang(
            AgentState::Ready,
            Duration::from_secs(180),
            Duration::from_secs(0), // F9: productive-silence — recent
            0,                      // input at T+0
            8_000,                  // heartbeat at T+8s (already responded)
        );
        assert!(!result, "input already responded → IdleLong");
        assert_eq!(h.state, HealthState::IdleLong);
    }

    #[test]
    fn classifier_returns_healthy_when_silence_below_threshold() {
        // Fresh agent: silent < threshold. Classifier returns Healthy
        // regardless of input/heartbeat data.
        let mut h = HealthTracker::new();
        let result = h.check_hang(
            AgentState::Ready,
            Duration::from_secs(60), // < 120s threshold
            Duration::from_secs(0),  // F9: productive-silence — recent
            10_000,
            0,
        );
        assert!(!result);
        assert_eq!(h.state, HealthState::Healthy);
    }

    #[test]
    fn classifier_idle_long_recovers_to_healthy_when_activity_resumes() {
        // Agent enters IdleLong at T+180s silent. Then activity resumes
        // (silent drops below threshold). State must transition back to
        // Healthy so future cron consumers don't see stale IdleLong.
        let mut h = HealthTracker::new();
        h.check_hang(
            AgentState::Ready,
            Duration::from_secs(180),
            Duration::from_secs(0),
            0,
            5_000,
        );
        assert_eq!(h.state, HealthState::IdleLong);
        // Activity resumes → silence < threshold.
        let result = h.check_hang(
            AgentState::Ready,
            Duration::from_secs(30),
            Duration::from_secs(0),
            0,
            5_000,
        );
        assert!(!result);
        assert_eq!(
            h.state,
            HealthState::Healthy,
            "IdleLong must recover to Healthy when silence drops"
        );
    }

    #[test]
    fn classifier_grace_window_prevents_flap() {
        // last_input at T+5s, last_heartbeat at T+0. Delta = 5_000ms = exactly
        // the grace window. Must NOT flag Hung — within grace.
        let mut h = HealthTracker::new();
        let result = h.check_hang(
            AgentState::Ready,
            Duration::from_secs(180),
            Duration::from_secs(0), // F9: productive-silence — recent
            5_000,                  // input
            0,                      // heartbeat — delta exactly 5_000ms
        );
        assert!(
            !result,
            "delta == grace window → not yet Hung (boundary inclusive)"
        );
        // delta = 5_001 > grace → Hung
        let result2 = h.check_hang(
            AgentState::Ready,
            Duration::from_secs(180),
            Duration::from_secs(0),
            5_001,
            0,
        );
        assert!(result2, "delta > grace → Hung");
    }

    #[test]
    fn classifier_hung_state_returns_false_on_subsequent_calls() {
        // First Hung detection returns true (caller escalates). Second
        // call with same state must return false (already escalated;
        // avoid duplicate escalation).
        let mut h = HealthTracker::new();
        assert!(h.check_hang(
            AgentState::Ready,
            Duration::from_secs(180),
            Duration::from_secs(0),
            10_000,
            0
        ));
        assert_eq!(h.state, HealthState::Hung);
        // Same conditions next tick → no re-escalation.
        assert!(!h.check_hang(
            AgentState::Ready,
            Duration::from_secs(180),
            Duration::from_secs(0),
            10_000,
            0
        ));
        assert_eq!(h.state, HealthState::Hung);
    }

    // Sprint 24 P2 F1 — fresh-heartbeat-PTY-silent classifier tests.
    // Catches stuck agents in tight MCP loops (heartbeat refreshing but
    // PTY producing no output) that would otherwise misclassify as IdleLong.

    #[test]
    fn classifier_returns_hung_on_fresh_heartbeat_pty_silent() {
        // Stuck-agent scenario: agent calling MCP tools (heartbeat fresh)
        // but producing no PTY output (silent past threshold). Classifier
        // must return Hung (escalation-worthy), NOT IdleLong.
        let mut h = HealthTracker::new();
        let now = crate::daemon::heartbeat_pair::now_ms();
        // Heartbeat very recent (1s ago), no input pending, PTY silent 180s.
        let result = h.check_hang(
            AgentState::Ready,
            Duration::from_secs(180), // > 120s threshold
            Duration::from_secs(0),   // F9: productive-silence — recent
            0,                        // no input pending
            now - 1_000,              // heartbeat 1s ago (fresh)
        );
        assert!(
            result,
            "heartbeat fresh + PTY silent → Hung (F1 cross-check)"
        );
        assert_eq!(h.state, HealthState::Hung);
    }

    #[test]
    fn classifier_returns_idle_long_on_normal_idle_stale_heartbeat() {
        // Regression: pure idle with stale heartbeat (older than silence
        // window). Must still classify as IdleLong, not Hung.
        let mut h = HealthTracker::new();
        // Heartbeat 300s ago (stale — older than 180s silence window).
        let now = crate::daemon::heartbeat_pair::now_ms();
        let result = h.check_hang(
            AgentState::Ready,
            Duration::from_secs(180),
            Duration::from_secs(0), // F9: productive-silence — recent
            0,                      // no input pending
            now - 300_000,          // heartbeat 300s ago (stale)
        );
        assert!(
            !result,
            "stale heartbeat + no input → IdleLong (no escalation)"
        );
        assert_eq!(h.state, HealthState::IdleLong);
    }

    // -----------------------------------------------------------------------
    // F9 productive-output gate tests (#685 sub-task 4, decision
    // d-20260513235514013631-0). Pins the dual-path contract:
    //   - Default (env var unset): productive-silence path produces telemetry
    //     but does NOT change Hung classification.
    //   - Activated (AGEND_PRODUCTIVE_GATE=1): productive-silence-exceeded +
    //     silent-NOT-exceeded triggers Hung.
    //   - Existing silent path unchanged in both modes (no regression on
    //     #659 silent-stuck detection).
    // Tests must serialise on the env var because Rust tests share process
    // env. Use a single Once-style mutex to avoid flake.
    // -----------------------------------------------------------------------

    /// Run `f` with `AGEND_PRODUCTIVE_GATE` env var set per `active`,
    /// restoring the prior value on return.
    ///
    /// **Mirror copy** of `tests/common/env_gate.rs::with_f9_gate`. Unit
    /// tests cannot directly import from `tests/common/`; the helper is
    /// duplicated to enable both unit and integration test reuse. Sub-task
    /// 5 decision `d-20260514015214320625-1` §1.D accepted the ~15 LOC
    /// duplication over exposing `pub mod test_util` in production code.
    /// Keep in lock-step with the integration-test copy.
    fn with_f9_gate<R>(active: bool, f: impl FnOnce() -> R) -> R {
        // Tests touch a shared process-wide env var — serialise via a
        // function-scoped mutex so parallel test threads don't race.
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let lock = LOCK.get_or_init(|| Mutex::new(()));
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("AGEND_PRODUCTIVE_GATE").ok();
        // SAFETY: this is a test-only helper; set_var/remove_var are
        // process-global mutations and we serialise with the mutex above.
        // The unsafe block annotation matches Rust 1.84+ semantics where
        // env mutations are wrapped in unsafe. On older toolchains the
        // wrapping is a no-op syntactically.
        unsafe {
            if active {
                std::env::set_var("AGEND_PRODUCTIVE_GATE", "1");
            } else {
                std::env::remove_var("AGEND_PRODUCTIVE_GATE");
            }
        }
        let result = f();
        unsafe {
            match prior {
                Some(v) => std::env::set_var("AGEND_PRODUCTIVE_GATE", v),
                None => std::env::remove_var("AGEND_PRODUCTIVE_GATE"),
            }
        }
        result
    }

    #[test]
    fn f9_default_shadow_does_not_classify_hung_on_productive_silence_alone() {
        // Default mode (env var unset): productive-silence above threshold
        // but silent below threshold → NO Hung classification (shadow only).
        // Pins the "additive, no regression" contract.
        with_f9_gate(false, || {
            let mut h = HealthTracker::new();
            let result = h.check_hang(
                AgentState::Thinking,
                Duration::from_secs(60),  // silent < 600s threshold
                Duration::from_secs(700), // silent_productive > 600s
                0,
                0,
            );
            assert!(
                !result,
                "shadow-mode must not flag Hung on productive-only path"
            );
            assert_ne!(
                h.state,
                HealthState::Hung,
                "shadow-mode must not mutate state to Hung"
            );
        });
    }

    #[test]
    fn f9_activated_classifies_hung_on_productive_silence_exceeded() {
        // Activated mode: productive-silence above threshold + silent
        // below threshold → Hung. This is the F9 grey-failure capture:
        // 1-byte spinner output keeps silent low while no real work
        // happens (silent_productive grows).
        with_f9_gate(true, || {
            let mut h = HealthTracker::new();
            let result = h.check_hang(
                AgentState::Thinking,
                Duration::from_secs(60),  // silent < 600s threshold
                Duration::from_secs(700), // silent_productive > 600s
                10_000,                   // input pending past heartbeat
                0,
            );
            assert!(
                result,
                "activated F9 gate flags Hung when productive-silence exceeds"
            );
            assert_eq!(h.state, HealthState::Hung);
        });
    }

    #[test]
    fn f9_does_not_regress_silent_path() {
        // Regression guard: when silent_productive is recent (any-output
        // path triggers Hung) the existing silent-side classification
        // path must still fire identically. F9 is strictly additive.
        with_f9_gate(true, || {
            let mut h = HealthTracker::new();
            // silent path exceeds; productive-silence is fresh.
            let result = h.check_hang(
                AgentState::Thinking,
                Duration::from_secs(700), // silent > 600s threshold
                Duration::from_secs(0),   // silent_productive recent
                10_000,
                0,
            );
            assert!(result, "silent path must still trigger Hung");
            assert_eq!(h.state, HealthState::Hung);
        });
    }
}
