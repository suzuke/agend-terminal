//! Prompt/latch lifecycle for [`StateTracker`]: when a latched prompt or active
//! state is released, and the #3294 dev-channel startup episode that needs a
//! faster release than the generic 120s window (consensus
//! `d-20260818164919002149-7`).
//!
//! Extracted from `state/mod.rs` because that file sits against the 2500-LOC
//! anti-monolith ceiling and latch release is the cohesive slice this change
//! touches — the alternative was deleting explanatory comments, which two
//! reviewers rightly pushed back on.
//!
//! The daemon launches managed Claude with
//! `--dangerously-load-development-channels`, so every start renders Claude's
//! dev-channel warning modal, whose footer is the generic confirm chrome the
//! `PermissionPrompt` pattern matches. Three earlier rounds tried to SUPPRESS
//! that classification and each was defeated by pane content: Claude restarts
//! with `--continue`, so restored history — or an agent merely reading this
//! file — can reproduce any shape a rule keys on. Suppression therefore let a
//! genuinely blocked agent land a non-error-class state, which is #3294
//! inverted and silent.
//!
//! So classification is honest here: every matching frame is `PermissionPrompt`.
//! The tag below changes only WHEN THE LATCH RELEASES, and a mislabelled tag can
//! at worst release a latch early — never hide a dialog. Its failure direction
//! is #3294 returning (loud, one spurious notification), never silence.

use super::{AgentState, StateTracker};
use std::time::Instant;

/// Anchored on the modal's own headline, the same anchor `backend.rs`'s dismiss
/// pattern uses, so this adds no new matching surface.
fn is_dev_channel_shaped(screen: &str) -> bool {
    static MARKER: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    MARKER
        .get_or_init(|| {
            regex::Regex::new(r"(?m)^[^A-Za-z\n]*WARNING: Loading development channels")
                .expect("dev-channel startup modal regex compiles")
        })
        .is_match(screen)
}

impl StateTracker {
    /// Tag a FRESH entry into `PermissionPrompt` that was driven by a dev-channel
    /// shaped frame, remembering the state to restore when the dialog goes away.
    /// An already-latched prompt is never retagged, so a Generic prompt cannot
    /// acquire the fast-release path by a later dev-shaped repaint.
    pub(super) fn note_prompt_episode(&mut self, prior: AgentState, screen: &str) {
        if self.current == AgentState::PermissionPrompt
            && prior != AgentState::PermissionPrompt
            && is_dev_channel_shaped(screen)
        {
            self.prompt_episode = Some(prior);
        }
    }

    /// Release a tagged latch on a frame that no longer detects a prompt.
    ///
    /// Goes through `record_set` rather than `transition`: a priority-DOWN move
    /// needs the current state held for `min_hold` (2s), and the real sequence is
    /// sub-second — modal, dismiss keystroke ~300ms later, then the repaint — so a
    /// release routed through `transition` would be silently dropped. `record_set`
    /// is the documented single funnel for `current` mutations (the same bypass
    /// `set_awaiting_operator` uses) and it emits the transition record, which is
    /// what lets the supervisor's net-transition reduction drop the whole
    /// sub-tick episode instead of notifying about it.
    pub(super) fn release_dev_prompt_latch(&mut self) {
        if self.current != AgentState::PermissionPrompt {
            return;
        }
        if let Some(prior) = self.prompt_episode.take() {
            self.record_set(prior);
        }
    }

    /// #3306: how long the CURRENT dev-tagged prompt episode has been held, or
    /// `None` when no dev-tagged episode is live (untagged prompt, released,
    /// or exited). The supervisor captures this under the core lock and feeds
    /// it to the deferred member-notify gate — `Some` defers the edge notify,
    /// and the held duration decides when a stuck (auto-dismiss failed) modal
    /// finally does notify the orchestrator. `since` is the prompt-entry
    /// instant and is stable across repeated dev frames (same-state
    /// transitions early-return), so the duration measures the episode, not
    /// the latest repaint.
    pub(crate) fn dev_prompt_episode_held(&self) -> Option<std::time::Duration> {
        (self.current == AgentState::PermissionPrompt && self.prompt_episode.is_some())
            .then(|| self.since.elapsed())
    }

