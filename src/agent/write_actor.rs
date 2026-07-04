//! P1-2607-followup / herdr-inspired② (t-20260704054929866637-67777-5):
//! single background thread that drives ALL registered PTY writes via
//! `poll(POLLOUT)`, replacing `write_with_timeout`'s thread-per-write
//! mechanism for any writer that has been [`register`]ed. Full design
//! rationale, prototype data, and the rejected alternatives are in
//! `PTY-WRITE-ACTOR-SPIKE.md` (workspace/gapfix-dev) — summary:
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
//!   209 microseconds. Residual edge case: if the true headroom at the
//!   moment poll fires ready happens to land under 64 bytes (narrower than
//!   the ~200-byte case actually tested), that one write could still block
//!   momentarily. Lead-accepted, not defended against further (2026-07-04):
//!   the stall is bounded by how fast the backend drains the small
//!   difference, a real wedge is already caught by the not-ready path
//!   before this matters, and — critically — it can only ever stall THIS
//!   actor's handling of that ONE writer's queue, never any caller's own
//!   thread nor any other agent's writer (see per-writer queues below). A
//!   strict improvement over today's per-writer OS-thread leak, not a new
//!   failure mode.
//! - **Per-writer queues, not one global FIFO** (deliberately unlike
//!   `daemon::delivery_worker`'s single-FIFO shape): a wedged writer's
//!   queue must never delay any OTHER writer's turn. `poll()` naturally
//!   multiplexes many fds without needing separate "lanes" for this.
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

use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::unix::io::RawFd;
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

/// How often the actor thread wakes up even with no fd readiness event, so
/// a newly-enqueued writer (not yet in the poll set) gets picked up
/// promptly. Not a latency-sensitive path elsewhere in this system already
/// tolerates larger waits (typed-inject's own pacing sleeps, readback
/// confirm), so this granularity is more than sufficient.
const IDLE_POLL_MS: i32 = 50;

/// One outstanding write request for a single fd's queue.
struct Job {
    data: Vec<u8>,
    offset: usize,
    done: SyncSender<io::Result<()>>,
}

impl Job {
    fn remaining(&self) -> &[u8] {
        &self.data[self.offset..]
    }
}

/// `Arc::as_ptr(writer) as usize` (same identity scheme `WRITE_IN_PROGRESS`
/// already uses, `mod.rs:2577`) -> the registered raw fd for that writer.
/// `register` always inserts (overwrite semantics), so a `PtyWriter`
/// allocated at a since-reused address always gets a fresh, correct entry
/// before it's ever used for a write — no explicit unregister-on-teardown
/// is needed.
static FD_BY_WRITER: OnceLock<Mutex<HashMap<usize, RawFd>>> = OnceLock::new();

/// Per-fd pending-job queues, shared between enqueuing callers and the
/// actor thread. A fd present as a key (even with an empty queue only
/// transiently, immediately before removal) is being watched by the actor.
static QUEUES: OnceLock<Mutex<HashMap<RawFd, VecDeque<Job>>>> = OnceLock::new();

fn fd_by_writer() -> &'static Mutex<HashMap<usize, RawFd>> {
    FD_BY_WRITER.get_or_init(|| Mutex::new(HashMap::new()))
}

fn queues() -> &'static Mutex<HashMap<RawFd, VecDeque<Job>>> {
    QUEUES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register `writer`'s raw fd (via `master.as_raw_fd()`) so future
