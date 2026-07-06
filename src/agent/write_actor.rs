//! P1-2607-followup / herdr-inspired② (t-20260704054929866637-67777-5):
//! one dedicated background thread PER registered PTY writer, driving that
//! writer's writes via `poll(POLLOUT)`, replacing `write_with_timeout`'s
//! thread-PER-WRITE mechanism for any writer that has been [`register`]ed.
//! Full design rationale, prototype data, and the rejected alternatives are
//! in `PTY-WRITE-ACTOR-SPIKE.md` (workspace/gapfix-dev) — summary:
//!
//! - **The fd stays in its normal BLOCKING mode, always.** The original
//!   plan (set `O_NONBLOCK`) was dropped: `portable_pty`'s Unix
//!   `take_writer()`/`try_clone_reader()` both `dup()` the SAME master fd,
//!   so `O_NONBLOCK` (which lives on the shared open file description, not
//!   the fd number) would also hit the READ side — and `pty_read_loop`
//!   treats any read error, including `WouldBlock`, as fatal. Never touch
//!   this fd's blocking mode.
//! - Instead, every write is gated behind `poll(fd, POLLOUT, timeout)`
//!   FIRST. Not-ready ⇒ the writer is genuinely wedged (backend not
//!   draining stdin) ⇒ don't even attempt a write; wait for the next
//!   readiness check. This is how a wedge is detected with ZERO blocking
//!   syscalls.
//! - Once POLLOUT is ready, the actual `write()` is bounded to
//!   [`CHUNK_SIZE`] (64) bytes — NOT the whole pending buffer. Prototyped:
//!   with only ~200 bytes of headroom and POLLOUT ready, a 4096-byte write
//!   blocked for 3+ seconds (poll only promises *some* room, not "as much
//!   as you're about to ask for"), but a 64-byte write (matching the chunk
//!   size `inject_with_target` already uses, `mod.rs:2806`) returned in
//!   209 microseconds.
//! - **Backpressure: bounded per-writer byte cap, drop on full** — mirrors
//!   `delivery_worker`'s `try_send`/drop contract (same LANGUAGE, not a new
//!   rule). A writer whose queue is already saturated (permanently wedged,
//!   nobody ever drains it) gets new writes dropped with an error, rather
//!   than growing without bound.
//! - Any writer NEVER [`register`]ed (Windows — `MasterPty::as_raw_fd()`
//!   returns `None` there — or any exotic backend without a raw fd) simply
//!   isn't in [`fd_for`]'s map, so `write_with_timeout` falls back to the
//!   pre-existing thread-per-write mechanism, completely unchanged. No
//!   `#[cfg(windows)]` needed anywhere in this file or the fallback path —
//!   it's a runtime "do we have a registered fd" branch, not a compile-time
//!   platform split.
//!
//! ## Architecture revision (2026-07-04): one thread per writer, not one global actor
//!
//! The original design (still described almost everywhere above) used ONE
//! global background thread multiplexing every registered fd via `poll()`,
//! with a single global `Mutex<HashMap<RawFd, _>>` holding every writer's
//! queue. That shape had two compounding bugs, both found empirically while
//! bringing up this module's own test suite (not merely hypothesized):
//!
//! 1. **fd-number reuse race**: OS fd numbers are recycled the moment the
//!    last dup of a closed pty's open file description goes away. A stale
//!    queue entry for a just-closed writer's fd number, if not proactively
//!    cleared, can be serviced by the actor against a BRAND NEW pty that
//!    happens to get the same fd number (observed directly in this test
//!    suite: `wedged_pty()`'s own 300ms post-`openpty()`-pre-`register()`
//!    window was enough for this to happen reliably). Fixed by requiring
//!    every caller to pair [`register`] with an explicit [`unregister`]
//!    BEFORE the underlying fd is actually closed — the fd's bookkeeping is
//!    provably empty by the time the OS could hand the number to anyone
//!    else. (This alone was the originally-hypothesized fix and is still in
//!    effect below, just re-scoped to one writer's own state rather than a
//!    shared map.)
//! 2. **Single-thread head-of-line blocking (the deeper bug)**: even with
//!    fix #1, `service_fd_once` (the old per-fd service step) held the
//!    ENTIRE global queue mutex across the raw `write()` syscall. The
//!    module's own design doc had already identified a "residual edge
//!    case" — POLLOUT fires ready but the true headroom is under
//!    [`CHUNK_SIZE`] bytes, so the bounded write can still block — and
//!    judged it "self-bounded, non-spreading... can only ever stall THIS
//!    actor's handling of that ONE writer's queue". That judgment was
//!    WRONG: holding the lock across the syscall meant a single blocked
//!    write froze every OTHER writer's enqueue AND every other fd's own
//!    service attempt, since they all contend on the same mutex — measured
//!    directly (temporary instrumentation) at a **9.7-second** block on one
//!    writer's write() call stalling an unrelated, healthy writer's write()
//!    past its own 2-second timeout. Worse: since a genuinely-wedged test
//!    pty's ~1KB buffer reliably lands on a sub-64-byte final chunk on its
//!    way to full, this isn't a rare edge case — it is the ROUTINE outcome
//!    of filling any sufficiently large backlog against a non-draining
//!    writer. A global single-threaded design (whether or not it holds a
//!    lock across the syscall) can never fully avoid this: while the ONE
//!    actor thread is parked inside one writer's blocking `write()`, it
//!    cannot service ANY other fd's readiness either, lock or no lock.
//!
//! Lead-decided fix (2026-07-04, evidence-driven — see task
//! `t-20260704054929866637-67777-5` history): **one dedicated thread per
//! registered writer**, each running its own tiny `poll`+bounded-`write`
//! loop against only its own fd, with its own queue. [`register`] spawns
//! it; [`unregister`] flips a per-writer `shutdown` flag and fails any
//! still-queued jobs (the thread notices `shutdown` and exits on its own —
//! not joined, see [`register`]'s spawn site for why). This is a deliberate
//! departure from `delivery_worker.rs`'s single-worker precedent, justified
//! by scale: thread count is bounded by the number of *live registered
//! writers*, i.e. roughly the daemon's live agent count (~10-15 in this
//! project's normal operation), not by write volume — nothing like the
//! per-write thread churn the original #2160/H13 bug produced. A wedged
//! writer's thread can still park in a single blocking `write()` for as
//! long as its backend never drains, exactly as before, but that is now
//! contained entirely within its OWN OS thread — provable by the OS
//! scheduler, not by application-level lock discipline — and never delays
//! any other writer's enqueue or service.
//!
//! ## Correctness fix (same day, CI-caught): don't rely solely on explicit `unregister`
//!
//! `unregister` is only wired at ONE production teardown site
//! (`cleanup_agent`). Pre-existing test code across the workspace
//! constructs real agents via `spawn_agent`/`spawn_ephemeral_worker` (which
//! call [`register`]) and tears them down by simply dropping the handle —
//! never calling `unregister`. Under `cargo nextest` (this module's own
//! test suite) that's invisible, since nextest isolates every test in its
//! own process. But CI's Coverage job uses plain `cargo test`, which runs
//! ALL unit tests in one shared process — every such leaked registration
//! left its dedicated thread running (polling every [`IDLE_POLL_MS`])
//! *for the rest of that process's life*, and the accumulating thread count
//! measurably slowed the whole suite (19x: 74s on `main` vs 1418s on this
//! branch) and was strongly correlated with a downstream, CPU/scheduling-
//! sensitive test (`state::tests::replay_manifest_regression`, which uses
//! wall-clock `Instant`-based thresholds) intermittently failing. Fix: each
//! [`WriterState`] also holds a `Weak` reference to the `PtyWriter` Arc
//! itself; [`writer_thread`] checks it every loop iteration (alongside
//! `shutdown`) and self-retires the moment the writer's LAST strong
//! reference is dropped, by ANY teardown path, with no explicit call
//! required. `unregister` remains the fast, synchronous path or production
//! code that knows the exact teardown moment (needed to close the fd-reuse
//! race window described above); the weak-reference check is the backstop
//! that makes correctness independent of every caller remembering it.
//!
//! **Residual, and why it's bounded**: relying purely on the weak-reference
//! backstop (no `unregister` call at all — today, only pre-existing test
//! leaks) reopens a NARROWER version of the fd-reuse race: an orphaned
//! writer's thread notices `liveness` failed to upgrade only on its own
//! polling schedule, not synchronously at the moment of drop, so a fd could
//! in principle be closed and handed to a brand new pty before that
//! orphaned thread notices. [`register`] closes this down to the same
//! single-in-flight-syscall residual `unregister` itself already accepts
//! (not zero, but bounded to at most one syscall, never an unbounded
//! polling-latency window): it scans the (small — bounded by live writer
//! count, ~10-15) [`WRITERS`] map for any OTHER entry that already claims
//! the SAME fd number being registered (only possible if the OS already
//! closed that entry's writer, since it wouldn't reuse a fd otherwise) and
//! retires it synchronously before proceeding — see this module's own test
//! `fd_reuse_race_during_weak_backstop_window_does_not_cross_deliver` for
//! the regression coverage.
//!
//! **Why not full `Drop`-based RAII on `PtyWriter` itself** (making
//! `unregister` synchronous and automatic everywhere, turning the weak-ref
//! polling into a pure fallback that should structurally never fire):
//! `PtyWriter` (`Arc<Mutex<Box<dyn Write + Send>>>`, defined in `mod.rs`) is
//! a plain type alias, not a distinct type this module owns — `impl Drop`
//! requires either a newtype wrapper (touching every existing construction
//! site across the codebase, `Arc::new(Mutex::new(...))` calls that
//! predate this module) or a `Drop` on the boxed `dyn Write` itself (loses
//! the writer/master pairing `register` needs, and still wouldn't fire
//! until the LAST `Arc` clone drops, which is exactly what `Weak::upgrade`
//! already detects without a wrapper). Given every production call site
//! already pairs `register`/`unregister` correctly, the weak-reference
//! backstop was the smaller, call-site-transparent fix for the ACTUAL gap
//! (pre-existing test code); a full RAII refactor was not attempted.
//!
//! ## Lazy thread spawn (same day, CI-caught): register() no longer starts a thread
//!
//! The weak-reference backstop (previous section) makes leaked
//! registrations eventually self-clean, but "eventually" still means every
//! leaked registration owns a real OS thread, polling every
//! [`IDLE_POLL_MS`], for however long it takes the scheduler to give it a
//! turn to notice. Under CI's Coverage job specifically (a coverage-
//! instrumented build, `cargo test`'s own default parallelism, and likely
//! only 2-4 vCPUs) that scheduling latency compounds across however many
//! pre-existing tests leak a registration (see the previous section) badly
//! enough that even with the backstop in place, the whole-suite slowdown
//! only partially recovered (1418s -> 1091s versus `main`'s 74s baseline,
//! measured directly). Most of these leaked registrations, though, never
//! actually have a write attempted through them during their test's short
//! life — [`register`] no longer spawns a thread at all; [`ensure_started`]
//! does, exactly once (idempotent via `WriterState::started`), on a
//! writer's FIRST real [`write`] call. A registered-but-never-written-to
//! writer (the common leaked-test shape) now costs one small heap
//! allocation and a map entry, not a whole OS thread — the fd-reuse guard
//! in [`register`] and the [`retire`] cleanup path are both unaffected
//! (whether a thread was ever spawned for a given entry doesn't change how
//! it's found/retired).

