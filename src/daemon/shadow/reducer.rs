//! #2413 Shadow Observer — Phase B reducer (Evidence → `ObservedStatus`).
//!
//! Folds three out-of-path signals into ONE status with precedence + decay + a
//! liveness backstop:
//!   1. typed hook-plane [`Evidence`] (real, `authority=Hook`) — the rich source;
//!   2. the screen-scrape `agent_state`, wrapped as a `Screen`-authority signal — the
//!      baseline we measure against (and override when hooks are fresher);
//!   3. cheap lsof/process liveness (`api_in_flight`, productive-silence, child-alive)
//!      — the BACKSTOP that decays a phantom-stuck state.
//!
//! The hard problem (§4 of SHADOW-OBSERVER-ARCH-2413.md): hook delivery is best-effort,
//! so a closing event (`Stop`/`PostToolUse`) can DROP — leaving an episode/tool span
//! open forever and the agent stuck reporting `Active`/`ToolUse` while it's idle. Time
//! alone never flips state; only liveness that CONTRADICTS an open span reconciles it.
//!
//! [`AgentRuntime`] is the per-agent accumulator; [`AgentRuntime::ingest`] folds one
//! Evidence and [`AgentRuntime::observe`] derives the status from the current screen +
//! liveness snapshot. Both are pure over their inputs (no globals) so the whole state
//! machine is unit-testable without a daemon; the per-tick driver (OUT OF SCOPE here)
//! supplies the snapshot under one `core.lock()`. Runs whenever the Shadow Observer is
//! enabled (default-ON; gated off by the `AGEND_SHADOW_OBSERVER=0` kill-switch).

// The whole reducer surface (ObservedStatus / AgentRuntime / ScreenSignal / Liveness +
// ObservedState::coarse) is now consumed by the Phase-B per-tick driver
// (`per_tick::shadow_observe` → `shadow::observe`) + the §5 correction telemetry +
// the additive list_instances surface — so the Phase-A blanket `#![allow(dead_code)]`
// is gone (driver landed, SHADOW-OBSERVER-ARCH-2413.md §6 step 3/5).

use super::evidence::{Authority, Confidence, Evidence, EvidenceKind};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// No hook Evidence within this window ⇒ the hook plane is stale; fall back to the
/// screen baseline. Mirrors `hook_shadow::HOOK_FRESHNESS` so the two planes age alike.
const HOOK_FRESHNESS_MS: u64 = 600_000;

/// Productive-output silence past which a screen-Idle + no-api contradiction is allowed
/// to reconcile an open episode/span to Idle. Long enough that a normal inter-tool gap
/// (model thinking between tools, output still flowing) does NOT trip it, short enough
/// that a genuinely-dropped terminal hook is caught within a few ticks.
const RECONCILE_SILENCE_MS: u64 = 8_000;

/// An open hook episode with NO fresh hook for this long is one *precondition* (not the
/// whole trigger) for the last-resort reconcile: its closing hook (Stop/PostToolUse) may
/// have dropped. Past this, a screen-Idle + sustained silence reconciles to Idle **even
/// if `api_in_flight` is still true** — the lsof signal is a proven-unreliable idle
/// *blocker* (lingering / CDN-shared sockets stay ESTABLISHED long after a turn ends, see
/// SHADOW-OBSERVER-QUANT-2413.md), so it is WEAK-POSITIVE-ONLY and must never wedge an
/// ended turn at Active forever (#2433 r6).
///
/// Staleness ALONE is not sufficient, because a long model-think is ALSO screen-idle +
/// productive-silent + hook-silent (claude fires no hooks between `TurnStarted` and the
/// first tool/Stop, and streams no output while thinking) — reconciling on time alone
/// would re-introduce the very false-idle this plane exists to beat (#2433 r6 round-2).
/// So [`AgentRuntime::reconcile_to_idle`] additionally requires the episode to have
/// PRODUCED output and then fallen quiet (a finished turn), never reconciling a
/// produced-nothing-yet think. Shorter than [`HOOK_FRESHNESS_MS`] (the full
/// screen-fallback): the produced-then-quiet episode decays before the plane is wholly stale.
const EPISODE_STALE_MS: u64 = 300_000;

/// Bound on the explain-trail kept per agent (last-N justifying evidence).
const TRAIL_CAP: usize = 8;

/// The reducer's normalized state. MVP = `Idle`/`Active`/`WaitingForUser` (high
/// reliability); the `Active` refinements (`ToolUse`/`Responding`/`Thinking`) are
/// emitted only when evidence is unambiguous — otherwise the reducer conservatively
/// reports the coarse `Active`. `RateLimited` carries from the screen baseline this
/// phase (the hook + API planes are blind to it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedState {
    Idle,
    /// Coarse "working" — used when a turn is open but the sub-state is ambiguous.
    Active,
    /// Refined `Active`: a turn is open with no tool span and no fresh output.
    Thinking,
    /// Refined `Active`: a tool invocation is open (no matching `ToolEnded`).
    ToolUse,
    /// Refined `Active`: assistant output streaming (Stream plane — mostly N/A this
    /// phase; folds into `Active` unless a `Responding` evidence is fresh).
    Responding,
    /// Blocked awaiting a human decision — the proxy-invisible state.
    WaitingForUser,
    RateLimited,
}

