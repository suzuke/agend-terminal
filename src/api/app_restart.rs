//! #2453 R2: app owner-restart request channel + atomic gate.
//!
//! `agend-terminal app` restart works by RE-EXEC (not spawn): job-control proof
//! (bash+zsh PTY harness, spike #2453 R2) shows a shell reclaims the terminal
//! after the tracked leader exits, so a spawned successor is backgrounded; `exec`
//! keeps the same PID and therefore the same shell job. The `restart_daemon`
//! handler (an API worker thread) hands a request to the TUI loop over an
//! INJECTED bounded channel — never a process-global. A shared [`AppRestartGate`]
//! enforces, via a genuine compare-and-swap (no read-then-write TOCTOU), that AT
//! MOST ONE request crosses `Serving → Probing → Committing` even under concurrent
//! API workers (decision d-20260712034222169749-5).

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

const SERVING: u8 = 0;
const PROBING: u8 = 1;
const COMMITTING: u8 = 2;

/// #2453 R2: the verdict the TUI loop returns to the (blocking) API handler over the
/// request's bounded oneshot, in response to the preflight probe.
///
/// `Prepared` (probe passed) does NOT commit — the gate STAYS `Probing`. The handler
/// returns the `prepared` JSON (an honest indeterminate attempt) and registers a
/// post-flush ack; the TUI transitions `Probing → Committing` and re-execs ONLY after
/// the transport confirms that reply flushed to the socket (see [`PostFlushSlot`]).
/// This closes the pre-flush-commit race: a failed flush leaves the gate recoverable
/// at `Probing`, and the `prepared` reply can't be lost to a teardown that outran the
/// writer. Unix-only machinery (Windows fail-closes at the handler and never reads this).
#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Debug, Clone)]
pub enum AppRestartVerdict {
    /// Probe passed; gate is `Probing`. The handler emits the `prepared` reply and
    /// registers a [`PostFlushSlot`] ack. NOT yet committed.
    Prepared,
    /// Probe failed / errored / timed out. Fleet + TUI intact; no restart.
    Aborted(String),
}

/// #2453 R2: a typed, single-shot action the restart handler registers into the
/// per-request [`PostFlushSlot`]. `handle_session` runs it EXACTLY ONCE, and ONLY
/// when the response write+flush both succeeded — it sends the TUI its
/// commit-permission (a `()` on `flush_ack`). On ANY non-success path (write fail,
/// flush fail, session exit before flush) the action is DROPPED un-run, so the
/// captured sender drops and the TUI's `flush_ack` receiver DISCONNECTS → the TUI
/// aborts (gate back to `Serving`). Drop-disconnect IS the cancel; there is no
/// explicit "cancelled" value. `Send` because the tool handler runs in a spawned
/// worker thread while `handle_session` runs the action on the API thread.
pub type AfterFlushAction = Box<dyn FnOnce() + Send>;

/// #2453 R2: a per-API-request, thread-safe, single-shot slot for an
/// [`AfterFlushAction`]. `handle_session` creates a fresh slot per request iteration
/// and threads a cheap `Arc` clone through `HandlerCtx` → `RuntimeContext` to the
/// tool handler — which runs in a SEPARATE worker thread (`handle_mcp_tool_inner`
/// spawns it and may abandon it on the tool timeout), so this is `Mutex`-guarded, not
/// a `RefCell`. After writing the response, `handle_session` calls
/// [`PostFlushSlot::run_after_flush`], which atomically takes the action and CLOSES
/// the slot under the lock, then (outside the lock) runs it on success / drops it on
/// failure. A late/timed-out worker's [`PostFlushSlot::register`] then no-ops, so a
/// stale worker can never act against a LATER request's flush.
#[derive(Clone, Default)]
pub struct PostFlushSlot(Arc<Mutex<PostFlushState>>);

#[derive(Default)]
struct PostFlushState {
    action: Option<AfterFlushAction>,
    closed: bool,
    /// #2453 R2 P0: set by the app-restart handler on EVERY gate-dependent response
    /// (prepared / retryable in_progress loser / aborted / timed-out) so `handle_session`
    /// evicts this request_id from the dedup cache. These responses reflect MOMENTARY
    /// gate state, so caching any of them would let a later same-id retry observe a
    /// stale transient (e.g. a cached `in_progress` after the winner aborted → the retry
    /// never re-enters the now-`Serving` gate). The gate — not the cache — is the
    /// idempotence authority. An armed slot (a registered `prepared` action) also implies
    /// non-cacheable, independent of this flag.
    non_cacheable: bool,
}