use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use parking_lot::{Condvar, Mutex};

use super::PtyWriter;

/// Bound on a single `write()` syscall attempted after `poll(POLLOUT)`
/// reports ready. Matches the chunk size `inject_with_target` already uses
/// (`mod.rs:2806`) — not a new constant. See the module doc for why this
/// specific size is safe (prototyped against a real wedged PTY).
const CHUNK_SIZE: usize = 64;

/// Backpressure cap: total un-written bytes queued for one writer before
/// further enqueues are dropped. Generous relative to the real PTY input
/// queues observed in prototyping (~1KB on this platform) — this exists to
/// bound memory for a writer that NEVER drains again (dead/zombie agent),
/// not to fire under normal operation.
const MAX_QUEUE_BYTES_PER_WRITER: usize = 1 << 20; // 1 MiB

/// How often a writer's thread wakes up even with no fd readiness event, so
/// a POLLHUP-without-POLLOUT fd (see [`writer_thread`]) doesn't tight-spin.
/// Not a latency-sensitive path elsewhere in this system already tolerates
/// larger waits (typed-inject's own pacing sleeps, readback confirm), so
/// this granularity is more than sufficient.
const IDLE_POLL_MS: i32 = 50;

/// One outstanding write request for a writer's queue.
struct Job {
    data: Vec<u8>,
    offset: usize,
    done: SyncSender<io::Result<()>>,
}

impl Job {
    fn remaining(&self) -> &[u8] {
        &self.data[self.offset..]
    }

    fn fail(self, err: io::Error) {
        let _ = self.done.try_send(Err(err));
    }
}

