//! Agent state detection via PTY output pattern matching.
//!
//! Dual buffer: ready_buf (8KB, one-time) + state_buf (2KB, rolling).
//! Hysteresis: error states instant, active 2s, passive 5s.

use crate::backend::Backend;
use regex::Regex;
use serde::Serialize;
use std::time::{Duration, Instant};

const STATE_BUF_MAX: usize = 2048;

/// Agent runtime state, ordered by priority (highest last).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub enum AgentState {
    Starting,
    Hang,
    /// Startup stalled on unexpected interactive prompt (e.g. codex update menu).
    /// Entered when Starting + stdout silent > threshold. Operator reply is
    /// routed as raw PTY keystrokes (not inbox-wrapped) via INJECT_RAW.
    AwaitingOperator,
    Ready,
    Idle,
    ToolUse,
    Thinking,
    PermissionPrompt,
    ContextFull,
    RateLimit,
    UsageLimit,
    AuthError,
    ApiError,
    Crashed,
    Restarting,
}

impl AgentState {
    /// Priority: higher = more urgent. Error states > prompts > active > passive.
    pub fn priority(self) -> u8 {
        match self {
            Self::Starting => 0,
            Self::Hang => 1,
            Self::AwaitingOperator => 2,
            Self::Ready => 3,
            Self::Idle => 4,
            Self::ToolUse => 5,
            Self::Thinking => 6,
            Self::PermissionPrompt => 7,
            Self::ContextFull => 8,
            Self::RateLimit => 9,
            Self::UsageLimit => 10,
            Self::AuthError => 11,
            Self::ApiError => 12,
            Self::Crashed => 13,
            Self::Restarting => 14,
        }
    }

    /// Is this an error state (instant transition, no hysteresis)?
    pub fn is_error(self) -> bool {
        self.priority() >= Self::ContextFull.priority()
    }

    /// Is this agent unavailable (restarting or crashed)?
    pub fn is_unavailable(self) -> bool {
        matches!(self, Self::Crashed | Self::Restarting)
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Hang => "hang",
            Self::AwaitingOperator => "awaiting_operator",
            Self::Ready => "ready",
            Self::Idle => "idle",
            Self::ToolUse => "tool_use",
            Self::Thinking => "thinking",
            Self::PermissionPrompt => "permission",
            Self::ContextFull => "context_full",
            Self::RateLimit => "rate_limit",
            Self::UsageLimit => "usage_limit",
            Self::AuthError => "auth_error",
            Self::ApiError => "api_error",
            Self::Crashed => "crashed",
            Self::Restarting => "restarting",
        }
    }
}

/// Compiled patterns for one backend.
pub struct StatePatterns {
    /// (state, regex) pairs in priority order (highest priority first).
    patterns: Vec<(AgentState, Regex)>,
}

