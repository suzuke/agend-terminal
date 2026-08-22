//! #3314: the dev-channel startup-modal gate.
//!
//! The daemon launches Claude with `--dangerously-load-development-channels`
//! (only when the workspace `mcp-config.json` declares the channel server —
//! `Backend::spawn_flags`), and Claude then blocks on a confirmation modal.
//! Auto-answering it is what keeps a fresh spawn from hanging.
//!
//! # Why recognition cannot be the safety mechanism
//!
//! The pane is a REPLAY surface, so any predicate over rendered rows can be
//! satisfied by replayed or quoted text. This is not a theory: measured against
//! the real captures in `tests/fixtures/devchannel-3314/`, a `--continue` replay
//! frame and a pasted-transcript frame BOTH satisfy every static line of the
//! modal, in order (see
//! `full_static_fingerprint_alone_does_not_separate_live_from_replay_3314`).
//! Terminal state is no better: the cursor is written BY the same PTY byte
//! stream, so a replay carrying a CUP forges it exactly as it forges text.
//!
//! Safety therefore comes from facts the daemon OWNS, not from the frame:
//!
//! * `armed` — this generation's argv actually carried the flag.
//! * `epoch` — every writer that can reach this PTY bumps it; a modal candidate
//!   is invalidated by any input, attach or resize since it was first seen.
//! * one-shot — at most one answer per process generation, spent only on a
//!   SUCCESSFUL enqueue, and never reset.
//!
//! The fingerprint is PRECISION, not safety: it keeps us from acting on a bare
//! marker line. It is order-relative and never row-absolute, because absolute
//! rows are not stable even within one CLI version — the same modal block
//! renders at rows 1-13 in one capture and rows 14-28 in another.
//!
//! # W3: the irreducible residual
//!
//! Between the last check and the `write(2)`, the child can repaint arbitrarily,
//! including answering the modal itself and opening a different dialog. A check
//! and a syscall cannot be made atomic with respect to another process's output,
//! so no discipline inside this daemon closes that window. What bounds the harm
//! is that a misfire writes exactly ONE `\r`, there is no retry, and the
//! one-shot means a generation cannot try again. Raw PTY text is unauthenticated
//! and is never proof of user intent.

/// The modal's static lines, in render order.
///
/// Version-exact: every entry was confirmed byte-present in the Claude 2.1.238
/// binary, and the captures they are matched against were rendered on 2.1.237
/// (see `tests/fixtures/devchannel-3314/MANIFEST.yaml`). The option NUMBERING is
/// deliberately absent — `2. Exit` is not a literal in the binary because the
/// list index is rendered dynamically, so keying on it would be keying on
/// something Claude computes rather than something it ships.
pub(crate) const MODAL_STATIC_LINES: &[&str] = &[
    "WARNING: Loading development channels",
    "is for local channel development",
    "Do not use this option to run channels",
    "Please use --channels to run a list of approved channels",
    "Channels:",
    "I am using this for local development",
    "Enter to confirm",
];

/// How long a candidate must stay byte-identical before it may be answered.
pub(crate) const MIN_STABLE_MS: u64 = 300;

/// Monotonic milliseconds, INJECTED. The decision path must never read a system
/// clock: a wall-clock read here would make every stability test timing
/// dependent, which is exactly the defect that let the first draft of the
/// one-shot regression pass against unfixed code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct LogicalMs(pub u64);

/// The complete-modal fingerprint: every static line present, in order.
///
/// Returns the matched region's digest, so "the same modal is still on screen"
/// is a byte comparison rather than a re-match. `None` when the frame does not
/// carry the whole modal — a bare marker line is not a modal.
pub(crate) fn complete_modal_digest(screen: &str) -> Option<u64> {
    let mut cursor = 0usize;
    let mut start = None;
    let mut end = 0usize;
    for line in MODAL_STATIC_LINES {
        let found = screen[cursor..].find(line)? + cursor;
        if start.is_none() {
            start = Some(found);
        }
        end = found + line.len();
        cursor = end;
    }
    let region = &screen[start?..end];
    let mut hash = 0xcbf29ce484222325u64;
    for byte in region.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    Some(hash)
}

/// Why the gate refused. Carried so the read loop can log a reason instead of
/// silently doing nothing, and so tests assert the CAUSE, not just the outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refused {
    /// This generation's argv never carried the flag, so no such modal exists.
    NotArmed,
    /// The generation already answered once. Never reset.
    Spent,
    /// The frame does not carry the complete modal.
    NoCompleteModal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GateOutcome {
    Refuse(Refused),
    /// A candidate is being observed but is not yet stable, or its epoch moved.
    Hold,
    /// Stable, unmodified, and unspent — the caller may enqueue exactly one CR
    /// and must then call [`DevModalGate::mark_enqueued`].
    Enqueue,
}

#[derive(Debug, Clone, Copy)]
struct Candidate {
    digest: u64,
    first_seen: LogicalMs,
    epoch_at: u64,
}

/// Per-process-generation state. Deliberately a plain value owned by the PTY
/// read loop: the read loop is spawned per generation, so this is
/// generation-scoped BY CONSTRUCTION, with no store keyed by agent name, no
/// eviction to forget, and no rollover race.
#[derive(Debug)]
pub(crate) struct DevModalGate {
    armed: bool,
    spent: bool,
    epoch: u64,
    candidate: Option<Candidate>,
}

impl DevModalGate {
    /// `armed` is this generation's argv fact, not an observation.
    pub(crate) fn new(armed: bool) -> Self {
        Self {
            armed,
            spent: false,
            epoch: 0,
            candidate: None,
        }
    }

    /// Record that SOMETHING reached this PTY: a daemon inject, the trust-dismiss
    /// CR, our own CR, a TUI attach or resize, or a socket client's data frame.
    /// Any of those invalidate an in-flight candidate, because the frame we were
    /// waiting on is no longer one nobody has touched.
    pub(crate) fn note_pty_activity(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
    }

    /// Offer one rendered frame. Pure with respect to time: `now` is supplied.
    pub(crate) fn observe(&mut self, screen: &str, now: LogicalMs) -> GateOutcome {
        if !self.armed {
            return GateOutcome::Refuse(Refused::NotArmed);
        }
        if self.spent {
            return GateOutcome::Refuse(Refused::Spent);
        }
        let Some(digest) = complete_modal_digest(screen) else {
            self.candidate = None;
            return GateOutcome::Refuse(Refused::NoCompleteModal);
        };
        match self.candidate {
            Some(prev) if prev.digest == digest && prev.epoch_at == self.epoch => {
                if now.0.saturating_sub(prev.first_seen.0) >= MIN_STABLE_MS {
                    GateOutcome::Enqueue
                } else {
                    GateOutcome::Hold
                }
            }
            _ => {
                self.candidate = Some(Candidate {
                    digest,
                    first_seen: now,
                    epoch_at: self.epoch,
                });
                GateOutcome::Hold
            }
        }
    }

    /// Spend the one-shot. Called ONLY after the CR was successfully enqueued —
    /// a collision or a failed enqueue must leave the generation able to try
    /// again, otherwise a lost attempt would strand the agent at the modal.
    pub(crate) fn mark_enqueued(&mut self) {
        self.spent = true;
        self.candidate = None;
    }
}