/// A registered writer's live state: its fd, its own dedicated queue, the
/// shutdown flag its dedicated [`writer_thread`] polls between attempts,
/// a `Weak` handle to the `PtyWriter` itself so the thread can detect "the
/// writer was dropped and nobody called `unregister`" (see the module
/// doc's "Correctness fix" section) independent of `shutdown`, and
/// `started` (see the module doc's "Lazy thread spawn" section) gating
/// whether a thread has been spawned for this writer at all yet. One
/// `Arc<WriterState>` is shared between the enqueuing side
/// ([`write`]/[`unregister`]) and at most one background thread.
///
/// `queue_notify` (#2620 fix) pairs with `queue`: [`writer_thread`] parks on
/// it instead of calling `poll()` while `queue` is empty (a healthy PTY fd
/// is essentially ALWAYS `POLLOUT`-ready, so polling with nothing to write
/// would busy-spin at 100% CPU — see [`writer_thread`]'s doc). [`write`]
/// notifies it after enqueuing; [`unregister`] and `register`'s stale-entry
/// cleanup notify it after flipping `shutdown`, so a parked thread retires
/// promptly instead of waiting out a full `IDLE_POLL_MS`.
struct WriterState {
    fd: RawFd,
    shutdown: AtomicBool,
    queue: Mutex<VecDeque<Job>>,
    queue_notify: Condvar,
    liveness: Weak<parking_lot::Mutex<Box<dyn io::Write + Send>>>,
    started: AtomicBool,
    /// Test-only observability: counts real `poll()` syscalls issued by
    /// [`writer_thread`]. Always present (not `#[cfg(test)]`) to keep the
    /// struct literal simple; the increment is one relaxed atomic op on an
    /// already-syscall-bound path, immaterial next to the `poll()` itself.
    /// See `tests::idle_writer_never_polls_while_queue_stays_empty` — the
    /// direct regression-proof that an empty queue costs ZERO `poll()`
    /// calls, not merely "fewer".
    poll_calls: AtomicU64,
}

/// `Arc::as_ptr(writer) as usize` (same identity scheme `WRITE_IN_PROGRESS`
/// already uses, `mod.rs:2577`) -> that writer's live state. Entries are
/// removed by [`unregister`] — without it this map would grow without
/// bound over a long-running daemon's lifetime (one entry per agent ever
/// spawned).
static WRITERS: OnceLock<Mutex<HashMap<usize, Arc<WriterState>>>> = OnceLock::new();

fn writers() -> &'static Mutex<HashMap<usize, Arc<WriterState>>> {
    WRITERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register `writer`'s raw fd (via `master.as_raw_fd()`) so future
/// `write_with_timeout` calls for it route through this actor instead of
/// spawning a disposable thread-per-write. Does NOT spawn this writer's
/// dedicated thread yet — see the module doc's "Lazy thread spawn" section
/// and [`ensure_started`], which does that on the first actual [`write`].
/// Call once, right after a `PtyWriter` is constructed from
/// `master.take_writer()`, while `master` is still available (both
/// production spawn sites already hold it at that point). A no-op if
/// `master.as_raw_fd()` returns `None` (Windows; any backend without a raw
/// fd) — that writer simply falls back to the historical mechanism,
/// forever, with no further action needed here.
///
/// Callers MUST pair this with [`unregister`] when the writer is torn down
/// (before the underlying fd is actually closed) — relying on a FUTURE
/// `register` call to clean up a stale entry leaves the fd-reuse race
/// window (see the module doc) open for as long as nothing else happens to
/// reuse that exact fd number.
pub(crate) fn register(writer: &PtyWriter, master: &dyn portable_pty::MasterPty) {
    let Some(fd) = master.as_raw_fd() else {
        return;
    };
    let key = Arc::as_ptr(writer) as usize;
    let state = Arc::new(WriterState {
        fd,
        shutdown: AtomicBool::new(false),
        queue: Mutex::new(VecDeque::new()),
        queue_notify: Condvar::new(),
        liveness: Arc::downgrade(writer),
        started: AtomicBool::new(false),
        poll_calls: AtomicU64::new(0),
    });

    let mut ws = writers().lock();
    // fd-reuse guard, orthogonal to the same-key check below: the OS only
    // ever hands `fd` to a brand new open() after fully closing whatever
    // held it before, so ANY other entry in this map still claiming `fd` is
    // necessarily orphaned (its writer was dropped -- otherwise the OS
    // couldn't have reused the number) and hasn't yet noticed via the
    // `liveness` backstop (see the module doc's "Correctness fix" section).
    // Retiring it HERE, synchronously, shrinks that backstop's inherent
    // polling-latency window down to the same single-in-flight-syscall
    // residual `unregister` itself accepts, instead of leaving it exposed
    // for up to one whole `IDLE_POLL_MS` cycle (or longer) of that orphaned
    // thread potentially still servicing writes against the reused fd.
    let stale_by_fd: Vec<usize> = ws
        .iter()
        .filter(|(k, v)| **k != key && v.fd == fd)
        .map(|(k, _)| *k)
        .collect();
    for stale_key in stale_by_fd {
        if let Some(stale) = ws.remove(&stale_key) {
            stale.shutdown.store(true, Ordering::Release);
            for job in stale.queue.lock().drain(..) {
                job.fail(io::Error::other(
                    "PTY writer re-registered at this fd number before the orphaned previous \
                     writer's thread noticed (fd reused)",
                ));
            }
            // #2620: wake a possibly-parked thread immediately rather than
            // making it wait out a full IDLE_POLL_MS to notice `shutdown`.
            stale.queue_notify.notify_one();
        }
    }

    let stale = ws.insert(key, Arc::clone(&state));
    drop(ws);
    // A leftover entry at this exact writer-pointer address means a
    // previous writer was never explicitly `unregister`ed before this new
    // registration (only possible if the allocator reused a freed `Arc`'s
    // address) -- retire its thread and fail its jobs now rather than
    // leaving two threads racing over what was that address's bookkeeping.
    if let Some(stale) = stale {
        stale.shutdown.store(true, Ordering::Release);
        for job in stale.queue.lock().drain(..) {
            job.fail(io::Error::other(
                "PTY writer re-registered at this address before the previous write completed",
            ));
        }
        // #2620: see the stale_by_fd loop above for why this notify matters.
        stale.queue_notify.notify_one();
    }
    // Deliberately no thread spawned here -- see the module doc's "Lazy
    // thread spawn" section. [`ensure_started`] spawns one on this writer's
    // FIRST actual [`write`] call.
}

/// Spawn `state`'s dedicated thread the first time it's actually needed
/// (see the module doc's "Lazy thread spawn" section), idempotently: the
/// `compare_exchange` ensures exactly one spawn per writer even if multiple
/// callers race to enqueue a write for it concurrently. A no-op on every
/// call after the first.
fn ensure_started(key: usize, state: &Arc<WriterState>) {
    if state
        .started
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return; // already started (or another racing caller just claimed it)
    }
    let fd = state.fd;
    let thread_state = Arc::clone(state);
    // fire-and-forget: this thread is retired by `unregister` flipping
    // `shutdown` (checked once per poll cycle, see `writer_thread`) -- it
    // is intentionally NOT joined. Joining synchronously here (or at
    // `unregister`) would risk blocking the daemon's teardown path for as
    // long as this writer's underlying pty stays wedged (up to however
    // long the backend never drains), reintroducing exactly the kind of
    // unbounded-wait-on-a-stuck-backend bug this whole module exists to
    // remove. Every job's caller-visible outcome (Ok/TimedOut/error) is
    // already delivered via its own one-shot channel independent of when
    // (or whether) this thread actually notices `shutdown` and exits.
    if let Err(e) = std::thread::Builder::new()
        .name("agend-pty-writer".into())
        .spawn(move || writer_thread(key, thread_state))
    {
        tracing::error!(
            error = %e,
            fd,
            "write_actor: failed to spawn writer thread — this writer falls back to per-write \
             TimedOut until a future write retries spawning"
        );
        // Allow a future write() call to retry the spawn instead of being
        // permanently wedged by this one failure.
        state.started.store(false, Ordering::Release);
    }
}