impl StatePatterns {
    /// Pattern sources: [実測] = verified from real capture, [文件] = from docs/source, [推測] = estimated
    /// Tested versions: Claude v2.1.89, Codex v0.118.0, OpenCode v1.4.0, Gemini v0.37.1
    pub fn for_backend(backend: &Backend) -> Self {
        let patterns = match backend {
            // Claude Code v2.1.89
            Backend::ClaudeCode => vec![
                // [docs] Claude Code SDK error handling
                (
                    AgentState::AuthError,
                    r"API key|authentication failed|unauthorized",
                ),
                // [docs] SDK retry logic for 429/overloaded
                (AgentState::RateLimit, r"overloaded|rate.?limit|429"),
                // [docs] Auto-compaction on context limit
                (
                    AgentState::ContextFull,
                    r"compacting context|context.*(full|limit)",
                ),
                // [estimated] Ink select component for permissions
                (
                    AgentState::PermissionPrompt,
                    r"Allow once|Allow always|approve",
                ),
                // [estimated] Ink render during processing
                (AgentState::Thinking, r"Thinking"),
                // [estimated] Tool name with spinner/status icon prefix
                (
                    AgentState::ToolUse,
                    r"[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏✓●].*(Read|Bash|Edit|Write|Grep|Glob)",
                ),
                // [measured] Prompt symbol in idle state
                (AgentState::Idle, r"❯"),
                // [measured] Shown after startup with --dangerously-skip-permissions
                (AgentState::Ready, r"bypass permissions"),
            ],
            // Kiro CLI (version TBD)
            Backend::KiroCli => vec![
                // [docs] Kiro auth error messages
                (
                    AgentState::AuthError,
                    r"Not authenticated|AccessDenied|denied access",
                ),
                // [docs] AWS quota errors
                (
                    AgentState::UsageLimit,
                    r"ServiceQuotaExceeded|InsufficientModelCapacity",
                ),
                // [docs] HTTP 429 handling
                (
                    AgentState::RateLimit,
                    r"Too Many Requests|ThrottlingError|429",
                ),
                // [docs] Context overflow triggers compaction
                (AgentState::ContextFull, r"context window overflow|/compact"),
                // [docs] Trust-based permission system
                (AgentState::PermissionPrompt, r"Allow this action|y/n/t"),
                // [docs] Processing indicator
                (AgentState::Thinking, r"Generating"),
                // [docs] Tool names in output
                (AgentState::ToolUse, r"execute_bash|fs_read|fs_write"),
                // [measured] Idle prompt
                (
                    AgentState::Idle,
                    r"\d+%\s*$|ask a question or describe a task",
                ),
                // [measured] Trust dialog completion / ready state
                (AgentState::Ready, r"Trust All Tools active|/quit to exit"),
            ],
            // Codex v0.118.0
            Backend::Codex => vec![
                // [docs] Requires OPENAI_API_KEY env
                (AgentState::AuthError, r"OPENAI_API_KEY|api.?key"),
                // [実測 v0.118.0] Quota exhausted message
                (AgentState::UsageLimit, r"hit your usage limit|try again at"),
                // [docs] HTTP 429 handling
                (AgentState::RateLimit, r"rate.?limit|429"),
                // [docs] Context overflow error
                (AgentState::ContextFull, r"ContextOverflow"),
                // [docs] Permission approval flow
                (
                    AgentState::PermissionPrompt,
                    r"Request approval|approve|deny",
                ),
                // [estimated] Processing state
                (AgentState::Thinking, r"Thinking"),
                // [estimated] Patch tool
                (AgentState::ToolUse, r"apply_patch"),
                // [measured] Prompt symbol + model info in status
                (AgentState::Idle, r"›"),
                // [measured] Version + model display
                (AgentState::Ready, r"OpenAI Codex|gpt-.*left"),
            ],
            // OpenCode v1.4.0
            Backend::OpenCode => vec![
                // [docs] HTTP error handling
                (AgentState::RateLimit, r"rate.?limit|429"),
                // [docs] Context overflow
                (AgentState::ContextFull, r"ContextOverflow"),
                // [docs] Permission UI
                (
                    AgentState::PermissionPrompt,
                    r"Permission required|Allow once|Allow always",
                ),
                // [docs] Busy text
                (AgentState::Thinking, r"Working"),
                // [measured] Update dialog that may block
                (
                    AgentState::PermissionPrompt,
                    r"Update Available|Skip\s+Confirm",
                ),
                // [measured] Input prompt text
                (AgentState::Idle, r"Ask anything"),
                // [measured] Ready state with keybinding hints
                (AgentState::Ready, r"Ask anything|tab agents"),
            ],
            // Gemini CLI v0.37.1
            Backend::Gemini => vec![
                // [docs] OAuth errors from API
                (
                    AgentState::AuthError,
                    r"OAuth not authenticated|OAuth expired|UNAUTHENTICATED|check API key",
                ),
                // [docs] Usage limit messages
                (
                    AgentState::UsageLimit,
                    r"Usage limit reached|Access resets at",
                ),
                // [docs] API resource exhaustion
                (AgentState::RateLimit, r"RESOURCE_EXHAUSTED|429"),
                // [docs] Token/quota limit
                (AgentState::ContextFull, r"quota.*exceeded|token.*limit"),
                // [docs] Permission select options
                (
                    AgentState::PermissionPrompt,
                    r"Allow once|Allow for this session|suggest changes",
                ),
                // [estimated] Processing indicator
                (AgentState::Thinking, r"Thinking"),
                // [estimated] MCP tool execution
                (AgentState::ToolUse, r"tool.*call|MCP.*tool"),
                // [measured] Input prompt text
                (AgentState::Idle, r"Type your message"),
                // [measured] Full ready prompt + YOLO mode
                (AgentState::Ready, r"Type your message|YOLO"),
            ],
            // Non-preset backends have no state-detection heuristics — pane
            // stays in whatever state the generic output pipeline sets. These
            // variants should never reach here in normal flow (state machine
            // is gated on preset variants today), but keep the match
            // exhaustive so we fail loudly if a caller does route them here.
            Backend::Shell | Backend::Raw(_) => vec![],
        };

        let compiled: Vec<_> = patterns
            .into_iter()
            .filter_map(|(state, pat)| match Regex::new(pat) {
                Ok(re) => Some((state, re)),
                Err(err) => {
                    tracing::warn!("invalid state pattern: {pat}: {err}");
                    None
                }
            })
            .collect();

        Self { patterns: compiled }
    }

