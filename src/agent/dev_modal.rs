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
//! * `epoch` — every byte writer and PTY output path bumps it. A complete modal
//!   repaint re-arms the same bounded worker at the new epoch; input or output
//!   that leaves no complete modal keeps the candidate stale and cancels it. A
//!   silent geometry change does not alter the observed frame and remains valid.
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
/// through 2.1.241 binaries, and the captures they are matched against were
/// rendered on 2.1.237, 2.1.240, and 2.1.241 (see the fixture manifest).
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

/// The subset the relaxed retry anchors on: everything ABOVE the interactive
/// option list.
///
/// #3547 D(ii). The dev-gated dismiss pattern already requires the `WARNING:`
/// line to be on screen before the gate is consulted at all, so the only
/// incomplete frame that can reach here is one whose TAIL is missing — a pane
/// too short for the whole modal, or one still painting it. These five lines are
/// the part that survives that, and each is static prose unique to this modal.
///
/// The two lines deliberately dropped (`I am using this for local development`,
/// `Enter to confirm`) are the interactive half — which is precisely the half
/// that says "there is something here you may answer with Enter".
///
/// **So an anchor match proves the warning TEXT is on screen. It does not prove
/// an answerable modal is on screen.** #3561 R1 found what that gap costs: a
/// frame whose upper half QUOTES the warning (the pattern's `[^A-Za-z\n]*`
/// prefix class admits `> `) and whose lower half carries a different prompt
/// actually blocking the pane satisfies the anchor, and `prompt_blocked` is true
/// — because of that other prompt. The CR would have answered it.
///
/// The tail must fit the modal's known option text, as well as the bounded
/// bottom region ([`ANCHOR_TAIL_MAX_LINES`]). The other-dismiss-pattern check
/// is additional protection; its pattern set does not enumerate every prompt.
pub(crate) const MODAL_ANCHOR_LINES: &[&str] = &[
    "WARNING: Loading development channels",
    "is for local channel development",
    "Do not use this option to run channels",
    "Please use --channels to run a list of approved channels",
    "Channels:",
];

/// How many consecutive incomplete frames arm the relaxed anchored retry.
///
/// #3547 D(ii). A frame mid-paint resolves within a frame or two, so this is
/// far above "still rendering" and far below "wait forever". It is counted in
/// frames rather than milliseconds on purpose: the count only advances when the
/// child actually emits output, so a quiet pane cannot age into the relaxed path
/// while nothing is being drawn.
pub(crate) const RELAXED_AFTER_INCOMPLETE_FRAMES: u32 = 24;

/// Bound the tail to the channel-line remainder, two options, and footer.
/// Line count alone is insufficient: a different prompt can occupy one line.
/// `anchor_reaches_bottom` also checks the option text and order.
pub(crate) const ANCHOR_TAIL_MAX_LINES: usize = 4;

/// Hard ceiling on answers per generation, across all fingerprints.
///
/// #3547 D(i) replaces the per-generation one-shot with a per-fingerprint one,
/// so a second, genuinely different modal can still be answered. This bounds
/// what that opens up: a pathological screen whose digest changes every frame
/// must not turn into an unbounded CR stream (the #2474 footgun). Three is one
/// for the modal, one for a re-render that our first answer did not clear, and
/// one spare.
pub(crate) const MAX_ANSWERS_PER_GENERATION: usize = 3;

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

/// Facts about the command that was ACTUALLY built and spawned for this
/// generation. Captured from the `CommandBuilder` before the child is exec'd.
#[derive(Debug, Clone, Default)]
pub(crate) struct SpawnProvenance {
    /// The built argv really contains `--dangerously-load-development-channels`.
    pub(crate) argv_has_dev_channel_flag: bool,
}

impl SpawnProvenance {
    /// Read the daemon-owned flag off the command that is about to be spawned.
    pub(crate) fn capture(cmd: &portable_pty::CommandBuilder) -> Self {
        Self {
            argv_has_dev_channel_flag: cmd
                .get_argv()
                .iter()
                .any(|arg| arg == "--dangerously-load-development-channels"),
        }
    }
}

