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

use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

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

/// How long after a generation starts this modal may still be answered.
///
/// The modal is a STARTUP artefact: Claude renders it before the session is
/// usable. Past this bound anything carrying the text is overwhelmingly likely
/// to be transcript, so eligibility ends rather than lingering for the life of
/// a long-running agent. Generous relative to the observed answer times
/// (+0.39s for the trust dialog, the dev modal moments later) and far shorter
/// than an agent's lifetime.
pub(crate) const ELIGIBILITY_EXPIRY_MS: u64 = 120_000;

/// Claude CLI versions whose modal rendering has actually been validated
/// against real captures. An unrecognised version DISABLES auto-answering
/// rather than assuming the geometry still holds — the fleet auto-updates
/// (2.1.235 -> .236 -> .237 -> .238 across four days), and a silently changed
/// modal must cost a hang plus an operator notice, never a stray keystroke.
pub(crate) const VALIDATED_CLAUDE_VERSIONS: &[&str] = &["2.1.237", "2.1.238"];

/// Resolve the version of the binary this generation will actually exec.
///
/// `~/.local/bin/claude` is a SYMLINK that auto-update flips while the fleet is
/// running, and old version binaries are retained on disk — so a process
/// launched before a flip keeps executing the older one while a decision-time
/// `claude --version` reports the newer. Canonicalising at SPAWN pins the
/// concrete versioned file this generation runs, which is the only identity
/// that can honestly gate its behaviour.
pub(crate) fn spawned_binary_version(command: &std::path::Path) -> Option<String> {
    let resolved = std::fs::canonicalize(command).ok()?;
    Some(resolved.file_name()?.to_string_lossy().into_owned())
}

pub(crate) fn version_is_validated(version: Option<&str>) -> bool {
    version.is_some_and(|v| VALIDATED_CLAUDE_VERSIONS.contains(&v))
}

/// Per-PTY write epochs, keyed by writer identity.
///
/// The gate lives in the read loop but the writers live on other threads, so
/// the counter is shared. `write_with_timeout` is the single chokepoint for
/// every PTY byte write in this daemon — `write_to_pty` delegates to it — so
/// bumping there covers injects, dismiss keystrokes, TUI socket client data and
/// anything added later, with no per-caller wiring to forget.
fn epochs() -> &'static Mutex<std::collections::HashMap<usize, Arc<AtomicU64>>> {
    static E: std::sync::OnceLock<Mutex<std::collections::HashMap<usize, Arc<AtomicU64>>>> =
        std::sync::OnceLock::new();
    E.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn writer_key(writer: &crate::agent::PtyWriter) -> usize {
    Arc::as_ptr(writer) as usize
}

/// Start tracking writes for this generation's PTY. Paired with
/// [`disarm_epoch`] so the map holds only live generations.
pub(crate) fn arm_epoch(writer: &crate::agent::PtyWriter) -> Arc<AtomicU64> {
    let counter = Arc::new(AtomicU64::new(0));
    epochs()
        .lock()
        .insert(writer_key(writer), Arc::clone(&counter));
    counter
}

pub(crate) fn disarm_epoch(writer: &crate::agent::PtyWriter) {
    epochs().lock().remove(&writer_key(writer));
}

/// Record that bytes were written into this PTY. Cheap and lock-bounded: one
/// map lookup on a path that is already about to make a syscall.
pub(crate) fn note_pty_write(writer: &crate::agent::PtyWriter) {
    let counter = epochs().lock().get(&writer_key(writer)).cloned();
    if let Some(counter) = counter {
        counter.fetch_add(1, Ordering::SeqCst);
    }
}

/// Snapshot handed to the writer thread so it can re-check IMMEDIATELY before
/// the syscall. It cannot close W3 — a check and a syscall are not atomic with
/// respect to another process's output — but it does close the much wider
/// window between deciding and writing.
#[derive(Clone)]
pub(crate) struct WriteBarrier {
    epoch: Arc<AtomicU64>,
    epoch_at_enqueue: u64,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}

impl WriteBarrier {
    /// May the enqueued keystroke still be written?
    pub(crate) fn still_valid(&self) -> bool {
        !self.cancelled.load(Ordering::SeqCst)
            && self.epoch.load(Ordering::SeqCst) == self.epoch_at_enqueue
    }
}

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
    /// This generation's argv never carried the flag, or the binary it execs is
    /// not a validated version, so nothing here may be auto-answered.
    NotArmed,
    /// The generation already answered once. Never reset.
    Spent,
    /// The frame does not carry the complete modal.
    NoCompleteModal,
    /// Past the startup window: anything carrying this text now is transcript.
    WindowExpired,
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
pub(crate) struct DevModalGate {
    armed: bool,
    spent: bool,
    epoch: Arc<AtomicU64>,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    candidate: Option<Candidate>,
}

impl DevModalGate {
    /// `armed` is this generation's argv-plus-binary fact, not an observation.
    pub(crate) fn new(armed: bool) -> Self {
        Self::with_epoch(
            armed,
            Arc::new(AtomicU64::new(0)),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
    }

    pub(crate) fn with_epoch(
        armed: bool,
        epoch: Arc<AtomicU64>,
        cancelled: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            armed,
            spent: false,
            epoch,
            cancelled,
            candidate: None,
        }
    }

    /// Test/observability handle on this generation's write epoch.
    #[cfg(test)]
    pub(crate) fn epoch_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.epoch)
    }

    /// The barrier the writer thread re-checks immediately before its syscall.
    pub(crate) fn write_barrier(&self) -> WriteBarrier {
        WriteBarrier {
            epoch: Arc::clone(&self.epoch),
            epoch_at_enqueue: self.epoch.load(Ordering::SeqCst),
            cancelled: Arc::clone(&self.cancelled),
        }
    }

    /// Record that SOMETHING reached this PTY: a daemon inject, the trust-dismiss
    /// CR, our own CR, a TUI attach or resize, or a socket client's data frame.
    /// Any of those invalidate an in-flight candidate, because the frame we were
    /// waiting on is no longer one nobody has touched.
    /// Test seam: production bumps the epoch at the PTY write chokepoint
    /// (`write_with_timeout`), so this exists for tests that need to simulate a
    /// writer without performing a write.
    #[cfg(test)]
    pub(crate) fn note_pty_activity(&mut self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }

    /// Offer one rendered frame. Pure with respect to time: `now` is supplied.
    pub(crate) fn observe(&mut self, screen: &str, now: LogicalMs) -> GateOutcome {
        if !self.armed {
            return GateOutcome::Refuse(Refused::NotArmed);
        }
        if self.spent {
            return GateOutcome::Refuse(Refused::Spent);
        }
        if now.0 > ELIGIBILITY_EXPIRY_MS {
            self.candidate = None;
            return GateOutcome::Refuse(Refused::WindowExpired);
        }
        let Some(digest) = complete_modal_digest(screen) else {
            self.candidate = None;
            return GateOutcome::Refuse(Refused::NoCompleteModal);
        };
        match self.candidate {
            Some(prev)
                if prev.digest == digest && prev.epoch_at == self.epoch.load(Ordering::SeqCst) =>
            {
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
                    epoch_at: self.epoch.load(Ordering::SeqCst),
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
