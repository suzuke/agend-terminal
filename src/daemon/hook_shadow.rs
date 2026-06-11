//! Hook-driven state SHADOW store (PoC, #hook-state-spike phase 2).
//!
//! Claude Code lifecycle hooks (injected per-workspace by `mcp_config.rs`
//! under the `AGEND_HOOK_STATE_POC=1` flag) report back via the
//! `agend-terminal hook-event` subcommand → the `HOOK_EVENT` API method →
//! here. SHADOW-MODE ONLY: events are recorded and compared against the
//! screen-heuristic state (`#hook-shadow` log) — they do NOT drive
//! transitions. Empirically verified before wiring (2026-06-11, live fleet
//! spawn): SessionStart / UserPromptSubmit / PreToolUse / PostToolUse / Stop
//! / Notification(idle_prompt) all fire as documented with the expected
//! payload fields; the TUI shows no artifacts (async hooks).
//!
//! Promotion to authoritative (hook state wins over heuristic within a
//! freshness window, the #1945 resolution pattern) is the PRODUCTION step,
//! gated on shadow agreement data from this PoC.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::OnceLock;

/// One agent's latest hook-derived observation.
///
/// PoC scaffolding (deferred consumers, not ghost — the #649-trio annotation
/// pattern): in shadow-mode the only readers are the `#hook-shadow` log (which
/// reads the values at record time) and tests; the PRODUCTION promotion step
/// (hook state wins over heuristic within a freshness window) is the consumer
/// of `snapshot_for` + these fields.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct HookShadow {
    /// Raw hook event name (`PreToolUse`, `Notification`, …).
    pub last_event: String,
    /// State the event maps to, when the mapping is authoritative-grade.
    /// `None` = event recorded but not state-mapped (e.g. `SessionEnd`).
    pub derived_state: Option<crate::state::AgentState>,
    /// Receipt time (daemon clock, epoch ms).
    pub at_ms: u64,
}

fn store() -> &'static Mutex<HashMap<String, HookShadow>> {
    static S: OnceLock<Mutex<HashMap<String, HookShadow>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Map a hook event to the `AgentState` it evidences. Derivations follow the
/// empirically-verified event semantics:
/// - `UserPromptSubmit` → Thinking (turn started)
/// - `PreToolUse` → ToolUse; `PostToolUse` → Thinking (between tools)
/// - `Stop` → Idle (turn ended; the `idle_prompt` Notification re-confirms)
/// - `Notification(permission_prompt)` → PermissionPrompt
/// - `Notification(idle_prompt)` → Idle
/// - `SessionStart` → Starting
/// - `StopFailure` → ApiError (docs-sourced; not yet empirically triggered —
///   shadow data will tell)
/// - `PreCompact` → ContextFull-adjacent signal; mapped to ContextFull so the
///   shadow comparison can measure it against the screen heuristic.
pub fn derive_state(
    hook_event_name: &str,
    notification_type: Option<&str>,
) -> Option<crate::state::AgentState> {
    use crate::state::AgentState;
    match hook_event_name {
        "SessionStart" => Some(AgentState::Starting),
        "UserPromptSubmit" => Some(AgentState::Thinking),
        "PreToolUse" => Some(AgentState::ToolUse),
        "PostToolUse" => Some(AgentState::Thinking),
        "Stop" => Some(AgentState::Idle),
        "Notification" => match notification_type {
            Some("permission_prompt") => Some(AgentState::PermissionPrompt),
            Some("idle_prompt") => Some(AgentState::Idle),
            _ => None,
        },
        "PermissionRequest" => Some(AgentState::PermissionPrompt),
        "StopFailure" => Some(AgentState::ApiError),
        "PreCompact" => Some(AgentState::ContextFull),
        _ => None,
    }
}

/// Record one hook event for `name`; returns the derived state (if mapped).
pub fn record_event(
    name: &str,
    hook_event_name: &str,
    notification_type: Option<&str>,
) -> Option<crate::state::AgentState> {
    let derived = derive_state(hook_event_name, notification_type);
    store().lock().insert(
        name.to_string(),
        HookShadow {
            last_event: hook_event_name.to_string(),
            derived_state: derived,
            at_ms: now_ms(),
        },
    );
    derived
}

/// Latest hook observation for `name` (shadow consumers / tests). Deferred:
/// the production resolution layer consumes this after the shadow gate.
#[allow(dead_code)]
pub fn snapshot_for(name: &str) -> Option<HookShadow> {
    store().lock().get(name).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AgentState;

    #[test]
    fn derive_map_covers_the_fragile_band() {
        assert_eq!(derive_state("PreToolUse", None), Some(AgentState::ToolUse));
        assert_eq!(
            derive_state("Notification", Some("permission_prompt")),
            Some(AgentState::PermissionPrompt)
        );
        assert_eq!(
            derive_state("Notification", Some("idle_prompt")),
            Some(AgentState::Idle)
        );
        assert_eq!(derive_state("Stop", None), Some(AgentState::Idle));
        assert_eq!(
            derive_state("UserPromptSubmit", None),
            Some(AgentState::Thinking)
        );
        // Unknown notification types stay unmapped (recorded, not derived).
        assert_eq!(derive_state("Notification", Some("auth_success")), None);
        // Unknown events stay unmapped — forward-compatible with new hooks.
        assert_eq!(derive_state("SomeFutureEvent", None), None);
    }

    #[test]
    fn record_and_snapshot_roundtrip() {
        let derived = record_event("shadow-test-agent", "PreToolUse", None);
        assert_eq!(derived, Some(AgentState::ToolUse));
        let snap = snapshot_for("shadow-test-agent").expect("recorded");
        assert_eq!(snap.last_event, "PreToolUse");
        assert_eq!(snap.derived_state, Some(AgentState::ToolUse));
        assert!(snap.at_ms > 0);
        assert!(snapshot_for("never-seen").is_none());
    }
}