    /// Match against state buffer, return highest-priority matching state.
    pub fn detect(&self, text: &str) -> Option<AgentState> {
        // Patterns are already in priority order (highest first)
        for (state, re) in &self.patterns {
            if re.is_match(text) {
                return Some(*state);
            }
        }
        None
    }
}

/// Tracks state with hysteresis.
pub struct StateTracker {
    pub current: AgentState,
    pub(crate) since: Instant,
    pub last_output: Instant,
    pub(crate) state_buf: String,
    patterns: Option<StatePatterns>,
}

impl StateTracker {
    pub fn new(backend: Option<&Backend>) -> Self {
        Self {
            current: AgentState::Starting,
            since: Instant::now(),
            last_output: Instant::now(),
            state_buf: String::with_capacity(STATE_BUF_MAX),
            patterns: backend.map(StatePatterns::for_backend),
        }
    }

    /// Feed new output data (already ANSI-stripped).
    pub fn feed(&mut self, stripped_text: &str) {
        self.last_output = Instant::now();
        self.state_buf.push_str(stripped_text);

        // Truncate to last STATE_BUF_MAX chars
        if self.state_buf.len() > STATE_BUF_MAX {
            let mut start = self.state_buf.len() - STATE_BUF_MAX;
            while !self.state_buf.is_char_boundary(start) {
                start += 1;
            }
            self.state_buf = self.state_buf[start..].to_string();
        }

        // Detect new state
        if let Some(ref patterns) = self.patterns {
            if let Some(detected) = patterns.detect(&self.state_buf) {
                self.transition(detected);
            }
        }
    }

    /// Get current state.
    pub fn get_state(&self) -> AgentState {
        self.current
    }

    /// Force state to Restarting (called by reaper on crash).
    pub fn set_restarting(&mut self) {
        self.current = AgentState::Restarting;
        self.since = Instant::now();
        self.state_buf.clear();
    }

    /// Force state to AwaitingOperator when startup stalls on an unexpected
    /// interactive prompt (codex update menu, claude trust prompt, etc.).
    ///
    /// Takes effect from `Starting` OR `Ready`. The Ready case covers backends
    /// whose ready_pattern matches the startup banner that also contains the
    /// interactive prompt (codex: `ready_pattern: "OpenAI Codex|›"` matches
    /// the `› 1. Update now` menu). The supervisor's predicate gates Ready
    /// transitions with a grace window so long-running idle agents aren't
    /// mis-flagged.
    ///
    /// Once the operator replies and the ready pattern matches against
    /// post-stall output, the usual `transition()` path lifts the state out
    /// (AwaitingOperator prio < Ready prio → higher always wins).
    pub fn set_awaiting_operator(&mut self) {
        if matches!(self.current, AgentState::Starting | AgentState::Ready) {
            self.current = AgentState::AwaitingOperator;
            self.since = Instant::now();
            // Fresh buffer: post-stall output (after the operator types)
            // must feed pattern detection without stale banner text.
            self.state_buf.clear();
        }
    }

