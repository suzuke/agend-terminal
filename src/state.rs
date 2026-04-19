//! Agent state detection via PTY output pattern matching.
//!
//! Detection runs against the current **vterm screen text** (caller supplies
//! it via `feed()`), not an accumulated byte buffer. Pattern hits therefore
//! reflect what the user would currently see on screen, so dismissing an
//! interactive prompt (e.g. codex update menu) drops the matching text from
//! the grid and the next `feed()` re-evaluates to the underlying Ready state
//! without stale-buffer lag.
//!
//! Hysteresis: error states instant, active 2s, passive 5s.
//!
//! Hash-based dedup in `feed()`: if the screen text is identical to the
//! previous snapshot, we skip both the silence-timer bump and pattern
//! detection. This keeps invisible terminal chatter (cursor blinks, etc.)
//! from resetting timers used by hang/awaiting detection.

use crate::backend::Backend;
use regex::Regex;
use serde::Serialize;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

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
    /// Hash of the last screen text fed to `feed()`. `None` before the first
    /// call. Used to skip re-detection when the screen hasn't changed —
    /// crucial for not resetting `last_output` on cursor-blink noise.
    last_screen_hash: Option<u64>,
    patterns: Option<StatePatterns>,
}

fn hash_screen(text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

impl StateTracker {
    pub fn new(backend: Option<&Backend>) -> Self {
        Self {
            current: AgentState::Starting,
            since: Instant::now(),
            last_output: Instant::now(),
            last_screen_hash: None,
            patterns: backend.map(StatePatterns::for_backend),
        }
    }

    /// Feed the current vterm screen text (ANSI already resolved by the
    /// terminal emulator — caller passes plain text rows).
    ///
    /// If the screen is identical to the previous snapshot (same hash) we
    /// skip: no silence-timer bump, no re-detection. This lets invisible
    /// terminal chatter (cursor blinks, bell, etc.) pass through without
    /// masking hang/awaiting detection.
    ///
    /// When the screen does change, `last_output` is bumped and pattern
    /// detection runs against the full screen text. Because we always feed
    /// the current grid (not an accumulation), dismissed prompts drop out of
    /// detection on the next call — no stale-buffer lag.
    pub fn feed(&mut self, screen_text: &str) {
        let hash = hash_screen(screen_text);
        if self.last_screen_hash == Some(hash) {
            return;
        }
        self.last_screen_hash = Some(hash);
        self.last_output = Instant::now();

        if let Some(ref patterns) = self.patterns {
            if let Some(detected) = patterns.detect(screen_text) {
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
    }

    /// Force state to AwaitingOperator when startup stalls on an unexpected
    /// interactive prompt. Only fires from `Starting` — Ready-state stalls
    /// are caught by pattern-based detection (see `InteractivePrompt` once
    /// added; until then they're missed, which is acceptable for the
    /// time-based fallback role).
    ///
    /// Once the operator unblocks the stall and the ready pattern matches
    /// fresh screen content, `transition()` lifts the state (Ready prio >
    /// AwaitingOperator prio → higher always wins).
    pub fn set_awaiting_operator(&mut self) {
        if matches!(self.current, AgentState::Starting) {
            self.current = AgentState::AwaitingOperator;
            self.since = Instant::now();
        }
    }

    fn transition(&mut self, new_state: AgentState) {
        if new_state == self.current {
            return;
        }

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
    fn dismissed_prompt_clears_on_next_feed() {
        // Screen-based detection: once the prompt text leaves the current
        // screen, the next feed re-evaluates without lag — the replacement
        // for the old state_buf-clearing behavior.
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Starting, 0);
        // Simulate the update-menu screen
        t.feed("Update available! 0.120.0 -> 0.121.0\n1. Update now\n2. Skip");
        // Then the banner re-renders after dismissal
        t.feed("bypass permissions");
        assert_eq!(t.get_state(), AgentState::Ready);
    }

    #[test]
    fn unchanged_screen_does_not_reset_last_output() {
        // Hash dedup: feeding the same screen twice must not bump
        // last_output (used by hang/awaiting-operator predicates).
        let mut t = StateTracker::new(Some(&Backend::ClaudeCode));
        t.feed("hello world");
        let first = t.last_output;
        std::thread::sleep(Duration::from_millis(20));
        t.feed("hello world");
        assert_eq!(t.last_output, first, "identical screen must not bump last_output");
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
    fn set_restarting_transitions_state() {
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Thinking, 5);
        t.set_restarting();
        assert_eq!(t.get_state(), AgentState::Restarting);
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
    fn set_awaiting_operator_noop_from_non_starting() {
        // Only Starting transitions. All other states (including Ready) are
        // no-ops so late-firing tick-loop detections can't corrupt a healthy
        // mid-task agent. Known interactive prompts from Ready are caught
        // by pattern-based detection, not this time-based fallback.
        for s in [
            AgentState::Ready,
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

    // ── Full pipeline: PTY bytes → VTerm → screen → StateTracker ────────
    //
    // Exercises the production path `agent::pty_read_loop` takes: push
    // raw bytes (with ANSI escapes) through the vterm, pull tail_lines of
    // the screen, feed to state. Without these, unit tests can drift from
    // how detection actually behaves once vterm rendering is involved —
    // wrapped lines, cleared screens, scroll-off, etc.

    use crate::vterm::VTerm;

    /// Drive one full PTY cycle: process bytes, snapshot screen, feed state.
    fn drive(vterm: &mut VTerm, state: &mut StateTracker, bytes: &[u8]) {
        vterm.process(bytes);
        let rows = vterm.rows() as usize;
        let screen = vterm.tail_lines(rows);
        state.feed(&screen);
    }

    #[test]
    fn pipeline_claude_ready_via_vterm() {
        let mut vt = VTerm::new(80, 24);
        let mut st = StateTracker::new(Some(&Backend::ClaudeCode));
        // Bytes include ANSI colors — vterm must resolve them so screen
        // text is plain and pattern can match.
        drive(
            &mut vt,
            &mut st,
            b"\x1b[1;32mClaude Code\x1b[0m ready (bypass permissions mode)\r\n",
        );
        assert_eq!(st.get_state(), AgentState::Ready);
    }

    #[test]
    fn pipeline_codex_ready_via_vterm() {
        let mut vt = VTerm::new(80, 24);
        let mut st = StateTracker::new(Some(&Backend::Codex));
        drive(&mut vt, &mut st, b"\x1b[1mOpenAI Codex\x1b[0m v0.120.0\r\n");
        assert_eq!(st.get_state(), AgentState::Ready);
    }

    #[test]
    fn pipeline_dismiss_drops_stale_pattern() {
        // Regression for the bug this whole refactor exists to fix: an
        // interactive prompt shows up, pattern matches; operator dismisses;
        // screen re-renders without the prompt text; state must re-evaluate
        // and fall back — NOT stay wedged on the stale buffered text.
        //
        // Uses codex-style flow: usage-limit banner (high priority) appears,
        // then a clear-screen + ready banner re-renders.
        let mut vt = VTerm::new(80, 24);
        let mut st = StateTracker::new(Some(&Backend::Codex));

        // Step 1: screen shows usage limit
        drive(
            &mut vt,
            &mut st,
            b"You've hit your usage limit. Try again at 10:00 AM.\r\n",
        );
        assert_eq!(st.get_state(), AgentState::UsageLimit);

        // Step 2: clear screen + fresh banner (simulates user re-auth / reset)
        // \x1b[2J clears screen, \x1b[H moves cursor home.
        // Advance `since` so the lower-priority Ready transition clears the
        // active-hold gate (UsageLimit is an error state; leaving requires
        // active hold 2s).
        st.since = std::time::Instant::now() - std::time::Duration::from_secs(3);
        drive(&mut vt, &mut st, b"\x1b[2J\x1b[HOpenAI Codex v0.120.0\r\n");
        assert_eq!(
            st.get_state(),
            AgentState::Ready,
            "after screen clear + ready banner, stale UsageLimit must release"
        );
    }

    #[test]
    fn pipeline_screen_unchanged_preserves_silence_timer() {
        // Cursor-blink-like bytes (show/hide cursor) must not bump
        // last_output since they leave the rendered grid unchanged.
        let mut vt = VTerm::new(80, 24);
        let mut st = StateTracker::new(Some(&Backend::Codex));
        drive(&mut vt, &mut st, b"OpenAI Codex v0.120.0\r\n");
        let before = st.last_output;
        std::thread::sleep(Duration::from_millis(20));
        // Cursor hide/show — visible grid unchanged.
        drive(&mut vt, &mut st, b"\x1b[?25l\x1b[?25h");
        assert_eq!(
            st.last_output, before,
            "cursor-visibility toggles must not reset silence timer"
        );
    }

    #[test]
    fn pipeline_usage_limit_instant_from_idle() {
        // Claude flow: agent sitting at idle prompt, then rate-limit burst
        // arrives. Error state must win immediately (no hysteresis).
        let mut vt = VTerm::new(80, 24);
        let mut st = StateTracker::new(Some(&Backend::ClaudeCode));
        drive(&mut vt, &mut st, b"bypass permissions\r\n> ready\r\n\xe2\x9d\xaf");
        assert!(matches!(
            st.get_state(),
            AgentState::Ready | AgentState::Idle
        ));
        drive(&mut vt, &mut st, b"\r\n\x1b[31m429 rate limit exceeded\x1b[0m\r\n");
        assert_eq!(st.get_state(), AgentState::RateLimit);
    }

    #[test]
    fn pipeline_opencode_ready_prompt_resolves_to_idle() {
        // OpenCode's input prompt "Ask anything" matches BOTH the Idle
        // pattern (listed first) and the Ready pattern, so first-match
        // returns Idle. This reflects the backend's pattern table as
        // currently configured — noted here so future tweaks don't
        // accidentally flip to Ready without intent. Semantically OK:
        // Idle is "waiting for input at an alive prompt", and that's
        // what the user sees when opencode is ready.
        let mut vt = VTerm::new(80, 24);
        let mut st = StateTracker::new(Some(&Backend::OpenCode));
        drive(
            &mut vt,
            &mut st,
            b"Ask anything   \xe2\x8c\x85 tab agents\r\n",
        );
        assert_eq!(st.get_state(), AgentState::Idle);
    }
}
