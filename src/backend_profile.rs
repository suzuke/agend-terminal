//! #8: per-backend detection data, co-located.
//!
//! The team voted (3-1, ORIGINAL/per-variant-structs unanimously rejected) for
//! the `BackendProfile` data-bundle: co-locate the per-backend detection data
//! that previously lived scattered across FIVE `match backend` sites in three
//! files — `state::patterns::{for_backend, compile_for}` (patterns), `behavioral::
//! {config_for, config_for_productivity}` (behavioral + productivity), and
//! `state::StateTracker::new` (initial_state). One backend's full detection
//! profile is gathered into ONE `*_profile()` block here; a single trivial
//! dispatch `match` ([`profile`]) replaces the five.
//!
//! Production routes EVERY backend through these profiles: `for_backend` /
//! `config_for[_productivity]` / `StateTracker::new` all source from
//! [`profile`], which is TOTAL (`&'static BackendProfile`, no `Option`). The
//! data-bundle migration train (step-0 #1683 reroute → OpenCode #1686 → Codex
//! #1687 → ClaudeCode #1689 → delete-legacy) was each proven byte-identical to
//! legacy at its own merge; #1580 then retired the last legacy backend
//! (`Gemini`), deleting the legacy detection spine (`compile_for`,
//! `config_for_legacy`, `config_for_productivity_legacy`, `legacy_initial_state`,
//! the `_ =>` catch-alls) and the runtime drift-guards — the exhaustive `match`
//! in [`profile`] now statically guarantees every backend has a profile.
//! Detection correctness is pinned by the fixture-replay suite + per-backend
//! state tests.

use crate::backend::Backend;
use crate::behavioral::{BehavioralConfig, MarkerCacheId, ProductivityConfig};
use crate::state::AgentState;
use std::sync::OnceLock;

/// All per-backend detection data for one backend, co-located.
///
/// #8 Phase 2 step-0: now consumed by PROD — `StatePatterns::for_backend`,
/// `behavioral::config_for[_productivity]`, and `StateTracker::new` route
/// every backend through [`profile`] (#1580: the legacy path is gone).
pub struct BackendProfile {
    /// Raw `(state, regex)` detection patterns in priority order — compiled by
    /// the state machine via `StatePatterns::from_raw_patterns`.
    pub patterns: Vec<(AgentState, &'static str)>,
    pub behavioral: BehavioralConfig,
    pub productivity: ProductivityConfig,
    pub initial_state: AgentState,
}

/// The single dispatch: `Backend → &'static BackendProfile`, lazy-cached
/// (compile-once, mirroring `StatePatterns::for_backend`'s `OnceLock`). This
/// `match` is the ONE unavoidable flat-enum lookup; the per-backend DATA lives
/// in the builders below, one block each.
///
/// #1580: now total (no `Option`). Gemini was the last legacy backend; with it
/// retired EVERY variant has a profile, so the type encodes "every backend has a
/// profile" and the legacy detection spine is gone. The exhaustive `match`
/// replaces the old drift-guard runtime invariant.
pub fn profile(backend: &Backend) -> &'static BackendProfile {
    static AGY: OnceLock<BackendProfile> = OnceLock::new();
    static KIRO: OnceLock<BackendProfile> = OnceLock::new();
    static OPENCODE: OnceLock<BackendProfile> = OnceLock::new();
    static CODEX: OnceLock<BackendProfile> = OnceLock::new();
    static CLAUDE: OnceLock<BackendProfile> = OnceLock::new();
    // Shell + Raw share one profile — every legacy source treats `Shell | Raw(_)`
    // identically (empty patterns, default behavioral, generic productivity, Ready).
    static EMPTY: OnceLock<BackendProfile> = OnceLock::new();
    match backend {
        Backend::Agy => AGY.get_or_init(agy_profile),
        Backend::KiroCli => KIRO.get_or_init(kirocli_profile),
        Backend::OpenCode => OPENCODE.get_or_init(opencode_profile),
        Backend::Codex => CODEX.get_or_init(codex_profile),
        Backend::ClaudeCode => CLAUDE.get_or_init(claudecode_profile),
        Backend::Shell | Backend::Raw(_) => EMPTY.get_or_init(empty_profile),
    }
}