/// `write_with_timeout` calls for it route through this actor instead of
/// spawning a disposable thread. Call once, right after a `PtyWriter` is
/// constructed from `master.take_writer()`, while `master` is still
/// available (both production spawn sites already hold it at that point).
/// A no-op if `master.as_raw_fd()` returns `None` (Windows; any backend
/// without a raw fd) — that writer simply falls back to the historical
/// mechanism, forever, with no further action needed here.
pub(crate) fn register(writer: &PtyWriter, master: &dyn portable_pty::MasterPty) {
    let Some(fd) = master.as_raw_fd() else {
        return;
    };
    let key = Arc::as_ptr(writer) as usize;
    fd_by_writer().lock().insert(key, fd);
    // OS fd numbers are recycled: a stale queue left behind by a PREVIOUS
    // (now-closed) pty that happened to get this exact fd number would
    // otherwise sit ahead of this brand-new writer's jobs, silently
    // corrupting delivery order (or, worse, permanently blocking it behind
    // an undeliverable leftover job). `register` always means "this fd is
    // a fresh pty as of now" -- any old queue entry for it is necessarily
    // for something else that's gone.
    queues().lock().remove(&fd);
    ensure_actor_running();
}

/// The registered fd for `writer`, if any. `write_with_timeout` uses this
/// to decide: actor path (`Some`) or the historical thread-per-write
/// fallback (`None`).
pub(crate) fn fd_for(writer: &PtyWriter) -> Option<RawFd> {
    let key = Arc::as_ptr(writer) as usize;
    fd_by_writer().lock().get(&key).copied()
}

/// Enqueue `data` for `fd` and wait up to `timeout` for it to fully land.
/// Mirrors `write_with_timeout`'s existing caller contract exactly:
/// `Ok(())` on full delivery, `Err(TimedOut)` if `timeout` elapses first
/// (the write keeps being serviced in the background regardless — same as
/// today's spawned thread continuing past the caller's timeout), `Err(..)`
/// of another kind on a real write error (e.g. `EPIPE` — the pty is gone).
pub(crate) fn write(fd: RawFd, data: Vec<u8>, timeout: Duration) -> io::Result<()> {
    let (tx, rx) = sync_channel(1);
    {
        let mut qs = queues().lock();
        let q = qs.entry(fd).or_default();
        let pending: usize = q.iter().map(|j| j.remaining().len()).sum();
        if pending + data.len() > MAX_QUEUE_BYTES_PER_WRITER {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "PTY write queue full (backpressure) — writer has not drained in a long time",
            ));
        }
        q.push_back(Job {
            data,
            offset: 0,
            done: tx,
        });
    }
    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "PTY write timed out (5s) — backend may be stuck",
        )),
    }
}

static ACTOR_STARTED: OnceLock<()> = OnceLock::new();

fn ensure_actor_running() {
    ACTOR_STARTED.get_or_init(|| {
        // fire-and-forget: daemon-lifetime background thread, mirrors
        // `daemon::delivery_worker`'s worker (AUDIT2-006) — no graceful
        // join, the OS reaps it at process exit. Every job's caller-visible
        // outcome (Ok/TimedOut/error) is already delivered via its own
        // one-shot channel before this thread would ever need joining.
        if let Err(e) = std::thread::Builder::new()
            .name("agend-pty-write-actor".into())
            .spawn(actor_loop)
        {
            tracing::error!(
                error = %e,
                "write_actor: failed to spawn actor thread — all registered writers fall back to \
                 per-write TimedOut until a future registration retries spawning"
            );
        }
    });
}

