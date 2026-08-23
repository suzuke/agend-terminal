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
//! # What the tests DO and DO NOT establish (r1 review N2/N3)
//!
//! The shipped contract is "never answers a stale frame AFTER the first CR",
//! NOT "never answers a stale frame". Every stale-frame regression primes the
//! generation with the live modal first, spending the one-shot, before feeding
//! the replayed or quoted frame. No test feeds a stale frame as the FIRST
//! sighting in an armed generation, because the fingerprint cannot reject one —
//! a complete replayed modal satisfies it exactly as a live one does. A reader
//! must not take a green run as proof of the stronger claim.
//!
//! The capture corpus also has NO frame carrying a COMPLETE fingerprint beside a
//! LIVE competing operator dialog. `competing.txt` refuses only because its
//! headline had scrolled off, so it is a non-discriminating control, not
//! coverage of the most dangerous shape. That is a capture gap, recorded as one.
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
/// and 2.1.240 binaries, and the captures they are matched against were rendered
/// on 2.1.237 and 2.1.240 (see `tests/fixtures/devchannel-3314/MANIFEST.yaml`).
/// The option NUMBERING is
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
/// (2.1.235 -> .236 -> .237 -> .238 -> .240), and a silently changed
/// modal must cost a hang plus an operator notice, never a stray keystroke.
pub(crate) const VALIDATED_CLAUDE_VERSIONS: &[&str] = &["2.1.237", "2.1.238", "2.1.240"];

/// Resolve the version of a concrete backend binary.
///
/// LAYOUT-DEPENDENT, deliberately disclosed (r1 review N4): this returns the
/// canonicalised file NAME, which is a version only because the installer lays
/// binaries out as `.../claude/versions/<version>`. A homebrew or npm layout
/// yields `claude`, which is not in the allowlist and therefore DISARMS. The
/// direction is fail-closed, but the gate should be read as "a layout we have
/// validated", not "the version".
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

/// Facts about the command that was ACTUALLY built and spawned for this
/// generation. Captured from the `CommandBuilder` itself and from the path
/// `build_command` resolved, BEFORE the child is exec'd — not re-derived
/// afterwards.
///
/// Re-deriving was the r1 defect: `Backend::spawn_flags` is not pure (it stats
/// and re-parses the workspace mcp-config at call time) and `which::which` is a
/// second, independent path resolution. Between the build and a later re-read,
/// a config edit or an auto-update symlink flip can make the answer disagree
/// with the process that is actually running — and it disagrees in the
/// FAIL-OPEN direction: arming a generation whose argv never carried the flag,
/// or one running a binary version we have never captured.
#[derive(Debug, Clone, Default)]
pub(crate) struct SpawnProvenance {
    /// The built argv really contains `--dangerously-load-development-channels`.
    pub(crate) argv_has_dev_channel_flag: bool,
    /// Version of the concrete binary this generation will exec, as resolved for
    /// the command itself. `None` when it cannot be identified — which disarms.
    pub(crate) binary_version: Option<String>,
}

impl SpawnProvenance {
    /// Read the two facts off the command that is about to be spawned.
    pub(crate) fn capture(cmd: &portable_pty::CommandBuilder, resolved: &std::path::Path) -> Self {
        Self {
            argv_has_dev_channel_flag: cmd
                .get_argv()
                .iter()
                .any(|arg| arg == "--dangerously-load-development-channels"),
            binary_version: spawned_binary_version(resolved),
        }
    }
}