    fn transition(&mut self, new_state: AgentState) {
        if new_state == self.current {
            return;
        }

        let old_state = self.current;

        // Error states: instant transition (no hysteresis)
        if new_state.is_error() {
            self.current = new_state;
            self.since = Instant::now();
        } else {
            // Higher priority than current: transition if current held > min duration
            let held = self.since.elapsed();
            let min_hold = if self.current.priority() <= AgentState::Idle.priority() {
                Duration::from_secs(5) // Passive states: 5s
            } else {
                Duration::from_secs(2) // Active states: 2s
            };

            // Higher priority always transitions
            if new_state.priority() > self.current.priority() {
                self.current = new_state;
                self.since = Instant::now();
            } else if held >= min_hold {
                // Lower priority only after min hold
                self.current = new_state;
                self.since = Instant::now();
            }
        }

        // Clear state_buf on actual transition to avoid stale pattern matches
        if self.current != old_state {
            self.state_buf.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::HealthTracker;

    /// Create a tracker with a given current state and elapsed time since that state began.
    fn tracker_at(backend: &Backend, state: AgentState, elapsed_secs: u64) -> StateTracker {
        let mut t = StateTracker::new(Some(backend));
        t.current = state;
        t.since = Instant::now() - Duration::from_secs(elapsed_secs);
        t
    }

    // ── P0: Core behavior ───────────────────────────────────────────────

    #[test]
    #[allow(clippy::unwrap_used)]
    fn error_state_instant_transition() {
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
        // Feed a rate limit pattern — error state should transition instantly
        t.feed("429 rate limit exceeded");
        assert_eq!(t.get_state(), AgentState::RateLimit);
    }

    #[test]
    fn active_to_passive_needs_hold() {
        let backend = Backend::ClaudeCode;

        // Thinking held for only 1s — transition to Idle should NOT happen
        let mut t = tracker_at(&backend, AgentState::Thinking, 1);
        t.transition(AgentState::Idle);
        assert_eq!(t.get_state(), AgentState::Thinking);

        // Thinking held for 3s (>= 2s active hold) — transition to Idle SHOULD happen
        let mut t = tracker_at(&backend, AgentState::Thinking, 3);
        t.transition(AgentState::Idle);
        assert_eq!(t.get_state(), AgentState::Idle);
    }

    #[test]
    fn passive_to_passive_needs_5s() {
        let backend = Backend::ClaudeCode;

        // Idle(3) → Ready(2): lower priority, passive hold = 5s
        // Idle held for 3s — should NOT transition
        let mut t = tracker_at(&backend, AgentState::Idle, 3);
        t.transition(AgentState::Ready);
        assert_eq!(t.get_state(), AgentState::Idle);

        // Idle held for 6s (>= 5s passive hold) — SHOULD transition
        let mut t = tracker_at(&backend, AgentState::Idle, 6);
        t.transition(AgentState::Ready);
        assert_eq!(t.get_state(), AgentState::Ready);
    }

    #[test]
    fn higher_priority_instant() {
        let backend = Backend::ClaudeCode;

        // Idle → Thinking: higher priority, should transition immediately even at 0s
        let mut t = tracker_at(&backend, AgentState::Idle, 0);
        t.transition(AgentState::Thinking);
        assert_eq!(t.get_state(), AgentState::Thinking);
    }

    #[test]
    fn error_recovery_needs_hold() {
        let backend = Backend::ClaudeCode;

        // RateLimit held for 1s — transition to Idle (lower priority) needs hold time
        // RateLimit priority > Idle priority, and RateLimit is active (priority > Idle),
        // so 2s active hold applies
        let mut t = tracker_at(&backend, AgentState::RateLimit, 1);
        t.transition(AgentState::Idle);
        assert_eq!(t.get_state(), AgentState::RateLimit);

        // RateLimit held for 3s (>= 2s) — should transition
        let mut t = tracker_at(&backend, AgentState::RateLimit, 3);
        t.transition(AgentState::Idle);
        assert_eq!(t.get_state(), AgentState::Idle);
    }

    // ── P1: Edge cases ──────────────────────────────────────────────────

    #[test]
    fn state_buf_clears_on_transition() {
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
        // Feed thinking pattern — Thinking > Idle so instant transition
        t.feed("Thinking");
        assert_eq!(t.get_state(), AgentState::Thinking);
        // state_buf should be cleared after transition
        assert!(t.state_buf.is_empty());
    }

    #[test]
    fn same_state_no_timer_reset() {
        let backend = Backend::ClaudeCode;
        let mut t = tracker_at(&backend, AgentState::Thinking, 10);
        let since_before = t.since;
        // Re-transition to same state — should be no-op
        t.transition(AgentState::Thinking);
        assert_eq!(t.since, since_before);
    }

    #[test]
    fn starting_hang_120s() {
        let mut h = HealthTracker::new();
        assert!(!h.check_hang(AgentState::Starting, Duration::from_secs(119)));
        assert!(h.check_hang(AgentState::Starting, Duration::from_secs(121)));
    }

    #[test]
    fn idle_never_hangs() {
        let mut h = HealthTracker::new();
        // Even with 10000s of silence, Idle should never be considered hung.
        assert!(!h.check_hang(AgentState::Idle, Duration::from_secs(10_000)));
    }

    #[test]
    fn thinking_hang_600s() {
        let mut h = HealthTracker::new();
        assert!(!h.check_hang(AgentState::Thinking, Duration::from_secs(599)));
        assert!(h.check_hang(AgentState::Thinking, Duration::from_secs(601)));
    }

    // ── P2: Pattern matching ────────────────────────────────────────────

    #[test]
    fn claude_tooluse_spinner_match() {
        let patterns = StatePatterns::for_backend(&Backend::ClaudeCode);
        let detected = patterns.detect("⠋Read file.txt");
        assert_eq!(detected, Some(AgentState::ToolUse));
    }

    #[test]
    fn pattern_does_not_cross_backends() {
        // Claude's "❯" idle pattern should not match on Gemini tracker
        let gemini_patterns = StatePatterns::for_backend(&Backend::Gemini);
        let detected = gemini_patterns.detect("❯");
        assert_ne!(detected, Some(AgentState::Idle));
    }

    #[test]
    fn empty_input_no_change() {
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Starting, 0);
        t.feed("");
        assert_eq!(t.get_state(), AgentState::Starting);
    }

    #[test]
    fn ready_detection() {
        let mut t = StateTracker::new(Some(&Backend::ClaudeCode));
        t.feed("bypass permissions");
        assert_eq!(t.get_state(), AgentState::Ready);
    }

    #[test]
    fn idle_detection() {
        let mut t = StateTracker::new(Some(&Backend::ClaudeCode));
        // First get to Ready so that Idle (lower prio than Starting) can be tested
        // Starting → Ready (higher prio) is instant
        t.feed("bypass permissions");
        assert_eq!(t.get_state(), AgentState::Ready);
        // Now wait enough time for passive hold (5s) then feed idle pattern
        t.since = Instant::now() - Duration::from_secs(6);
        t.feed("❯");
        assert_eq!(t.get_state(), AgentState::Idle);
    }

    // ── Additional edge cases ───────────────────────────────────────────

    #[test]
    fn context_full_instant_transition() {
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Thinking, 0);
        t.feed("compacting context");
        assert_eq!(t.get_state(), AgentState::ContextFull);
    }

