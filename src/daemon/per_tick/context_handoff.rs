//! #2007 — context-full safety net (Plan A, operator-approved): at the
//! handoff threshold (85%), inject ONE `[AGEND-AUTO kind=context-handoff]`
//! nudge telling the agent to write its working state to SESSION-HANDOFF.md
//! and annotate the task board. Restart stays human/lead-driven — this is a
//! safety net against the 6/10 97%-stall incident shape, not seamless
//! succession (Plan B, deferred until the #1523 hook track stabilizes).
//!
//! Signal: `StateTracker::resolved_context` — statusline `pattern` ONLY.
//! Per-backend honesty: today only the claude backend renders a readable
//! context statusline, so only claude agents ever produce a reading; kiro
//! (pie icon, pattern TBD), codex (hidden by default), opencode/agy (no
//! passive signal) yield `None` and are NEVER injected — the fallback is
//! "do nothing", documented, not "guess" (multi-backend principle).
//!
//! NOISE BUDGET (hard requirement, per operator) — the #2008 four
//! principles apply:
//! - ONE injection per episode: latch on crossing; re-arm only after the
//!   usage drops below `threshold - HYSTERESIS_PCT` (compact/restart).
//!   Never re-fires on a timer.
//! - ONE optional escalation: at 92% with no SESSION-HANDOFF.md write since
//!   the injection, notify the operator channels once. Nothing else.
//! - Auto-resolve: the latch clears silently on drop — no "resolved"
//!   chatter.
//! - Idle agents are NOT injected (idle context-full is not urgent): the
//!   episode is marked in the event log instead; if the agent wakes while
//!   still above threshold, the one-per-episode injection then fires.
//!
//! Latch state is in-memory: a daemon restart re-fires at most once per
//! still-high agent — same accepted trade-off as `context_alert`
//! (current-state nudge, single, self-limiting).

use super::{PerTickHandler, TickContext};
use parking_lot::Mutex;
use std::collections::HashMap;

/// Handoff-injection threshold (percent). Override: `AGEND_CONTEXT_HANDOFF_PCT`.
const DEFAULT_HANDOFF_PCT: f32 = 85.0;
/// Operator-escalation threshold (percent). Override:
/// `AGEND_CONTEXT_HANDOFF_ESCALATE_PCT`.
const DEFAULT_ESCALATE_PCT: f32 = 92.0;
/// Re-arm requires dropping this far below the handoff threshold
/// (compact/restart), so boundary wobble can't start a second episode.
const HYSTERESIS_PCT: f32 = 5.0;

/// The handoff file the injection asks for, relative to the agent's
/// working directory. Matches the manual-rescue convention from the 6/10
/// incident.
pub(crate) const HANDOFF_FILENAME: &str = "SESSION-HANDOFF.md";

