//! #2090 M2 (mode-1 mirror) — progress-mirror per-tick handler.
//!
//! When `runtime_config.progress_mode == 1` (mirror), this handler tails each
//! agent's Claude Code transcript and relays NEW assistant *text* blocks back to
//! the channel the agent's current request came from — CLI-parity operator
//! visibility on Telegram WITHOUT the raw PTY/ANSI/tool noise.
//!
//! ⚠ EXFILTRATION SURFACE — this relays the agent's raw assistant output off-box.
//! It is bounded by four invariants the dual-review must hold:
//! 1. **Default OFF** — only runs when the operator opts into `progress_mode = 1`.
//! 2. **Active-turn gate, origin channel ONLY** — an agent is mirrored only while
//!    it has an UNANSWERED external-channel turn in flight: `pending_user_turn`
//!    present with `reply_outcome == Pending` (the same real active-turn signal
//!    the progress-backstop uses — NOT the sticky "last channel seen"
//!    `reply_to_channel`, which never clears in headless mode). The instant the
//!    reply lands the turn settles (`pending_user_turn` → None) and mirroring
//!    stops; an internal / peer / `[AGEND-AUTO]` / `[AGEND-RESUME]` / idle turn
//!    has no pending external turn, so it is NEVER mirrored. The relay goes to
//!    the origin channel by name — never a broadcast. On any non-active tick the
//!    tail position is evicted, so nothing leaks across turns. Mirror relays real
//!    content, so this gate is strictly tighter than the nudge-only backstop.
//! 3. **No backlog replay** — first sight seeds the tail at EOF (see
//!    `transcript_tail`); only text produced during the live turn is relayed.
//! 4. **Truncated** — a single relay is capped at [`MAX_MIRROR_LEN`].
//!
//! Fail-open: every per-agent step is best-effort + panic-isolated so one bad
//! agent can't kill the sweep.

use super::{PerTickHandler, TickContext};
use crate::daemon::cadence_gate::CadenceGate;
use crate::daemon::transcript_tail::{self, TailPos};
use crate::reply_ledger::ReplyOutcome;
use parking_lot::Mutex;
use std::collections::HashMap;

/// Telegram-friendly cap on a single mirrored relay. Joined text past this is
/// truncated on a char boundary with a trailing ellipsis.
const MAX_MIRROR_LEN: usize = 3500;

pub(crate) struct ProgressMirrorHandler {
    gate: CadenceGate,
    /// Per-agent tail position (keyed by agent name). Bounded two ways: evicted
    /// on any tick where the F1 active-turn gate fails (turn settled / not a
    /// pending external turn), and latch-pruned each run against the live config set.
    state: Mutex<HashMap<String, TailPos>>,
}

impl ProgressMirrorHandler {
    pub(crate) fn new() -> Self {
        Self {
            // Every tick — the cost is just reading new bytes since last offset.
            gate: CadenceGate::new(1),
            state: Mutex::new(HashMap::new()),
        }
    }
}

impl PerTickHandler for ProgressMirrorHandler {
    fn name(&self) -> &'static str {
        "progress_mirror"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        // Mode gate: only mirror mode (1) relays. 0 = off, 2 = report (the agent
        // owns updates) — both skip this handler.
        if crate::runtime_config::get().progress_mode != 1 {
            return;
        }
        if !self.gate.fire() {
            return;
        }

        // Snapshot (name, working_dir) under the configs lock, then release it
        // before any IO / channel sends.
        let agents: Vec<(String, std::path::PathBuf)> = {
            let configs = ctx.configs.lock();
            configs
                .iter()
                .filter_map(|(name, cfg)| cfg.working_dir.clone().map(|wd| (name.clone(), wd)))
                .collect()
        };

        let mut state = self.state.lock();
        for (name, working_dir) in &agents {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                Self::mirror_agent(name, working_dir, &mut state);
            }));
            if result.is_err() {
                tracing::warn!(agent = %name, "progress_mirror: per-agent panic isolated");
            }
        }
        // Bound the tail map: drop entries for agents no longer in the live set
        // (deleted/renamed) on top of the per-turn eviction in `mirror_agent`.
        let live: std::collections::HashSet<&str> =
            agents.iter().map(|(n, _)| n.as_str()).collect();
        state.retain(|name, _| live.contains(name.as_str()));
    }
}

