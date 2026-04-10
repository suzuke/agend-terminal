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
        }
    }

    /// Is this an error state (instant transition, no hysteresis)?
    pub fn is_error(self) -> bool {
        self.priority() >= Self::ContextFull.priority()
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
        }
    }
}

/// Compiled patterns for one backend.
pub struct StatePatterns {
    /// (state, regex) pairs in priority order (highest priority first).
    patterns: Vec<(AgentState, Regex)>,
}

impl StatePatterns {
    pub fn for_backend(backend: &Backend) -> Self {
        let patterns = match backend {
            Backend::ClaudeCode => vec![
                (AgentState::AuthError, r"API key|authentication failed"),
                (AgentState::RateLimit, r"overloaded|rate.?limit|429"),
                (AgentState::ContextFull, r"compacting context|context.*(full|limit|window)"),
                (AgentState::PermissionPrompt, r"Allow once|Allow always|approve this"),
                (AgentState::Thinking, r"Thinking"),
                (AgentState::ToolUse, r"[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏✓●].*(Read|Bash|Edit|Write|Grep|Glob)"),
                (AgentState::Idle, r"❯"),
                (AgentState::Ready, r"bypass permissions|Type your"),
            ],
            Backend::KiroCli => vec![
                (AgentState::AuthError, r"Not authenticated|AccessDenied|denied access"),
                (AgentState::UsageLimit, r"ServiceQuotaExceeded|InsufficientModelCapacity"),
                (AgentState::RateLimit, r"Too Many Requests|ThrottlingError|429"),
                (AgentState::ContextFull, r"context window overflow|/compact"),
                (AgentState::PermissionPrompt, r"Allow this action|y/n/t"),
                (AgentState::Thinking, r"Generating"),
                (AgentState::ToolUse, r"execute_bash|fs_read|fs_write"),
                (AgentState::Ready, r"All tools are now trusted"),
            ],
            Backend::Codex => vec![
                (AgentState::AuthError, r"OPENAI_API_KEY"),
                (AgentState::RateLimit, r"rate.?limit|429"),
                (AgentState::ContextFull, r"ContextOverflow"),
                (AgentState::PermissionPrompt, r"Request approval|approve|deny"),
                (AgentState::Thinking, r"Thinking"),
                (AgentState::ToolUse, r"apply_patch"),
                (AgentState::Ready, r"(?i)idle|codex"),
            ],
            Backend::OpenCode => vec![
                (AgentState::RateLimit, r"rate.?limit|429"),
                (AgentState::ContextFull, r"ContextOverflow"),
                (AgentState::PermissionPrompt, r"Permission required|Allow once|Allow always"),
                (AgentState::Thinking, r"Working"),
                (AgentState::Ready, r"(?i)idle|opencode"),
            ],
            Backend::Gemini => vec![
                (AgentState::AuthError, r"OAuth not authenticated|OAuth expired|UNAUTHENTICATED|check API key"),
                (AgentState::UsageLimit, r"Usage limit reached|Access resets at"),
                (AgentState::RateLimit, r"RESOURCE_EXHAUSTED|429"),
                (AgentState::ContextFull, r"quota.*exceeded|token.*limit"),
                (AgentState::PermissionPrompt, r"Allow once|Allow for this session|suggest changes"),
                (AgentState::Thinking, r"Thinking"),
                (AgentState::ToolUse, r"tool.*call|MCP.*tool"),
                (AgentState::Ready, r"(?i)idle|gemini"),
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

    /// Check for hang (call periodically or on query).
    pub fn check_hang(&mut self) {
        if self.current != AgentState::Starting
            && self.last_output.elapsed() > Duration::from_secs(60)
        {
            self.current = AgentState::Hang;
        }
    }

    /// Get current state (with hang check).
    pub fn get_state(&mut self) -> AgentState {
        self.check_hang();
        self.current
    }

    fn transition(&mut self, new_state: AgentState) {
        if new_state == self.current {
            return;
        }

        // Error states: instant transition (no hysteresis)
        if new_state.is_error() {
            self.current = new_state;
            self.since = Instant::now();
            return;
        }

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