fn handoff_threshold() -> f32 {
    std::env::var("AGEND_CONTEXT_HANDOFF_PCT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_HANDOFF_PCT)
}

fn escalate_threshold() -> f32 {
    std::env::var("AGEND_CONTEXT_HANDOFF_ESCALATE_PCT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_ESCALATE_PCT)
}

/// Episode phase per agent. One episode = one continuous stay above the
/// handoff threshold.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
enum Phase {
    /// Below threshold (or never seen): the next crossing acts.
    #[default]
    Armed,
    /// Crossed while Idle — marked in the event log only. Wakes into
    /// `Injected` if the agent becomes active while still above threshold.
    IdleMarked,
    /// The one-per-episode injection happened (or was attempted — a failed
    /// PTY write is NOT retried; the escalation stage is the safety net's
    /// safety net). `escalated` caps the operator notification at one.
    Injected {
        injected_at_ms: i64,
        escalated: bool,
    },
}

#[derive(Debug, Default)]
struct EpisodeState {
    phase: Phase,
}

/// What the tick should do for this agent right now.
#[derive(Debug, PartialEq)]
enum Action {
    /// Inject the one-shot `[AGEND-AUTO kind=context-handoff]` nudge.
    Inject,
    /// Idle at threshold — record the episode in the event log only.
    MarkIdle,
    /// 92%+ and no handoff file write since injection — notify the operator
    /// channels once.
    Escalate,
}

/// Pure per-agent decision. `handoff_fresh` answers "has SESSION-HANDOFF.md
/// been written since the injection?" and is only consulted in the
/// `Injected` phase (callers may compute it lazily).
fn decide(
    state: &mut EpisodeState,
    pct: f32,
    is_idle: bool,
    handoff_fresh: impl Fn(i64) -> bool,
    now_ms: i64,
    handoff_pct: f32,
    escalate_pct: f32,
) -> Option<Action> {
    // Auto-resolve (silent): compact/restart dropped the usage — re-arm.
    if pct < handoff_pct - HYSTERESIS_PCT {
        state.phase = Phase::Armed;
        return None;
    }
    if pct < handoff_pct {
        // Between the hysteresis floor and the threshold: hold whatever
        // phase we're in (no re-arm, no action — anti-wobble).
        return None;
    }
    match state.phase {
        Phase::Armed => {
            if is_idle {
                state.phase = Phase::IdleMarked;
                Some(Action::MarkIdle)
            } else {
                state.phase = Phase::Injected {
                    injected_at_ms: now_ms,
                    escalated: false,
                };
                Some(Action::Inject)
            }
        }
        Phase::IdleMarked => {
            if is_idle {
                None
            } else {
                // Woke up still above threshold — the episode's single
                // injection fires now (the idle mark was not the injection).
                state.phase = Phase::Injected {
                    injected_at_ms: now_ms,
                    escalated: false,
                };
                Some(Action::Inject)
            }
        }
        Phase::Injected {
            injected_at_ms,
            escalated,
        } => {
            if !escalated && pct >= escalate_pct && !handoff_fresh(injected_at_ms) {
                state.phase = Phase::Injected {
                    injected_at_ms,
                    escalated: true,
                };
                Some(Action::Escalate)
            } else {
                None
            }
        }
    }
}

/// The one-shot nudge. Carries the actionable `[AGEND-HANDOFF]` marker (#2282) so
/// the agent ACTS on it — the prior `[AGEND-AUTO kind=context-handoff]` tag was
/// suppressed by the "never act on [AGEND-AUTO]" blanket (the save the nudge asks
/// for was silently skipped). Injected verbatim (`auto_kind = None`) since the
/// marker is already in the payload. Single line: a multi-line PTY injection risks
/// splitting into multiple submits.
fn handoff_payload(pct: f32) -> String {
    format!(
        "{marker} context usage at {pct:.0}% — before it runs out: (1) write {HANDOFF_FILENAME} \
         in your working directory (current task + state, key decisions, next steps, \
         open branches/PRs); (2) add a brief handoff note to your active task on the \
         board (task action=update); then continue working. One-shot reminder — the \
         daemon will not repeat it this episode.",
        marker = crate::agent::DAEMON_HANDOFF_INJECT_MARKER
    )
}

/// True if the agent's `SESSION-HANDOFF.md` was modified at/after
/// `since_ms` (epoch ms). Missing file / unreadable mtime → false (the
/// honest direction: escalation fires rather than silently assuming the
/// handoff happened). Wall-clock comparison — never `Instant` arithmetic
/// (windows underflow class).
fn handoff_written_since(working_dir: Option<&std::path::Path>, since_ms: i64) -> bool {
    let Some(dir) = working_dir else {
        return false;
    };
    let Ok(meta) = std::fs::metadata(dir.join(HANDOFF_FILENAME)) else {
        return false;
    };
    let Ok(mtime) = meta.modified() else {
        return false;
    };
    let Ok(epoch) = mtime.duration_since(std::time::UNIX_EPOCH) else {
        return false;
    };
    (epoch.as_millis() as i64) >= since_ms
}

pub(crate) struct ContextHandoffHandler {
    gate: crate::daemon::cadence_gate::CadenceGate,
    states: Mutex<HashMap<String, EpisodeState>>,
}

impl ContextHandoffHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new(every_n_ticks),
            states: Mutex::new(HashMap::new()),
        }
    }
}

