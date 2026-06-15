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
    /// #2044: STICKY timestamp of the last `UserPromptSubmit` — NOT overwritten
    /// by later events (unlike `at_ms`/`last_event`, which `record_event`
    /// replaces wholesale). A submitted prompt is the proof a daemon inject
    /// physically reached the prompt (a dialog-swallowed inject submits
    /// nothing → no `UserPromptSubmit`), so the inject-delivery watchdog needs
    /// it to survive the PreToolUse/Stop events that follow within its window.
    pub last_user_prompt_submit_ms: Option<u64>,
}

fn store() -> &'static Mutex<HashMap<String, HookShadow>> {
    static S: OnceLock<Mutex<HashMap<String, HookShadow>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// CR-2026-06-14: drop a deleted/redeployed agent's shadow entry. The global
/// `store()` is keyed by agent NAME and only ever inserted into
/// (`record_event`), so without an eviction path it grows one permanent entry
/// per distinctly-named agent ever seen, and a same-name redeploy inherits the
/// prior instance's last observation. Called from `full_delete_instance`.
///
/// Deliberately lifecycle-keyed, NOT an age-out sweep: the #1523 `ToolUse`
/// snapshot is allowed to outlive [`HOOK_FRESHNESS`] (event-pair closed, no
/// clock backstop), so an age-based `.retain` would wrongly demote a
/// long-running tool — eviction keys on agent deletion instead.
pub(crate) fn forget(name: &str) {
    store().lock().remove(name);
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
///
/// NO CLOCK BACKSTOP (reviewer-2 #2014, accepted as documented): a dropped
/// `PostToolUse` and a legitimately-long tool are INDISTINGUISHABLE in state —
/// both read heuristic-Idle + hook-ToolUse, which is exactly the shape the
/// promotion protects. Adding a max-age clock bound would re-admit the #1985
/// failure mode (a long tool demoted to a false idle-nudge) to "fix" a much
/// narrower one. The mitigation chain instead: `Stop` fires at every turn end
/// (so a double-drop — both PostToolUse AND Stop lost — is very narrow),
/// `SessionStart` on the next restart clears a stuck observation, and the
/// `#hook-shadow` log keeps it operator-visible. The trade is a DEFINITELY-fixed
/// false-nudge against a missed-nudge only on a double-drop corner — worth it.
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

/// #2016: is THIS backend's snapshot state currently DRIVEN by hooks (promoted),
/// vs still shadow-only? Same gate `authoritative_state` applies — for the
/// `#hook-shadow` log to describe the live disposition honestly.
pub fn is_promoted(backend_command: &str) -> bool {
    promotion_enabled() && crate::backend::Backend::parse_str(backend_command).has_state_hooks()
}

/// #1523 PROMOTION (phased v1): the authoritative `AgentState` written to the
/// daemon's per-tick SNAPSHOT (`snapshot.json`).
///
/// When the flag is on AND `backend_command` is a STRONG (hook-instrumented)
/// backend, a `Fresh` hook resolution WINS over the screen `heuristic`;
/// `Stale`/`Unknown` (or flag-off / a non-hook backend) fall back to `heuristic`
/// — **byte-identical** to pre-promotion. Hooks ENHANCE; the heuristic remains
/// the complete fallback path, so a backend without hooks (or a stale window) is
/// never worse off than before.
///
/// SCOPE (reviewer-2 #2014): this promotes the SNAPSHOT-scoped consumers — the
/// #1985 nudge surface: `dispatch_idle`, the pane-state badge, and anything that
/// reads `agent_state_of` / `snapshot.json`. It is NOT yet a global chokepoint.
/// Several per-tick deciders read the RAW screen heuristic
/// (`core.state.get_state()`) directly and are UNCHANGED in v1: supervisor
/// reactions (#1946), hang detection, the recovery dispatcher, the idle /
/// anti-stall watchdog, `conflict_notify`, and the `query` / `list` API (live
/// registry read).
/// Promoting those raw read sites is #1523 epic **phase-2** (post-soak). The
/// worst snapshot-vs-raw divergence is independently bounded by the #1999
/// throttle-gate and health-gating, so the phased boundary is safe for v1.
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
    let now = now_ms();
    let mut guard = store().lock();
    // #2044: carry the sticky UserPromptSubmit timestamp forward across the
    // wholesale overwrite, refreshing it only on a UserPromptSubmit event.
    let prior_ups = guard.get(name).and_then(|s| s.last_user_prompt_submit_ms);
    let last_user_prompt_submit_ms = if hook_event_name == "UserPromptSubmit" {
        Some(now)
    } else {
        prior_ups
    };
    guard.insert(
        name.to_string(),
        HookShadow {
            last_event: hook_event_name.to_string(),
            derived_state: derived,
            at_ms: now,
            last_user_prompt_submit_ms,
        },
    );
    derived
}

