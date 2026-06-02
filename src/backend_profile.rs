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
//! Migrated here: `Shell`/`Raw` (empty — zero-risk shape proof) + `Agy` (the
//! smallest REAL pattern set — proves the harness catches real-data drift).

use crate::backend::Backend;
use crate::behavioral::{BehavioralConfig, MarkerCacheId, ProductivityConfig};
use crate::state::AgentState;
use std::sync::OnceLock;

/// All per-backend detection data for one backend, co-located.
#[allow(dead_code)] // VALIDATION scaffold: consumed by the harness, not yet by prod.
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
#[allow(dead_code)] // VALIDATION: exposed via BackendBehavior::profile; prod reroute is the migration PR.
pub fn profile(backend: &Backend) -> Option<&'static BackendProfile> {
    static AGY: OnceLock<BackendProfile> = OnceLock::new();
    // Shell + Raw share one profile — every legacy source treats `Shell | Raw(_)`
    // identically (empty patterns, default behavioral, generic productivity, Ready).
    static EMPTY: OnceLock<BackendProfile> = OnceLock::new();
    match backend {
        Backend::Agy => Some(AGY.get_or_init(agy_profile)),
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
    use crate::behavioral::{config_for, config_for_productivity};
    use crate::state::patterns::StatePatterns;
    use crate::state::StateTracker;

    fn migrated() -> Vec<Backend> {
        vec![Backend::Agy, Backend::Shell, Backend::Raw("x".to_string())]
    }

    /// PRIMARY parity proof (structural, COMPLETE): the profile's raw patterns
    /// equal the legacy compiled patterns' source strings + states + order.
    /// Identical regex strings + order ⟹ identical detection — stronger than a
    /// corpus sample (this is the answer to codex's "snapshot ≠ proof").
    #[test]
    fn profile_patterns_byte_identical_to_legacy() {
        for b in migrated() {
            let legacy: Vec<(AgentState, String)> =
                StatePatterns::for_backend(&b).pattern_sources();
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
            let legacy = StatePatterns::for_backend(&b);
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
                format!("{:?}", config_for(&b)),
                "behavioral parity for {b:?}"
            );
            assert_eq!(
                format!("{:?}", p.productivity),
                format!("{:?}", config_for_productivity(&b)),
                "productivity parity for {b:?}"
            );
            assert_eq!(
                p.initial_state,
                StateTracker::new(Some(&b)).current,
                "initial_state parity for {b:?}"
            );
        }
    }

    /// Non-migrated backends still return `None` (legacy path owns them).
    #[test]
    fn unmigrated_backends_have_no_profile_yet() {
        for b in [
            Backend::ClaudeCode,
            Backend::KiroCli,
            Backend::Codex,
            Backend::OpenCode,
            Backend::Gemini,
        ] {
            assert!(profile(&b).is_none(), "{b:?} must stay on the legacy path");
        }
    }
}
