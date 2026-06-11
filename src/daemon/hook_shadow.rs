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

/// Freshness window for a hook-derived state: a derived state is only VALID
/// while a hook event arrived within this window — beyond it the state is
/// STALE and resolution falls back to the screen heuristic. Kills the
/// shadow-data top disagreement (8× hook=Starting vs screen=Idle: a boot/
/// respawn SessionStart with no follow-up event lingered as "Starting"
/// forever on idle agents). 600s matches the existing freshness-constant
/// style (`CONTEXT_FRESH`) and comfortably covers long local tool runs
/// (a 9-min Bash emits PreToolUse then nothing until PostToolUse).
pub const HOOK_FRESHNESS: std::time::Duration = std::time::Duration::from_secs(600);

/// Freshness-resolved hook state (the Phase-1 resolution layer, shadow form).
#[derive(Debug, Clone, PartialEq)]
pub enum HookResolution {
    /// A state-mapped hook event within the freshness window.
    Fresh(crate::state::AgentState),
    /// The last event is older than [`HOOK_FRESHNESS`] (or was never
    /// state-mapped) — consumers fall back to the screen heuristic.
    Stale,
    /// No hook event ever recorded for this agent.
    Unknown,
}

/// Resolve `name`'s hook state through the freshness window: `Fresh(state)`
/// only when a state-mapped event arrived within [`HOOK_FRESHNESS`]; stale or
/// unmapped observations resolve `Stale` (fall back to heuristic) — a
/// boot-time `Starting` is never carried as the current state hours later.
///
/// #1523 (decision d-20260611051442661249-0): `ToolUse` is the ONE EXCEPTION —
/// it is EVENT-PAIR-closed, not freshness-bounded. `PreToolUse` opens the tool;
/// only the NEXT hook event (`PostToolUse`→Thinking / `Stop`→Idle) closes it, by
/// OVERWRITING this snapshot. So while the snapshot still reads `ToolUse`, the
/// tool is genuinely still running — a long (>HOOK_FRESHNESS) tool must NOT be
/// demoted back to the screen heuristic, which is exactly the #1985 nudge class
/// the promotion exists to fix. A crashed-mid-tool agent (no closing event ever)
/// is reaped by the liveness watchdog independently, so leaving `ToolUse` open is
/// safe; the bound is the event, not the clock.
pub fn resolved_state_for(name: &str) -> HookResolution {
    use crate::state::AgentState;
    let Some(snap) = snapshot_for(name) else {
        return HookResolution::Unknown;
    };
    let age_ms = now_ms().saturating_sub(snap.at_ms);
    match snap.derived_state {
        // ToolUse: valid until the PostToolUse/Stop event overwrites it (above).
        Some(AgentState::ToolUse) => HookResolution::Fresh(AgentState::ToolUse),
        Some(state) if age_ms <= HOOK_FRESHNESS.as_millis() as u64 => HookResolution::Fresh(state),
        _ => HookResolution::Stale,
    }
}

/// #1523: is the hook→authoritative promotion enabled? Gated on the same flag
/// that injects the hooks, so promotion can never read hooks that aren't wired.
fn promotion_enabled() -> bool {
    std::env::var("AGEND_HOOK_STATE_POC").as_deref() == Ok("1")
}

/// #1523 PROMOTION: the authoritative `AgentState` for the daemon's snapshot.
///
/// When the flag is on AND `backend_command` is a STRONG (hook-instrumented)
/// backend, a `Fresh` hook resolution WINS over the screen `heuristic`;
/// `Stale`/`Unknown` (or flag-off / a non-hook backend) fall back to `heuristic`
/// — **byte-identical** to pre-promotion. Hooks ENHANCE; the heuristic remains
/// the complete fallback path, so a backend without hooks (or a stale window) is
/// never worse off than before. This is the single chokepoint
/// (`per_tick::snapshot`) so every downstream consumer (`agent_state_of`,
/// watchdogs, pane badge) inherits the authoritative state from one place.
pub fn authoritative_state(
    backend_command: &str,
    name: &str,
    heuristic: crate::state::AgentState,
) -> crate::state::AgentState {
    authoritative_state_inner(promotion_enabled(), backend_command, name, heuristic)
}