/// Retire `writer`'s registration: signal its dedicated thread to stop and
/// fail any jobs still queued for it. Call this BEFORE the underlying
/// master/fd is actually closed (i.e. before the last `Arc`/`Box` holding it
/// drops) — that ordering is what closes the fd-reuse race window (see the
/// module doc): once this returns, this writer's queue is guaranteed empty,
/// so even if the OS immediately hands the same fd number to a brand new
/// pty, there is nothing stale left to misdeliver against it. The thread
/// itself is not joined (see the rationale in [`register`]'s spawn site);
/// it exits on its own once it next checks `shutdown`, which for a writer
/// currently blocked inside one `write()` call may be after that call
/// finally returns.
pub(crate) fn unregister(writer: &PtyWriter) {
    let key = Arc::as_ptr(writer) as usize;
    let Some(state) = writers().lock().remove(&key) else {
        return;
    };
    state.shutdown.store(true, Ordering::Release);
    for job in state.queue.lock().drain(..) {
        job.fail(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "PTY writer torn down before this write completed",
        ));
    }
    // #2620: wake a parked writer_thread immediately so it retires now
    // instead of waiting out a full IDLE_POLL_MS to notice `shutdown`.
    state.queue_notify.notify_one();
}

/// SINGLE chokepoint for evicting a live `AgentHandle` from `registry`:
/// removes it and unregisters its `pty_writer` (via [`unregister`]) BEFORE
/// the handle — and its `pty_writer`/`pty_master` — actually drops.
///
/// #P1-2607-followup (reviewer4, PR #2620 REJECTED finding): every
/// registry-eviction site must go through this, not a bare
/// `registry.lock().remove(id)` — [`register`] is lazy-spawn (no thread
/// exists for a writer that's never had a write attempted through it), so
/// the weak-reference backstop inside [`writer_thread`] never runs for such
/// a writer; explicit `unregister` at EVERY teardown path is its only
/// cleanup. A full grep sweep of the codebase's `registry.remove` call
/// sites found five production sites needing this: `agent::cleanup_agent`,
/// `daemon::lifecycle::delete_transaction`,
/// `daemon::lifecycle::SpawnRollback::drop`, `daemon::handle_clean_exit`,
/// and `app::pane_factory::reap_late_registration_if_shutdown` — all now
/// route through here (`agent::remove_and_unregister` on Unix).
pub(crate) fn remove_and_unregister(
    registry: &super::AgentRegistry,
    id: &crate::types::InstanceId,
) -> Option<super::AgentHandle> {
    let removed = super::lock_registry(registry).remove(id);
    if let Some(handle) = &removed {
        unregister(&handle.pty_writer);
    }
    removed
}

/// The registered fd for `writer`, if any. Test/introspection only --
/// production callers use [`write`] directly (it resolves the writer's
/// state internally). `pub(super)` so `agent`'s own cross-module test
/// helpers (e.g. verifying `daemon::lifecycle`'s teardown paths actually
/// unregister) can reach it too, not just this module's own tests.
#[cfg(test)]
pub(super) fn fd_for(writer: &PtyWriter) -> Option<RawFd> {
    let key = Arc::as_ptr(writer) as usize;
    writers().lock().get(&key).map(|s| s.fd)
}

/// #2620 regression-proof: how many real `poll()` syscalls `writer_thread`
/// has issued for `writer` so far. `None` if unregistered.
#[cfg(test)]
fn poll_call_count(writer: &PtyWriter) -> Option<u64> {
    let key = Arc::as_ptr(writer) as usize;
    writers()
        .lock()
        .get(&key)
        .map(|s| s.poll_calls.load(Ordering::Relaxed))
}

/// Enqueue `data` for `writer` and wait up to `timeout` for it to fully
/// land. `None` if `writer` isn't currently registered (never was, or
/// already `unregister`ed) — `write_with_timeout` uses this to decide:
/// actor path (`Some`) or the historical thread-per-write fallback
/// (`None`). Mirrors `write_with_timeout`'s existing caller contract
/// exactly on the `Some` path: `Ok(())` on full delivery, `Err(TimedOut)`
/// if `timeout` elapses first (the write keeps being serviced in the
/// background regardless — same as today's spawned thread continuing past
/// the caller's timeout), `Err(..)` of another kind on a real write error
/// (e.g. `EPIPE` — the pty is gone).
pub(crate) fn write(
    writer: &PtyWriter,
    data: Vec<u8>,
    timeout: Duration,
) -> Option<io::Result<()>> {
    let key = Arc::as_ptr(writer) as usize;
    let state = writers().lock().get(&key).cloned()?;
    ensure_started(key, &state);
    let (tx, rx) = sync_channel(1);
    {
        let mut q = state.queue.lock();
        let pending: usize = q.iter().map(|j| j.remaining().len()).sum();
        if pending + data.len() > MAX_QUEUE_BYTES_PER_WRITER {
            return Some(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "PTY write queue full (backpressure) — writer has not drained in a long time",
            )));
        }
        q.push_back(Job {
            data,
            offset: 0,
            done: tx,
        });
    }
    // #2620: wake writer_thread if it's parked (queue was empty) so it
    // proceeds to poll+service this job immediately instead of waiting out
    // IDLE_POLL_MS. Notifying after the lock is dropped is safe here: the
    // push above happened under the same `queue` lock `writer_thread`
    // re-checks before parking, so there's no window where a wakeup could
    // be missed.
    state.queue_notify.notify_one();
    Some(match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "PTY write timed out (5s) — backend may be stuck",
        )),
    })
}

/// Shared cleanup for both `writer_thread`'s self-retire (shutdown flag OR
/// dead `liveness` weak ref) and would-be future callers: fail whatever's
/// left in the queue, then remove `key` from the [`WRITERS`] map -- but
/// ONLY if the map's CURRENT entry at `key` is still THIS `state` (compares
/// by `Arc::ptr_eq`). Without that check, a thread retiring late (e.g.
/// after being stuck mid-syscall) could otherwise delete a BRAND NEW
/// registration that has since reused the same writer-pointer address.
fn retire(key: usize, state: &Arc<WriterState>, reason: &str) {
    for job in state.queue.lock().drain(..) {
        job.fail(io::Error::new(io::ErrorKind::BrokenPipe, reason));
    }
    let mut ws = writers().lock();
    if ws
        .get(&key)
        .is_some_and(|current| Arc::ptr_eq(current, state))
    {
        ws.remove(&key);
    }
}