impl PerTickHandler for ContextHandoffHandler {
    fn name(&self) -> &'static str {
        "context_handoff"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if !self.gate.fire() {
            return;
        }

        // Phase 1 (cheap, locks only): snapshot resolved context + idleness.
        // Agents without a pattern reading (every non-claude backend today)
        // produce nothing here and are never injected.
        let mut snapshot: Vec<(String, f32, bool)> = Vec::new();
        // #latch-prune (cleanup-on-delete, #1923 G5 class): capture ALL live
        // agent names so the per-agent `states` episode-latch can drop deleted
        // agents below — else a same-name redeploy inherits a stale episode
        // (e.g. an Injected latch suppressing the new agent's first handoff).
        let live: std::collections::HashSet<String> = {
            let reg = crate::agent::lock_registry(ctx.registry);
            let mut live = std::collections::HashSet::new();
            for handle in reg.values() {
                live.insert(handle.name.as_str().to_string());
                let core = handle.core.lock();
                if let Some((pct, _source)) = core.state.resolved_context() {
                    let is_idle = core.state.get_state() == crate::state::AgentState::Idle;
                    snapshot.push((handle.name.as_str().to_string(), pct, is_idle));
                }
            }
            live
        };

        let handoff_pct = handoff_threshold();
        let escalate_pct = escalate_threshold();
        let now_ms = chrono::Utc::now().timestamp_millis();
        let mut states = self.states.lock();
        for (name, pct, is_idle) in snapshot {
            let state = states.entry(name.clone()).or_default();
            let working_dir = ctx
                .configs
                .lock()
                .get(&name)
                .and_then(|c| c.working_dir.clone());
            let action = decide(
                state,
                pct,
                is_idle,
                |since| handoff_written_since(working_dir.as_deref(), since),
                now_ms,
                handoff_pct,
                escalate_pct,
            );
            match action {
                Some(Action::Inject) => {
                    let tgt = {
                        let reg = crate::agent::lock_registry(ctx.registry);
                        crate::fleet::resolve_uuid(ctx.home, &name)
                            .and_then(|id| reg.get(&id))
                            .map(crate::agent::InjectTarget::from_handle)
                    };
                    let ok = tgt.is_some_and(|t| {
                        crate::agent::inject_with_target_gated(
                            &t,
                            &name,
                            handoff_payload(pct).as_bytes(),
                            false,
                            // #2282: None → inject verbatim; the payload carries the
                            // actionable `[AGEND-HANDOFF]` marker, NOT the never-act
                            // `[AGEND-AUTO kind=...]` tag that suppressed the save.
                            None,
                        )
                        .is_ok()
                    });
                    if ok {
                        tracing::info!(agent = %name, pct, "context_handoff: injected one-shot handoff nudge");
                        crate::event_log::log(
                            ctx.home,
                            "context_handoff_injected",
                            &name,
                            &format!(
                                "context at {pct:.0}% — handoff nudge injected (one per episode)"
                            ),
                        );
                    } else {
                        // No retry (noise budget): the 92% escalation is the
                        // safety net's safety net for a lost nudge.
                        tracing::warn!(agent = %name, pct, "context_handoff: inject failed — not retrying (escalation stage covers)");
                    }
                }
                Some(Action::MarkIdle) => {
                    tracing::info!(agent = %name, pct, "context_handoff: idle at threshold — marked, not injected");
                    crate::event_log::log(
                        ctx.home,
                        "context_full_idle",
                        &name,
                        &format!(
                            "context at {pct:.0}% while Idle — not injecting (idle context-full \
                             is not urgent); injection fires if it wakes while still high"
                        ),
                    );
                }
                Some(Action::Escalate) => {
                    let msg = format!(
                        "[context-handoff] agent '{name}' context at {pct:.0}% and no \
                         {HANDOFF_FILENAME} update since the {handoff_pct:.0}% nudge — \
                         consider a manual handoff + restart_instance. (One-time notice.)"
                    );
                    crate::channel::notify_all_escalation_channels(
                        &name,
                        crate::channel::NotifySeverity::Warn,
                        &msg,
                        false,
                    );
                    crate::event_log::log(ctx.home, "context_handoff_escalated", &name, &msg);
                    tracing::warn!(agent = %name, pct, "context_handoff: escalated to operator (once)");
                }
                None => {}
            }
        }
        // #latch-prune: drop episode latches for agents gone from the registry
        // (cleanup-on-delete) so a deleted agent leaves no stale episode state.
        states.retain(|name, _| live.contains(name));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const HP: f32 = 85.0;
    const EP: f32 = 92.0;

