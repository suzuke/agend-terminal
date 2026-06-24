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
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// #t-26795: a process-global MONOTONIC sequence stamped on every
    /// [`record_event`]. The SRL hook-override compares an agent's latest active-hook
    /// seq against a per-episode FLOOR — a STRICTLY greater seq proves a NEW hook
    /// arrived since the floor was consumed (forward progress), which an epoch-ms
    /// timestamp cannot guarantee under wall-clock rollback. Only same-agent
    /// latest-vs-floor is ever compared, so the global counter needs no per-agent map.
    pub seq: u64,
}

fn store() -> &'static Mutex<HashMap<String, HookShadow>> {
    static S: OnceLock<Mutex<HashMap<String, HookShadow>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// #t-26795: the next process-global monotonic hook sequence. Starts at 1 so the
/// SRL-override floor's `unwrap_or(0)` default (an agent with no prior hook) is
/// strictly below any real seq.
fn next_seq() -> u64 {
    static SEQ: AtomicU64 = AtomicU64::new(1);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// CR-2026-06-14: drop a deleted/redeployed agent's shadow entry. The global
/// `store()` is keyed by agent NAME and only ever inserted into
/// (`record_event`), so without an eviction path it grows one permanent entry
/// per distinctly-named agent ever seen, and a same-name redeploy inherits the
/// prior instance's last observation. Called from `full_delete_instance`.
///
/// Deliberately lifecycle-keyed, NOT an age-out sweep: the #1523 open-tool
/// snapshot (`last_event == "PreToolUse"`) is allowed to outlive
/// [`HOOK_FRESHNESS`] (event-pair closed, no clock backstop), so an age-based
/// `.retain` would wrongly demote a long-running tool — eviction keys on agent
/// deletion instead.
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
/// #1523 (decision d-20260611051442661249-0): an open TOOL is the ONE EXCEPTION —
/// it is EVENT-PAIR-closed, not freshness-bounded. The exception now keys on the
/// OPENING EVENT (`last_event == "PreToolUse"`), NOT the derived state: post the
/// `Thinking`/`ToolUse`→`Active` merge the state can no longer distinguish a
/// tool-open from thinking, but only `PreToolUse` ever set the old `ToolUse`, so
/// keying on the event is exactly equivalent. `PreToolUse` opens the tool; only
/// the NEXT hook event (`PostToolUse`→Active / `Stop`→Idle) closes it, by
/// OVERWRITING this snapshot. So while the snapshot's `last_event` is still
/// `PreToolUse`, the tool is genuinely still running — a long (>HOOK_FRESHNESS)
/// tool must NOT be demoted back to the screen heuristic, which is exactly the
/// #1985 nudge class the promotion exists to fix. A crashed-mid-tool agent (no
/// closing event ever) is reaped by the liveness watchdog independently, so
/// leaving the open tool fresh is safe; the bound is the event, not the clock.
///
/// NO CLOCK BACKSTOP (reviewer-2 #2014, accepted as documented): a dropped
/// `PostToolUse` and a legitimately-long tool are INDISTINGUISHABLE in state —
/// both read heuristic-Idle + a hook snapshot whose `last_event` is `PreToolUse`,
/// which is exactly the shape the promotion protects. Adding a max-age clock bound
/// would re-admit the #1985
/// failure mode (a long tool demoted to a false idle-nudge) to "fix" a much
/// narrower one. The mitigation chain instead: `Stop` fires at every turn end
/// (so a double-drop — both PostToolUse AND Stop lost — is very narrow),
/// `SessionStart` on the next restart clears a stuck observation, and the
/// `#hook-shadow` log keeps it operator-visible. The trade is a DEFINITELY-fixed
/// false-nudge against a missed-nudge only on a double-drop corner — worth it.
pub fn resolved_state_for(name: &str) -> HookResolution {
    match snapshot_for(name) {
        Some(snap) => resolve_snapshot(&snap),
        None => HookResolution::Unknown,
    }
}

/// Pure freshness/state resolution over a SINGLE, already-read snapshot.
///
/// #t-26795 (r6 finding-3): split out so a caller that also needs a field of the
/// snapshot (e.g. [`fresh_active_hook_seq`]) resolves state AND that field from
/// the SAME clone under ONE `store()` lock. Calling `snapshot_for` and then
/// `resolved_state_for` separately re-locks and re-reads, which a concurrent
/// [`record_event`] can tear — pairing generation-A's `at_ms` with generation-B's
/// resolved state.
fn resolve_snapshot(snap: &HookShadow) -> HookResolution {
    let age_ms = now_ms().saturating_sub(snap.at_ms);
    match snap.derived_state {
        // #1523/#1985: a tool stays active EVENT-PAIR-closed — `PreToolUse` opens it and
        // only the next event (PostToolUse/Stop) closes it by overwriting this snapshot,
        // so a long (>HOOK_FRESHNESS) tool must NOT freshness-stale. Keyed on the opening
        // EVENT (not the now-merged Active state): only `PreToolUse` ever set the old
        // `derived_state == ToolUse`, so this is exactly equivalent post-merge.
        Some(state) if snap.last_event == "PreToolUse" => HookResolution::Fresh(state),
        Some(state) if age_ms <= HOOK_FRESHNESS.as_millis() as u64 => HookResolution::Fresh(state),
        _ => HookResolution::Stale,
    }
}

/// #t-26795 (SRL hook-override): the monotonic [`HookShadow::seq`] of the latest hook
/// event IF it resolves to a FRESH, ACTIVE state, else `None`. The supervisor's
/// ServerRateLimit retry compares this against the per-episode floor — a seq STRICTLY
/// greater than the floor proves a NEW tool-call/thinking hook arrived since the floor
/// was last consumed (forward progress → the agent is executing), so a sticky
/// screen-scraped `ServerRateLimit` is stale and the retry must not fire.
///
/// ACTIVE = `Active` only. `Idle` is deliberately EXCLUDED: a fresh
/// `Idle` is the turn-ENDED / prior-turn signal (ambiguous w.r.t. a brand-new SRL),
/// and idle-recovery is already covered by the supervisor's productive-output
/// `recovered` gate — so this stays the narrow "agent is provably mid-work" signal.
///
/// Flag-INDEPENDENT of `AGEND_HOOK_STATE_POC`: it reads the shadow snapshot that
/// [`record_event`] populates unconditionally for any hook event. (The hooks only
/// FIRE when the flag wires them into the backend — that is the data's presence, not
/// this read's gating; a missing snapshot simply yields `None` = fail-safe.)
pub fn fresh_active_hook_seq(name: &str) -> Option<u64> {
    use crate::state::AgentState;
    // #t-26795 (r6 finding-3): ONE snapshot clone — resolve the state AND read its
    // monotonic `seq` from the SAME generation. A second `resolved_state_for(name)`
    // would re-lock and re-read, which a concurrent `record_event` can tear (gen-A
    // seq paired with gen-B state).
    let snap = snapshot_for(name)?;
    match resolve_snapshot(&snap) {
        HookResolution::Fresh(AgentState::Active) => Some(snap.seq),
        _ => None,
    }
}

/// #t-26795: the agent's latest recorded hook seq (ANY state), or 0 if none. The
/// SRL-override seeds the per-episode floor with THIS at onset — so a hook recorded
/// BEFORE the rate-limit began (seq ≤ floor) can't override a genuine new SRL
/// (edge-a), while a hook recorded AFTER onset gets a strictly greater global seq.
pub fn latest_hook_seq(name: &str) -> u64 {
    snapshot_for(name).map(|s| s.seq).unwrap_or(0)
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

// #2413 (B): the #1523 `authoritative_state` SNAPSHOT promotion (claude-hook-only,
// `AGEND_HOOK_STATE_POC`-gated POC) was REMOVED here — superseded by the multi-backend
// Shadow Observer `observed_status` promotion at the snapshot chokepoint
// (`per_tick/snapshot.rs`, gated by `shadow::operated_dispatch_enabled` + the shared
// `shadow::gate`). The rest of this module's hook-shadow STORE stays LIVE: it feeds
// supervisor hang/recovery (`fresh_active_hook_seq`/`latest_hook_seq`/`record_event`),
// `inject_delivery` (`last_user_prompt_submit_for`), `recovery_shadow`/`divergence_telemetry`
// (`resolved_state_for`), and `hook_event` (`is_promoted`) — so `AGEND_HOOK_STATE_POC`,
// `promotion_enabled`, and `is_promoted` are intentionally KEPT (they are not the dead POC).

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Map a hook event to the `AgentState` it evidences. Derivations follow the
/// empirically-verified event semantics:
/// - `UserPromptSubmit` → Active (turn started)
/// - `PreToolUse` → Active (tool open); `PostToolUse` → Active (between tools)
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
        "UserPromptSubmit" => Some(AgentState::Active),
        "PreToolUse" => Some(AgentState::Active),
        "PostToolUse" => Some(AgentState::Active),
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
            seq: next_seq(),
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
        derived_state: Some(crate::state::AgentState::Active),
        at_ms: ms,
        last_user_prompt_submit_ms: Some(ms),
        seq: next_seq(),
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

    /// #t-26795: `fresh_active_hook_seq` returns a seq ONLY for a fresh ACTIVE hook
    /// (`Active`). Idle (turn-ended) is excluded; an aged-out non-tool Active hook is
    /// Stale → None; an open tool (`PreToolUse`) never stales (event-pair). Absent → None.
    #[test]
    #[serial]
    fn fresh_active_hook_seq_active_only() {
        record_event("fah-tooluse", "PreToolUse", None);
        assert!(
            fresh_active_hook_seq("fah-tooluse").is_some(),
            "an open tool is active"
        );
        record_event("fah-thinking", "PostToolUse", None);
        assert!(
            fresh_active_hook_seq("fah-thinking").is_some(),
            "a between-tools Active hook is active"
        );
        record_event("fah-idle", "Stop", None);
        assert!(
            fresh_active_hook_seq("fah-idle").is_none(),
            "Idle (turn-ended) is excluded — left to the productive-output recovery path"
        );
        assert!(
            fresh_active_hook_seq("fah-absent").is_none(),
            "no hook → None"
        );
        record_event("fah-stale", "PostToolUse", None);
        backdate_for_test("fah-stale", HOOK_FRESHNESS.as_millis() as u64 + 1000);
        assert!(
            fresh_active_hook_seq("fah-stale").is_none(),
            "a non-tool Active hook aged past freshness → Stale → None"
        );
    }

    /// #t-26795 (r6 finding-3): `fresh_active_hook_seq` resolves state and reads the
    /// `seq` from ONE snapshot clone via the pure `resolve_snapshot` seam, so the
    /// returned seq always belongs to the same generation whose state was resolved.
    /// The torn double-read this replaces is timing-dependent (needs a concurrent
    /// `record_event` to land between two locks), so this pins the seam + the
    /// seq↔state pairing contract — the race-freedom itself rests on the structural
    /// single-clone invariant, not on this test.
    #[test]
    #[serial]
    fn fresh_active_hook_returns_its_own_snapshot_seq() {
        record_event("f3-consistency", "PreToolUse", None);
        let snap = snapshot_for("f3-consistency").expect("recorded");
        assert_eq!(
            resolve_snapshot(&snap),
            HookResolution::Fresh(AgentState::Active),
            "pure resolver maps the cloned snapshot"
        );
        assert_eq!(
            fresh_active_hook_seq("f3-consistency"),
            Some(snap.seq),
            "returned seq is the SAME snapshot's seq (one clone, no torn read)"
        );
    }

    #[test]
    fn derive_map_covers_the_fragile_band() {
        assert_eq!(derive_state("PreToolUse", None), Some(AgentState::Active));
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
            Some(AgentState::Active)
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

    /// An open tool is EVENT-PAIR-closed: a long tool (`PreToolUse`, then no event
    /// past HOOK_FRESHNESS) stays Fresh(Active) — NOT demoted to the heuristic (the
    /// #1985 nudge class). The never-stale exception keys on `last_event ==
    /// "PreToolUse"`. Only PostToolUse/Stop closes it (by overwriting).
    #[test]
    fn long_tool_stays_tooluse_past_freshness() {
        record_event("longtool", "PreToolUse", None);
        backdate_for_test("longtool", HOOK_FRESHNESS.as_millis() as u64 + 600_000); // +10min past window
        assert_eq!(
            resolved_state_for("longtool"),
            HookResolution::Fresh(AgentState::Active),
            "a long tool stays Active until PostToolUse/Stop — never freshness-stale"
        );
        // PostToolUse closes the pair (last_event no longer PreToolUse → freshness applies).
        record_event("longtool", "PostToolUse", None);
        assert_eq!(
            resolved_state_for("longtool"),
            HookResolution::Fresh(AgentState::Active)
        );
    }

    /// state-merge (Thinking+ToolUse→Active) — pins the SRL-recovery freshness
    /// CONTRAST the `resolve_snapshot` re-key must preserve. Post-merge a tool-open
    /// AND a prompt-submit derive the SAME `Active`, so the never-freshness-stale
    /// exception keys on the OPENING EVENT (`last_event == "PreToolUse"`), NOT the
    /// now-ambiguous state: a long TOOL stays Fresh+active (the supervisor's
    /// `fresh_active_hook_seq` keeps suppressing a stale screen-SRL — the #1985
    /// class), while a long THINKING (UserPromptSubmit, no closing event) stales so
    /// the heuristic falls back in. Reverse-mutation: drop the `last_event ==
    /// "PreToolUse"` arm → the long-tool assertions go RED (event-key is load-bearing).
    #[test]
    fn long_active_freshness_keys_on_tool_open_event_not_merged_state() {
        // (a) long TOOL — PreToolUse opener aged well past the window → stays Fresh(Active).
        record_event("la-tool", "PreToolUse", None);
        backdate_for_test("la-tool", HOOK_FRESHNESS.as_millis() as u64 + 600_000);
        assert_eq!(
            resolved_state_for("la-tool"),
            HookResolution::Fresh(AgentState::Active),
            "a long tool (PreToolUse opener) must NOT freshness-stale (#1985, event-pair-closed)"
        );
        assert!(
            fresh_active_hook_seq("la-tool").is_some(),
            "the long tool must stay an active SRL-recovery signal (feeds the supervisor floor)"
        );
        // (b) long THINKING — same merged `Active`, same age, but UserPromptSubmit
        // (no tool-open, no closing event) → Stale. Distinguished ONLY by last_event.
        record_event("la-think", "UserPromptSubmit", None);
        backdate_for_test("la-think", HOOK_FRESHNESS.as_millis() as u64 + 600_000);
        assert_eq!(
            resolved_state_for("la-think"),
            HookResolution::Stale,
            "a long thinking (no tool-open) must freshness-stale — heuristic falls back in"
        );
        assert!(
            fresh_active_hook_seq("la-think").is_none(),
            "a staled thinking hook is not a fresh-active SRL-recovery signal"
        );
    }

    // #2413 (B): the `authoritative_state` / `authoritative_state_inner` promotion tests
    // (`flag_off_is_byte_identical_to_heuristic`, `claude_fresh_hook_wins_over_heuristic`,
    // `stale_hook_falls_back_to_heuristic`, `backend_strength_gates_promotion`,
    // `env_flag_gates_promotion_end_to_end`) were REMOVED with the functions they tested —
    // the #1523 snapshot promotion is superseded by the Shadow Observer gate (see the
    // removal note above `is_promoted`). The hook-shadow STORE tests (`resolved_state_for`,
    // freshness, `record_event`, `is_promoted`) remain — that infra is still live.

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
            HookResolution::Fresh(AgentState::Active)
        );
    }

    #[test]
    fn record_and_snapshot_roundtrip() {
        let derived = record_event("shadow-test-agent", "PreToolUse", None);
        assert_eq!(derived, Some(AgentState::Active));
        let snap = snapshot_for("shadow-test-agent").expect("recorded");
        assert_eq!(snap.last_event, "PreToolUse");
        assert_eq!(snap.derived_state, Some(AgentState::Active));
        assert!(snap.at_ms > 0);
        assert!(snapshot_for("never-seen").is_none());
    }
}