impl ObservedState {
    /// MVP coarsening: every refined `Active` sub-state collapses to `Active`. Used by
    /// the additive surface when only the 3 high-reliability buckets are wanted.
    pub fn coarse(self) -> ObservedState {
        match self {
            ObservedState::Thinking | ObservedState::ToolUse | ObservedState::Responding => {
                ObservedState::Active
            }
            other => other,
        }
    }
}

/// A bounded reference to a piece of evidence that justified the current state — the
/// explain-trail, for debugging + the quantification diff (not the whole buffer).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRef {
    pub kind: String,
    pub authority: Authority,
    pub at_ms: u64,
}

/// The additive status hung beside `agent_state` (never replacing it). `since_ms` is
/// STABLE — only reset when `state` actually changes — so "how long Active/Waiting" is
/// meaningful across re-derives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedStatus {
    pub state: ObservedState,
    pub confidence: Confidence,
    pub authority: Authority,
    pub evidence: Vec<EvidenceRef>,
    pub since_ms: u64,
}

/// Coarse screen signal the reducer consumes — the driver maps the 18-variant
/// `crate::state::AgentState` into these buckets so the reducer doesn't couple to the
/// whole enum. (See `SHADOW-OBSERVER-ARCH-2413.md` §7 for the AgentState→ScreenSignal map.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenSignal {
    /// Prompt-ready / nothing rendering.
    Idle,
    /// ToolUse / Thinking / Starting / Restarting — actively rendering work.
    Working,
    /// PermissionPrompt / InteractivePrompt / AwaitingOperator — a human gate.
    Approval,
    /// RateLimit / ServerRateLimit / UsageLimit.
    RateLimited,
    /// Hang / Crashed / errors / anything non-decisive for liveness reconcile.
    Other,
}

/// Cheap out-of-path liveness snapshot for the backstop (all already computed per-agent
/// elsewhere — see arch §7). `productive_silent_ms` is the sustained-quiet measure the
/// reconcile uses (no separate screen-idle-since tracking needed).
#[derive(Debug, Clone, Copy)]
pub struct Liveness {
    /// lsof: a live socket to an LLM endpoint (the strongest false-idle beat).
    pub api_in_flight: bool,
    /// Since the last *productive* output (F9-gated), in ms.
    pub productive_silent_ms: u64,
    /// The agent's child process still exists.
    pub child_alive: bool,
}

/// Per-agent accumulator. Folded with hook Evidence over time (`ingest`), then queried
/// for a status (`observe`). Cheap + `Clone` for snapshotting.
#[derive(Debug, Clone, Default)]
pub struct AgentRuntime {
    episode_open: bool,
    episode_since_ms: u64,
    tool_open: Option<String>,
    tool_since_ms: u64,
    waiting: bool,
    waiting_since_ms: u64,
    /// Absolute epoch-ms the rate-limit clears (from `RateLimited.retry_at_ms`), if any.
    rate_limited_until_ms: Option<u64>,
    /// Newest real-time OBSERVER-plane evidence timestamp (`Hook` for claude, `Stream` for
    /// codex) — drives the freshness fallback. #2413 Phase D generalized this from Hook-only
    /// so the codex rollout (`Stream`) plane gets parity; claude has ONLY `Hook`, so its
    /// behavior is unchanged (this is `max` of Hook timestamps exactly as before).
    last_observer_ms: u64,
    /// Which observer plane produced that newest evidence — the authority the active-family
    /// status is LABELED with when the observer plane is fresh (`Hook`→claude, `Stream`→
    /// codex). `None` until any Hook/Stream evidence arrives (then the Idle/Screen fallback
    /// labels apply). Screen/lsof/Inferred are NOT observer planes and never set this.
    last_observer_authority: Option<Authority>,
    /// Newest `Responding` evidence — distinguishes the `Responding` refinement.
    last_responding_ms: u64,
    /// Stable-`since` bookkeeping: the last derived state + when it was entered.
    last_state: Option<ObservedState>,
    last_state_since_ms: u64,
    /// Bounded explain-trail (newest last).
    trail: VecDeque<EvidenceRef>,
}