    /// Drop the tag whenever the prompt is left by any path, so it can never
    /// outlive the episode that created it. Called from `record_set`, the single
    /// funnel, BEFORE `current` moves.
    pub(super) fn clear_prompt_episode_on_exit(&mut self, new_state: AgentState) {
        if self.current == AgentState::PermissionPrompt && new_state != AgentState::PermissionPrompt
        {
            self.prompt_episode = None;
        }
    }

    /// Fallback when the screen changed but no pattern matched.
    ///
    /// Active-state markers (Thinking "esc to cancel", ToolUse tool banners)
    /// can stop rendering while the CLI still shows on-screen content that
    /// happens not to match the backend's Idle pattern either — e.g. a
    /// mid-scroll render between the spinner clearing and the prompt
    /// re-appearing. Without a fallback the tracker would stay latched on
    /// the prior active state indefinitely.
    ///
    /// If the current state is a self-expiring active state
    /// (Thinking / ToolUse) and it has been held longer than
    /// `LATCHED_STATE_EXPIRY`, drop to Idle. Everything else is excluded:
    /// InteractivePrompt / PermissionPrompt need explicit operator action,
    /// errors transition instantly on the next matching screen, and
    /// Starting / AwaitingOperator / Hang are driven by their own
    /// supervisors (see `daemon::supervisor`).
    pub(super) fn maybe_expire_latched_state(&mut self) {
        // F39: scrollback re-detection (Scenarios A/B) preserved by
        // transition() same-state early-return + feed() hash-dedup; Scenario C
        // priority oscillation between Thinking and other states resets `since`
        // per bounce and is the unaddressed bug surface. See
        // docs/HUNG-STATE-TRANSITIONS.md §F39.3.
        // Active states (Thinking / ToolUse) expire on the short window —
        // their trigger patterns (spinners, tool-call banners) commonly
        // stop rendering mid-operation even when the agent is still
        // working, so a brief latch is fine but holding beyond
        // LATCHED_STATE_EXPIRY is almost always stale.
        let short_expiring = matches!(self.current, AgentState::Active);
        if short_expiring && self.since.elapsed() >= Self::LATCHED_STATE_EXPIRY {
            self.transition(AgentState::Idle);
            return;
        }
        // RateLimit expires on its own 5-minute window. Real rate limits
        // clear in seconds-to-minutes; stuck for hours is a false positive.
        let rate_limit_expiring = matches!(self.current, AgentState::RateLimit);
        if rate_limit_expiring && self.since.elapsed() >= Self::RATE_LIMIT_EXPIRY {
            self.transition(AgentState::Idle);
            return;
        }
        // #1955: UsageLimit expires on its release deadline (anchored on the
        // banner's own unlock hint at latch time, else the conservative
        // window). This arm covers the banner-scrolled-away case (detection
        // returns None); a still-visible banner releases at the detection
        // override instead (the level-triggered re-match never reaches here).
        // A pre-#1955 latch carries no deadline → conservative window from
        // `since`.
        if matches!(self.current, AgentState::UsageLimit) {
            let deadline_passed = self
                .usage_limit_release_at
                .map_or(self.since.elapsed() >= Self::USAGE_LIMIT_EXPIRY, |at| {
                    Instant::now() >= at
                });
            if deadline_passed {
                self.usage_limit_release_at = None;
                self.transition(AgentState::Idle);
                return;
            }
        }
        // Prompt states (InteractivePrompt / PermissionPrompt) expire on
        // the longer window. When the screen goes stable after the
        // operator dismisses the dialog, feed()'s hash-dedup skips
        // `detect()` and the state never re-evaluates — which is how
        // `dev-reviewer` stayed flagged as "卡在互動 prompt" long after
        // the prompt was gone. The 2-minute bound gives a real operator
        // reaction window while still guaranteeing self-recovery.
        let long_expiring = matches!(
            self.current,
            AgentState::InteractivePrompt | AgentState::PermissionPrompt
        );
        if long_expiring && self.since.elapsed() >= Self::INTERACTIVE_EXPIRY {
            self.transition(AgentState::Idle);
        }
    }
}