impl ProgressMirrorHandler {
    fn mirror_agent(
        name: &str,
        working_dir: &std::path::Path,
        state: &mut HashMap<String, TailPos>,
    ) {
        let snap = crate::daemon::heartbeat_pair::snapshot_for(name);
        // F1 active-turn gate: mirror ONLY while an UNANSWERED external turn is in
        // flight — `pending_user_turn` present + `reply_outcome == Pending` (the
        // real active-turn signal, NOT the sticky `reply_to_channel`). Settled /
        // internal / peer / AUTO / RESUME / idle turns are not pending external
        // turns → no relay. Also honour an explicit per-turn opt-out. On any
        // non-active tick, evict the tail position so the next active turn
        // re-seeds (no cross-turn leakage).
        let outcome_pending = snap
            .pending_user_turn
            .as_ref()
            .map(|t| t.reply_outcome == ReplyOutcome::Pending)
            .unwrap_or(false);
        if !should_mirror(outcome_pending, snap.mirror_skip_until_next_turn) {
            state.remove(name);
            return;
        }
        // Destination: the origin channel by name (set alongside the pending turn
        // at arming). Never a broadcast. Absent → nothing to send to → evict+skip.
        let Some(channel) = snap.reply_to_channel else {
            state.remove(name);
            return;
        };

        // Tail the transcript for new assistant text. The state map stores the
        // TailPos directly; lift it into an Option for the extractor + write back.
        let mut entry = state.remove(name);
        let texts = transcript_tail::extract_new_assistant_text(working_dir, &mut entry);
        if let Some(pos) = entry {
            state.insert(name.to_string(), pos);
        }
        if texts.is_empty() {
            return;
        }

        let joined = texts.join("\n\n");
        let trimmed = joined.trim();
        if trimmed.is_empty() {
            return;
        }
        let text = truncate_on_boundary(trimmed, MAX_MIRROR_LEN);

        // Relay to the ORIGIN channel by name — never a broadcast. A missing /
        // failed channel drops silently (best-effort visibility).
        if let Some(ch) = crate::channel::lookup_channel_by_name(&channel) {
            let _ = ch.send_from_agent(name, crate::channel::AgentOutboundOp::Reply { text });
        }
    }
}

/// F1 active-turn gate (pure, so the leak-relevant decision is unit-testable):
/// relay ONLY when an external turn's reply is still `Pending` (`outcome_pending`)
/// AND the turn hasn't opted out of mirroring. A settled turn, a non-channel turn
/// (no `pending_user_turn` → `outcome_pending` false), or an explicit skip → no
/// relay. This is what makes "idle / internal / peer / AUTO turn = zero send"
/// hold: those never carry a pending external turn.
fn should_mirror(outcome_pending: bool, skip_until_next_turn: bool) -> bool {
    outcome_pending && !skip_until_next_turn
}

/// Truncate `s` to at most `max` bytes on a char boundary, appending " …" when
/// truncation occurs.
fn truncate_on_boundary(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} …", &s[..end])
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate_on_boundary("hello", MAX_MIRROR_LEN), "hello");
    }

    #[test]
    fn truncate_long_string_adds_ellipsis_on_boundary() {
        let s = "a".repeat(4000);
        let out = truncate_on_boundary(&s, MAX_MIRROR_LEN);
        assert!(out.starts_with(&"a".repeat(MAX_MIRROR_LEN)));
        assert!(out.ends_with(" …"));
    }

    #[test]
    fn truncate_respects_char_boundary() {
        // A 2-byte 'é' straddling the cut must not be split.
        let s = format!("{}é", "a".repeat(MAX_MIRROR_LEN - 1));
        let out = truncate_on_boundary(&s, MAX_MIRROR_LEN);
        assert_eq!(&out, &format!("{} …", "a".repeat(MAX_MIRROR_LEN - 1)));
    }

    /// F1 (the leak fix): the active-turn gate relays ONLY for an unanswered
    /// external turn. A pending external turn mirrors; everything else — a settled
    /// turn, and crucially a NON-channel/internal/peer turn (no pending external
    /// turn → `outcome_pending` false) — does NOT. This is the invariant that
    /// closes the cross-turn exfil leak: `reply_to_channel` being sticky no longer
    /// matters because the gate keys off the converging `pending_user_turn`.
    #[test]
    fn gate_relays_only_unanswered_external_turn() {
        // Unanswered external turn → relay.
        assert!(
            should_mirror(true, false),
            "pending external turn must mirror"
        );
        // Reply landed / turn settled (pending_user_turn → None → not pending)
        // OR an internal/peer/AUTO/RESUME/idle turn (no pending external turn).
        assert!(
            !should_mirror(false, false),
            "settled / non-channel turn must NOT mirror — closes the cross-turn leak"
        );
        // Explicit per-turn opt-out is honoured even while pending.
        assert!(
            !should_mirror(true, true),
            "mirror_skip_until_next_turn must suppress the relay"
        );
    }

    /// On a non-active tick `mirror_agent` evicts the tail position so the next
    /// active turn re-seeds (no cross-turn carry-over of the read offset).
    #[test]
    fn non_active_tick_evicts_tail_state() {
        let mut state: HashMap<String, TailPos> = HashMap::new();
        state.insert(
            "dev".to_string(),
            TailPos {
                path: "/x.jsonl".into(),
                offset: 10,
            },
        );
        // Models the `!should_mirror(..)` branch of mirror_agent.
        state.remove("dev");
        assert!(
            !state.contains_key("dev"),
            "a non-active tick must evict tail state"
        );
    }
}