    fn dec(s: &mut EpisodeState, pct: f32, idle: bool, fresh: bool) -> Option<Action> {
        decide(s, pct, idle, |_| fresh, 1_000, HP, EP)
    }

    /// §3.9 (1): threshold crossing injects exactly once; staying high never
    /// repeats (no timer re-fire — the hard noise-budget requirement).
    #[test]
    fn crossing_injects_once_and_never_repeats() {
        let mut s = EpisodeState::default();
        assert_eq!(dec(&mut s, 86.0, false, false), Some(Action::Inject));
        assert_eq!(dec(&mut s, 87.0, false, false), None);
        assert_eq!(
            dec(&mut s, 91.9, false, false),
            None,
            "below escalate — silent"
        );
    }

    /// §3.9 (2): dropping below the hysteresis floor re-arms (compact /
    /// restart); a fresh crossing injects again — one per NEW episode.
    #[test]
    fn drop_rearms_next_crossing_injects() {
        let mut s = EpisodeState::default();
        assert_eq!(dec(&mut s, 86.0, false, false), Some(Action::Inject));
        assert_eq!(
            dec(&mut s, 30.0, false, false),
            None,
            "auto-resolve is silent"
        );
        assert_eq!(s.phase, Phase::Armed, "latch re-armed");
        assert_eq!(dec(&mut s, 88.0, false, false), Some(Action::Inject));
    }

    /// Boundary wobble (84↔86) neither re-arms nor re-fires.
    #[test]
    fn boundary_wobble_is_silent() {
        let mut s = EpisodeState::default();
        assert_eq!(dec(&mut s, 86.0, false, false), Some(Action::Inject));
        assert_eq!(
            dec(&mut s, 84.0, false, false),
            None,
            "above floor — holds phase"
        );
        assert_eq!(
            dec(&mut s, 86.0, false, false),
            None,
            "wobble back — no second inject"
        );
    }

    /// §3.9 (3): 92% with no handoff write escalates EXACTLY once; a fresh
    /// handoff file suppresses the escalation entirely.
    #[test]
    fn escalates_once_at_92_without_handoff() {
        let mut s = EpisodeState::default();
        assert_eq!(dec(&mut s, 86.0, false, false), Some(Action::Inject));
        assert_eq!(dec(&mut s, 93.0, false, false), Some(Action::Escalate));
        assert_eq!(
            dec(&mut s, 95.0, false, false),
            None,
            "second 92%+ tick — silent"
        );

        let mut s = EpisodeState::default();
        assert_eq!(dec(&mut s, 86.0, false, false), Some(Action::Inject));
        assert_eq!(
            dec(&mut s, 93.0, false, true),
            None,
            "handoff file written since inject — no operator escalation"
        );
    }

    /// §3.9 (4): idle at the threshold is marked, not injected; waking while
    /// still high fires the episode's single injection.
    #[test]
    fn idle_marks_then_wake_injects() {
        let mut s = EpisodeState::default();
        assert_eq!(dec(&mut s, 86.0, true, false), Some(Action::MarkIdle));
        assert_eq!(
            dec(&mut s, 87.0, true, false),
            None,
            "still idle — once only"
        );
        assert_eq!(
            dec(&mut s, 87.0, false, false),
            Some(Action::Inject),
            "woke while high"
        );
        assert_eq!(dec(&mut s, 88.0, false, false), None);
    }

    /// Idle episodes never escalate (idle context-full is not urgent).
    #[test]
    fn idle_episode_does_not_escalate() {
        let mut s = EpisodeState::default();
        assert_eq!(dec(&mut s, 86.0, true, false), Some(Action::MarkIdle));
        assert_eq!(
            dec(&mut s, 95.0, true, false),
            None,
            "idle at 95 — still silent"
        );
    }