impl AgentRuntime {
    /// Fold one piece of Evidence into the accumulator. Idempotent-ish: replaying the
    /// same terminal event twice is harmless (close is monotone).
    pub fn ingest(&mut self, ev: &Evidence) {
        // #2413 Phase D: track the newest REAL-TIME observer evidence + which plane it came
        // from (`Hook`=claude lifecycle hooks, `Stream`=codex rollout tail). Generalized from
        // the prior Hook-only `last_hook_ms.max(at_ms)`; for a claude (Hook-only) agent this
        // is byte-identical (same max, authority always `Hook`). Screen/lsof/Inferred are not
        // observer planes and never advance the freshness clock.
        if matches!(ev.authority, Authority::Hook | Authority::Stream)
            && ev.at_ms >= self.last_observer_ms
        {
            self.last_observer_ms = ev.at_ms;
            self.last_observer_authority = Some(ev.authority);
        }
        match &ev.kind {
            EvidenceKind::TurnStarted => {
                self.open_episode(ev.at_ms);
                // A fresh turn supersedes a prior unresolved approval.
                self.clear_waiting();
            }
            EvidenceKind::TurnEnded { .. } | EvidenceKind::SessionExited => {
                self.close_episode();
            }
            // An idle prompt is the agent telling us the turn is over and it's ready.
            EvidenceKind::PromptReady => {
                self.close_episode();
            }
            EvidenceKind::ToolStarted { name } => {
                // A tool implies an active turn even if the TurnStarted hook dropped.
                if !self.episode_open {
                    self.open_episode(ev.at_ms);
                }
                self.tool_open = Some(name.clone().unwrap_or_default());
                self.tool_since_ms = ev.at_ms;
            }
            EvidenceKind::ToolEnded => {
                self.tool_open = None;
                // Tool proceeded ⇒ any approval that gated it is resolved.
                self.clear_waiting();
            }
            EvidenceKind::ApprovalRequired => {
                self.waiting = true;
                self.waiting_since_ms = ev.at_ms;
                if !self.episode_open {
                    self.open_episode(ev.at_ms);
                }
            }
            EvidenceKind::RateLimited { retry_at_ms } => {
                self.rate_limited_until_ms = *retry_at_ms;
            }
            EvidenceKind::Responding => {
                self.last_responding_ms = ev.at_ms;
                if !self.episode_open {
                    self.open_episode(ev.at_ms);
                }
            }
            // No state effect — accounting only.
            EvidenceKind::TokenUsage { .. } => {}
        }
        self.push_trail(ev);
    }

    fn open_episode(&mut self, at_ms: u64) {
        if !self.episode_open {
            self.episode_open = true;
            self.episode_since_ms = at_ms;
        }
    }

    fn close_episode(&mut self) {
        self.episode_open = false;
        self.tool_open = None;
        self.clear_waiting();
    }

    fn clear_waiting(&mut self) {
        self.waiting = false;
    }

    fn push_trail(&mut self, ev: &Evidence) {
        if self.trail.len() >= TRAIL_CAP {
            self.trail.pop_front();
        }
        self.trail.push_back(EvidenceRef {
            kind: evidence_kind_tag(&ev.kind).to_string(),
            authority: ev.authority,
            at_ms: ev.at_ms,
        });
    }

    /// Derive the status from the accumulator + the current screen + liveness snapshot.
    /// `now_ms` is the decision time (decay/freshness reference). Mutates only the
    /// `since_ms` bookkeeping (so `&mut self`); the state logic itself is a pure
    /// function of (accumulator, screen, live, now).
    pub fn observe(
        &mut self,
        screen: ScreenSignal,
        live: &Liveness,
        now_ms: u64,
    ) -> ObservedStatus {
        let (state, authority, confidence) = self.derive(screen, live, now_ms);

        // Stable `since`: only move the clock when the state actually changes.
        let since_ms = if self.last_state == Some(state) {
            self.last_state_since_ms
        } else {
            self.last_state = Some(state);
            self.last_state_since_ms = now_ms;
            now_ms
        };

        ObservedStatus {
            state,
            confidence,
            authority,
            evidence: self.trail.iter().cloned().collect(),
            since_ms,
        }
    }