fn actor_loop() {
    loop {
        let fds: Vec<RawFd> = queues().lock().keys().copied().collect();
        if fds.is_empty() {
            std::thread::sleep(Duration::from_millis(IDLE_POLL_MS as u64));
            continue;
        }

        let mut pollfds: Vec<libc::pollfd> = fds
            .iter()
            .map(|&fd| libc::pollfd {
                fd,
                events: libc::POLLOUT,
                revents: 0,
            })
            .collect();
        // Safety: `pollfds` is a valid, non-empty, properly-sized slice for
        // the duration of this call; `poll` only reads/writes within it.
        let n = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, IDLE_POLL_MS) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                tracing::warn!(error = %err, "write_actor: poll() failed, retrying");
            }
            continue;
        }
        if n == 0 {
            continue; // idle timeout, nothing ready -- loop to pick up new registrations
        }
        // #P1-2607 (macOS finding): a fully-wedged PTY master reports bare
        // POLLHUP (no POLLOUT) once its input queue is completely full --
        // NOT necessarily a dead/closed fd. Treating POLLHUP as fatal here
        // would misclassify an ordinary wedge as a permanent error and
        // drop every queued job for it. Only POLLOUT means "safe to
        // attempt a bounded write"; a genuinely dead fd is discovered via
        // the write() syscall's own error (EPIPE/EBADF/etc.) once POLLOUT
        // does fire, or -- if it truly never does -- the caller's existing
        // 5s timeout applies, same as any other wedge.
        //
        // Critically: POLLHUP/POLLERR/POLLNVAL are reported UNCONDITIONALLY
        // by poll() regardless of the requested `events` mask, so a
        // permanently-wedged fd makes `n > 0` on EVERY call without ever
        // waiting out `IDLE_POLL_MS` -- if nothing in this batch actually
        // had POLLOUT, this is not a real readiness event; sleep before
        // the next iteration or this becomes a tight, CPU-burning spin.
        let mut any_actionable = false;
        for pfd in &pollfds {
            if pfd.revents & libc::POLLOUT != 0 {
                any_actionable = true;
                service_fd_once(pfd.fd);
            }
        }
        if !any_actionable {
            std::thread::sleep(Duration::from_millis(IDLE_POLL_MS as u64));
        }
    }
}