/// Promotion core, split out so tests exercise the flag/backend/freshness logic
/// without mutating the process-global `AGEND_HOOK_STATE_POC` env var.
fn authoritative_state_inner(
    enabled: bool,
    backend_command: &str,
    name: &str,
    heuristic: crate::state::AgentState,
) -> crate::state::AgentState {
    if !enabled || !crate::backend::Backend::parse_str(backend_command).has_state_hooks() {
        return heuristic;
    }
    match resolved_state_for(name) {
        HookResolution::Fresh(state) => state,
        HookResolution::Stale | HookResolution::Unknown => heuristic,
    }
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
///
/// TRANSIENT events do NOT map to a persistent state (shadow data round 1:
/// 1× hook=ContextFull vs screen=Thinking — `PreCompact` is a one-shot
/// "compacting now" moment, the agent keeps working right after, so latching
/// it as a persistent ContextFull misreads the event class). `PreCompact` is
/// recorded (the event is still visible in the store/log) but derives no
/// state. `SessionStart` → Starting stays mapped — boot IS a real if
/// short-lived state — and is bounded by [`HOOK_FRESHNESS`] + replaced by the
/// next event (the freshness layer is what keeps it from lingering).
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
        // Transient: one-shot moment, not a persistent state (see fn doc).
        "PreCompact" => None,
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

/// Test seam: age `name`'s observation by `ms` (freshness-expiry tests).
#[cfg(test)]
pub(crate) fn backdate_for_test(name: &str, ms: u64) {
    if let Some(snap) = store().lock().get_mut(name) {
        snap.at_ms = snap.at_ms.saturating_sub(ms);
    }
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

    /// Refinement round 1: a fresh state-mapped event resolves Fresh; past
    /// the window it resolves Stale (fall back to heuristic) — a boot
    /// `Starting` can no longer linger as the current state (the 8×
    /// Starting-vs-Idle shadow disagreement).
    #[test]
    fn freshness_window_expires_stale_hook_state() {
        record_event("fresh-test", "SessionStart", None);
        assert_eq!(
            resolved_state_for("fresh-test"),
            HookResolution::Fresh(AgentState::Starting),
            "within the window the boot state is valid"
        );
        backdate_for_test("fresh-test", HOOK_FRESHNESS.as_millis() as u64 + 1_000);
        assert_eq!(
            resolved_state_for("fresh-test"),
            HookResolution::Stale,
            "past the window the observation is stale — heuristic falls back in"
        );
        assert_eq!(
            resolved_state_for("never-recorded-agent"),
            HookResolution::Unknown
        );
    }

    // ── #1523 promotion §3.9 ────────────────────────────────────────────

    /// ToolUse is EVENT-PAIR-closed: a long tool (PreToolUse, then no event past
    /// HOOK_FRESHNESS) stays Fresh(ToolUse) — NOT demoted to the heuristic (the
    /// #1985 nudge class). Only PostToolUse/Stop closes it (by overwriting).
    #[test]
    fn long_tool_stays_tooluse_past_freshness() {
        record_event("longtool", "PreToolUse", None);
        backdate_for_test("longtool", HOOK_FRESHNESS.as_millis() as u64 + 600_000); // +10min past window
        assert_eq!(
            resolved_state_for("longtool"),
            HookResolution::Fresh(AgentState::ToolUse),
            "a long tool stays ToolUse until PostToolUse/Stop — never freshness-stale"
        );
        assert_eq!(
            authoritative_state_inner(true, "claude", "longtool", AgentState::Idle),
            AgentState::ToolUse,
            "the still-open tool wins over a (wrong) heuristic"
        );
        // PostToolUse closes the pair (→ Thinking; freshness applies again).
        record_event("longtool", "PostToolUse", None);
        assert_eq!(
            resolved_state_for("longtool"),
            HookResolution::Fresh(AgentState::Thinking)
        );
    }

    /// §3.9: flag OFF → byte-identical to the heuristic, even with a fresh hook.
    #[test]
    fn flag_off_is_byte_identical_to_heuristic() {
        record_event("flagoff", "UserPromptSubmit", None); // → Thinking, fresh
        assert_eq!(
            authoritative_state_inner(false, "claude", "flagoff", AgentState::Idle),
            AgentState::Idle,
            "flag off must return the heuristic verbatim"
        );
    }

    /// §3.9: flag ON + STRONG backend + Fresh hook → the hook state is authoritative.
    #[test]
    fn claude_fresh_hook_wins_over_heuristic() {
        record_event("claude-fresh", "UserPromptSubmit", None); // → Thinking, fresh
        assert_eq!(
            authoritative_state_inner(true, "claude", "claude-fresh", AgentState::Idle),
            AgentState::Thinking,
            "a fresh hook state wins over the heuristic"
        );
    }

    /// §3.9: flag ON + STRONG backend but STALE hook → fall back to the heuristic.
    #[test]
    fn stale_hook_falls_back_to_heuristic() {
        record_event("claude-stale", "SessionStart", None); // → Starting
        backdate_for_test("claude-stale", HOOK_FRESHNESS.as_millis() as u64 + 1_000);
        assert_eq!(
            authoritative_state_inner(true, "claude", "claude-stale", AgentState::Idle),
            AgentState::Idle,
            "a stale hook state falls back to the heuristic (the floor)"
        );
    }

    /// §3.9: a non-STRONG backend (no hooks fire) always uses the heuristic; agy
    /// IS strong.
    #[test]
    fn backend_strength_gates_promotion() {
        record_event("codex-agent", "UserPromptSubmit", None); // → Thinking, fresh
        assert_eq!(
            authoritative_state_inner(true, "codex", "codex-agent", AgentState::Idle),
            AgentState::Idle,
            "codex is not a hook backend — heuristic only"
        );
        record_event("agy-agent", "UserPromptSubmit", None);
        assert_eq!(
            authoritative_state_inner(true, "agy", "agy-agent", AgentState::Idle),
            AgentState::Thinking,
            "agy is a STRONG backend — its fresh hook wins"
        );
    }

    /// Refinement round 1: `PreCompact` is TRANSIENT — recorded (event
    /// visible) but mapped to NO persistent state (the ContextFull-vs-Thinking
    /// shadow disagreement), and a subsequent real event takes over cleanly.
    #[test]
    fn precompact_is_transient_not_latched() {
        let derived = record_event("compact-test", "PreCompact", None);
        assert_eq!(derived, None, "PreCompact derives no persistent state");
        let snap = snapshot_for("compact-test").expect("event still recorded");
        assert_eq!(snap.last_event, "PreCompact");
        assert_eq!(snap.derived_state, None);
        assert_eq!(
            resolved_state_for("compact-test"),
            HookResolution::Stale,
            "unmapped observation resolves Stale (heuristic drives)"
        );
        // The next real event takes over (boot Starting analogue: replaced,
        // not lingering).
        record_event("compact-test", "UserPromptSubmit", None);
        assert_eq!(
            resolved_state_for("compact-test"),
            HookResolution::Fresh(AgentState::Thinking)
        );
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
