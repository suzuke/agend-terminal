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
//! supplies the snapshot under one `core.lock()`. Runs only under `AGEND_SHADOW_OBSERVER`.

// The core reducer surface (ObservedStatus / AgentRuntime / ScreenSignal / Liveness) is
// now consumed by the Phase-B per-tick driver (`per_tick::shadow_observe` →
// `shadow::observe`) + the additive list_instances surface (SHADOW-OBSERVER-ARCH-2413.md
// §6 step 3). The residual under this allow is `ObservedState::coarse()` — reserved for
// the §5 quantification harness's coarse-bucket (Idle/Active/WaitingForUser) reporting,
// which lands next. Narrow to that one item (or drop the allow) once the harden lands.
#![allow(dead_code)]

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
    /// Newest Hook-authority evidence timestamp — drives the freshness fallback.
    last_hook_ms: u64,
    /// Newest `Responding` evidence — distinguishes the `Responding` refinement.
    last_responding_ms: u64,
    /// Stable-`since` bookkeeping: the last derived state + when it was entered.
    last_state: Option<ObservedState>,
    last_state_since_ms: u64,
    /// Bounded explain-trail (newest last).
    trail: VecDeque<EvidenceRef>,
}

impl AgentRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one piece of Evidence into the accumulator. Idempotent-ish: replaying the
    /// same terminal event twice is harmless (close is monotone).
    pub fn ingest(&mut self, ev: &Evidence) {
        if ev.authority == Authority::Hook {
            self.last_hook_ms = self.last_hook_ms.max(ev.at_ms);
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
        if waiting && !self.reconcile_to_idle(screen, live) {
            let (auth, conf) = if self.waiting {
                (Authority::Hook, Confidence::Confirmed)
            } else {
                (Authority::Screen, Confidence::Strong)
            };
            return (ObservedState::WaitingForUser, auth, conf);
        }

        // Hook freshness: if no hook evidence in the window, the hook plane is stale —
        // defer to the screen baseline (Weak), the conservative fallback.
        let hook_fresh =
            now_ms.saturating_sub(self.last_hook_ms) <= HOOK_FRESHNESS_MS && self.last_hook_ms > 0;

        // (P3/P4/P5) Active family — only if an episode/tool is open AND liveness does
        // NOT contradict it (the phantom-stuck decay). If it DOES contradict, fall to Idle.
        let active_open = self.episode_open || self.tool_open.is_some();
        if active_open && !self.reconcile_to_idle(screen, live) {
            // Mid-API false-idle beat: screen looks idle but a fresh hook episode + a
            // live socket prove work in flight → keep Active. This is the headline win
            // over raw screen-scrape.
            let mid_api_false_idle =
                screen == ScreenSignal::Idle && hook_fresh && live.api_in_flight;

            let authority = if hook_fresh {
                Authority::Hook
            } else {
                Authority::Screen
            };
            let confidence = if hook_fresh {
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
        let conf = if hook_fresh {
            Confidence::Strong
        } else {
            Confidence::Weak
        };
        (ObservedState::Idle, Authority::Screen, conf)
    }

    /// The liveness backstop: an open episode/span should be force-closed to Idle when
    /// liveness CONTRADICTS it — the screen is back at the prompt, no LLM socket is
    /// live, and productive output has been quiet past the threshold. Time alone never
    /// trips this; it requires the screen-idle + no-api + sustained-silence conjunction,
    /// so a normal mid-turn pause (model thinking, socket still open) does NOT decay.
    fn reconcile_to_idle(&self, screen: ScreenSignal, live: &Liveness) -> bool {
        screen == ScreenSignal::Idle
            && !live.api_in_flight
            && live.productive_silent_ms >= RECONCILE_SILENCE_MS
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
        let mut rt = AgentRuntime::new();
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
        let mut rt = AgentRuntime::new();
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
        let mut rt = AgentRuntime::new();
        rt.ingest(&hook(EvidenceKind::TurnStarted, 1_000));
        // ... no TurnEnded ever arrives (dropped). Much later, screen is idle + quiet.
        let s = rt.observe(ScreenSignal::Idle, &live_quiet(30_000), 60_000);
        assert_eq!(s.state, ObservedState::Idle, "phantom episode must decay");
        assert_eq!(s.authority, Authority::Inferred, "marked as a reconcile");
    }

    /// A dropped `PostToolUse` leaves the tool span open; same contradiction closes it.
    #[test]
    fn dropped_posttool_reconciles_tool_span() {
        let mut rt = AgentRuntime::new();
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
        let mut rt = AgentRuntime::new();
        rt.ingest(&hook(EvidenceKind::TurnStarted, 1_000));
        let s = rt.observe(ScreenSignal::Idle, &live_busy(), 1_500);
        assert_ne!(
            s.state,
            ObservedState::Idle,
            "live socket beats idle screen"
        );
        assert_eq!(s.authority, Authority::Hook);
    }

    /// Time alone (no contradicting liveness — socket still live) must NOT decay an open
    /// span. Guards against over-eager TTL decay.
    #[test]
    fn ttl_open_span_without_liveness_does_not_flip() {
        let mut rt = AgentRuntime::new();
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
        let mut rt = AgentRuntime::new();
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
        let mut rt = AgentRuntime::new();
        rt.ingest(&hook(EvidenceKind::TurnStarted, 1_000));
        let s = rt.observe(ScreenSignal::RateLimited, &live_busy(), 1_100);
        assert_eq!(s.state, ObservedState::RateLimited);

        // WaitingForUser > ToolUse: a tool span is open but an approval is pending.
        let mut rt = AgentRuntime::new();
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
        let mut rt = AgentRuntime::new();
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
        let mut rt = AgentRuntime::new();
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
        let mut rt = AgentRuntime::new();
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
        let mut rt = AgentRuntime::new();
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
        let mut rt = AgentRuntime::new();
        rt.ingest(&hook(EvidenceKind::TurnStarted, 1_000));
        let s = rt.observe(ScreenSignal::Working, &live_busy(), 1_100);
        let j = serde_json::to_value(&s).expect("serialize");
        assert!(j.get("state").is_some());
        assert!(j.get("since_ms").is_some());
        let back: ObservedStatus = serde_json::from_value(j).expect("deserialize");
        assert_eq!(back, s);
    }
}