    /// The pure state derivation: precedence + liveness backstop reconcile.
    fn derive(
        &self,
        screen: ScreenSignal,
        live: &Liveness,
        now_ms: u64,
    ) -> (ObservedState, Authority, Confidence) {
        // A dead process trumps everything — the agent isn't running.
        if !live.child_alive {
            return (
                ObservedState::Idle,
                Authority::ProcessHeuristic,
                Confidence::Strong,
            );
        }

        // (P1) Rate-limited: hook `RateLimited` window still open, or the screen says so.
        let rate_limited = self
            .rate_limited_until_ms
            .is_some_and(|until| until > now_ms)
            || screen == ScreenSignal::RateLimited;
        if rate_limited {
            let auth = if matches!(self.rate_limited_until_ms, Some(u) if u > now_ms) {
                Authority::Hook
            } else {
                Authority::Screen
            };
            return (ObservedState::RateLimited, auth, Confidence::Strong);
        }

        // (P2) Waiting for a human. Hook `ApprovalRequired` is `Confirmed`; the screen
        // prompt alone is `Strong`. A sustained-idle + no-api contradiction can clear a
        // phantom-waiting (the approval was answered without a closing hook).
        let waiting = self.waiting || screen == ScreenSignal::Approval;
        if waiting && !self.reconcile_to_idle(screen, live, now_ms) {
            let (auth, conf) = if self.waiting {
                (Authority::Hook, Confidence::Confirmed)
            } else {
                (Authority::Screen, Confidence::Strong)
            };
            return (ObservedState::WaitingForUser, auth, conf);
        }

        // Observer-plane freshness: if no Hook/Stream evidence in the window, the real-time
        // observer plane is stale — defer to the screen baseline (Weak), the conservative
        // fallback. (#2413 Phase D: generalized from Hook-only to {Hook|Stream}.)
        let observer_fresh = now_ms.saturating_sub(self.last_observer_ms) <= HOOK_FRESHNESS_MS
            && self.last_observer_ms > 0;

        // (P3/P4/P5) Active family — only if an episode/tool is open AND liveness does
        // NOT contradict it (the phantom-stuck decay). If it DOES contradict, fall to Idle.
        let active_open = self.episode_open || self.tool_open.is_some();
        if active_open && !self.reconcile_to_idle(screen, live, now_ms) {
            // Mid-API false-idle beat: screen looks idle but a fresh observer episode (hook
            // or stream) + a live socket prove work in flight → keep Active. This is the
            // headline win over raw screen-scrape (now claude AND codex — #2413 Phase D).
            let mid_api_false_idle =
                screen == ScreenSignal::Idle && observer_fresh && live.api_in_flight;

            let authority = if observer_fresh {
                // Label with the plane that produced the fresh evidence: `Hook` for claude,
                // `Stream` for codex (#2413 Phase D). Falls back to `Hook` only in the
                // can't-happen case of observer_fresh with no recorded authority.
                self.last_observer_authority.unwrap_or(Authority::Hook)
            } else {
                Authority::Screen
            };
            let confidence = if observer_fresh {
                Confidence::Strong
            } else {
                Confidence::Probable
            };

            // Refine only when unambiguous; else the coarse `Active`.
            let state = if self.tool_open.is_some() {
                ObservedState::ToolUse
            } else if now_ms.saturating_sub(self.last_responding_ms) < RECONCILE_SILENCE_MS
                && self.last_responding_ms > 0
            {
                ObservedState::Responding
            } else if self.tool_open.is_none()
                && !self.waiting
                && live.productive_silent_ms >= RECONCILE_SILENCE_MS
            {
                // Turn open, no tool, no approval, quiet output → derived Thinking.
                ObservedState::Thinking
            } else if mid_api_false_idle {
                // Screen idle but provably active — don't claim a refinement, just Active.
                ObservedState::Active
            } else {
                ObservedState::Active
            };
            return (state, authority, confidence);
        }

        // (P6) Idle — nothing open, or an open span reconciled away.
        // If we reconciled an open span (hooks said active, liveness disagreed), mark
        // the inference so a consumer can see the dropped-terminal-hook recovery.
        if active_open {
            // active_open but we're here ⇒ reconcile_to_idle fired.
            return (
                ObservedState::Idle,
                Authority::Inferred,
                Confidence::Probable,
            );
        }
        // Genuinely idle. Authority is the screen unless a hook PromptReady/TurnEnded
        // is what closed us — either way Idle is well-supported.
        let conf = if observer_fresh {
            Confidence::Strong
        } else {
            Confidence::Weak
        };
        (ObservedState::Idle, Authority::Screen, conf)
    }

