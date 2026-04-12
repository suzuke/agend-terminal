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
            Self::Ready => 2,
            Self::Idle => 3,
            Self::ToolUse => 4,
            Self::Thinking => 5,
            Self::PermissionPrompt => 6,
            Self::ContextFull => 7,
            Self::RateLimit => 8,
            Self::UsageLimit => 9,
            Self::AuthError => 10,
            Self::ApiError => 11,
            Self::Crashed => 12,
            Self::Restarting => 13,
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
                // [文件] Claude Code SDK error handling
                (
                    AgentState::AuthError,
                    r"API key|authentication failed|unauthorized",
                ),
                // [文件] SDK retry logic for 429/overloaded
                (AgentState::RateLimit, r"overloaded|rate.?limit|429"),
                // [文件] Auto-compaction on context limit
                (
                    AgentState::ContextFull,
                    r"compacting context|context.*(full|limit)",
                ),
                // [推測] Ink select component for permissions
                (
                    AgentState::PermissionPrompt,
                    r"Allow once|Allow always|approve",
                ),
                // [推測] Ink render during processing
                (AgentState::Thinking, r"Thinking"),
                // [推測] Tool name with spinner/status icon prefix
                (
                    AgentState::ToolUse,
                    r"[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏✓●].*(Read|Bash|Edit|Write|Grep|Glob)",
                ),
                // [実測] Prompt symbol in idle state
                (AgentState::Idle, r"❯"),
                // [実測] Shown after startup with --dangerously-skip-permissions
                (AgentState::Ready, r"bypass permissions"),
            ],
            // Kiro CLI (version TBD)
            Backend::KiroCli => vec![
                // [文件] Kiro auth error messages
                (
                    AgentState::AuthError,
                    r"Not authenticated|AccessDenied|denied access",
                ),
                // [文件] AWS quota errors
                (
                    AgentState::UsageLimit,
                    r"ServiceQuotaExceeded|InsufficientModelCapacity",
                ),
                // [文件] HTTP 429 handling
                (
                    AgentState::RateLimit,
                    r"Too Many Requests|ThrottlingError|429",
                ),
                // [文件] Context overflow triggers compaction
                (AgentState::ContextFull, r"context window overflow|/compact"),
                // [文件] Trust-based permission system
                (AgentState::PermissionPrompt, r"Allow this action|y/n/t"),
                // [文件] Processing indicator
                (AgentState::Thinking, r"Generating"),
                // [文件] Tool names in output
                (AgentState::ToolUse, r"execute_bash|fs_read|fs_write"),
                // [実測] Idle prompt with percentage
                (AgentState::Idle, r"\d+%\s*!>"),
                // [実測] Trust dialog completion
                (AgentState::Ready, r"All tools are now trusted"),
            ],
            // Codex v0.118.0
            Backend::Codex => vec![
                // [文件] Requires OPENAI_API_KEY env
                (AgentState::AuthError, r"OPENAI_API_KEY|api.?key"),
                // [実測 v0.118.0] Quota exhausted message
                (AgentState::UsageLimit, r"hit your usage limit|try again at"),
                // [文件] HTTP 429 handling
                (AgentState::RateLimit, r"rate.?limit|429"),
                // [文件] Context overflow error
                (AgentState::ContextFull, r"ContextOverflow"),
                // [文件] Permission approval flow
                (
                    AgentState::PermissionPrompt,
                    r"Request approval|approve|deny",
                ),
                // [推測] Processing state
                (AgentState::Thinking, r"Thinking"),
                // [推測] Patch tool
                (AgentState::ToolUse, r"apply_patch"),
                // [実測] Prompt symbol + model info in status
                (AgentState::Idle, r"›"),
                // [実測] Version + model display
                (AgentState::Ready, r"OpenAI Codex|gpt-.*left"),
            ],
            // OpenCode v1.4.0
            Backend::OpenCode => vec![
                // [文件] HTTP error handling
                (AgentState::RateLimit, r"rate.?limit|429"),
                // [文件] Context overflow
                (AgentState::ContextFull, r"ContextOverflow"),
                // [文件] Permission UI
                (
                    AgentState::PermissionPrompt,
                    r"Permission required|Allow once|Allow always",
                ),
                // [文件] Busy text
                (AgentState::Thinking, r"Working"),
                // [実測] Update dialog that may block
                (
                    AgentState::PermissionPrompt,
                    r"Update Available|Skip\s+Confirm",
                ),
                // [実測] Input prompt text
                (AgentState::Idle, r"Ask anything"),
                // [実測] Ready state with keybinding hints
                (AgentState::Ready, r"Ask anything|tab agents"),
            ],
            // Gemini CLI v0.37.1
            Backend::Gemini => vec![
                // [文件] OAuth errors from API
                (
                    AgentState::AuthError,
                    r"OAuth not authenticated|OAuth expired|UNAUTHENTICATED|check API key",
                ),
                // [文件] Usage limit messages
                (
                    AgentState::UsageLimit,
                    r"Usage limit reached|Access resets at",
                ),
                // [文件] API resource exhaustion
                (AgentState::RateLimit, r"RESOURCE_EXHAUSTED|429"),
                // [文件] Token/quota limit
                (AgentState::ContextFull, r"quota.*exceeded|token.*limit"),
                // [文件] Permission select options
                (
                    AgentState::PermissionPrompt,
                    r"Allow once|Allow for this session|suggest changes",
                ),
                // [推測] Processing indicator
                (AgentState::Thinking, r"Thinking"),
                // [推測] MCP tool execution
                (AgentState::ToolUse, r"tool.*call|MCP.*tool"),
                // [実測] Input prompt text
                (AgentState::Idle, r"Type your message"),
                // [実測] Full ready prompt + YOLO mode
                (AgentState::Ready, r"Type your message|YOLO"),
            ],
        };

        let compiled: Vec<_> = patterns
            .into_iter()
            .filter_map(|(state, pat)| Regex::new(pat).ok().map(|re| (state, re)))
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
    since: Instant,
    pub last_output: Instant,
    state_buf: String,
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