    #[test]
    fn auth_error_instant_transition() {
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
        t.feed("API key invalid");
        assert_eq!(t.get_state(), AgentState::AuthError);
    }

    #[test]
    fn permission_prompt_higher_than_thinking() {
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Thinking, 0);
        // PermissionPrompt (priority 6) > Thinking (priority 5) — instant
        t.feed("Allow once");
        assert_eq!(t.get_state(), AgentState::PermissionPrompt);
    }

    #[test]
    fn set_restarting_clears_buf() {
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Thinking, 5);
        t.state_buf.push_str("some old data");
        t.set_restarting();
        assert_eq!(t.get_state(), AgentState::Restarting);
        assert!(t.state_buf.is_empty());
    }

    #[test]
    fn state_buf_truncates_to_max() {
        let mut t = StateTracker::new(Some(&Backend::ClaudeCode));
        // Feed more than STATE_BUF_MAX bytes
        let big_input = "x".repeat(STATE_BUF_MAX + 500);
        t.feed(&big_input);
        assert!(t.state_buf.len() <= STATE_BUF_MAX);
    }

    #[test]
    fn error_state_is_error() {
        assert!(AgentState::ContextFull.is_error());
        assert!(AgentState::RateLimit.is_error());
        assert!(AgentState::UsageLimit.is_error());
        assert!(AgentState::AuthError.is_error());
        assert!(AgentState::ApiError.is_error());
        assert!(AgentState::Crashed.is_error());
        assert!(AgentState::Restarting.is_error());
        assert!(!AgentState::Thinking.is_error());
        assert!(!AgentState::Idle.is_error());
        assert!(!AgentState::Starting.is_error());
    }

    #[test]
    fn awaiting_operator_not_error() {
        // AwaitingOperator means "needs human keystrokes", not a failure mode.
        assert!(!AgentState::AwaitingOperator.is_error());
        assert!(!AgentState::AwaitingOperator.is_unavailable());
    }

    #[test]
    fn awaiting_operator_priority_between_hang_and_ready() {
        // Hang < AwaitingOperator < Ready so it preempts Starting/Hang in
        // tab-bar highest-priority display but doesn't outrank real activity.
        assert!(AgentState::Hang.priority() < AgentState::AwaitingOperator.priority());
        assert!(AgentState::AwaitingOperator.priority() < AgentState::Ready.priority());
    }

    #[test]
    fn awaiting_operator_display_name() {
        assert_eq!(
            AgentState::AwaitingOperator.display_name(),
            "awaiting_operator"
        );
    }

    #[test]
    fn set_awaiting_operator_from_starting() {
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Starting, 5);
        t.set_awaiting_operator();
        assert_eq!(t.current, AgentState::AwaitingOperator);
    }

    #[test]
    fn set_awaiting_operator_from_ready() {
        // Ready must also transition: codex's ready_pattern matches the
        // startup banner that contains the update menu, so the agent reports
        // Ready while still blocked on keystrokes. The supervisor's predicate
        // gates Ready with a grace window to avoid flagging idle agents.
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Ready, 5);
        t.set_awaiting_operator();
        assert_eq!(t.current, AgentState::AwaitingOperator);
    }

    #[test]
    fn set_awaiting_operator_noop_from_other_states() {
        // Only Starting/Ready should transition; from any other state it's a
        // no-op so late-firing tick-loop detections can't corrupt a healthy
        // mid-task agent.
        for s in [
            AgentState::Idle,
            AgentState::Thinking,
            AgentState::ToolUse,
            AgentState::PermissionPrompt,
            AgentState::AwaitingOperator,
            AgentState::Crashed,
        ] {
            let mut t = tracker_at(&Backend::ClaudeCode, s, 10);
            t.set_awaiting_operator();
            assert_eq!(t.current, s, "state {:?} should be unchanged", s);
        }
    }

    #[test]
    fn ready_pattern_lifts_awaiting_operator() {
        // Once operator unblocks the stall and the ready banner fires,
        // transition() takes the usual higher-priority-wins path.
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Starting, 5);
        t.set_awaiting_operator();
        assert_eq!(t.current, AgentState::AwaitingOperator);
        t.feed("bypass permissions");
        assert_eq!(t.current, AgentState::Ready);
    }
}
