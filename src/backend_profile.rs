//! #8 Phase 2 VALIDATION (branch `spike/backend-profile-validation`).
//!
//! The team voted (3-1, ORIGINAL/per-variant-structs unanimously rejected) for
//! the `BackendProfile` data-bundle: co-locate the per-backend detection data
//! that today lives scattered across FIVE `match backend` sites in three files —
//! `state::patterns::{for_backend, compile_for}` (patterns), `behavioral::
//! {config_for, config_for_productivity}` (behavioral + productivity), and
//! `state::StateTracker::new` (initial_state). One backend's full detection
//! profile is gathered into ONE block here; a single trivial dispatch `match`
//! replaces the five.
//!
//! This is a VALIDATION, not a migration: production is NOT rerouted (the legacy
//! match paths are untouched, zero risk on the high-stakes #1523 detection
//! surface). The equivalence harness below proves the moved data is
//! BYTE-IDENTICAL to legacy for the migrated backends; the migration train then
//! reroutes production one backend per PR, gated by that harness.
//!
//! Migrated so far: `Shell`/`Raw` (empty), `Agy` (#1672), `KiroCli` (#1674),
//! and `OpenCode` (#8 step-1, this PR — 12 patterns, incl. the shared
//! `SERVER_RATE_LIMIT_NET_ERRORS` const). Production is rerouted as of step-0
//! (#1683): `profile()`-Some backends run detection through the bundle, legacy
//! is the parity baseline until the train ends. One backend per PR, each proven
//! byte-identical by the harness below.

use crate::backend::Backend;
use crate::behavioral::{BehavioralConfig, MarkerCacheId, ProductivityConfig};
use crate::state::AgentState;
use std::sync::OnceLock;