/// Attempt exactly ONE bounded write for `fd`'s front job. One attempt per
/// readiness event, deliberately -- avoids any risk of a burst of writes
/// re-creating the "ask for more than the momentary headroom" hazard the
/// 64-byte bound exists to avoid.
fn service_fd_once(fd: RawFd) {
    let mut qs = queues().lock();
    let Some(q) = qs.get_mut(&fd) else { return };

    let Some(job) = q.front_mut() else {
        qs.remove(&fd);
        return;
    };

    let remaining = job.remaining();
    let n = remaining.len().min(CHUNK_SIZE);
    let chunk_ptr = remaining.as_ptr();
    // Safety: `chunk_ptr` points at `n` valid, initialized bytes borrowed
    // from `job.data` for the duration of this syscall only.
    let ret = unsafe { libc::write(fd, chunk_ptr as *const libc::c_void, n) };
    if ret < 0 {
        let err = io::Error::last_os_error();
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
            let _ = job.done.try_send(Err(io::Error::new(err.kind(), err.to_string())));
        }
        qs.remove(&fd);
        return;
    }
    job.offset += ret as usize;
    if job.offset >= job.data.len() {
        let job = q.pop_front().expect("front() just returned Some");
        let _ = job.done.try_send(Ok(()));
        if q.is_empty() {
            qs.remove(&fd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// The actor + its queues/registrations are PROCESS-GLOBAL singletons
    /// (mirrors `daemon::delivery_worker`'s one-worker-per-process shape).
    /// Under `cargo test`'s default parallel execution, two of these tests
    /// running concurrently would otherwise contend for the same actor
    /// thread's poll cycles and real wall-clock timing assertions become
    /// flaky under system load -- same class of problem this codebase's
    /// `worktree_cleanup.rs::tests::ENV_LOCK` already solves for shared
    /// process-global env-var state. Every test below takes this lock
    /// first.
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
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
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
    }

    #[test]
    fn unregistered_writer_falls_back() {
        let _lock = TEST_LOCK.lock();
        // A plain in-memory writer never passed through `register` --
        // must NOT resolve to any fd (exercises the fallback path).
        let writer: PtyWriter = Arc::new(parking_lot::Mutex::new(Box::new(Vec::new())
            as Box<dyn std::io::Write + Send>));
        assert!(fd_for(&writer).is_none());
    }

    #[test]
    fn write_succeeds_when_buffer_has_room() {
        let _lock = TEST_LOCK.lock();
        let (writer, mut child, _master) = wedged_pty("write-ok", 5);
        let fd = fd_for(&writer).expect("registered");
        let result = write(fd, b"hello".to_vec(), Duration::from_secs(2));
        assert!(result.is_ok(), "a fresh pty must accept a small write: {result:?}");
        let _ = child.kill();
    }

    /// The core P1-2607 property: a write to a GENUINELY wedged writer
    /// times out (matching `write_with_timeout`'s existing `TimedOut`
    /// contract) rather than hanging the calling thread indefinitely.
    #[test]
    fn write_times_out_on_wedged_writer_without_hanging_caller() {
        let _lock = TEST_LOCK.lock();
        let (writer, mut child, _master) = wedged_pty("wedge-timeout", 10);
        let fd = fd_for(&writer).expect("registered");
        // Fill the queue past the pty's real capacity (prototyped ~1KB on
        // this platform) so the actor genuinely can't drain it.
        let filler = vec![b'x'; 4096];
        let _ = write(fd, filler, Duration::from_millis(500));

        let start = std::time::Instant::now();
        let result = write(fd, b"more".to_vec(), Duration::from_millis(500));
        let elapsed = start.elapsed();
        assert!(result.is_err(), "a wedged writer's write must report an error, not silently succeed");
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::TimedOut,
            "a wedged writer must surface TimedOut, matching write_with_timeout's existing contract"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "must fail-fast within the given timeout, not hang; took {elapsed:?}"
        );
        let _ = child.kill();
    }

    /// #P1-2607 wedge ISOLATION: one wedged writer's queue must never
    /// delay writes to a DIFFERENT, healthy writer. This is the whole
    /// point of per-writer queues instead of one global FIFO
    /// (`delivery_worker.rs`'s existing shape).
    #[test]
    fn wedged_writer_does_not_block_a_different_healthy_writer() {
        let _lock = TEST_LOCK.lock();
        let (wedged_writer, mut wedged_child, _wedged_master) = wedged_pty("isolation-wedged", 10);
        let wedged_fd = fd_for(&wedged_writer).expect("registered");
        // Saturate the wedged writer's queue.
        let filler = vec![b'x'; 4096];
        let _ = write(wedged_fd, filler, Duration::from_millis(200));

        // A second, healthy pty whose slave DOES drain stdin (`cat`).
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
        let healthy_fd = fd_for(&healthy_writer).expect("registered");

        let start = std::time::Instant::now();
        let result = write(healthy_fd, b"hello\n".to_vec(), Duration::from_secs(2));
        let elapsed = start.elapsed();
        assert!(
            result.is_ok(),
            "a healthy writer must succeed even while another writer is wedged: {result:?}"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "a healthy writer must not be delayed by an unrelated wedged writer's queue; took {elapsed:?}"
        );

        let _ = wedged_child.kill();
        let _ = healthy_child.kill();
    }

    /// #P1-2607 backpressure: a writer whose queue is saturated (never
    /// drains) must reject further writes (drop, surfaced as an error)
    /// rather than growing its queue without bound.
    #[test]
    fn saturated_queue_drops_further_writes_instead_of_growing_unbounded() {
        let _lock = TEST_LOCK.lock();
        let (writer, mut child, _master) = wedged_pty("backpressure", 10);
        let fd = fd_for(&writer).expect("registered");
        // Fill to exactly the cap with a very short wait, then immediately
        // try to push a chunk far larger than anything the actor could
        // plausibly have drained in that short window (prototyped: this
        // platform's real PTY input queue holds ~1KB total, so even a
        // generous drain estimate is nowhere near this 64KB probe size) --
        // keeps the boundary check robust against exactly-at-the-cap
        // timing flakiness.
        let big = vec![b'x'; MAX_QUEUE_BYTES_PER_WRITER];
        let _ = write(fd, big, Duration::from_millis(20));

        let start = std::time::Instant::now();
        let result = write(fd, vec![b'y'; 64 * 1024], Duration::from_millis(200));
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
    }
}