    /// Below threshold: no action, armed or not.
    #[test]
    fn below_threshold_never_acts() {
        let mut s = EpisodeState::default();
        assert_eq!(dec(&mut s, 0.0, false, false), None);
        assert_eq!(dec(&mut s, 84.9, false, false), None);
    }

    /// handoff_written_since: missing dir/file → false (escalate rather than
    /// silently assume the handoff happened).
    #[test]
    fn handoff_written_since_fails_closed() {
        assert!(!handoff_written_since(None, 0));
        let dir = std::env::temp_dir().join(format!("agend-2007-hw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!handoff_written_since(Some(&dir), 0), "no file → false");
        std::fs::write(dir.join(HANDOFF_FILENAME), "state").unwrap();
        assert!(
            handoff_written_since(Some(&dir), 0),
            "fresh file counts (mtime ≥ since)"
        );
        let future = chrono::Utc::now().timestamp_millis() + 3_600_000;
        assert!(
            !handoff_written_since(Some(&dir), future),
            "file older than the injection → false"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #latch-prune (cleanup-on-delete, #1923 G5 class): a per-agent episode
    /// latch for an agent no longer in the registry is dropped on the next
    /// `run` (real entry, empty registry = deleted) — so a same-name redeploy
    /// can't inherit a stale Injected/IdleMarked episode that suppresses its
    /// first handoff nudge.
    #[test]
    fn deleted_agent_episode_pruned_on_run() {
        use parking_lot::Mutex as PLMutex;
        use std::collections::HashMap;
        use std::sync::Arc;
        let home =
            std::env::temp_dir().join(format!("agend-ctxhandoff-prune-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        let registry: crate::agent::AgentRegistry = Arc::new(PLMutex::new(HashMap::new()));
        let externals: crate::agent::ExternalRegistry = Arc::new(PLMutex::new(HashMap::new()));
        let configs: Arc<PLMutex<HashMap<String, crate::daemon::AgentConfig>>> =
            Arc::new(PLMutex::new(HashMap::new()));
        let h = ContextHandoffHandler::new(1); // fire every tick (no boot-grace)
        h.states
            .lock()
            .insert("ghost".to_string(), EpisodeState::default());
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };
        h.run(&ctx);
        assert!(
            !h.states.lock().contains_key("ghost"),
            "a deleted agent's episode latch must be pruned on run (cleanup-on-delete)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #latch-prune reverse-regression (reviewer-2 #2097): a LIVE agent without
    /// a context reading this tick must KEEP its episode latch — `live.insert`
    /// must be UNCONDITIONAL, not gated on `resolved_context()`. If it regressed
    /// to the reading-subset, a working agent's in-progress episode would be
    /// wrongly dropped (re-arming a duplicate handoff). Real `run()` entry.
    #[test]
    fn live_agent_without_context_reading_keeps_episode() {
        use parking_lot::Mutex as PLMutex;
        use std::collections::HashMap;
        use std::sync::Arc;
        let home =
            std::env::temp_dir().join(format!("agend-ctxhandoff-keep-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        let registry: crate::agent::AgentRegistry = Arc::new(PLMutex::new(HashMap::new()));
        let (handle, _reader) = crate::daemon::per_tick::mock_live_agent_no_context("alive");
        registry.lock().insert(handle.id, handle);
        let externals: crate::agent::ExternalRegistry = Arc::new(PLMutex::new(HashMap::new()));
        let configs: Arc<PLMutex<HashMap<String, crate::daemon::AgentConfig>>> =
            Arc::new(PLMutex::new(HashMap::new()));
        let h = ContextHandoffHandler::new(1);
        h.states
            .lock()
            .insert("alive".to_string(), EpisodeState::default());
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };
        h.run(&ctx);
        assert!(
            h.states.lock().contains_key("alive"),
            "a LIVE agent with no context reading must KEEP its episode latch — `live.insert` \
             must be UNCONDITIONAL, not gated on resolved_context()"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