/// Agy — moved VERBATIM from the four legacy sites (patterns.rs:693,
/// behavioral.rs:98 + :459, state/mod.rs:619). The harness proves byte-identity.
fn agy_profile() -> BackendProfile {
    BackendProfile {
        patterns: vec![
            (
                AgentState::PermissionPrompt,
                r"Requesting permission for:|Do you trust the contents of this project|tab Amend · e edit command",
            ),
            (AgentState::ToolUse, r"●\s+[A-Z][a-zA-Z]+\("),
            (AgentState::Thinking, r"esc to cancel"),
            (AgentState::Idle, r"\? for shortcuts"),
            (AgentState::Ready, r"Antigravity CLI|Type your message"),
        ],
        behavioral: BehavioralConfig {
            silence_thinking_ms: 3000,
            silence_idle_ms: 8000,
            supports_cursor_query: false,
        },
        productivity: ProductivityConfig {
            markers: crate::behavioral::AGY_PRODUCTIVE_MARKERS,
            use_heartbeat: true,
            heartbeat_fresh_window_ms: 10_000,
            cache_id: Some(MarkerCacheId::Agy),
        },
        initial_state: AgentState::Starting,
    }
}

/// KiroCli — moved VERBATIM from the four legacy sites (patterns.rs:270,
/// behavioral.rs config_for + config_for_productivity, state/mod.rs initial).
/// The ServerRateLimit entry references the SAME shared const the legacy arm
/// does (`SERVER_RATE_LIMIT_NET_ERRORS`), so it stays byte-identical. The
/// harness proves it.
fn kirocli_profile() -> BackendProfile {
    BackendProfile {
        patterns: vec![
            (
                AgentState::AuthError,
                r"Not authenticated|AccessDenied|denied access",
            ),
            (
                AgentState::UsageLimit,
                r"ServiceQuotaExceeded|InsufficientModelCapacity|you have reached the limit",
            ),
            (
                AgentState::RateLimit,
                r"Too Many Requests|ThrottlingError|ThrottlingException|Rate exceeded|\b429\b",
            ),
            (
                AgentState::ServerRateLimit,
                crate::state::patterns::SERVER_RATE_LIMIT_NET_ERRORS,
            ),
            (
                AgentState::ContextFull,
                r"context window overflow|compacting context",
            ),
            (
                AgentState::PermissionPrompt,
                r"requires approval|ESC to close \| Tab to edit",
            ),
            (
                AgentState::GitConflict,
                r"Automatic merge failed; fix conflicts|CONFLICT \(content\)|Resolve all conflicts manually|Failed to merge submodule|Failed to merge in",
            ),
            (AgentState::ToolUse, r"execute_bash|fs_read|fs_write"),
            (AgentState::Thinking, r"Kiro is working|esc to cancel"),
            (
                AgentState::Idle,
                r"\d+%\s*$|ask a question or describe a task",
            ),
            (AgentState::Ready, r"Trust All Tools active|/quit to exit"),
        ],
        behavioral: BehavioralConfig {
            silence_thinking_ms: 2500,
            silence_idle_ms: 7000,
            supports_cursor_query: true,
        },
        productivity: ProductivityConfig {
            markers: crate::behavioral::KIRO_PRODUCTIVE_MARKERS,
            use_heartbeat: true,
            heartbeat_fresh_window_ms: 10_000,
            cache_id: Some(MarkerCacheId::Kiro),
        },
        initial_state: AgentState::Starting,
    }
}

