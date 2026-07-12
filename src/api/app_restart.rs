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
use std::sync::Arc;

const SERVING: u8 = 0;
const PROBING: u8 = 1;
const COMMITTING: u8 = 2;

/// The verdict the TUI loop returns to the (blocking) API worker over the request's
/// bounded oneshot.
#[derive(Debug, Clone)]
pub enum AppRestartVerdict {
    /// Probe passed, gate is `Committing`, exec is imminent (the API socket drops
    /// as the process re-execs). Emitted BEFORE teardown/exec.
    Committing,
    /// Probe failed / errored / timed out. Fleet + TUI intact; no restart.
    Aborted(String),
}

/// A restart request handed from the handler to the TUI loop, carrying a bounded
/// oneshot for the verdict.
#[derive(Debug)]
pub struct AppRestartRequest {
    pub reply: crossbeam_channel::Sender<AppRestartVerdict>,
}

/// Injected sender (handler → TUI loop); bounded (capacity 1) at the composition root.
pub type AppRestartSender = crossbeam_channel::Sender<AppRestartRequest>;

/// The injected app-restart capability: the request sender + the shared gate.
/// Created at the app API composition root ([`crate::app`]), threaded through
/// `serve` → API `HandlerCtx` → MCP `RuntimeContext` to the `restart_daemon`
/// handler. `Clone` is cheap (a channel `Sender` + an `Arc`). Absent (`None`) on
/// the daemon / verify roots, which fail-closed.
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
    /// loser (gate already `Probing`/`Committing`) gets `false` so the handler
    /// fails closed "already in progress" WITHOUT sending a second request.
    pub fn try_begin_probe(&self) -> bool {
        // #2453 R2 RED: intentionally-buggy read-then-write (TOCTOU) so the
        // deterministic two-worker test fails first (w1=true, w2=true). The very
        // next commit fixes this to a genuine CAS (compare_exchange). `yield_now`
        // widens the race window so the failure is reliable.
        if self.0.load(Ordering::Acquire) == SERVING {
            std::thread::yield_now();
            self.0.store(PROBING, Ordering::Release);
            true
        } else {
            false
        }
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

    pub fn is_committing(&self) -> bool {
        self.0.load(Ordering::Acquire) == COMMITTING
    }
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
            let w1 = h1.join().unwrap();
            let w2 = h2.join().unwrap();
            assert!(
                w1 ^ w2,
                "iter {i}: exactly one worker must win the claim; got w1={w1} w2={w2}"
            );
            assert!(!gate.is_committing(), "iter {i}: neither claim advances to committing");
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
}
