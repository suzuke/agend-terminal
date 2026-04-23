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
        }
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
    crash_times: VecDeque<Instant>,
    total_crashes: u32,
    max_retries: u32,
    last_notification: Option<Instant>,
    error_events: VecDeque<(Instant, AgentState)>,
    pub last_output: Instant,
    pub current_reason: Option<BlockedReason>,
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
        }
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
    pub fn check_hang(&mut self, agent_state: AgentState, silent: Duration) -> bool {
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

        let is_hang = match agent_state {
            AgentState::Idle => false, // Waiting for input
            AgentState::Starting => silent > Duration::from_secs(120),
            AgentState::Thinking | AgentState::ToolUse => silent > Duration::from_secs(600),
            _ => silent > Duration::from_secs(120),
        };

        if is_hang && self.state != HealthState::Hung {
            self.state = HealthState::Hung;
            return true; // First hang detection
        }
        if !is_hang && self.state == HealthState::Hung {
            self.state = HealthState::Healthy;
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
    #[allow(dead_code)]
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
    pub fn maybe_decay(&mut self) {
        if self.total_crashes == 0 {
            return;
        }
        let last_crash = match self.crash_times.back() {
            Some(t) => *t,
            None => return,
        };
        if last_crash.elapsed() >= STABILITY_WINDOW {
            self.total_crashes = self.total_crashes.saturating_sub(1);
            if self.total_crashes == 0 {
                self.crash_times.clear();
            }
            // Recover from Failed/Unstable if crashes decayed enough
            if self.total_crashes < DEFAULT_MAX_RETRIES && self.state == HealthState::Failed {
                self.state = HealthState::Recovering;
            }
            if self.total_crashes < 3 && self.state == HealthState::Unstable {
                self.state = HealthState::Healthy;
            }
        }
    }

    /// Reset health state (e.g., after manual restart).
    #[allow(dead_code)]
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
        assert!(!h.check_hang(AgentState::Idle, Duration::from_secs(300))); // Idle never hangs
    }

    #[test]
    fn test_hang_thinking_long_timeout() {
        let mut h = HealthTracker::new();
        assert!(!h.check_hang(AgentState::Thinking, Duration::from_secs(100))); // 100s < 600s
        assert!(h.check_hang(AgentState::Thinking, Duration::from_secs(700))); // 700s > 600s
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
        assert!(!h.check_hang(AgentState::Thinking, Duration::from_secs(700)));
        assert_ne!(h.state, HealthState::Hung);

        // Also test QuotaExceeded and AwaitingOperator
        h.clear_blocked_reason();
        h.set_blocked_reason(BlockedReason::QuotaExceeded);
        assert!(!h.check_hang(AgentState::Thinking, Duration::from_secs(700)));

        h.clear_blocked_reason();
        h.set_blocked_reason(BlockedReason::AwaitingOperator);
        assert!(!h.check_hang(AgentState::Thinking, Duration::from_secs(700)));

        // PermissionPrompt does NOT suppress hang check
        h.clear_blocked_reason();
        h.set_blocked_reason(BlockedReason::PermissionPrompt);
        assert!(h.check_hang(AgentState::Thinking, Duration::from_secs(700)));
    }

    #[test]
    fn test_clear_blocked_reason_resumes_hang_check() {
        let mut h = HealthTracker::new();
        h.set_blocked_reason(BlockedReason::RateLimit {
            retry_after_secs: None,
        });
        assert!(!h.check_hang(AgentState::Thinking, Duration::from_secs(700)));

        h.clear_blocked_reason();
        assert!(h.check_hang(AgentState::Thinking, Duration::from_secs(700)));
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
        let output = "Error: 429 overloaded";
        let reason = crate::state::classify_pty_output(&backend, output);
        assert!(reason.is_some(), "should classify as blocked");

        // Dry-run: do NOT call set_blocked_reason
        // (in production, daemon checks AGEND_WATCHDOG_DRY_RUN)
        assert!(h.current_reason.is_none(), "dry-run must not mutate health state");

        // check_hang should still fire (no reason set)
        assert!(h.check_hang(AgentState::Thinking, Duration::from_secs(700)));
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
        assert!(!h.check_hang(AgentState::Thinking, Duration::from_secs(700)));
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
        assert!(h.check_hang(AgentState::Thinking, Duration::from_secs(700)));
    }
}