/// OpenCode (#8 Phase 2 step-1) — moved VERBATIM from the legacy sites
/// (patterns.rs `compile_for` Backend::OpenCode arm, behavioral.rs
/// `config_for_legacy` + `config_for_productivity_legacy`, managed→Starting
/// initial state). The ServerRateLimit entry references the SAME shared
/// `SERVER_RATE_LIMIT_NET_ERRORS` const and the markers reference the SAME
/// `OPENCODE_PRODUCTIVE_MARKERS` const the legacy arms use, so they stay
/// byte-identical. The harness proves it.
fn opencode_profile() -> BackendProfile {
    BackendProfile {
        patterns: vec![
            (
                AgentState::RateLimit,
                r"API rate limited \(429\)|Rate limited\. Quick retry|API rate limit exceeded",
            ),
            (
                AgentState::ServerRateLimit,
                crate::state::patterns::SERVER_RATE_LIMIT_NET_ERRORS,
            ),
            (AgentState::UsageLimit, r"Quota Limit Exceeded"),
            (
                AgentState::ApiError,
                r"Error from provider:|request validation errors",
            ),
            (AgentState::ContextFull, r"ContextOverflow"),
            (
                AgentState::PermissionPrompt,
                r"Permission required|Allow once\s+Allow always\s+Reject",
            ),
            (
                AgentState::GitConflict,
                r"Automatic merge failed; fix conflicts|CONFLICT \(content\)|Resolve all conflicts manually|Failed to merge submodule|Failed to merge in",
            ),
            (
                AgentState::ToolUse,
                r"✱\s+(Read|Write|Edit|Glob|Grep|Bash|List|Task)\b|~\s+(Reading|Writing|Editing|Searching|Listing|Globbing|Grepping)\b",
            ),
            (AgentState::Thinking, r"esc interrupt"),
            (
                AgentState::PermissionPrompt,
                r"Update Available|Skip\s+Confirm",
            ),
            (AgentState::Idle, r"Ask anything"),
            (AgentState::Ready, r"Ask anything|tab agents"),
        ],
        behavioral: BehavioralConfig {
            silence_thinking_ms: 3000,
            silence_idle_ms: 8000,
            supports_cursor_query: false,
        },
        productivity: ProductivityConfig {
            markers: crate::behavioral::OPENCODE_PRODUCTIVE_MARKERS,
            use_heartbeat: true,
            heartbeat_fresh_window_ms: 10_000,
            cache_id: Some(MarkerCacheId::OpenCode),
        },
        initial_state: AgentState::Starting,
    }
}

/// Codex (#8 Phase 2 step-2) — moved VERBATIM from the legacy sites (patterns.rs
/// `compile_for` Backend::Codex arm, behavioral.rs `config_for_legacy` +
/// `config_for_productivity_legacy`, managed→Starting). ServerRateLimit
/// references the SAME shared `SERVER_RATE_LIMIT_NET_ERRORS` const and the
/// markers the SAME `CODEX_PRODUCTIVE_MARKERS` const the legacy arms use → byte-
/// identical. Includes the #1634 ModelUnsupported pattern (HIGH_FP, #919 red-
/// anchor gated downstream — unchanged by this move). The harness proves it.
fn codex_profile() -> BackendProfile {
    BackendProfile {
        patterns: vec![
            (AgentState::AuthError, r"OPENAI_API_KEY|api.?key"),
            (AgentState::UsageLimit, r"hit your usage limit|try again at"),
            (
                AgentState::RateLimit,
                r"rate_limit_exceeded|RateLimitError|hit your rate limit",
            ),
            (
                AgentState::ServerRateLimit,
                crate::state::patterns::SERVER_RATE_LIMIT_NET_ERRORS,
            ),
            (AgentState::ContextFull, r"ContextOverflow"),
            (
                AgentState::ModelUnsupported,
                r"invalid_request_error|model is not supported|Model metadata for .*? not found",
            ),
            (
                AgentState::PermissionPrompt,
                r"Would you like to run the following command\?|Press enter to confirm or esc to cancel|No, and tell Codex what to do differently",
            ),
            (
                AgentState::GitConflict,
                r"Automatic merge failed; fix conflicts|CONFLICT \(content\)|Resolve all conflicts manually|Failed to merge submodule|Failed to merge in",
            ),
            (AgentState::Thinking, r"Working|esc to interrupt"),
            (AgentState::Idle, r"›"),
            (AgentState::Ready, r"OpenAI Codex|gpt-.*left"),
        ],
        behavioral: BehavioralConfig {
            silence_thinking_ms: 3000,
            silence_idle_ms: 8000,
            supports_cursor_query: false,
        },
        productivity: ProductivityConfig {
            markers: crate::behavioral::CODEX_PRODUCTIVE_MARKERS,
            use_heartbeat: true,
            heartbeat_fresh_window_ms: 10_000,
            cache_id: Some(MarkerCacheId::Codex),
        },
        initial_state: AgentState::Starting,
    }
}