impl PostFlushSlot {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register the action to run after THIS request's response flushes. Returns
    /// `false` (no-op) when the slot is already closed (its response was processed)
    /// or already holds an action — so a late / duplicate / timed-out worker cannot
    /// hijack a later response's flush (the #2453 R2 "a concurrent response cannot
    /// consume the slot" invariant). Only the Unix restart handler registers.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn register(&self, action: AfterFlushAction) -> bool {
        let mut st = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if st.closed || st.action.is_some() {
            return false;
        }
        st.action = Some(action);
        true
    }

    /// Called by `handle_session` after the write+flush attempt. Atomically takes the
    /// registered action AND closes the slot under the lock (so any late `register`
    /// sees `closed`), then releases the lock and — only if `flushed_ok` — runs it.
    /// On `!flushed_ok` the action is dropped un-run → its captured sender drops →
    /// the TUI's `flush_ack` disconnects → abort. Idempotent.
    pub fn run_after_flush(&self, flushed_ok: bool) {
        let action = {
            let mut st = self.0.lock().unwrap_or_else(|e| e.into_inner());
            st.closed = true;
            st.action.take()
        };
        if flushed_ok {
            if let Some(action) = action {
                action();
            }
        }
        // else: `action` drops here un-run → sender drops → TUI flush_ack disconnects.
    }

    /// #2453 R2 P0: mark THIS response non-cacheable. The app-restart handler calls
    /// this on every gate-dependent path (prepared / retryable in_progress loser /
    /// aborted / timed-out) so a same-id retry always re-enters the handler and the
    /// [`AppRestartGate`] decides from CURRENT state — closing the "cached transient
    /// loser wedges retry-after-abort" composition hole. Idempotent.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn mark_non_cacheable(&self) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .non_cacheable = true;
    }

    /// #2453 R2 P0: whether `handle_session` must evict this request_id from the dedup
    /// cache. True when the handler marked the response non-cacheable OR the slot is
    /// armed (a registered `prepared` action is inherently non-cacheable). Non-consuming.
    pub fn is_non_cacheable(&self) -> bool {
        let st = self.0.lock().unwrap_or_else(|e| e.into_inner());
        st.non_cacheable || (!st.closed && st.action.is_some())
    }
}

/// A restart request handed from the handler to the TUI loop. `reply` carries the
/// TUI's verdict ([`AppRestartVerdict`]); `flush_ack` is how the TUI waits for the
/// transport's post-flush commit-permission (a `()` on success; a DISCONNECT — the
/// action dropped un-run — means abort) before it commits + re-execs.
#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Debug)]
pub struct AppRestartRequest {
    pub reply: crossbeam_channel::Sender<AppRestartVerdict>,
    pub flush_ack: crossbeam_channel::Receiver<()>,
}

/// Injected sender (handler → TUI loop); bounded (capacity 1) at the composition root.
pub type AppRestartSender = crossbeam_channel::Sender<AppRestartRequest>;

/// The injected app-restart capability: the request sender + the shared gate.
/// Created at the app API composition root ([`crate::app`]), threaded through
/// `serve` → API `HandlerCtx` → MCP `RuntimeContext` to the `restart_daemon`
/// handler. `Clone` is cheap (a channel `Sender` + an `Arc`). Absent (`None`) on
/// the daemon / verify roots, which fail-closed. Unix-only: Windows fail-closes at
/// the handler and never reads these fields → precise per-item allow (NOT a broad
/// module suppression).
#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Clone)]
pub struct AppRestart {
    pub tx: AppRestartSender,
    pub gate: AppRestartGate,
}

/// Shared atomic restart gate. `Clone` shares the SAME atomic (`Arc`): injected
/// into the handler side (via `RuntimeContext`) AND owned by the TUI loop.
/// Default = `Serving`.
#[derive(Clone, Default)]
pub struct AppRestartGate(Arc<AtomicU8>);

impl AppRestartGate {
    pub fn new() -> Self {
        Self(Arc::new(AtomicU8::new(SERVING)))
    }