/// Is this generation eligible for startup-modal auto-answering at all?
///
/// PURE: a function of the captured provenance and nothing else. It touches no
/// filesystem and resolves no paths, so it cannot disagree with the process that
/// is running.
pub(crate) fn armed_for_spawn(provenance: &SpawnProvenance) -> bool {
    provenance.argv_has_dev_channel_flag
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
    name: &str,
) -> (GenerationGuard, DevModalGate) {
    let epoch = arm_epoch(writer);
    let generation_over = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let guard = GenerationGuard {
        writer: Arc::clone(writer),
        generation_over: Arc::clone(&generation_over),
    };
    // #3547: replace any previous generation's tally wholesale, so a reader can
    // never mix two generations' counts.
    let tally = publish_tally(name, armed);
    (
        guard,
        DevModalGate::with_epoch(armed, epoch, generation_over, deleted, tally),
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

/// Record child output that can repaint a startup-modal candidate while its
/// delayed confirmation is pending.
pub(crate) fn note_pty_output(writer: &crate::agent::PtyWriter) {
    note_pty_write(writer);
}

/// Snapshot handed to the writer thread so it can re-check IMMEDIATELY before
/// the syscall. It cannot close W3 — a check and a syscall are not atomic with
/// respect to another process's output — but it does close the much wider
/// window between deciding and writing.
#[derive(Clone)]
pub(crate) struct WriteBarrier {
    epoch: Arc<AtomicU64>,
    candidate_epoch: Arc<AtomicU64>,
    /// This GENERATION is over — set when the read loop exits for any reason
    /// (EOF, read error, shutdown). Distinct from `deleted`: a child that simply
    /// exited is not a deleted instance, and conflating them would mislabel a
    /// crashed agent.
    generation_over: Arc<std::sync::atomic::AtomicBool>,
    /// The INSTANCE is being deleted (daemon/lifecycle.rs, crash_disposition).
    deleted: Arc<std::sync::atomic::AtomicBool>,
}

impl WriteBarrier {
    /// Wait until the latest complete-modal frame has remained untouched for
    /// `stable_for`. Repaints restart this one worker's window; output that
    /// removes the modal leaves `candidate_epoch` stale and cancels it.
    pub(crate) fn wait_until_stable(
        &self,
        stable_for: std::time::Duration,
        max_wait: std::time::Duration,
    ) -> bool {
        let deadline = std::time::Instant::now() + max_wait;
        let mut observed_epoch = self.epoch.load(Ordering::SeqCst);
        loop {
            std::thread::sleep(stable_for);
            if self.generation_over.load(Ordering::SeqCst)
                || self.deleted.load(Ordering::SeqCst)
                || std::time::Instant::now() >= deadline
            {
                return false;
            }
            let current_epoch = self.epoch.load(Ordering::SeqCst);
            if current_epoch != observed_epoch {
                observed_epoch = current_epoch;
                continue;
            }
            return self.candidate_epoch.load(Ordering::SeqCst) == current_epoch;
        }
    }

    /// May the enqueued keystroke still be written? Checked immediately before
    /// the syscall — see the W3 note in this module's docs for what it does NOT
    /// close.
    pub(crate) fn still_valid(&self) -> bool {
        !self.generation_over.load(Ordering::SeqCst)
            && !self.deleted.load(Ordering::SeqCst)
            && self.epoch.load(Ordering::SeqCst) == self.candidate_epoch.load(Ordering::SeqCst)
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
    digest_of_lines(screen, MODAL_STATIC_LINES)
}

/// The relaxed fingerprint: [`MODAL_ANCHOR_LINES`] present, in order.
///
/// #3547 D(ii). Same machinery and same digest shape as the complete match, so
/// a relaxed candidate goes through the identical stability and epoch checks —
/// only the required line set is smaller. The digests cannot collide across the
/// two: a complete match hashes a strictly longer region.
pub(crate) fn anchored_modal_digest(screen: &str) -> Option<u64> {
    let (digest, end) = digest_and_end_of_lines(screen, MODAL_ANCHOR_LINES)?;
    anchor_reaches_bottom(screen, end).then_some(digest)
}

fn digest_of_lines(screen: &str, lines: &[&str]) -> Option<u64> {
    digest_and_end_of_lines(screen, lines).map(|(digest, _)| digest)
}

/// As [`digest_of_lines`], but also reports the byte offset just past the last
/// matched literal, so a caller can ask what is BELOW the match.
fn digest_and_end_of_lines(screen: &str, lines: &[&str]) -> Option<(u64, usize)> {
    let mut cursor = 0usize;
    let mut start = None;
    let mut end = 0usize;
    for line in lines {
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
    Some((hash, end))
}

/// Require the known modal tail rather than accepting arbitrary short prompts.
/// The first line is the remainder of `Channels: <channel>`. After it, only
/// the modal's ordered options/footer (possibly cut off by pane height) fit.
fn anchor_reaches_bottom(screen: &str, end: usize) -> bool {
    let tail = &screen[end..];
    if tail.lines().filter(|line| !line.trim().is_empty()).count() > ANCHOR_TAIL_MAX_LINES {
        return false;
    }
    let Some((_, options)) = tail.split_once('\n') else {
        return true;
    };
    let options = options.split_whitespace().collect::<Vec<_>>().join(" ");
    let expected =
        "❯ 1. I am using this for local development 2. Exit Enter to confirm · Esc to cancel";
    expected.starts_with(&options)
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

impl Refused {
    fn as_str(self) -> &'static str {
        match self {
            Refused::NotArmed => "NotArmed",
            Refused::Spent => "Spent",
            Refused::NoCompleteModal => "NoCompleteModal",
            Refused::WindowExpired => "WindowExpired",
        }
    }
}

/// #3547: what this agent's newest generation gate has actually done, published
/// so the stall path can READ it.
///
/// Observability only — the gate never consults it, so a wrong or stale tally
/// can mislead a human but cannot change a decision. That is what makes keying
/// it by agent name safe here while the gate itself stays generation-scoped by
/// construction: a new generation replaces the whole entry, and the worst
/// staleness is a tally from a generation that has already ended.
///
/// Exists because #3548's first-Refuse log cannot answer the question it was
/// added for. The gate is consulted on every PTY read, and the first few reads
/// of a healthy generation happen before the modal is painted — so the FIRST
/// refuse is `NoCompleteModal` almost every time, including on every successful
/// dismiss. What discriminates a real miss is the LAST refuse before the stall
/// plus the shape of the distribution, which is what this carries.
#[derive(Debug, Default)]
pub(crate) struct RefuseTally {
    armed: std::sync::atomic::AtomicBool,
    not_armed: AtomicU64,
    spent: AtomicU64,
    no_complete_modal: AtomicU64,
    window_expired: AtomicU64,
    answered: AtomicU64,
    relaxed_answers: AtomicU64,
    /// 0 = nothing refused yet; otherwise a [`Refused`] discriminant + 1.
    last: AtomicU64,
}

impl RefuseTally {
    fn record_refuse(&self, reason: Refused) {
        let (counter, tag) = match reason {
            Refused::NotArmed => (&self.not_armed, 1),
            Refused::Spent => (&self.spent, 2),
            Refused::NoCompleteModal => (&self.no_complete_modal, 3),
            Refused::WindowExpired => (&self.window_expired, 4),
        };
        counter.fetch_add(1, Ordering::Relaxed);
        self.last.store(tag, Ordering::Relaxed);
    }

    fn record_answer(&self, relaxed: bool) {
        self.answered.fetch_add(1, Ordering::Relaxed);
        if relaxed {
            self.relaxed_answers.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn last_refuse(&self) -> Option<Refused> {
        match self.last.load(Ordering::Relaxed) {
            1 => Some(Refused::NotArmed),
            2 => Some(Refused::Spent),
            3 => Some(Refused::NoCompleteModal),
            4 => Some(Refused::WindowExpired),
            _ => None,
        }
    }

    /// One line, safe to paste into a stalled-pane capture.
    pub(crate) fn summary_line(&self) -> String {
        format!(
            "dev_modal: armed={} answered={} (relaxed={}) last_refuse={}              refuses{{NotArmed:{},Spent:{},NoCompleteModal:{},WindowExpired:{}}}",
            self.armed.load(Ordering::Relaxed),
            self.answered.load(Ordering::Relaxed),
            self.relaxed_answers.load(Ordering::Relaxed),
            self.last_refuse().map_or("none", Refused::as_str),
            self.not_armed.load(Ordering::Relaxed),
            self.spent.load(Ordering::Relaxed),
            self.no_complete_modal.load(Ordering::Relaxed),
            self.window_expired.load(Ordering::Relaxed),
        )
    }
}

fn tallies() -> &'static Mutex<std::collections::HashMap<String, Arc<RefuseTally>>> {
    static T: std::sync::OnceLock<Mutex<std::collections::HashMap<String, Arc<RefuseTally>>>> =
        std::sync::OnceLock::new();
    T.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Start a fresh tally for `name`, replacing any previous generation's.
pub(crate) fn publish_tally(name: &str, armed: bool) -> Arc<RefuseTally> {
    let tally = Arc::new(RefuseTally::default());
    tally.armed.store(armed, Ordering::Relaxed);
    tallies()
        .lock()
        .insert(name.to_string(), Arc::clone(&tally));
    tally
}

/// The newest published tally for `name`, rendered for a stalled-pane capture.
pub(crate) fn refuse_summary(name: &str) -> Option<String> {
    let tally = tallies().lock().get(name).cloned()?;
    Some(tally.summary_line())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GateOutcome {
    Refuse(Refused),
    /// A candidate is being observed but is not yet stable, or its epoch moved.
    Hold,
    /// The first complete sighting may start the delayed writer. Its barrier
    /// supplies the stability window when the child emits no second frame.
    Schedule,
    /// Stable, unmodified, and unspent — the caller may enqueue exactly one CR
    /// and must mark its [`EnqueueReceipt`] only after successful delivery.
    Enqueue,
}

#[derive(Debug, Clone, Copy)]
struct Candidate {
    digest: u64,
    first_seen: LogicalMs,
    epoch_at: u64,
    /// #3547 D(ii): matched the anchored subset rather than the whole modal.
    /// Carried only so the tally can separate relaxed answers from strict ones.
    relaxed: bool,
}

/// #3547 D(i): which fingerprints this generation has already answered, and how
/// many answers it has spent in total.
#[derive(Debug, Default)]
struct Answered {
    digests: Vec<u64>,
    count: usize,
}

/// Per-process-generation state. Deliberately a plain value owned by the PTY
/// read loop: the read loop is spawned per generation, so this is
/// generation-scoped BY CONSTRUCTION, with no store keyed by agent name, no
/// eviction to forget, and no rollover race.
pub(crate) struct DevModalGate {
    armed: bool,
    epoch: Arc<AtomicU64>,
    generation_over: Arc<std::sync::atomic::AtomicBool>,
    deleted: Arc<std::sync::atomic::AtomicBool>,
    candidate_epoch: Arc<AtomicU64>,
    candidate: Option<Candidate>,
    /// #3547 D(i): the one-shot, keyed by FINGERPRINT instead of by generation.
    ///
    /// The old per-generation bit meant a second, genuinely different modal in
    /// the same generation could never be answered — it refused as `Spent`
    /// forever, with the modal still on screen. Keying on the digest keeps the
    /// property that actually matters (never answer the SAME frame twice) and
    /// drops the one that stranded agents. [`MAX_ANSWERS_PER_GENERATION`] bounds
    /// what that opens up. Shared with the detached writer through
    /// [`EnqueueReceipt`], which is why it is behind an `Arc`.
    answered: Arc<Mutex<Answered>>,
    /// #3547 D(ii): consecutive frames that carried no COMPLETE modal. Reset by
    /// any complete sighting and by every non-`NoCompleteModal` outcome.
    consecutive_no_complete: u32,
    /// #3547 D(ii): this frame's `is_dismissible_prompt_state` fact, pushed in by
    /// the read loop. Kept as gate state rather than an `observe` parameter for
    /// the same reason `armed` is: the decision path stays a pure function of
    /// (gate state, screen, `now`) with no clock and no registry read of its own.
    prompt_blocked: bool,
    /// #3561 R1 B1, remedy (b): does this frame carry a DIFFERENT dismissible
    /// prompt? Injected by the read loop like `prompt_blocked`, and for the same
    /// reason it is needed: `prompt_blocked` says only that SOMETHING is holding
    /// the pane, never which thing. When something else on screen is answerable,
    /// a CR aimed at a merely-quoted warning would land on that instead.
    other_prompt_on_screen: bool,
    /// #3547: observability sink. Never read by the decision path.
    tally: Arc<RefuseTally>,
    /// #3547 P0-near Task2: per-generation first-Refuse flag. Owned by the PTY
    /// read loop like everything else here (generation-scoped BY CONSTRUCTION),
    /// so no store keyed by agent name and no eviction. Plain bool: every
    /// caller holds `&mut`, so no atomic is needed. Observability only — never
    /// read by the gate decision itself.
    first_refuse_logged: bool,
}

impl DevModalGate {
    /// `armed` is this generation's argv-plus-binary fact, not an observation.
    /// The tally is detached: callers that want it published under an agent name
    /// go through [`arm_generation`].
    pub(crate) fn new(armed: bool) -> Self {
        Self::with_epoch(
            armed,
            Arc::new(AtomicU64::new(0)),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            Arc::new(RefuseTally::default()),
        )
    }

    pub(crate) fn with_epoch(
        armed: bool,
        epoch: Arc<AtomicU64>,
        generation_over: Arc<std::sync::atomic::AtomicBool>,
        deleted: Arc<std::sync::atomic::AtomicBool>,
        tally: Arc<RefuseTally>,
    ) -> Self {
        let candidate_epoch = Arc::new(AtomicU64::new(epoch.load(Ordering::SeqCst)));
        Self {
            armed,
            answered: Arc::new(Mutex::new(Answered::default())),
            epoch,
            generation_over,
            deleted,
            candidate_epoch,
            candidate: None,
            consecutive_no_complete: 0,
            prompt_blocked: false,
            other_prompt_on_screen: false,
            tally,
            first_refuse_logged: false,
        }
    }

    /// #3547 D(ii): record this frame's prompt-state fact. The read loop already
    /// computes it (`is_dismissible_prompt_state`), so pushing it in keeps
    /// `observe` free of any state lookup of its own.
    pub(crate) fn set_prompt_blocked(&mut self, prompt_blocked: bool) {
        self.prompt_blocked = prompt_blocked;
    }

    /// #3561 R1 B1, remedy (b): record whether any OTHER dismiss pattern matches
    /// this frame. Computed by the scan that is about to consult the gate, so
    /// the gate still reads nothing but injected facts.
    pub(crate) fn set_other_prompt_on_screen(&mut self, other_prompt_on_screen: bool) {
        self.other_prompt_on_screen = other_prompt_on_screen;
    }

    /// #3547 P0-near Task2: claim the per-generation first-Refuse log slot.
    /// Returns true exactly once per generation; observability only, the gate
    /// decision never consults it.
    pub(crate) fn claim_first_refuse_log(&mut self) -> bool {
        if self.first_refuse_logged {
            false
        } else {
            self.first_refuse_logged = true;
            true
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
            candidate_epoch: Arc::clone(&self.candidate_epoch),
            generation_over: Arc::clone(&self.generation_over),
            deleted: Arc::clone(&self.deleted),
        }
    }

    /// Record that SOMETHING reached this PTY: a daemon inject, the trust-dismiss
    /// CR, our own CR, a child repaint, or a socket client's data frame.
    /// Any of those invalidate an in-flight candidate, because the frame we were
    /// waiting on is no longer one nobody has touched.
    /// Test seam: production bumps the epoch at the PTY write chokepoint
    /// (`write_with_timeout`), so this exists for tests that need to simulate a
    /// writer without performing a write.
    #[cfg(test)]
    pub(crate) fn note_pty_activity(&mut self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }

    /// Record a refusal in the tally and return it.
    fn refuse(&mut self, reason: Refused) -> GateOutcome {
        if reason != Refused::NoCompleteModal {
            self.consecutive_no_complete = 0;
        }
        self.tally.record_refuse(reason);
        GateOutcome::Refuse(reason)
    }

    /// #3547 D(ii): may this incomplete frame be matched on the anchored subset?
    ///
    /// Every condition here is a fact the daemon owns or has already computed —
    /// none of it is read off the frame beyond the anchor match itself.
    fn relaxed_digest(&self, screen: &str) -> Option<u64> {
        if !self.prompt_blocked || self.consecutive_no_complete < RELAXED_AFTER_INCOMPLETE_FRAMES {
            return None;
        }
        // #3561 R1 B1 remedy (b): something else on this frame is answerable, so
        // the pane is not blocked on OUR modal and a CR would land on that.
        if self.other_prompt_on_screen {
            return None;
        }
        anchored_modal_digest(screen)
    }

    /// #3547 D(i): may this fingerprint still be answered?
    ///
    /// Three separate refusals live here, and the third is the one that keeps
    /// #3314 intact. Relaxing the one-shot from per-generation to per-fingerprint
    /// on its own would re-open exactly what #3314 closed: a modal REPLAYED or
    /// QUOTED into the transcript after the real one was answered carries
    /// different surrounding bytes, so it is a different digest, so it would be
    /// answered again — and recognition cannot tell a replay from a live modal
    /// (measured, see the #3314 fixtures).
    ///
    /// What separates them is not the frame, it is the pane: a genuine second
    /// modal BLOCKS the agent, while a replay scrolls past an agent that is
    /// running. So a second answer is admitted only while `prompt_blocked` holds.
    /// That is the same daemon-owned fact D(ii) uses, not a property of the text.
    fn already_answered(&self, digest: u64) -> bool {
        let answered = self.answered.lock();
        answered.digests.contains(&digest)
            || answered.count >= MAX_ANSWERS_PER_GENERATION
            || (answered.count > 0 && !self.prompt_blocked)
    }

    /// Offer one rendered frame. Pure with respect to time: `now` is supplied.
    pub(crate) fn observe(&mut self, screen: &str, now: LogicalMs) -> GateOutcome {
        if !self.armed {
            return self.refuse(Refused::NotArmed);
        }
        if now.0 > ELIGIBILITY_EXPIRY_MS {
            self.candidate = None;
            return self.refuse(Refused::WindowExpired);
        }
        let (digest, relaxed) = match complete_modal_digest(screen) {
            Some(digest) => {
                self.consecutive_no_complete = 0;
                (digest, false)
            }
            None => {
                self.consecutive_no_complete = self.consecutive_no_complete.saturating_add(1);
                match self.relaxed_digest(screen) {
                    Some(digest) => (digest, true),
                    None => {
                        self.candidate = None;
                        // Counted BEFORE the refusal so the frame that crosses
                        // the threshold is the one that arms the relaxed path.
                        self.tally.record_refuse(Refused::NoCompleteModal);
                        return GateOutcome::Refuse(Refused::NoCompleteModal);
                    }
                }
            }
        };
        if self.already_answered(digest) {
            return self.refuse(Refused::Spent);
        }
        let current_epoch = self.epoch.load(Ordering::SeqCst);
        self.candidate_epoch.store(current_epoch, Ordering::SeqCst);
        match self.candidate {
            Some(prev) if prev.digest == digest && prev.epoch_at == current_epoch => {
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
                    epoch_at: current_epoch,
                    relaxed,
                });
                GateOutcome::Schedule
            }
        }
    }

    /// A detached writer cannot borrow the read-loop-owned gate. This handle
    /// carries only the monotonic one-shot bit, so a successful writer can
    /// spend it without moving candidate recognition off the read loop.
    pub(crate) fn enqueue_receipt(&self) -> EnqueueReceipt {
        EnqueueReceipt {
            answered: Arc::clone(&self.answered),
            tally: Arc::clone(&self.tally),
            digest: self.candidate.map(|c| c.digest),
            relaxed: self.candidate.is_some_and(|c| c.relaxed),
        }
    }
}

#[derive(Clone)]
pub(crate) struct EnqueueReceipt {
    answered: Arc<Mutex<Answered>>,
    tally: Arc<RefuseTally>,
    /// The fingerprint this answer is for. `None` only if the receipt was taken
    /// with no candidate on the gate, which still consumes budget — the ceiling
    /// must bound answers we cannot attribute just as tightly as ones we can.
    digest: Option<u64>,
    relaxed: bool,
}

impl EnqueueReceipt {
    pub(crate) fn mark_enqueued(&self) {
        {
            let mut answered = self.answered.lock();
            if let Some(digest) = self.digest {
                if !answered.digests.contains(&digest) {
                    answered.digests.push(digest);
                }
            }
            answered.count = answered.count.saturating_add(1);
        }
        self.tally.record_answer(self.relaxed);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod write_barrier_tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    /// Build a barrier whose epochs AGREE, so `wait_until_stable` returns true
    /// unless a predicate says otherwise. Colocated with `WriteBarrier` because
    /// its fields are module-private — this needs no production-visible seam.
    fn barrier(generation_over: bool, deleted: bool) -> WriteBarrier {
        WriteBarrier {
            epoch: Arc::new(AtomicU64::new(7)),
            candidate_epoch: Arc::new(AtomicU64::new(7)),
            generation_over: Arc::new(AtomicBool::new(generation_over)),
            deleted: Arc::new(AtomicBool::new(deleted)),
        }
    }

    /// One sleep of `stable_for`, then a decision. `max_wait` is deliberately far
    /// out of reach so a `false` can only come from a predicate, never from the
    /// deadline — that is what makes the two false cases below non-vacuous.
    const STABLE_FOR: std::time::Duration = std::time::Duration::from_millis(1);
    const UNREACHABLE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

    /// KNOWN-TRUE CONTROL. Without this, a `wait_until_stable` that always
    /// returned false would satisfy both negative tests below.
    #[test]
    fn stable_frame_with_matching_candidate_epoch_passes() {
        assert!(
            barrier(false, false).wait_until_stable(STABLE_FOR, UNREACHABLE_DEADLINE),
            "control: candidate_epoch == epoch with neither predicate set must pass, \
             otherwise the negative tests below prove nothing"
        );
    }

    /// #3421 item 2: isolates `generation_over`. `deleted` stays false, the epochs
    /// still agree, and the deadline is unreachable — deleting the
    /// `generation_over` check from `wait_until_stable` makes this return true.
    #[test]
    fn generation_over_cancels_the_wait_on_its_own() {
        assert!(
            !barrier(true, false).wait_until_stable(STABLE_FOR, UNREACHABLE_DEADLINE),
            "generation_over must cancel the wait by itself (deleted=false, epochs agree)"
        );
    }

    /// #3421 item 2: isolates `deleted`, the mirror of the test above.
    #[test]
    fn deleted_cancels_the_wait_on_its_own() {
        assert!(
            !barrier(false, true).wait_until_stable(STABLE_FOR, UNREACHABLE_DEADLINE),
            "deleted must cancel the wait by itself (generation_over=false, epochs agree)"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod resilience_3547_tests {
    use super::*;

    const FRAME_LIVE_MODAL: &str =
        include_str!("../../tests/fixtures/devchannel-3314/live_modal.txt");
    /// A genuinely different modal: same shape, different channel list. The
    /// digest covers only the region BETWEEN the first and last static line, so
    /// a variant must differ INSIDE it — appending to the frame changes nothing,
    /// which is itself the property that makes the fingerprint stable.
    fn frame_for_channel(channel: &str) -> String {
        FRAME_LIVE_MODAL.replace("server:agend-claude-channel", channel)
    }

    /// The same real frame with its LAST line cut — the shape a pane one row too
    /// short renders. Six of the seven static lines survive; `Enter to confirm`
    /// does not, so `complete_modal_digest` can never match it.
    fn frame_missing_tail() -> String {
        let mut lines: Vec<&str> = FRAME_LIVE_MODAL.lines().collect();
        while let Some(last) = lines.last() {
            if last.contains("Enter to confirm") {
                lines.pop();
                break;
            }
            lines.pop();
        }
        lines.join("\n")
    }

    /// Drive one gate to the point where a candidate is stable enough to answer.
    fn stabilise(gate: &mut DevModalGate, screen: &str, start: u64) -> GateOutcome {
        gate.observe(screen, LogicalMs(start));
        gate.observe(screen, LogicalMs(start + MIN_STABLE_MS))
    }

    /// #3547 RED-1 / D(ii): a pane too short for the whole modal must not refuse
    /// forever. Before D(ii) this frame returned `NoCompleteModal` on every one
    /// of the thousands of reads a stranded generation performs, with no path out.
    #[test]
    fn short_pane_escalates_to_the_anchored_subset_after_k_frames() {
        let screen = frame_missing_tail();
        assert!(
            complete_modal_digest(&screen).is_none(),
            "fixture must be incomplete or the test proves nothing"
        );
        assert!(
            anchored_modal_digest(&screen).is_some(),
            "the anchored subset must still match a tail-cut modal"
        );
        let mut gate = DevModalGate::new(true);
        gate.set_prompt_blocked(true);
        // The Kth consecutive incomplete frame is the one that crosses, so the
        // first K-1 must still refuse.
        for frame in 0..(RELAXED_AFTER_INCOMPLETE_FRAMES - 1) {
            assert_eq!(
                gate.observe(&screen, LogicalMs(frame.into())),
                GateOutcome::Refuse(Refused::NoCompleteModal),
                "frame {frame} is below the relaxed threshold and must still refuse"
            );
        }
        let armed = gate.observe(
            &screen,
            LogicalMs(u64::from(RELAXED_AFTER_INCOMPLETE_FRAMES - 1)),
        );
        assert_eq!(
            armed,
            GateOutcome::Schedule,
            "the frame that crosses the threshold must adopt the anchored candidate"
        );
        assert_eq!(
            gate.observe(
                &screen,
                LogicalMs(u64::from(RELAXED_AFTER_INCOMPLETE_FRAMES - 1) + MIN_STABLE_MS)
            ),
            GateOutcome::Enqueue,
            "a stable anchored candidate must become answerable"
        );
    }

    /// #3547 RED-1b: the relaxed path is gated on the pane actually being blocked.
    /// A frame that merely fails to complete while the agent is running must keep
    /// refusing no matter how many times it is seen.
    #[test]
    fn short_pane_never_escalates_while_the_agent_is_not_prompt_blocked() {
        let screen = frame_missing_tail();
        let mut gate = DevModalGate::new(true);
        gate.set_prompt_blocked(false);
        for frame in 0..(RELAXED_AFTER_INCOMPLETE_FRAMES * 4) {
            assert_eq!(
                gate.observe(&screen, LogicalMs(frame.into())),
                GateOutcome::Refuse(Refused::NoCompleteModal),
                "frame {frame}: an unblocked pane must never reach the relaxed path"
            );
        }
    }

    /// #3547 RED-2 / D(i): a SECOND, genuinely different modal in the same
    /// generation must still be answerable. The per-generation one-shot refused
    /// it as `Spent` forever, leaving the pane blocked — the B2 break point.
    #[test]
    fn a_second_distinct_modal_in_one_generation_is_still_answerable() {
        let second = frame_for_channel("server:some-other-channel");
        assert_ne!(
            complete_modal_digest(FRAME_LIVE_MODAL),
            complete_modal_digest(&second),
            "the two modals must differ or this proves nothing"
        );
        let mut gate = DevModalGate::new(true);
        gate.set_prompt_blocked(true);
        assert_eq!(
            stabilise(&mut gate, FRAME_LIVE_MODAL, 0),
            GateOutcome::Enqueue
        );
        gate.enqueue_receipt().mark_enqueued();
        assert_eq!(
            gate.observe(FRAME_LIVE_MODAL, LogicalMs(1_000)),
            GateOutcome::Refuse(Refused::Spent),
            "the fingerprint just answered must never be answered twice"
        );
        assert_eq!(
            stabilise(&mut gate, &second, 2_000),
            GateOutcome::Enqueue,
            "a different modal on a blocked pane must still be answerable"
        );
    }

    /// #3547 D(i) bound: the per-fingerprint one-shot must not become an
    /// unbounded keystroke source when every frame carries a new digest.
    #[test]
    fn answers_are_capped_per_generation() {
        let mut gate = DevModalGate::new(true);
        gate.set_prompt_blocked(true);
        let mut answered = 0usize;
        for round in 0..(MAX_ANSWERS_PER_GENERATION + 3) {
            // A fresh digest every round — varied INSIDE the fingerprinted region.
            let screen = frame_for_channel(&format!("server:round-{round}"));
            if stabilise(&mut gate, &screen, (round as u64 + 1) * 10_000) == GateOutcome::Enqueue {
                gate.enqueue_receipt().mark_enqueued();
                answered += 1;
            }
        }
        assert_eq!(
            answered, MAX_ANSWERS_PER_GENERATION,
            "the generation must stop answering at the ceiling"
        );
    }

    /// #3547 RED-4 (reverse): the healthy path is untouched. A complete modal on
    /// an unblocked pane is answered exactly as before, by the STRICT fingerprint,
    /// and nothing relaxed is recorded.
    #[test]
    fn a_normal_generation_answers_strictly_and_records_no_relaxed_answer() {
        let mut gate = DevModalGate::new(true);
        gate.set_prompt_blocked(false);
        assert_eq!(
            stabilise(&mut gate, FRAME_LIVE_MODAL, 0),
            GateOutcome::Enqueue
        );
        gate.enqueue_receipt().mark_enqueued();
        let summary = gate.tally.summary_line();
        assert!(
            summary.contains("answered=1 (relaxed=0)"),
            "a strict answer must not be reported as relaxed: {summary}"
        );
    }

    /// The #3561 R1 frame: the warning QUOTED into the transcript (the pattern's
    /// `[^A-Za-z\n]*` prefix class admits `> `), with a DIFFERENT prompt below
    /// it actually holding the pane.
    fn quoted_warning_then(trailing: &str) -> String {
        let quoted: String = FRAME_LIVE_MODAL
            .lines()
            .take_while(|line| !line.contains("I am using this for local development"))
            .map(|line| format!("> {line}\n"))
            .collect();
        format!("{quoted}{trailing}")
    }

    /// #3561 R1 B1 (reviewer probe C): a quoted warning above a prompt that is
    /// really blocking the pane must NEVER be escalated. `prompt_blocked` is true
    /// here — because of that OTHER prompt — which is exactly why it alone was
    /// not enough. Before the fix this reached `Enqueue` and the CR would have
    /// answered the tool-approval prompt.
    #[test]
    fn a_quoted_warning_above_a_blocking_prompt_never_escalates_3561() {
        let screen = quoted_warning_then(
            "  Tool use: Bash\n  kubectl delete namespace prod\n\n  Do you want to proceed?\n  \u{276f} 1. Yes\n    2. No\n",
        );
        assert!(
            complete_modal_digest(&screen).is_none(),
            "the quote is incomplete, or this frame proves nothing"
        );
        let mut gate = DevModalGate::new(true);
        gate.set_prompt_blocked(true);
        // Remedy (b) is what production would inject here; leave it false so this
        // test isolates remedy (a) — the transcript below the quote.
        gate.set_other_prompt_on_screen(false);
        for frame in 0..(RELAXED_AFTER_INCOMPLETE_FRAMES * 2) {
            assert_eq!(
                gate.observe(&screen, LogicalMs(frame.into())),
                GateOutcome::Refuse(Refused::NoCompleteModal),
                "frame {frame}: a quoted warning with a transcript under it must never escalate"
            );
        }
    }

    /// The other-pattern fact remains an additional refusal even when the
    /// modal tail itself is recognized. Production wiring has real-entry tests.
    #[test]
    fn a_recognized_short_modal_is_refused_by_the_other_prompt_fact_3561() {
        let screen = frame_missing_tail();
        assert!(
            anchored_modal_digest(&screen).is_some(),
            "this frame must pass the bottom-region rule, or it does not isolate remedy (b)"
        );
        let mut gate = DevModalGate::new(true);
        gate.set_prompt_blocked(true);
        gate.set_other_prompt_on_screen(true);
        for frame in 0..(RELAXED_AFTER_INCOMPLETE_FRAMES * 2) {
            assert_eq!(
                gate.observe(&screen, LogicalMs(frame.into())),
                GateOutcome::Refuse(Refused::NoCompleteModal),
                "frame {frame}: another answerable prompt on the frame must block escalation"
            );
        }
    }

    /// #3561 R1 reverse (reviewer probes A/B): frames that do not carry the
    /// anchor literals at all never come close, however long they are blocked.
    #[test]
    fn unrelated_blocking_prompts_never_reach_the_anchor_3561() {
        for (label, screen) in [
            (
                "tool-approval",
                "  Tool use: Bash\n  rm -rf /tmp/scratch\n\n  Do you want to proceed?\n  \u{276f} 1. Yes\n    2. No\n",
            ),
            (
                "model-menu",
                "  Select model\n  \u{276f} 1. claude-opus-5\n    2. claude-sonnet-5\n    3. claude-haiku-4-5\n",
            ),
        ] {
            assert!(
                anchored_modal_digest(screen).is_none(),
                "{label}: must not match the anchor subset"
            );
            let mut gate = DevModalGate::new(true);
            gate.set_prompt_blocked(true);
            for frame in 0..48u32 {
                assert_eq!(
                    gate.observe(screen, LogicalMs(frame.into())),
                    GateOutcome::Refuse(Refused::NoCompleteModal),
                    "{label} frame {frame}: must never escalate"
                );
            }
        }
    }

    /// #3547 observability: the tally reports the LAST refuse, not the first.
    /// #3548's first-Refuse log cannot discriminate, because the first reads of
    /// every healthy generation precede the modal being painted.
    #[test]
    fn the_tally_reports_the_last_refuse_and_the_distribution() {
        let mut gate = DevModalGate::new(true);
        gate.set_prompt_blocked(false);
        gate.observe("nothing here", LogicalMs(0));
        gate.observe("still nothing", LogicalMs(1));
        gate.observe("anything", LogicalMs(ELIGIBILITY_EXPIRY_MS + 1));
        let summary = gate.tally.summary_line();
        assert!(
            summary.contains("last_refuse=WindowExpired"),
            "the LAST refuse must be reported: {summary}"
        );
        assert!(
            summary.contains("NoCompleteModal:2"),
            "the distribution must survive the last refuse: {summary}"
        );
    }
}