/// What a writer's dedicated thread does on its next loop iteration —
/// extracted as a pure function (no fd/lock access) so the state machine can
/// be pinned by unit tests directly. `shutdown`/`liveness_dead` take
/// priority over an empty queue (retiring beats parking indefinitely on a
/// torn-down writer); otherwise an empty queue means genuinely nothing to
/// service, so the thread parks instead of polling — see [`writer_thread`]'s
/// doc for why polling an empty queue was the #2620 busy-loop bug.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NextAction {
    Retire,
    Park,
    Poll,
}

fn decide_next_action(shutdown: bool, liveness_dead: bool, queue_empty: bool) -> NextAction {
    if shutdown || liveness_dead {
        NextAction::Retire
    } else if queue_empty {
        NextAction::Park
    } else {
        NextAction::Poll
    }
}

/// A registered writer's dedicated background thread: service at most one
/// bounded write per readiness event, repeat. Runs until [`unregister`]
/// flips `state.shutdown`, OR (the backstop — see the module doc's
/// "Correctness fix" section) `state.liveness` fails to upgrade, meaning the
/// `PtyWriter`'s last strong reference was dropped by some OTHER teardown
/// path that never called `unregister`.
///
/// #2620: a healthy PTY fd is essentially ALWAYS `POLLOUT`-ready (kernel
/// write buffers stay far from full in normal operation), so unconditionally
/// `poll()`ing even with an empty queue meant: poll returns ready
/// immediately -> [`service_once`] no-ops on the empty queue -> loop back ->
/// poll again immediately -> zero-sleep busy loop (confirmed on a live
/// daemon: two `agend-pty-writer` threads at 99.6% samples inside `poll()`,
/// `sys` time far exceeding `user`). [`decide_next_action`] now gates
/// polling behind "queue is non-empty"; while empty, the thread parks on
/// `state.queue_notify` instead, woken by [`write`]'s enqueue notify (so
/// write latency doesn't regress — enqueue-to-wake is ~0ms) with a
/// `wait_timeout(IDLE_POLL_MS)` ceiling that preserves this loop's original
/// per-`IDLE_POLL_MS` liveness/shutdown-backstop patrol cadence even when
/// never notified (the module doc's "Correctness fix" section is the reason
/// that cadence exists — the park must keep re-checking it, not wait
/// forever).
fn writer_thread(key: usize, state: Arc<WriterState>) {
    loop {
        let shutdown = state.shutdown.load(Ordering::Acquire);
        let liveness_dead = state.liveness.upgrade().is_none();
        let mut q = state.queue.lock();
        let queue_empty = q.is_empty();

        match decide_next_action(shutdown, liveness_dead, queue_empty) {
            NextAction::Retire => {
                drop(q);
                retire(
                    key,
                    &state,
                    "PTY writer torn down before this write completed",
                );
                return;
            }
            NextAction::Park => {
                // Still holding `q`, acquired above BEFORE the `queue_empty`
                // check: `wait_timeout` atomically releases it and
                // reacquires before returning, so a `write()` enqueuing in
                // between (it needs this same lock to push) can never be
                // missed — either it landed before we got here (queue_empty
                // would already be false) or it happens while we're parked
                // (its notify wakes us).
                state
                    .queue_notify
                    .wait_for(&mut q, Duration::from_millis(IDLE_POLL_MS as u64));
                continue;
            }
            NextAction::Poll => drop(q), // don't hold the queue lock across poll()/write()
        }

        let mut pollfd = libc::pollfd {
            fd: state.fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        state.poll_calls.fetch_add(1, Ordering::Relaxed);
        // Safety: `pollfd` is a valid single-element buffer for the
        // duration of this call.
        let n = unsafe { libc::poll(&mut pollfd, 1, IDLE_POLL_MS) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                tracing::warn!(error = %err, fd = state.fd, "write_actor: poll() failed, retrying");
            }
            continue;
        }
        if n == 0 {
            continue; // idle timeout, nothing ready -- loop to recheck shutdown
        }
        if pollfd.revents & libc::POLLOUT != 0 {
            service_once(&state);
            continue;
        }
        // #P1-2607 (macOS finding): a fully-wedged PTY master reports bare
        // POLLHUP (no POLLOUT) once its input queue is completely full --
        // NOT necessarily a dead/closed fd, and POLLHUP/POLLERR/POLLNVAL
        // are reported UNCONDITIONALLY by poll() regardless of the
        // requested `events` mask. Treating POLLHUP as fatal here would
        // misclassify an ordinary wedge as a permanent error; but taking no
        // action AND not sleeping would spin this thread at 100% CPU
        // forever on a permanently-wedged fd (`n > 0` on every call even
        // though nothing is actionable). Only POLLOUT means "safe to
        // attempt a bounded write"; a genuinely dead fd is discovered via
        // the write() syscall's own error (EPIPE/EBADF/etc.) once POLLOUT
        // does fire, or -- if it truly never does -- the caller's existing
        // timeout applies, same as any other wedge.
        std::thread::sleep(Duration::from_millis(IDLE_POLL_MS as u64));
    }
}