    /// Atomically claim the gate `Serving → Probing`. Returns `true` iff THIS
    /// caller won the claim. Genuine CAS — at most one concurrent caller wins; a
    /// loser (gate already `Probing`/`Committing`) gets `false` so the handler returns
    /// a RETRYABLE "in progress" non-success WITHOUT sending a second request. Only the
    /// Unix restart handler (+ unit tests) call this → unused on the Windows bin build.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn try_begin_probe(&self) -> bool {
        // Genuine CAS: only the caller that atomically flips SERVING→PROBING wins.
        // No read-then-write TOCTOU (decision d-20260712034222169749-5).
        self.0
            .compare_exchange(SERVING, PROBING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Advance `Probing → Committing` (only the claim winner, after a passing
    /// probe). `compare_exchange` so a stray non-owner call can't corrupt state.
    pub fn to_committing(&self) -> bool {
        self.0
            .compare_exchange(PROBING, COMMITTING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Abort `Probing → Serving` (probe failed/timed out; fleet intact). Never
    /// rolls back `Committing`.
    pub fn abort_to_serving(&self) -> bool {
        self.0
            .compare_exchange(PROBING, SERVING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Test-only state accessors — `#[cfg(test)]` because production has no
    /// caller (the handler + TUI loop drive the gate via `try_begin_probe` /
    /// `to_committing` / `abort_to_serving`; these read-only peeks exist purely
    /// to assert gate state in unit tests). Gating them keeps the strict
    /// `-D warnings` bin build free of a dead_code warning without a broad
    /// `#[allow]` and without touching the CAS methods.
    #[cfg(test)]
    pub fn is_committing(&self) -> bool {
        self.0.load(Ordering::Acquire) == COMMITTING
    }
    #[cfg(test)]
    pub fn is_serving(&self) -> bool {
        self.0.load(Ordering::Acquire) == SERVING
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    /// #2453 R2 (locked, decision d-…034222169749-5): under two concurrent API
    /// workers racing to claim the gate, EXACTLY ONE wins `Serving → Probing`.
    /// Deterministic across many iterations — a read-then-write TOCTOU gate lets
    /// BOTH through when they interleave, which this test catches.
    #[test]
    fn two_workers_exactly_one_wins_the_probe_claim() {
        for i in 0..2000 {
            let gate = AppRestartGate::new();
            let barrier = Arc::new(Barrier::new(2));
            let g1 = gate.clone();
            let b1 = Arc::clone(&barrier);
            let g2 = gate.clone();
            let b2 = Arc::clone(&barrier);
            let h1 = std::thread::spawn(move || {
                b1.wait();
                g1.try_begin_probe()
            });
            let h2 = std::thread::spawn(move || {
                b2.wait();
                g2.try_begin_probe()
            });
            let w1 = h1.join().expect("worker 1 joined");
            let w2 = h2.join().expect("worker 2 joined");
            assert!(
                w1 ^ w2,
                "iter {i}: exactly one worker must win the claim; got w1={w1} w2={w2}"
            );
            assert!(
                !gate.is_committing(),
                "iter {i}: neither claim advances to committing"
            );
        }
    }

    /// Lifecycle: Serving→Probing→Committing, duplicate blocked, abort reusable.
    #[test]
    fn gate_lifecycle_probe_commit_and_abort() {
        let g = AppRestartGate::new();
        assert!(g.is_serving());
        assert!(g.try_begin_probe(), "first claim wins");
        assert!(!g.try_begin_probe(), "duplicate blocked while Probing");
        assert!(g.to_committing(), "Probing→Committing");
        assert!(g.is_committing());
        assert!(!g.abort_to_serving(), "cannot abort once Committing");

        let g2 = AppRestartGate::new();
        assert!(g2.try_begin_probe());
        assert!(g2.abort_to_serving(), "Probing→Serving on abort");
        assert!(g2.is_serving());
        assert!(g2.try_begin_probe(), "gate reusable after abort");
    }

    /// #2453 R2 barrier: PostFlushSlot is single-shot AND closes after the response.
    /// A second `register` on an occupied slot, or ANY `register` after the response
    /// flushed (closed), is rejected — a late / timed-out / concurrent worker cannot
    /// hijack a later response's flush.
    #[test]
    fn post_flush_slot_is_single_shot_and_closes() {
        let slot = PostFlushSlot::new();
        assert!(slot.register(Box::new(|| {})), "first register wins");
        assert!(
            !slot.register(Box::new(|| {})),
            "a second register on an occupied slot must be rejected"
        );
        slot.run_after_flush(true); // runs the action + CLOSES the slot
        assert!(
            !slot.register(Box::new(|| {})),
            "register after the response flushed (slot closed) must be rejected"
        );
    }

    /// #2453 R2 barrier: a FAILED write/flush must drop the registered action UN-RUN
    /// so its captured sender drops and the receiver DISCONNECTS — that disconnect is
    /// how the TUI learns to abort (roll the gate back to Serving). A successful flush
    /// runs the action (delivers the commit-permission).
    #[test]
    fn post_flush_failure_drops_action_success_runs_it() {
        // Failure path: the action is dropped un-run → the receiver disconnects.
        let slot = PostFlushSlot::new();
        let (tx, rx) = crossbeam_channel::bounded::<()>(1);
        assert!(slot.register(Box::new(move || {
            let _ = tx.send(());
        })));
        slot.run_after_flush(false);
        assert_eq!(
            rx.try_recv(),
            Err(crossbeam_channel::TryRecvError::Disconnected),
            "a failed flush must drop the ack un-run → disconnect (the TUI aborts)"
        );

        // Success path: the action runs → the commit-permission is delivered.
        let slot = PostFlushSlot::new();
        let (tx, rx) = crossbeam_channel::bounded::<()>(1);
        assert!(slot.register(Box::new(move || {
            let _ = tx.send(());
        })));
        slot.run_after_flush(true);
        assert_eq!(
            rx.try_recv(),
            Ok(()),
            "a successful flush must run the action → deliver commit-permission"
        );
    }
}