/// All per-backend detection data for one backend, co-located.
///
/// #8 Phase 2 step-0: now consumed by PROD — `StatePatterns::for_backend`,
/// `behavioral::config_for[_productivity]`, and `StateTracker::new` route
/// migrated backends through [`profile`] (else legacy). No longer a scaffold.
pub struct BackendProfile {
    /// Raw `(state, regex)` detection patterns in priority order — compiled by
    /// the state machine exactly as `StatePatterns::compile_for` does today.
    pub patterns: Vec<(AgentState, &'static str)>,
    pub behavioral: BehavioralConfig,
    pub productivity: ProductivityConfig,
    pub initial_state: AgentState,
}

/// The single dispatch: `Backend → &'static BackendProfile`, lazy-cached
/// (compile-once, mirroring `StatePatterns::for_backend`'s `OnceLock`). Returns
/// `None` for backends not yet migrated (their data still lives on the legacy
/// match path). This `match` is the ONE unavoidable flat-enum lookup; the
/// per-backend DATA lives in the builders below, one block each.
pub fn profile(backend: &Backend) -> Option<&'static BackendProfile> {
    static AGY: OnceLock<BackendProfile> = OnceLock::new();
    static KIRO: OnceLock<BackendProfile> = OnceLock::new();
    static OPENCODE: OnceLock<BackendProfile> = OnceLock::new();
    // Shell + Raw share one profile — every legacy source treats `Shell | Raw(_)`
    // identically (empty patterns, default behavioral, generic productivity, Ready).
    static EMPTY: OnceLock<BackendProfile> = OnceLock::new();
    match backend {
        Backend::Agy => Some(AGY.get_or_init(agy_profile)),
        Backend::KiroCli => Some(KIRO.get_or_init(kirocli_profile)),
        Backend::OpenCode => Some(OPENCODE.get_or_init(opencode_profile)),
        Backend::Shell | Backend::Raw(_) => Some(EMPTY.get_or_init(empty_profile)),
        _ => None,
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
            markers: crate::behavioral::GEMINI_PRODUCTIVE_MARKERS,
            use_heartbeat: true,
            heartbeat_fresh_window_ms: 10_000,
            cache_id: Some(MarkerCacheId::Gemini),
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

#[cfg(test)]
mod tests {
    use super::*;
    // #8 Phase 2 step-0: parity tests must reference the TRUE-legacy companions,
    // NOT the now-profile-routed prod fns (`config_for`/`for_backend`/`new`) — using
    // the prod fns would make every assertion circular (profile vs profile).
    use crate::behavioral::{config_for_legacy, config_for_productivity_legacy};
    use crate::state::patterns::StatePatterns;
    use crate::state::StateTracker;

    fn migrated() -> Vec<Backend> {
        vec![
            Backend::Agy,
            Backend::KiroCli,
            Backend::OpenCode,
            Backend::Shell,
            Backend::Raw("x".to_string()),
        ]
    }

    /// PRIMARY parity proof (structural, COMPLETE): the profile's raw patterns
    /// equal the legacy compiled patterns' source strings + states + order.
    /// Identical regex strings + order ⟹ identical detection — stronger than a
    /// corpus sample (this is the answer to codex's "snapshot ≠ proof").
    #[test]
    fn profile_patterns_byte_identical_to_legacy() {
        for b in migrated() {
            // TRUE legacy = compile_for directly (for_backend is now profile-routed).
            let legacy: Vec<(AgentState, String)> =
                StatePatterns::compile_for(&b).pattern_sources();
            let prof: Vec<(AgentState, String)> = profile(&b)
                .expect("migrated")
                .patterns
                .iter()
                .map(|(s, p)| (*s, (*p).to_string()))
                .collect();
            assert_eq!(prof, legacy, "pattern source/state/order parity for {b:?}");
        }
    }

    /// SECONDARY (behavioral): compile the profile's patterns and confirm
    /// `detect` is identical to the legacy compiled patterns across a corpus.
    /// Redundant given the structural proof above, but exercises the full
    /// compile+detect pipeline.
    #[test]
    fn profile_detect_matches_legacy_on_corpus() {
        let corpus = [
            "Requesting permission for: write",
            "Do you trust the contents of this project",
            "tab Amend · e edit command",
            "● Bash(ls -la)",
            "● GenerateImage(prompt)",
            "Analyzing the request… esc to cancel",
            "? for shortcuts",
            "Antigravity CLI v2",
            "Type your message",
            "just some unrelated prose with no anchors",
            "● lowercase(x)", // must NOT match ToolUse ([A-Z] anchor)
            "",
        ];
        for b in migrated() {
            let legacy = StatePatterns::compile_for(&b); // TRUE legacy, not the rerouted entry
            let prof = StatePatterns::from_raw_patterns(&profile(&b).expect("migrated").patterns);
            for item in corpus {
                assert_eq!(
                    prof.detect(item),
                    legacy.detect(item),
                    "detect parity for {b:?} on {item:?}"
                );
            }
        }
    }

    /// Behavioral + productivity + initial_state byte-identical to legacy
    /// (Debug-string equality — the configs are `Copy`, not `PartialEq`).
    #[test]
    fn profile_configs_byte_identical_to_legacy() {
        for b in migrated() {
            let p = profile(&b).expect("migrated");
            assert_eq!(
                format!("{:?}", p.behavioral),
                format!("{:?}", config_for_legacy(&b)),
                "behavioral parity for {b:?}"
            );
            assert_eq!(
                format!("{:?}", p.productivity),
                format!("{:?}", config_for_productivity_legacy(&b)),
                "productivity parity for {b:?}"
            );
            assert_eq!(
                p.initial_state,
                StateTracker::legacy_initial_state(Some(&b)),
                "initial_state parity for {b:?}"
            );
        }
    }

    /// Non-migrated backends still return `None` (legacy path owns them).
    /// OpenCode left this set in step-1 (#8 Phase 2) — it is now `migrated()`.
    #[test]
    fn unmigrated_backends_have_no_profile_yet() {
        for b in [Backend::ClaudeCode, Backend::Codex, Backend::Gemini] {
            assert!(profile(&b).is_none(), "{b:?} must stay on the legacy path");
        }
    }

    /// #8 Phase 2 step-0 MERGE GATE — profile-ON-vs-OFF classify parity at the
    /// PROD entry. `for_backend` (now profile-routed for migrated backends) must
    /// detect byte-identically to the TRUE-legacy `compile_for` for EVERY backend
    /// across the corpus: migrated → the profile path equals legacy; un-migrated →
    /// both are legacy (guards the fork never perturbs Claude/Codex/OpenCode/Gemini).
    /// Complements the structural `profile_patterns_byte_identical_to_legacy`
    /// (identical sources ⟹ identical detection on ALL inputs) — together: complete
    /// parity, not a snapshot. The full-fixture corpus is also covered empirically
    /// by `replay_manifest_regression` running against this rerouted prod path.
    #[test]
    fn for_backend_classify_parity_with_legacy_step0() {
        let corpus = [
            "Requesting permission for: write",
            "Do you trust the contents of this project",
            "● Bash(ls -la)",
            "esc to cancel",
            "ECONNRESET",
            "Too Many Requests",
            "ServiceQuotaExceeded",
            "Trust All Tools active",
            "ask a question or describe a task",
            "Antigravity CLI v2",
            "Type your message",
            "? for shortcuts",
            "Automatic merge failed; fix conflicts",
            "just unrelated prose with no anchors",
            "● lowercase(x)",
            "",
        ];
        for b in [
            Backend::ClaudeCode,
            Backend::KiroCli,
            Backend::Codex,
            Backend::OpenCode,
            Backend::Gemini,
            Backend::Agy,
            Backend::Shell,
            Backend::Raw("x".to_string()),
        ] {
            let prod = StatePatterns::for_backend(&b); // profile-routed entry
            let legacy = StatePatterns::compile_for(&b); // TRUE legacy
            for item in corpus {
                assert_eq!(
                    prod.detect(item),
                    legacy.detect(item),
                    "for_backend (prod) vs compile_for (legacy) parity for {b:?} on {item:?}"
                );
            }
        }
    }
}