/// Attempt exactly ONE bounded write for this writer's front job. One
/// attempt per readiness event, deliberately -- avoids any risk of a burst
/// of writes re-creating the "ask for more than the momentary headroom"
/// hazard the 64-byte bound exists to avoid. May block for an arbitrary
/// duration if the true headroom at the moment `poll` fired ready was under
/// [`CHUNK_SIZE`] bytes and the backend never drains further (a real,
/// permanent wedge) -- contained entirely within this writer's own
/// dedicated thread (see the module doc's "Architecture revision"), never
/// delaying any other writer's enqueue or service.
fn service_once(state: &WriterState) {
    let (chunk, offset_before) = {
        let q = state.queue.lock();
        let Some(job) = q.front() else { return };
        let remaining = job.remaining();
        let n = remaining.len().min(CHUNK_SIZE);
        (remaining[..n].to_vec(), job.offset)
    };

    // Safety: `chunk` is a valid, initialized, `chunk.len()`-byte buffer
    // owned by this stack frame for the duration of this syscall.
    let ret = unsafe { libc::write(state.fd, chunk.as_ptr() as *const libc::c_void, chunk.len()) };
    let write_err = if ret < 0 {
        Some(io::Error::last_os_error())
    } else {
        None
    };

    let mut q = state.queue.lock();
    let Some(job) = q.front_mut() else {
        // `unregister` drained the queue while the syscall was in flight --
        // that already resolved this job's caller-visible outcome; discard.
        return;
    };
    if job.offset != offset_before {
        // Single-thread-per-writer invariant violated (shouldn't happen --
        // no other code path advances `offset`) -- be defensive rather than
        // apply a `ret` computed against a since-changed job.
        return;
    }

    let Some(err) = write_err else {
        job.offset += ret as usize;
        if job.offset >= job.data.len() {
            let job = q.pop_front().expect("front() just returned Some");
            let _ = job.done.try_send(Ok(()));
        }
        return;
    };
    if err.kind() == io::ErrorKind::WouldBlock {
        // Spurious/racy readiness (POLLOUT fired but the buffer filled
        // again before we got to it) -- try again on the next cycle.
        return;
    }
    // A genuine write error (EPIPE/EBADF/etc.) means the fd is dead --
    // every OTHER queued job for it would fail identically, so fail
    // the whole queue now instead of rediscovering the same error one
    // job at a time.
    while let Some(job) = q.pop_front() {
        job.fail(io::Error::new(err.kind(), err.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    use std::sync::atomic::{AtomicU32, Ordering as StdOrdering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// Registrations are process-global (the `WRITERS` map), but each
    /// writer now gets its OWN dedicated thread + queue -- unlike the
    /// original single-global-actor design, tests no longer contend for a
    /// SHARED thread's poll cycles. This lock is kept anyway: several tests
    /// below assert real wall-clock timing bounds (e.g. "must complete
    /// within 500ms"), which is still flaky if run concurrently with other
    /// tests under system load (extra scheduler contention, not a shared
    /// actor-thread bottleneck) -- same rationale `worktree_cleanup.rs::
    /// tests::ENV_LOCK` already applies for shared process-global state.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    // ── #2620: decide_next_action pure-decision pins ──────────────────

    #[test]
    fn decide_next_action_shutdown_wins_over_empty_queue() {
        assert_eq!(
            decide_next_action(true, false, true),
            NextAction::Retire,
            "shutdown must retire even with an empty queue"
        );
    }

    #[test]
    fn decide_next_action_dead_liveness_wins_over_work_available() {
        assert_eq!(
            decide_next_action(false, true, false),
            NextAction::Retire,
            "a dropped writer (dead liveness) must retire even with queued work"
        );
    }

    #[test]
    fn decide_next_action_parks_on_empty_queue() {
        assert_eq!(
            decide_next_action(false, false, true),
            NextAction::Park,
            "nothing to service -> park instead of polling (the #2620 fix)"
        );
    }

    #[test]
    fn decide_next_action_polls_when_work_is_queued() {
        assert_eq!(
            decide_next_action(false, false, false),
            NextAction::Poll,
            "queued work -> poll+service, unchanged from pre-#2620 behavior"
        );
    }

    /// #2620 core regression-proof: an idle writer (queue empty, nothing
    /// enqueued) must issue ZERO `poll()` syscalls while idle -- not
    /// "fewer", zero, since the fix's whole point is that an empty queue
    /// never reaches the poll() call at all (it parks instead). Pre-fix,
    /// a healthy fd here would have busy-spun through many thousands of
    /// poll() calls over this same window.
    #[test]
    fn idle_writer_never_polls_while_queue_stays_empty() {
        let _lock = TEST_LOCK.lock();
        let (writer, mut child, _master) = wedged_pty("idle-no-poll", 5);

        // One real write to spawn the thread (lazy start) and drive the
        // queue back to empty once it lands.
        let result = write(&writer, b"hello".to_vec(), Duration::from_secs(2))
            .expect("registered writer must resolve to Some");
        assert!(result.is_ok(), "priming write must succeed: {result:?}");

        let baseline = poll_call_count(&writer).expect("writer must still be registered");
        std::thread::sleep(Duration::from_millis(3 * IDLE_POLL_MS as u64));
        let after_idle = poll_call_count(&writer).expect("writer must still be registered");
        assert_eq!(
            after_idle, baseline,
            "an idle writer with an empty queue must not call poll() at all \
             (busy-loop regression, #2620); baseline={baseline} after_idle={after_idle}"
        );

        // Write latency must not have regressed: a park must wake promptly
        // on write()'s notify, not wait out IDLE_POLL_MS.
        let start = std::time::Instant::now();
        let result = write(&writer, b"world".to_vec(), Duration::from_secs(2))
            .expect("registered writer must resolve to Some");
        let elapsed = start.elapsed();
        assert!(result.is_ok(), "post-idle write must succeed: {result:?}");
        assert!(
            elapsed < Duration::from_millis(IDLE_POLL_MS as u64),
            "a parked writer must wake on write()'s notify near-instantly, not wait out \
             IDLE_POLL_MS; took {elapsed:?}"
        );

        let _ = child.kill();
        unregister(&writer);
    }

    /// A real pty pair whose slave is put into raw mode and never reads
    /// stdin (`stty raw -echo; sleep N`) -- the exact wedge condition
    /// prototyped for the spike (PTY-WRITE-ACTOR-SPIKE.md §3). Returns the
    /// registered `PtyWriter` (already wired through [`register`]) plus the
    /// child (kill it when done) and the master (keep it alive -- dropping
    /// it closes the fd).
    fn wedged_pty(
        tag: &str,
        sleep_secs: u32,
    ) -> (
        PtyWriter,
        Box<dyn portable_pty::Child + Send + Sync>,
        Box<dyn portable_pty::MasterPty + Send>,
    ) {
        let id = COUNTER.fetch_add(1, StdOrdering::Relaxed);
        let _ = tag;
        let _ = id;
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let mut cmd = CommandBuilder::new("sh");
        cmd.arg("-c");
        cmd.arg(format!("stty raw -echo; sleep {sleep_secs}"));
        let child = pair.slave.spawn_command(cmd).expect("spawn sh");
        drop(pair.slave);
        std::thread::sleep(Duration::from_millis(300));

        let writer: PtyWriter = Arc::new(parking_lot::Mutex::new(
            pair.master.take_writer().expect("take_writer"),
        ));
        register(&writer, pair.master.as_ref());
        (writer, child, pair.master)
    }

    #[test]
    fn register_populates_fd_for_a_real_pty() {
        let _lock = TEST_LOCK.lock();
        let (writer, mut child, _master) = wedged_pty("register", 5);
        assert!(
            fd_for(&writer).is_some(),
            "a real PTY's writer must resolve to a registered fd"
        );
        let _ = child.kill();
        unregister(&writer);
    }

    #[test]
    fn unregistered_writer_falls_back() {
        let _lock = TEST_LOCK.lock();
        // A plain in-memory writer never passed through `register` --
        // must NOT resolve to any fd (exercises the fallback path).
        let writer: PtyWriter = Arc::new(parking_lot::Mutex::new(
            Box::new(Vec::new()) as Box<dyn std::io::Write + Send>
        ));
        assert!(fd_for(&writer).is_none());
        assert!(write(&writer, b"x".to_vec(), Duration::from_millis(100)).is_none());
    }

    #[test]
    fn write_succeeds_when_buffer_has_room() {
        let _lock = TEST_LOCK.lock();
        let (writer, mut child, _master) = wedged_pty("write-ok", 5);
        let result = write(&writer, b"hello".to_vec(), Duration::from_secs(2))
            .expect("registered writer must resolve to Some");
        assert!(
            result.is_ok(),
            "a fresh pty must accept a small write: {result:?}"
        );
        let _ = child.kill();
        unregister(&writer);
    }

    /// The core P1-2607 property: a write to a GENUINELY wedged writer
    /// times out (matching `write_with_timeout`'s existing `TimedOut`
    /// contract) rather than hanging the calling thread indefinitely.
    #[test]
    fn write_times_out_on_wedged_writer_without_hanging_caller() {
        let _lock = TEST_LOCK.lock();
        let (writer, mut child, _master) = wedged_pty("wedge-timeout", 10);
        // Fill the queue past the pty's real capacity so the actor genuinely
        // can't drain it. The real kernel pty input queue is platform-
        // dependent (prototyped ~1KB on macOS; CI's ubuntu-latest runner
        // accepted a 4096-byte filler without wedging, i.e. Linux's is
        // larger) -- use the same `MAX_QUEUE_BYTES_PER_WRITER` filler size
        // the backpressure test below already uses, comfortably exceeding
        // any realistic platform's real pty buffer.
        let filler = vec![b'x'; MAX_QUEUE_BYTES_PER_WRITER];
        let _ = write(&writer, filler, Duration::from_millis(500));

        let start = std::time::Instant::now();
        let result = write(&writer, b"more".to_vec(), Duration::from_millis(500))
            .expect("registered writer must resolve to Some");
        let elapsed = start.elapsed();
        assert!(
            result.is_err(),
            "a wedged writer's write must report an error, not silently succeed"
        );
        assert_eq!(
            result.expect_err("checked is_err above").kind(),
            std::io::ErrorKind::TimedOut,
            "a wedged writer must surface TimedOut, matching write_with_timeout's existing contract"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "must fail-fast within the given timeout, not hang; took {elapsed:?}"
        );
        let _ = child.kill();
        unregister(&writer);
    }

    /// #P1-2607 wedge ISOLATION, thread-boundary semantics (architecture
    /// revision 2026-07-04): a wedged writer's dedicated thread can be
    /// parked inside a single blocking `write()` for as long as its backend
    /// never drains -- that is now an OS-thread-scheduling fact, not a
    /// shared-lock one -- and it must never delay a DIFFERENT, healthy
    /// writer's enqueue (`write`) or its own thread's service of it. This
    /// test previously failed under the original single-global-actor-thread
    /// design (confirmed empirically: the wedged writer's thread blocked
    /// ~9.7s inside one `write()` syscall, and because that design held a
    /// SHARED queue lock across the syscall, this healthy writer's own
    /// `write()` call timed out waiting for that same lock).
    #[test]
    fn wedged_writer_does_not_block_a_different_healthy_writer() {
        let _lock = TEST_LOCK.lock();
        let (wedged_writer, mut wedged_child, _wedged_master) = wedged_pty("isolation-wedged", 10);
        // Saturate the wedged writer's queue -- its dedicated thread will
        // park inside a blocking write() for as long as the backend (which
        // never reads) never drains, entirely within its own OS thread.
        let filler = vec![b'x'; 4096];
        let _ = write(&wedged_writer, filler, Duration::from_millis(200));

        // A second, healthy pty whose slave DOES drain stdin (`cat`), on
        // its own separate dedicated thread.
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let cmd = CommandBuilder::new("cat");
        let mut healthy_child = pair.slave.spawn_command(cmd).expect("spawn cat");
        drop(pair.slave);
        let healthy_writer: PtyWriter = Arc::new(parking_lot::Mutex::new(
            pair.master.take_writer().expect("take_writer"),
        ));
        register(&healthy_writer, pair.master.as_ref());

        let start = std::time::Instant::now();
        let result = write(&healthy_writer, b"hello\n".to_vec(), Duration::from_secs(2))
            .expect("registered writer must resolve to Some");
        let elapsed = start.elapsed();
        assert!(
            result.is_ok(),
            "a healthy writer must succeed even while another writer's thread is wedged: {result:?}"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "a healthy writer must not be delayed by an unrelated wedged writer's own thread; took {elapsed:?}"
        );

        let _ = wedged_child.kill();
        let _ = healthy_child.kill();
        unregister(&wedged_writer);
        unregister(&healthy_writer);
    }

    /// #P1-2607 backpressure: a writer whose queue is saturated (never
    /// drains) must reject further writes (drop, surfaced as an error)
    /// rather than growing its queue without bound.
    #[test]
    fn saturated_queue_drops_further_writes_instead_of_growing_unbounded() {
        let _lock = TEST_LOCK.lock();
        let (writer, mut child, _master) = wedged_pty("backpressure", 10);
        // Fill to exactly the cap with a very short wait, then immediately
        // try to push a chunk far larger than anything the actor could
        // plausibly have drained in that short window (prototyped: this
        // platform's real PTY input queue holds ~1KB total, so even a
        // generous drain estimate is nowhere near this 64KB probe size) --
        // keeps the boundary check robust against exactly-at-the-cap
        // timing flakiness.
        let big = vec![b'x'; MAX_QUEUE_BYTES_PER_WRITER];
        let _ = write(&writer, big, Duration::from_millis(20));

        let start = std::time::Instant::now();
        let result = write(&writer, vec![b'y'; 64 * 1024], Duration::from_millis(200))
            .expect("registered writer must resolve to Some");
        let elapsed = start.elapsed();
        assert!(
            result.is_err(),
            "a write to an already-saturated queue must be rejected, not silently queued forever"
        );
        assert!(
            elapsed < Duration::from_millis(100),
            "the saturated-queue rejection must be immediate (checked at enqueue time), not wait \
             out the full timeout; took {elapsed:?}"
        );

        let _ = child.kill();
        unregister(&writer);
    }

    /// #P1-2607-followup fd-reuse regression: a fresh registration at a fd
    /// number that a PREVIOUS (properly unregistered) writer used must never
    /// see, service, or be delayed by that old writer's backlog. Forces the
    /// exact race the actor thread can otherwise hit: enqueue a large,
    /// slow-to-drain job on writer A, `unregister` A (simulating a teardown
    /// that races the actor's own service loop), then immediately open a
    /// brand new writer B and assert its queue starts empty and a small
    /// write on it succeeds promptly regardless of A's leftover backlog.
    #[test]
    fn fd_reuse_does_not_inherit_a_torn_down_writers_backlog() {
        let _lock = TEST_LOCK.lock();
        let (writer_a, mut child_a, _master_a) = wedged_pty("fd-reuse-a", 10);

        // Saturate A's queue with a job that has no realistic chance of
        // fully draining before we tear A down (mirrors
        // `saturated_queue_...`'s own filler size/timeout).
        let filler = vec![b'x'; MAX_QUEUE_BYTES_PER_WRITER];
        let _ = write(&writer_a, filler, Duration::from_millis(20));

        // Explicit teardown BEFORE the master/child are dropped -- this is
        // the fix under test: `unregister` must synchronously drain+fail A's
        // backlog so nothing survives to be misdelivered if fd_a's number
        // gets reused.
        unregister(&writer_a);
        let _ = child_a.kill();
        drop(_master_a);

        // A brand new writer, registered fresh. Whether or not the OS
        // actually recycled A's fd number here, the invariant under test is
        // unconditional: this writer's queue must be empty at registration
        // and a small write must land quickly, never blocked or corrupted
        // by A's backlog.
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let cmd = CommandBuilder::new("cat");
        let mut child_b = pair.slave.spawn_command(cmd).expect("spawn cat");
        drop(pair.slave);
        let writer_b: PtyWriter = Arc::new(parking_lot::Mutex::new(
            pair.master.take_writer().expect("take_writer"),
        ));
        register(&writer_b, pair.master.as_ref());

        let start = std::time::Instant::now();
        let result = write(&writer_b, b"hello\n".to_vec(), Duration::from_secs(2))
            .expect("registered writer must resolve to Some");
        let elapsed = start.elapsed();
        assert!(
            result.is_ok(),
            "a freshly registered writer must not inherit a torn-down writer's backlog, even if \
             the OS reused its fd number: {result:?}"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "a fresh writer must not be delayed by a torn-down writer's leftover queue; took {elapsed:?}"
        );

        let _ = child_b.kill();
        unregister(&writer_b);
    }

    /// #P1-2607-followup correctness fix: a writer whose caller drops it
    /// WITHOUT ever calling `unregister` (exactly what several pre-existing
    /// tests elsewhere in the workspace do, e.g. `agent::tests` constructing
    /// real agents via `spawn_agent` and just dropping the handle) must not
    /// leak its dedicated thread forever. Registers a writer, drops every
    /// strong reference to it (no `unregister` call), and asserts the
    /// `WRITERS` map entry disappears on its own within a bounded time --
    /// the `liveness` weak-reference backstop noticing and self-retiring.
    #[test]
    fn dropped_writer_without_unregister_is_eventually_reaped() {
        let _lock = TEST_LOCK.lock();
        let (writer, mut child, master) = wedged_pty("leak-backstop", 2);
        // Capture identity BEFORE dropping -- `writers()` is a process-global
        // map shared with every OTHER test in this binary (this test suite
        // runs under plain `cargo test`'s shared-process parallelism, not
        // just this module's own serialized tests), so asserting on the
        // map's total size would be flaky against concurrently-registered,
        // unrelated writers. Only this specific key's presence matters.
        let key = Arc::as_ptr(&writer) as usize;
        assert!(
            fd_for(&writer).is_some(),
            "must be registered right after wedged_pty"
        );
        // `register` no longer spawns a thread by itself (see the module
        // doc's "Lazy thread spawn" section) -- a registered-but-never-
        // written-to writer has nothing to self-retire (no thread exists to
        // notice the liveness check), and is expected to leave a small,
        // thread-less map entry until something calls `unregister` (every
        // production teardown path does). This test is specifically about
        // the thread-based backstop, so trigger `ensure_started` via one
        // real write first, matching the writer shapes that actually pay
        // for a dedicated thread.
        let _ = write(&writer, b"x".to_vec(), Duration::from_millis(200));

        // Drop EVERY strong reference -- no `unregister` call, simulating a
        // teardown path that doesn't know about it.
        drop(writer);
        drop(master);
        let _ = child.kill();

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut still_present = writers().lock().contains_key(&key);
        while still_present && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            still_present = writers().lock().contains_key(&key);
        }
        assert!(
            !still_present,
            "a writer dropped without `unregister` must eventually be reaped by the liveness \
             backstop, not leak its thread + WRITERS entry forever"
        );
    }

    /// #P1-2607-followup: the narrower fd-reuse race the pure weak-reference
    /// backstop reopens (see the module doc's "Residual, and why it's
    /// bounded" section) — writer A is dropped WITHOUT `unregister` (the
    /// exact pre-existing-test-leak shape this backstop exists for), and a
    /// new writer B is registered immediately after, racing A's thread
    /// before it has any chance to notice its `liveness` weak ref failed on
    /// its own polling schedule. Whether or not the OS actually recycles
    /// A's exact fd number for B here, the invariant under test is
    /// unconditional: B's own write must land quickly and correctly,
    /// verifying `register`'s synchronous by-fd-number retirement (not just
    /// by-writer-pointer-key) closes this down to the same
    /// single-in-flight-syscall residual `unregister` itself accepts.
    #[test]
    fn fd_reuse_race_during_weak_backstop_window_does_not_cross_deliver() {
        let _lock = TEST_LOCK.lock();
        let (writer_a, mut child_a, master_a) = wedged_pty("weak-fd-reuse-a", 10);
        let filler = vec![b'x'; MAX_QUEUE_BYTES_PER_WRITER];
        let _ = write(&writer_a, filler, Duration::from_millis(20));

        // Drop EVERYTHING for A -- deliberately no `unregister` call, so
        // the only path to cleanup is the weak-ref backstop, which hasn't
        // had a single loop iteration to notice yet.
        drop(writer_a);
        drop(master_a);
        let _ = child_a.kill();

        // Register B immediately -- no delay, maximizing the chance of
        // racing A's still-orphaned thread (and, if the OS reuses fd
        // numbers eagerly as observed elsewhere in this suite, likely
        // landing on the exact same fd number A just freed).
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let cmd = CommandBuilder::new("cat");
        let mut child_b = pair.slave.spawn_command(cmd).expect("spawn cat");
        drop(pair.slave);
        let writer_b: PtyWriter = Arc::new(parking_lot::Mutex::new(
            pair.master.take_writer().expect("take_writer"),
        ));
        register(&writer_b, pair.master.as_ref());

        let start = std::time::Instant::now();
        let result = write(&writer_b, b"hello\n".to_vec(), Duration::from_secs(2))
            .expect("registered writer must resolve to Some");
        let elapsed = start.elapsed();
        assert!(
            result.is_ok(),
            "writer B must succeed even if it raced an orphaned (leaked, never-unregistered) \
             writer A's still-live thread for the same fd number: {result:?}"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "writer B must not be delayed by A's leftover backlog even without an explicit \
             `unregister` on A; took {elapsed:?}"
        );

        let _ = child_b.kill();
        unregister(&writer_b);
    }
}