    /// The liveness backstop: an open episode/span should be force-closed to Idle when
    /// liveness CONTRADICTS it. Both paths require the screen back at the prompt + output
    /// quiet past [`RECONCILE_SILENCE_MS`] — a normal mid-turn pause (output still
    /// flowing) never decays.
    ///
    /// Two ways an open episode is contradicted:
    /// 1. **No live socket either** (`!api_in_flight`) — screen-idle + no-api + silence is
    ///    the clean contradiction: the turn is genuinely over (catches a dropped
    ///    `Stop`/`PostToolUse` even on an open tool span — a dead socket means no work).
    /// 2. **Socket stuck `true`, but corroborated finished** — `api_in_flight` lingers
    ///    ESTABLISHED after a turn (and can false-positive on CDN-shared telemetry IPs;
    ///    the quantification proved this), so it is WEAK-POSITIVE-ONLY: it must neither
    ///    *block* an idle reconcile forever NOR be ignored so eagerly that a still-working
    ///    turn reads Idle. Reconcile only with (a) no open tool span, (b) a stale hook
    ///    plane (≥ [`EPISODE_STALE_MS`] ⇒ dropped close), and (c) produced-then-quiet (a
    ///    delivered response). #2433 r6 rounds 1-3 pin each of these.
    fn reconcile_to_idle(&self, screen: ScreenSignal, live: &Liveness, now_ms: u64) -> bool {
        if screen != ScreenSignal::Idle || live.productive_silent_ms < RECONCILE_SILENCE_MS {
            return false;
        }
        // Path 1: no live socket → clean contradiction.
        if !live.api_in_flight {
            return true;
        }
        // Path 2: socket stuck true (lingering). `api_in_flight` is weak-positive-only, so
        // it must not BLOCK a reconcile forever — but it also must not be ignored so
        // eagerly that a still-working turn is called Idle. THREE conditions, all required:
        //  (a) NO open tool span — an open tool can legitimately run SILENTLY for minutes
        //      (a long Bash/build), so a live socket + open tool = genuinely `ToolUse`; we
        //      never reconcile it here (#2433 r6 round-3). A dropped `PostToolUse` is still
        //      caught by Path 1 once the socket dies (no-api is a clean contradiction).
        //      Empirically a *running* tool reads screen=Working (verified: `sleep 22`
        //      held agent_state=`thinking` for its whole 23s — SHADOW-OBSERVER-QUANT-2413),
        //      so the screen==Idle precondition above already blocks reconcile for a live
        //      tool; (a) only guards the artificial screen==Idle + open-tool worst-case.
        //      DOCUMENTED RESIDUAL (the out-of-path limit, only an in-path proxy fully
        //      closes it): if `PostToolUse` drops WHILE the socket lingers (api stuck true)
        //      and the screen has returned to Idle, the span stays `ToolUse` until the next
        //      hook (Stop / next `TurnStarted` / `PromptReady`) — bounded + self-healing.
        //  (b) the hook plane is stale (no fresh hook for ≥ EPISODE_STALE_MS ⇒ a closing
        //      Stop was almost certainly dropped), AND
        //  (c) the episode actually PRODUCED output and THEN fell quiet — i.e. the turn
        //      delivered its answer and looks finished. A turn silent SINCE IT STARTED
        //      (last productive output predates `episode_since`) is still thinking and
        //      STAYS Active no matter how long (#2433 r6 round-2) — productive-silence is
        //      the `Thinking` signature, so reconciling on it alone would re-create the
        //      very false-idle this plane exists to beat.
        let observer_stale = self.last_observer_ms > 0
            && now_ms.saturating_sub(self.last_observer_ms) >= EPISODE_STALE_MS;
        let last_output_ms = now_ms.saturating_sub(live.productive_silent_ms);
        let episode_produced_output = last_output_ms >= self.episode_since_ms;
        self.tool_open.is_none() && observer_stale && episode_produced_output
    }
}