/// ClaudeCode (#8 Phase 2 step-3) — moved VERBATIM from the legacy sites
/// (patterns.rs `compile_for` Backend::ClaudeCode arm, behavioral.rs
/// `config_for_legacy` + `config_for_productivity_legacy`, managed→Starting).
/// The two ServerRateLimit entries (one literal alternation + the shared
/// `SERVER_RATE_LIMIT_NET_ERRORS` const) and the unicode spinner/glyph patterns
/// (`⠋…`, `✓●⏺`, sparkle `✻✢✶✳✽`, `❯`, `\x{2026}`) are copied byte-for-byte;
/// markers reference the SAME `CLAUDE_PRODUCTIVE_MARKERS` const. Does NOT include
/// any model-unsupported pattern (#1646 is blocked-on-fixture, not in legacy).
/// The harness proves byte-identity.
fn claudecode_profile() -> BackendProfile {
    BackendProfile {
        patterns: vec![
            (
                AgentState::AuthError,
                r"API key|authentication failed|unauthorized|API Error: 40[13]\b",
            ),
            (
                AgentState::ServerRateLimit,
                r"Server is temporarily limiting requests|temporarily limiting.*not your usage|API Error: 5\d{2}\b|server-side issue.*temporary|API Error: Repeated 529 Overloaded|overloaded_error|api_error|timeout_error",
            ),
            (
                AgentState::ServerRateLimit,
                crate::state::patterns::SERVER_RATE_LIMIT_NET_ERRORS,
            ),
            (
                AgentState::RateLimit,
                r"API Error: Request rejected \(429\)|rate_limit_error|hit a rate limit",
            ),
            (
                AgentState::UsageLimit,
                r"You've hit your session limit|You've hit your weekly limit|You've hit your Opus limit|Credit balance is too low|credit_balance_too_low",
            ),
            (
                AgentState::ContextFull,
                r"compacting context|context.*(full|limit)",
            ),
            (
                AgentState::PermissionPrompt,
                r"Esc to cancel · Tab to amend|allow all edits during this session|Enter to confirm · Esc to cancel",
            ),
            (
                AgentState::GitConflict,
                r"Automatic merge failed; fix conflicts|CONFLICT \(content\)|Resolve all conflicts manually|Failed to merge submodule|Failed to merge in",
            ),
            (
                AgentState::ToolUse,
                r"(?m)^(?:[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏]\s+(?:Read|Bash|Edit|Write|Grep|Glob|Listing|Reading|Writing|Searching|Editing)|[✓●⏺]\s+(?:Listing|Reading|Writing|Searching|Editing))\b",
            ),
            (
                AgentState::Thinking,
                r"(?i)[✻✢✶✳✽*·]\s*\w+\x{2026}|\w+\x{2026}\s*\((?:\d+[smh]|running )|thought for [0-9]+s",
            ),
            (AgentState::Idle, r"❯"),
            (AgentState::Ready, r"bypass permissions"),
        ],
        behavioral: BehavioralConfig {
            silence_thinking_ms: 2000,
            silence_idle_ms: 6000,
            supports_cursor_query: true,
        },
        productivity: ProductivityConfig {
            markers: crate::behavioral::CLAUDE_PRODUCTIVE_MARKERS,
            use_heartbeat: true,
            heartbeat_fresh_window_ms: 10_000,
            cache_id: Some(MarkerCacheId::Claude),
        },
        initial_state: AgentState::Starting,
    }
}

/// Shell / Raw — moved VERBATIM (empty patterns, default behavioral, generic
/// productivity, Ready initial state).
fn empty_profile() -> BackendProfile {
    BackendProfile {
        patterns: vec![],
        behavioral: BehavioralConfig::default(),
        productivity: ProductivityConfig {
            markers: crate::behavioral::GENERIC_PRODUCTIVE_MARKERS,
            use_heartbeat: false,
            heartbeat_fresh_window_ms: 0,
            cache_id: Some(MarkerCacheId::Generic),
        },
        initial_state: AgentState::Ready,
    }
}