/// Is this generation eligible for startup-modal auto-answering at all?
///
/// PURE: a function of the captured provenance and nothing else. It touches no
/// filesystem and resolves no paths, so it cannot disagree with the process that
/// is running.
pub(crate) fn armed_for_spawn(provenance: &SpawnProvenance, agent: &str) -> bool {
    if !provenance.argv_has_dev_channel_flag {
        return false;
    }
    let armed = version_is_validated(provenance.binary_version.as_deref());
    if !armed {
        tracing::info!(
            agent,
            version = ?provenance.binary_version,
            "#3314: startup-modal auto-answer disarmed — unvalidated backend version"
        );
    }
    armed
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

/// Is this writer still tracked? The leak this exists to observe is invisible
/// from outside — a stale entry keeps counting writes for a dead generation and
/// is inherited by the next writer allocated at the same address.
#[cfg(test)]
pub(crate) fn epoch_is_armed(writer: &crate::agent::PtyWriter) -> bool {
    epochs().lock().contains_key(&writer_key(writer))
}

/// #3315 B2: RAII end-of-generation. The read loop used to cancel and disarm
/// with two TRAILING statements, which an unwind skips — leaving the generation
/// live (a CR still queued behind the 300ms write delay would pass its barrier
/// and land after the loop was gone) and leaking the writer's epoch entry, which
/// the next writer allocated at the same address would then inherit.
/// `dismiss::InFlightGuard` is the same shape for the same reason.
///
/// Owning an `Arc` clone of the writer is load-bearing, not incidental: the
/// registry is keyed by pointer identity, so keeping the allocation alive until
/// Drop is what makes the key still mean this generation when we remove it.
///
/// Dropping during an unwind takes the `epochs()` lock, which is only safe
/// because no caller holds it across a panic point: every holder in this module
/// is a single map operation. A Drop that could re-enter a lock its own thread
/// already holds would turn a panic into a deadlock.
pub(crate) struct GenerationGuard {
    writer: crate::agent::PtyWriter,
    generation_over: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for GenerationGuard {
    fn drop(&mut self) {
        // Cancel FIRST, then stop tracking — a queued keystroke holds its own
        // Arc on the flag, so removing the registry entry alone would not stop it.
        self.generation_over
            .store(true, std::sync::atomic::Ordering::SeqCst);
        disarm_epoch(&self.writer);
    }
}

/// Arm one generation: write tracking, the gate, and the guard that ends both.
/// Handing them out together is the point — the teardown cannot be forgotten,
/// re-ordered, or skipped by an early exit, because it is a Drop and not a step.
pub(crate) fn arm_generation(
    writer: &crate::agent::PtyWriter,
    armed: bool,
    deleted: Arc<std::sync::atomic::AtomicBool>,
) -> (GenerationGuard, DevModalGate) {
    let epoch = arm_epoch(writer);
    let generation_over = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let guard = GenerationGuard {
        writer: Arc::clone(writer),
        generation_over: Arc::clone(&generation_over),
    };
    (
        guard,
        DevModalGate::with_epoch(armed, epoch, generation_over, deleted),
    )
}

/// Record that bytes were written into this PTY. Cheap and lock-bounded: one
/// map lookup on a path that is already about to make a syscall.
pub(crate) fn note_pty_write(writer: &crate::agent::PtyWriter) {
    let counter = epochs().lock().get(&writer_key(writer)).cloned();
    if let Some(counter) = counter {
        counter.fetch_add(1, Ordering::SeqCst);
    }
}

/// A geometry change on this PTY. NOT a byte write, so it does not pass through
/// `write_with_timeout` and needs its own call site (`Pane::resize_pty`).
///
/// The initial baseline resize needs no special case: a bump with no candidate
/// in flight is a no-op by construction, because there is nothing to invalidate.
/// That is the exemption, structurally rather than as a flag someone has to
/// remember to clear.
pub(crate) fn note_pty_resize(writer: &crate::agent::PtyWriter) {
    note_pty_write(writer);
}

/// Snapshot handed to the writer thread so it can re-check IMMEDIATELY before
/// the syscall. It cannot close W3 — a check and a syscall are not atomic with
/// respect to another process's output — but it does close the much wider
/// window between deciding and writing.
#[derive(Clone)]
pub(crate) struct WriteBarrier {
    epoch: Arc<AtomicU64>,
    epoch_at_enqueue: u64,
    /// This GENERATION is over — set when the read loop exits for any reason
    /// (EOF, read error, shutdown). Distinct from `deleted`: a child that simply
    /// exited is not a deleted instance, and conflating them would mislabel a
    /// crashed agent.
    generation_over: Arc<std::sync::atomic::AtomicBool>,
    /// The INSTANCE is being deleted (daemon/lifecycle.rs, crash_disposition).
    deleted: Arc<std::sync::atomic::AtomicBool>,
}

impl WriteBarrier {
    /// May the enqueued keystroke still be written? Checked immediately before
    /// the syscall — see the W3 note in this module's docs for what it does NOT
    /// close.
    pub(crate) fn still_valid(&self) -> bool {
        !self.generation_over.load(Ordering::SeqCst)
            && !self.deleted.load(Ordering::SeqCst)
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
        let (relative_start, relative_end) = find_wrapped_literal(&screen[cursor..], line)?;
        let found = relative_start + cursor;
        if start.is_none() {
            start = Some(found);
        }
        end = relative_end + cursor;
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

/// Find an ASCII literal while tolerating terminal-induced wrapping inside its
/// whitespace runs. The returned byte range stays in the original frame so the
/// stability digest remains sensitive to the exact rendered bytes.
fn find_wrapped_literal(haystack: &str, needle: &str) -> Option<(usize, usize)> {
    let first_token = needle.split_ascii_whitespace().next()?;
    for (start, _) in haystack.match_indices(first_token) {
        let mut hay = start;
        let mut pat = 0;
        let hay_bytes = haystack.as_bytes();
        let pat_bytes = needle.as_bytes();
        while pat < pat_bytes.len() {
            if pat_bytes[pat].is_ascii_whitespace() {
                while pat < pat_bytes.len() && pat_bytes[pat].is_ascii_whitespace() {
                    pat += 1;
                }
                if hay >= hay_bytes.len() || !hay_bytes[hay].is_ascii_whitespace() {
                    break;
                }
                while hay < hay_bytes.len() && hay_bytes[hay].is_ascii_whitespace() {
                    hay += 1;
                }
            } else if hay < hay_bytes.len() && hay_bytes[hay] == pat_bytes[pat] {
                hay += 1;
                pat += 1;
            } else {
                break;
            }
        }
        if pat == pat_bytes.len() {
            return Some((start, hay));
        }
    }
    None
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
    generation_over: Arc<std::sync::atomic::AtomicBool>,
    deleted: Arc<std::sync::atomic::AtomicBool>,
    candidate: Option<Candidate>,
}

impl DevModalGate {
    /// `armed` is this generation's argv-plus-binary fact, not an observation.
    pub(crate) fn new(armed: bool) -> Self {
        Self::with_epoch(
            armed,
            Arc::new(AtomicU64::new(0)),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
    }

    pub(crate) fn with_epoch(
        armed: bool,
        epoch: Arc<AtomicU64>,
        generation_over: Arc<std::sync::atomic::AtomicBool>,
        deleted: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            armed,
            spent: false,
            epoch,
            generation_over,
            deleted,
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
            generation_over: Arc::clone(&self.generation_over),
            deleted: Arc::clone(&self.deleted),
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