/// Stable short tag for an `EvidenceKind` (the explain-trail label).
fn evidence_kind_tag(kind: &EvidenceKind) -> &'static str {
    match kind {
        EvidenceKind::TurnStarted => "turn_started",
        EvidenceKind::Responding => "responding",
        EvidenceKind::TurnEnded { .. } => "turn_ended",
        EvidenceKind::ToolStarted { .. } => "tool_started",
        EvidenceKind::ToolEnded => "tool_ended",
        EvidenceKind::ApprovalRequired => "approval_required",
        EvidenceKind::RateLimited { .. } => "rate_limited",
        EvidenceKind::TokenUsage { .. } => "token_usage",
        EvidenceKind::PromptReady => "prompt_ready",
        EvidenceKind::SessionExited => "session_exited",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn hook(kind: EvidenceKind, at_ms: u64) -> Evidence {
        Evidence::hook(kind, at_ms)
    }

    /// Liveness with a live agent, no api socket, quiet, child alive — the "idle screen"
    /// baseline most reconcile tests start from.
    fn live_quiet(productive_silent_ms: u64) -> Liveness {
        Liveness {
            api_in_flight: false,
            productive_silent_ms,
            child_alive: true,
        }
    }

    fn live_busy() -> Liveness {
        Liveness {
            api_in_flight: true,
            productive_silent_ms: 0,
            child_alive: true,
        }
    }

    #[test]
    fn turn_then_idle_basic() {
        let mut rt = AgentRuntime::default();
        rt.ingest(&hook(EvidenceKind::TurnStarted, 1_000));
        let s = rt.observe(ScreenSignal::Working, &live_busy(), 1_100);
        assert!(matches!(
            s.state,
            ObservedState::Active | ObservedState::Thinking
        ));
        rt.ingest(&hook(EvidenceKind::TurnEnded { stop_reason: None }, 2_000));
        let s = rt.observe(ScreenSignal::Idle, &live_quiet(0), 2_100);
        assert_eq!(s.state, ObservedState::Idle);
    }

    #[test]
    fn tool_span_is_tooluse() {
        let mut rt = AgentRuntime::default();
        rt.ingest(&hook(EvidenceKind::TurnStarted, 1_000));
        rt.ingest(&hook(
            EvidenceKind::ToolStarted {
                name: Some("Bash".into()),
            },
            1_100,
        ));
        let s = rt.observe(ScreenSignal::Working, &live_busy(), 1_200);
        assert_eq!(s.state, ObservedState::ToolUse);
        assert_eq!(s.authority, Authority::Hook);
    }

    /// THE headline reconcile: a dropped `Stop` hook leaves the episode open, but the
    /// screen is back at the prompt + no api socket + quiet ⇒ decay to Idle (Inferred).
    #[test]
    fn dropped_stop_reconciles_to_idle() {
        let mut rt = AgentRuntime::default();
        rt.ingest(&hook(EvidenceKind::TurnStarted, 1_000));
        // ... no TurnEnded ever arrives (dropped). Much later, screen is idle + quiet.
        let s = rt.observe(ScreenSignal::Idle, &live_quiet(30_000), 60_000);
        assert_eq!(s.state, ObservedState::Idle, "phantom episode must decay");
        assert_eq!(s.authority, Authority::Inferred, "marked as a reconcile");
    }

    /// A dropped `PostToolUse` leaves the tool span open; same contradiction closes it.
    #[test]
    fn dropped_posttool_reconciles_tool_span() {
        let mut rt = AgentRuntime::default();
        rt.ingest(&hook(EvidenceKind::TurnStarted, 1_000));
        rt.ingest(&hook(
            EvidenceKind::ToolStarted {
                name: Some("Bash".into()),
            },
            1_100,
        ));
        // PostToolUse + Stop dropped. Screen idle, quiet, no api.
        let s = rt.observe(ScreenSignal::Idle, &live_quiet(20_000), 40_000);
        assert_eq!(s.state, ObservedState::Idle);
        assert_eq!(s.authority, Authority::Inferred);
    }

    /// The mid-API false-idle BEAT: screen renders idle mid-request, but a fresh hook
    /// episode + a live socket prove work in flight ⇒ stay Active (raw screen-scrape
    /// would wrongly say Idle here — this is the quantified win).
    #[test]
    fn mid_api_false_idle_stays_active() {
        let mut rt = AgentRuntime::default();
        rt.ingest(&hook(EvidenceKind::TurnStarted, 1_000));
        let s = rt.observe(ScreenSignal::Idle, &live_busy(), 1_500);
        assert_ne!(
            s.state,
            ObservedState::Idle,
            "live socket beats idle screen"
        );
        assert_eq!(s.authority, Authority::Hook);
    }

    /// #2413 Phase D: a codex agent observed via the `Stream` plane gets PARITY with claude
    /// — the freshness/authority gate is generalized to {Hook|Stream}, so the active-family
    /// status is labeled `authority=Stream` (NOT the `Screen` fallback) and the mid-API
    /// false-idle beat fires for it too. The codex analogue of `mid_api_false_idle_stays_active`.
    #[test]
    fn codex_stream_evidence_gets_stream_authority_and_mid_api_beat() {
        let mut rt = AgentRuntime::default();
        rt.ingest(&Evidence::stream(EvidenceKind::TurnStarted, 1_000));
        let s = rt.observe(ScreenSignal::Idle, &live_busy(), 1_500);
        assert_ne!(
            s.state,
            ObservedState::Idle,
            "fresh Stream episode + live socket beats idle screen (codex mid-API beat)"
        );
        assert_eq!(
            s.authority,
            Authority::Stream,
            "codex active status is labeled Stream, not the Screen fallback"
        );
    }

    /// #2413 Phase D guardrail: generalizing the freshness gate to {Hook|Stream} must leave
    /// a HOOK-ONLY (claude) agent BYTE-IDENTICAL — its active status stays `authority=Hook`.
    /// (The whole existing claude reducer suite also pins this; this is the explicit
    /// Phase-D no-regression anchor the lead asked for.)
    #[test]
    fn hook_only_agent_unchanged_after_phase_d_generalization() {
        let mut rt = AgentRuntime::default();
        rt.ingest(&hook(EvidenceKind::TurnStarted, 1_000));
        let s = rt.observe(ScreenSignal::Idle, &live_busy(), 1_500);
        assert_eq!(
            s.authority,
            Authority::Hook,
            "claude (Hook-only) authority unaffected by the {{Hook|Stream}} generalization"
        );
    }

    /// #2433 r6 REJECT (verbatim reviewer test): a dropped terminal hook + a STUCK
    /// `api_in_flight=true` (lingering socket — the quantification proved sockets stay
    /// ESTABLISHED after a turn) must NOT wedge the ended turn at Active forever. Once the
    /// hook plane is stale + screen Idle + productive silence, `api_in_flight` is ignored
    /// (weak-positive-only) and the episode reconciles to Idle.
    #[test]
    fn review_pr2433_stale_api_socket_does_not_keep_dropped_stop_active_forever() {
        let mut rt = AgentRuntime::default();
        rt.ingest(&hook(EvidenceKind::TurnStarted, 1_000));
        let live = Liveness {
            api_in_flight: true,
            productive_silent_ms: 60_000,
            child_alive: true,
        };
        let s = rt.observe(
            ScreenSignal::Idle,
            &live,
            1_000 + HOOK_FRESHNESS_MS + 60_000,
        );
        assert_eq!(
            s.state,
            ObservedState::Idle,
            "stale api_in_flight alone must not keep a dropped terminal hook active forever"
        );
    }

    /// The symmetric guard for the #2433 fix: a FRESH hook episode (recent TurnStarted,
    /// hook age < EPISODE_STALE_MS) + `api_in_flight=true` + screen Idle + silence ≥ 8s —
    /// i.e. the real mid-API thinking phase from the quantification — must STILL stay
    /// Active. `api_in_flight` as weak-positive must not be reconciled away while hooks are
    /// fresh, or the fix would have regressed the headline mid-API false-idle win.
    #[test]
    fn fresh_episode_with_live_socket_stays_active_despite_silence() {
        let mut rt = AgentRuntime::default();
        rt.ingest(&hook(EvidenceKind::TurnStarted, 1_000));
        let live = Liveness {
            api_in_flight: true,
            productive_silent_ms: 30_000,
            child_alive: true,
        };
        // Hook age 100s ≪ EPISODE_STALE_MS (300s) ⇒ fresh ⇒ no last-resort reconcile.
        let s = rt.observe(ScreenSignal::Idle, &live, 1_000 + 100_000);
        assert_ne!(
            s.state,
            ObservedState::Idle,
            "fresh episode + live socket = mid-API in flight → stay Active, not reconciled"
        );
    }

    /// #2433 r6 round-2 (verbatim reviewer test): a REAL long silent API turn — a turn
    /// that has been thinking >300s with `api_in_flight=true` and has NEVER produced output
    /// (productive-silence spans the whole episode) — must NOT be reconciled to Idle by the
    /// last-resort path. Productive-silence is the `Thinking` signature, so the stale-hook
    /// reconcile additionally requires the episode to have produced-then-quieted; a
    /// produced-nothing-yet think stays Active no matter how long.
    #[test]
    fn review_pr2433_long_silent_api_turn_over_300s_currently_reconciles_idle() {
        let mut rt = AgentRuntime::default();
        rt.ingest(&hook(EvidenceKind::TurnStarted, 1_000));
        let live = Liveness {
            api_in_flight: true,
            productive_silent_ms: 301_000,
            child_alive: true,
        };
        let s = rt.observe(ScreenSignal::Idle, &live, 1_000 + EPISODE_STALE_MS + 1);
        assert_ne!(
            s.state,
            ObservedState::Idle,
            "a real >300s silent API turn would be misclassified idle by the last-resort path"
        );
    }

    /// #2433 r6 round-3 (verbatim reviewer test): an OPEN tool span (a long-running silent
    /// tool — e.g. a multi-minute Bash/build) with a live API socket must stay `ToolUse`,
    /// NOT be reconciled to Idle just because the turn produced output earlier. A live
    /// socket + open tool span = genuinely working; the api-stuck last-resort excludes open
    /// tools (a dropped `PostToolUse` is still caught by the no-api Path 1 when the socket
    /// dies).
    #[test]
    fn review_pr2433_long_silent_open_tool_after_output_stays_tooluse() {
        let mut rt = AgentRuntime::default();
        rt.ingest(&hook(EvidenceKind::TurnStarted, 1_000));
        rt.ingest(&hook(EvidenceKind::Responding, 2_000));
        rt.ingest(&hook(
            EvidenceKind::ToolStarted {
                name: Some("Bash".into()),
            },
            3_000,
        ));
        let now_ms = 3_000 + EPISODE_STALE_MS + 1;
        let live = Liveness {
            api_in_flight: true,
            productive_silent_ms: now_ms - 2_000,
            child_alive: true,
        };
        let s = rt.observe(ScreenSignal::Idle, &live, now_ms);
        assert_eq!(
            s.state,
            ObservedState::ToolUse,
            "an open tool span with a live API socket must not be reconciled to idle just because prior output exists"
        );
    }

    /// Time alone (no contradicting liveness — socket still live) must NOT decay an open
    /// span. Guards against over-eager TTL decay.
    #[test]
    fn ttl_open_span_without_liveness_does_not_flip() {
        let mut rt = AgentRuntime::default();
        rt.ingest(&hook(
            EvidenceKind::ToolStarted {
                name: Some("Bash".into()),
            },
            1_000,
        ));
        // Long time later, but the socket is STILL live ⇒ still working.
        let s = rt.observe(ScreenSignal::Working, &live_busy(), 999_000);
        assert_eq!(
            s.state,
            ObservedState::ToolUse,
            "no contradiction → no decay"
        );
    }

    /// `ApprovalRequired` → WaitingForUser, then `ToolEnded` (proceeded) clears it.
    #[test]
    fn approval_then_proceed_clears_waiting() {
        let mut rt = AgentRuntime::default();
        rt.ingest(&hook(EvidenceKind::TurnStarted, 1_000));
        rt.ingest(&hook(EvidenceKind::ApprovalRequired, 1_100));
        let s = rt.observe(ScreenSignal::Approval, &live_busy(), 1_200);
        assert_eq!(s.state, ObservedState::WaitingForUser);
        assert_eq!(s.authority, Authority::Hook);
        rt.ingest(&hook(EvidenceKind::ToolEnded, 1_300));
        let s = rt.observe(ScreenSignal::Working, &live_busy(), 1_400);
        assert_ne!(
            s.state,
            ObservedState::WaitingForUser,
            "proceeded, not stuck"
        );
    }

    /// Precedence: rate-limit outranks an open episode; approval outranks tool-use.
    #[test]
    fn precedence_orders() {
        // RateLimited > everything: open episode + screen rate-limited.
        let mut rt = AgentRuntime::default();
        rt.ingest(&hook(EvidenceKind::TurnStarted, 1_000));
        let s = rt.observe(ScreenSignal::RateLimited, &live_busy(), 1_100);
        assert_eq!(s.state, ObservedState::RateLimited);

        // WaitingForUser > ToolUse: a tool span is open but an approval is pending.
        let mut rt = AgentRuntime::default();
        rt.ingest(&hook(
            EvidenceKind::ToolStarted {
                name: Some("Bash".into()),
            },
            1_000,
        ));
        rt.ingest(&hook(EvidenceKind::ApprovalRequired, 1_050));
        let s = rt.observe(ScreenSignal::Working, &live_busy(), 1_100);
        assert_eq!(s.state, ObservedState::WaitingForUser);
    }

    /// `since_ms` is stable while the state holds, and resets when it changes.
    #[test]
    fn since_ms_stable_across_redrive() {
        let mut rt = AgentRuntime::default();
        rt.ingest(&hook(EvidenceKind::TurnStarted, 1_000));
        let a = rt.observe(ScreenSignal::Working, &live_busy(), 1_100);
        let b = rt.observe(ScreenSignal::Working, &live_busy(), 5_000);
        assert_eq!(a.since_ms, b.since_ms, "same state ⇒ since frozen");
        rt.ingest(&hook(EvidenceKind::TurnEnded { stop_reason: None }, 6_000));
        let c = rt.observe(ScreenSignal::Idle, &live_quiet(0), 6_100);
        assert_eq!(c.since_ms, 6_100, "state changed ⇒ since reset");
    }

    /// A dead child trumps stale "active" hooks.
    #[test]
    fn dead_child_is_idle() {
        let mut rt = AgentRuntime::default();
        rt.ingest(&hook(EvidenceKind::TurnStarted, 1_000));
        let live = Liveness {
            api_in_flight: false,
            productive_silent_ms: 0,
            child_alive: false,
        };
        let s = rt.observe(ScreenSignal::Working, &live, 1_100);
        assert_eq!(s.state, ObservedState::Idle);
        assert_eq!(s.authority, Authority::ProcessHeuristic);
    }

    /// Stale hook plane (no evidence in the freshness window) falls back to the screen
    /// baseline with Weak confidence rather than asserting a stale hook state.
    #[test]
    fn stale_hook_falls_back_to_screen() {
        let mut rt = AgentRuntime::default();
        rt.ingest(&hook(EvidenceKind::TurnStarted, 1_000));
        // Way past freshness; screen idle, quiet ⇒ Idle/Weak (not phantom Active).
        let s = rt.observe(
            ScreenSignal::Idle,
            &live_quiet(30_000),
            1_000 + HOOK_FRESHNESS_MS + 60_000,
        );
        assert_eq!(s.state, ObservedState::Idle);
    }

    /// A non-decisive screen signal (`Other` = Hang/Crashed/error chrome) does NOT
    /// trip the idle-reconcile (it isn't `Idle`), so an open episode with a live agent
    /// stays Active rather than being wrongly decayed. (A genuinely crashed agent is
    /// caught by `child_alive=false`, not the screen.)
    #[test]
    fn screen_other_is_non_decisive() {
        let mut rt = AgentRuntime::default();
        rt.ingest(&hook(EvidenceKind::TurnStarted, 1_000));
        // Other screen + quiet + no api, but child alive: reconcile needs screen==Idle,
        // so it does NOT fire → stays Active.
        let s = rt.observe(ScreenSignal::Other, &live_quiet(30_000), 60_000);
        assert_ne!(
            s.state,
            ObservedState::Idle,
            "Other screen never reconciles"
        );
    }

    #[test]
    fn coarse_collapses_active_family() {
        assert_eq!(ObservedState::ToolUse.coarse(), ObservedState::Active);
        assert_eq!(ObservedState::Thinking.coarse(), ObservedState::Active);
        assert_eq!(ObservedState::Responding.coarse(), ObservedState::Active);
        assert_eq!(
            ObservedState::WaitingForUser.coarse(),
            ObservedState::WaitingForUser
        );
        assert_eq!(ObservedState::Idle.coarse(), ObservedState::Idle);
    }

    #[test]
    fn observed_status_serde_roundtrips() {
        let mut rt = AgentRuntime::default();
        rt.ingest(&hook(EvidenceKind::TurnStarted, 1_000));
        let s = rt.observe(ScreenSignal::Working, &live_busy(), 1_100);
        let j = serde_json::to_value(&s).expect("serialize");
        assert!(j.get("state").is_some());
        assert!(j.get("since_ms").is_some());
        let back: ObservedStatus = serde_json::from_value(j).expect("deserialize");
        assert_eq!(back, s);
    }
}