/// #2044: the sticky timestamp (epoch ms) of `name`'s last `UserPromptSubmit`,
/// or `None` if none recorded (no hook history / non-hook backend). The
/// inject-delivery watchdog compares it against an inject time to confirm the
/// inject reached the prompt.
pub fn last_user_prompt_submit_for(name: &str) -> Option<u64> {
    store()
        .lock()
        .get(name)
        .and_then(|s| s.last_user_prompt_submit_ms)
}

/// Test seam: age `name`'s observation by `ms` (freshness-expiry tests).
#[cfg(test)]
pub(crate) fn backdate_for_test(name: &str, ms: u64) {
    if let Some(snap) = store().lock().get_mut(name) {
        snap.at_ms = snap.at_ms.saturating_sub(ms);
    }
}

/// Test seam (#2044): pin `name`'s sticky last-UserPromptSubmit to an explicit
/// epoch-ms — lets the inject-delivery tests place the UPS deterministically
/// before/after a controlled inject time without clock-collision fragility.
#[cfg(test)]
pub(crate) fn set_user_prompt_submit_for_test(name: &str, ms: u64) {
    let mut guard = store().lock();
    let entry = guard.entry(name.to_string()).or_insert(HookShadow {
        last_event: "UserPromptSubmit".to_string(),
        derived_state: Some(crate::state::AgentState::Thinking),
        at_ms: ms,
        last_user_prompt_submit_ms: Some(ms),
    });
    entry.last_user_prompt_submit_ms = Some(ms);
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
    use serial_test::serial;

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

    /// §3.9: a non-STRONG backend (no hooks fire) always uses the heuristic. In
    /// v1 only claude is strong — agy is heuristic-only (configure_agy injects no
    /// hooks; production-verified 0 events).
    #[test]
    fn backend_strength_gates_promotion() {
        record_event("codex-agent", "UserPromptSubmit", None); // → Thinking, fresh
        assert_eq!(
            authoritative_state_inner(true, "codex", "codex-agent", AgentState::Idle),
            AgentState::Idle,
            "codex is not a hook backend — heuristic only"
        );
        // agy is NOT strong in v1 — even a (manually-injected) hook is ignored.
        record_event("agy-agent", "UserPromptSubmit", None);
        assert_eq!(
            authoritative_state_inner(true, "agy", "agy-agent", AgentState::Idle),
            AgentState::Idle,
            "agy is heuristic-only in v1 (no hook injection)"
        );
        // claude IS strong — its fresh hook wins.
        record_event("claude-agent", "UserPromptSubmit", None);
        assert_eq!(
            authoritative_state_inner(true, "claude", "claude-agent", AgentState::Idle),
            AgentState::Thinking,
            "claude is the v1 STRONG backend"
        );
    }

    /// §3.9 probe-6 (reviewer-2 #2014): the REAL env-gate wiring — the public
    /// `authoritative_state` resolves the flag through `promotion_enabled()`
    /// reading `AGEND_HOOK_STATE_POC`. `#[serial]` + an RAII guard handle the
    /// process-global env var (restored even on panic).
    #[test]
    #[serial(hook_state_poc)] // shares the AGEND_HOOK_STATE_POC env with mcp_config's flag test
    fn env_flag_gates_promotion_end_to_end() {
        struct EnvGuard(Option<String>);
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match &self.0 {
                    Some(v) => std::env::set_var("AGEND_HOOK_STATE_POC", v),
                    None => std::env::remove_var("AGEND_HOOK_STATE_POC"),
                }
            }
        }
        let _guard = EnvGuard(std::env::var("AGEND_HOOK_STATE_POC").ok());

        record_event("env-claude", "UserPromptSubmit", None); // → Thinking, fresh

        // Flag unset → heuristic (the byte-identical path), via the real gate.
        std::env::remove_var("AGEND_HOOK_STATE_POC");
        assert_eq!(
            authoritative_state("claude", "env-claude", AgentState::Idle),
            AgentState::Idle,
            "flag unset → heuristic (real env gate)"
        );

        // Flag = 1 → the fresh claude hook wins, through promotion_enabled().
        std::env::set_var("AGEND_HOOK_STATE_POC", "1");
        assert_eq!(
            authoritative_state("claude", "env-claude", AgentState::Idle),
            AgentState::Thinking,
            "flag=1 → the fresh claude hook wins via the real env gate"
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
