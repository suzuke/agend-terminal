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

use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use parking_lot::Mutex;

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

/// A registered writer's live state: its fd, its own dedicated queue, and
/// the shutdown flag its dedicated [`writer_thread`] polls between
/// attempts. One `Arc<WriterState>` is shared between the enqueuing side
/// ([`write`]/[`unregister`]) and exactly one background thread — see the
/// module doc's "Architecture revision" section for why this replaced a
/// single global actor thread.
struct WriterState {
    fd: RawFd,
    shutdown: AtomicBool,
    queue: Mutex<VecDeque<Job>>,
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

/// Register `writer`'s raw fd (via `master.as_raw_fd()`) and spawn its
/// dedicated writer thread, so future `write_with_timeout` calls for it
/// route through this actor instead of spawning a disposable
/// thread-per-write. Call once, right after a `PtyWriter` is constructed
/// from `master.take_writer()`, while `master` is still available (both
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
    });
    let stale = writers().lock().insert(key, Arc::clone(&state));
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
    }
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
        .spawn(move || writer_thread(state))
    {
        tracing::error!(
            error = %e,
            fd,
            "write_actor: failed to spawn writer thread — this writer falls back to per-write \
             TimedOut until a future registration retries spawning"
        );
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
}

/// The registered fd for `writer`, if any. Test/introspection only --
/// production callers use [`write`] directly (it resolves the writer's
/// state internally).
#[cfg(test)]
fn fd_for(writer: &PtyWriter) -> Option<RawFd> {
    let key = Arc::as_ptr(writer) as usize;
    writers().lock().get(&key).map(|s| s.fd)
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
    Some(match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "PTY write timed out (5s) — backend may be stuck",
        )),
    })
}

/// A registered writer's dedicated background thread: poll its own fd,
/// service at most one bounded write per readiness event, repeat. Runs
/// until [`unregister`] flips `state.shutdown`.
fn writer_thread(state: Arc<WriterState>) {
    loop {
        if state.shutdown.load(Ordering::Acquire) {
            for job in state.queue.lock().drain(..) {
                job.fail(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "PTY writer torn down before this write completed",
                ));
            }
            return;
        }

        let mut pollfd = libc::pollfd {
            fd: state.fd,
            events: libc::POLLOUT,
            revents: 0,
        };
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
        // Fill the queue past the pty's real capacity (prototyped ~1KB on
        // this platform) so the actor genuinely can't drain it.
        let filler = vec![b'x'; 4096];
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
}
