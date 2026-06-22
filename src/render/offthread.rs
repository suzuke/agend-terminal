//! Option X (off-thread parse) — per-pane parser thread + lock-free snapshot.
//!
//! Confirm-first data (cpu_us probe #2403, 11:27 restart) showed the freeze is
//! ~68% CPU-bound `vterm.process` on the MAIN render thread. This module moves
//! that parse OFF the main thread: a per-pane parser thread owns its own
//! [`VTerm`], consumes the pane's PTY-output channel, and publishes an immutable
//! [`GridSnapshot`] via [`ArcSwap`]. The render loop then just `load()`s the
//! latest snapshot (lock-free) and paints it — zero parse on the main thread.
//!
//! **Flag-gated, default OFF** (`AGEND_OFFTHREAD_PARSE`): when off, nothing here
//! runs and the existing main-thread drain path is byte-identical. P1 = shadow /
//! measurable; the default is NOT flipped (that is P3).
//!
//! Correctness (spike ④): the parser thread shares NO lock with `AgentCore` — it
//! owns a per-pane `VTerm` + a per-pane channel + a per-pane `ArcSwap`. It does
//! not reintroduce `core.lock` contention. The snapshot is immutable, so render
//! loads a whole `Arc` and never observes a torn grid.

use crate::vterm::{GridSnapshot, VTerm};
use arc_swap::ArcSwap;
use crossbeam_channel::{select, Receiver, Sender};
use std::cell::Cell;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Max snapshot-publish rate: coalesce all parser events within this window into
/// ONE snapshot + one render wakeup. Bounds the per-pane snapshot-copy cost under
/// an output flood (a burst becomes a single ~rows×cols `Cell` copy) while
/// staying well under one render frame (~33ms @ 30fps, #2346) so display latency
/// is sub-frame. P1 tunable; instrument (`#offthread-snapshot`) measures the real
/// cost so the rate can be revisited.
const SNAPSHOT_COALESCE_MS: u64 = 16;

/// `AGEND_OFFTHREAD_PARSE`: enable the off-thread parser path. Read once + cached
/// (same pattern as `AGEND_FREEZE_INSTRUMENT` gates). Default OFF → zero behavior,
/// no parser thread spawned, main-thread drain path unchanged.
pub fn offthread_parse_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("AGEND_OFFTHREAD_PARSE").is_ok_and(|v| !v.is_empty() && v != "0")
    })
}

/// Reuse the existing freeze instrument gate for `#offthread-snapshot` telemetry.
fn instrument_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("AGEND_FREEZE_INSTRUMENT").is_ok_and(|v| !v.is_empty() && v != "0")
    })
}

/// Main-thread handle to a pane's off-thread parser: read the latest snapshot
/// (lock-free) and route resizes to the thread that owns the `VTerm`.
///
/// `!Sync` (holds a `Cell` for render-thread-only resize dedup) — fine, a `Pane`
/// lives only on the main render thread.
pub struct OffthreadHandle {
    snapshot: Arc<ArcSwap<GridSnapshot>>,
    resize_tx: Sender<(u16, u16)>,
    /// Last dims sent to the parser thread — render-thread-only, so a steady-state
    /// frame doesn't spam identical resizes (the parser/alacritty would no-op them
    /// anyway, but this avoids the channel traffic).
    last_sent_dims: Cell<(u16, u16)>,
    /// Cancellation signal to the parser thread. Sending (or dropping this on
    /// `OffthreadHandle::drop`) wakes the parser out of its `select!` so it exits
    /// even while its data channel is still open — see [`Drop`] below (#2404 r6 ①).
    cancel_tx: Sender<()>,
    /// The parser thread, joined on drop for a leak-free, observable reap.
    join: Option<JoinHandle<()>>,
}

impl OffthreadHandle {
    /// Load the latest published snapshot (lock-free `ArcSwap::load_full`).
    pub fn load(&self) -> Arc<GridSnapshot> {
        self.snapshot.load_full()
    }

    /// Route a resize to the parser thread (which owns the `VTerm`). Deduped:
    /// returns `false` (no send) when dims are unchanged since the last send.
    /// A closed channel (parser thread gone) is treated as a no-op.
    pub fn request_resize(&self, cols: u16, rows: u16) -> bool {
        if self.last_sent_dims.get() == (cols, rows) {
            return false;
        }
        self.last_sent_dims.set((cols, rows));
        self.resize_tx.send((cols, rows)).is_ok()
    }
}

impl Drop for OffthreadHandle {
    /// Reap the parser thread when the pane is dropped (#2404 r6 ① — fixes the
    /// thread leak). RAII here covers EVERY pane-drop path (tab/pane/split close,
    /// app shutdown, re-attach replace) — strictly more robust than signalling at
    /// individual teardown sites.
    ///
    /// Why a signal is required: the parser holds a clone of the pane's `rx`, so
    /// when a pane closes while its agent is still alive the data channel stays
    /// open and the parser would never exit on its own. The explicit
    /// `cancel_tx.send` wakes it out of `select!`; it returns WITHOUT publishing (no
    /// wakeup send), so the join is bounded (the wakeup channel is unbounded → never
    /// blocks, and `drain_pending_data` is `len()`-bounded + cancel-checked, so even
    /// a sustained flood can't wedge it — see `parser_loop`).
    ///
    /// Forwarder interaction: once the parser exits it drops its `rx` clone, so the
    /// pane's forwarder reverts to its PRE-EXISTING managed lifecycle — it exits on
    /// its next `fwd_tx.send` after all receivers (pane.rx + this clone) are gone.
    /// Off-thread parse therefore does NOT extend the forwarder's lifetime. The one
    /// residual is pre-existing and NOT off-thread-introduced: a forwarder for a
    /// closed pane whose agent then goes silent lingers (blocked on the upstream
    /// channel) until that agent speaks again or dies — bounded, flag-independent,
    /// tracked as follow-up t-20260622053855100612-41860-5.
    fn drop(&mut self) {
        let _ = self.cancel_tx.send(());
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Spawn a per-pane parser thread that owns `vterm`, consumes `data_rx` (a clone
/// of the pane's PTY-output channel — the main-thread drain no-ops in off-thread
/// mode so this is the sole consumer), and publishes [`GridSnapshot`]s via
/// `ArcSwap`. Returns the main-thread [`OffthreadHandle`], or `None` if the OS
/// thread could not be created — the caller then falls back to the main-thread
/// drain path instead of stranding the pane with no parser (#2404 r6 ③).
///
/// The thread is reaped on [`OffthreadHandle::drop`] (NOT fire-and-forget): the
/// stored `JoinHandle` is joined there after a cancel signal, so a pane close can
/// never leak it (#2404 r6 ①). The initial snapshot is a blank grid so render
/// always has something to paint.
pub fn spawn_offthread_parser(
    pane_id: usize,
    name: String,
    data_rx: Receiver<Vec<u8>>,
    vterm: VTerm,
    wakeup_tx: Sender<usize>,
) -> Option<OffthreadHandle> {
    let cols = vterm.cols();
    let rows = vterm.rows();
    let snapshot = Arc::new(ArcSwap::from_pointee(GridSnapshot::blank(cols, rows)));
    let (resize_tx, resize_rx) = crossbeam_channel::unbounded::<(u16, u16)>();
    let (cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();
    let publisher = Arc::clone(&snapshot);

    // NOT fire-and-forget: the JoinHandle is stored on the returned
    // `OffthreadHandle` and joined in its `Drop` (after a cancel signal) for a
    // bounded, leak-free reap on pane close (#2404 r6 ①). On spawn failure return
    // `None` so the caller keeps the byte-identical main-thread path (#2404 r6 ③).
    let join = std::thread::Builder::new()
        .name(format!("{name}_parse"))
        .spawn(move || {
            parser_loop(
                pane_id, &name, data_rx, resize_rx, cancel_rx, vterm, publisher, wakeup_tx,
            );
        })
        .ok()?;

    Some(OffthreadHandle {
        snapshot,
        resize_tx,
        last_sent_dims: Cell::new((cols, rows)),
        cancel_tx,
        join: Some(join),
    })
}

enum Event {
    Data(Result<Vec<u8>, crossbeam_channel::RecvError>),
    Resize(Result<(u16, u16), crossbeam_channel::RecvError>),
}

/// Flush the data backlog present RIGHT NOW into `vterm` at the CURRENT dims, then
/// return. Called before applying a resize so a queued resize never reorders ahead
/// of already-enqueued bytes (#2404 r6 ②): the main-thread path parses queued bytes
/// at the old size and only then resizes, and `select!` alone is non-deterministic.
///
/// BOUNDED to the `len()` snapshot (NOT drain-until-empty): a continuous producer
/// (forwarder refilling faster than we parse) would make a drain-until-empty loop
/// never return, hanging the handle's `Drop`-join on the render thread (#2404 r6
/// re-review ①). Only bytes already queued when the resize was dequeued must
/// precede it; anything added afterward is genuinely concurrent. Also checks
/// `cancel_rx` each iteration and returns `true` if a cancel arrived, so a pane
/// close during a large backlog still returns promptly (caller then exits).
///
/// Residual window (P1, documented, pre-existing): the forwarder feeds this channel
/// asynchronously, so old-dims bytes it hasn't transferred yet are not flushed, and
/// there is a symmetric ε-skew (the render thread issues `resize_pty`/SIGWINCH while
/// the parser applies `vterm.resize`). The MAIN-THREAD path has the same
/// async-forwarder window (its next frame parses late bytes at whatever dims
/// `pane.vterm` then holds), so this is not a regression, and an in-band single
/// channel would not close it either (the late bytes still arrive after the
/// resize). Most TUIs repaint on SIGWINCH, which typically overwrites any briefly
/// mis-wrapped cells. (#2404 r6 ② deeper edge.)
fn drain_pending_data(
    data_rx: &Receiver<Vec<u8>>,
    cancel_rx: &Receiver<()>,
    vterm: &mut VTerm,
) -> bool {
    for _ in 0..data_rx.len() {
        if cancel_rx.try_recv().is_ok() {
            return true;
        }
        match data_rx.try_recv() {
            Ok(d) => vterm.process(&d),
            Err(_) => break,
        }
    }
    false
}

/// The parser thread body: block for an event, coalesce a burst within
/// [`SNAPSHOT_COALESCE_MS`], apply all events to the owned `VTerm`, then publish
/// ONE snapshot + one render wakeup. Exits when the data channel disconnects OR a
/// cancel arrives (pane closed — see [`OffthreadHandle`]'s `Drop`); the cancel
/// path returns WITHOUT publishing so the handle's join is never blocked.
#[allow(clippy::too_many_arguments)] // thread entry point: per-pane channels + publish context
fn parser_loop(
    pane_id: usize,
    name: &str,
    data_rx: Receiver<Vec<u8>>,
    resize_rx: Receiver<(u16, u16)>,
    cancel_rx: Receiver<()>,
    mut vterm: VTerm,
    publisher: Arc<ArcSwap<GridSnapshot>>,
    wakeup_tx: Sender<usize>,
) {
    let coalesce = Duration::from_millis(SNAPSHOT_COALESCE_MS);

    loop {
        // Cancel has deterministic priority over a continuously-ready data arm
        // (crossbeam `select!` is unbiased, so a flood could otherwise starve it):
        // check it first each iteration so a pane close is observed in bounded time
        // even under a sustained flood. (#2404 r6 re-review ①)
        if cancel_rx.try_recv().is_ok() {
            return;
        }

        // Phase 1 — block for the first event of a burst. The cancel arm wakes an
        // idle parser immediately; the explicit check above covers the flood case.
        let first = select! {
            recv(cancel_rx) -> _ => return,
            recv(data_rx) -> m => Event::Data(m),
            recv(resize_rx) -> m => Event::Resize(m),
        };
        match first {
            // Data channel closed = agent gone + forwarder exited → done.
            Event::Data(Err(_)) => return,
            // Resize channel closed = handle dropped (cancel is the primary path,
            // this is the backstop) → done.
            Event::Resize(Err(_)) => return,
            Event::Data(Ok(d)) => vterm.process(&d),
            Event::Resize(Ok((c, r))) => {
                if drain_pending_data(&data_rx, &cancel_rx, &mut vterm) {
                    return;
                }
                vterm.resize(c, r);
            }
        }

        // Phase 2 — coalesce within the window. Bounded by the deadline AND
        // cancel-responsive even under a sustained flood: a flood keeps `data_rx`
        // perpetually ready so the `default(timeout)` arm would never fire — break
        // on the deadline explicitly so we always publish + loop back (re-checking
        // cancel). (#2404 r6 re-review ①)
        let deadline = Instant::now() + coalesce;
        loop {
            if cancel_rx.try_recv().is_ok() {
                return;
            }
            let timeout = deadline.saturating_duration_since(Instant::now());
            if timeout.is_zero() {
                break;
            }
            let ev = select! {
                recv(data_rx) -> m => Some(Event::Data(m)),
                recv(resize_rx) -> m => Some(Event::Resize(m)),
                default(timeout) => None,
            };
            match ev {
                None => break, // coalesce window elapsed
                Some(Event::Data(Ok(d))) => vterm.process(&d),
                Some(Event::Data(Err(_))) => {
                    // pane gone mid-burst: publish what we have, then exit.
                    publish(&vterm, &publisher, &wakeup_tx, pane_id, name);
                    return;
                }
                Some(Event::Resize(Ok((c, r)))) => {
                    if drain_pending_data(&data_rx, &cancel_rx, &mut vterm) {
                        return;
                    }
                    vterm.resize(c, r);
                }
                Some(Event::Resize(Err(_))) => return,
            }
        }

        // Phase 3 — one snapshot per coalesced burst.
        publish(&vterm, &publisher, &wakeup_tx, pane_id, name);
    }
}

/// Build + publish an immutable snapshot and wake the render loop once.
fn publish(
    vterm: &VTerm,
    publisher: &Arc<ArcSwap<GridSnapshot>>,
    wakeup_tx: &Sender<usize>,
    pane_id: usize,
    name: &str,
) {
    let probe = instrument_enabled().then(Instant::now);
    let snap = vterm.snapshot();
    let bytes = snap.cells.len() * std::mem::size_of::<alacritty_terminal::term::cell::Cell>();
    publisher.store(Arc::new(snap));
    if let Some(start) = probe {
        tracing::info!(
            tag = "#offthread-snapshot",
            pane_id,
            agent = name,
            build_us = start.elapsed().as_micros() as u64,
            snapshot_bytes = bytes,
            "off-thread snapshot published"
        );
    }
    let _ = wakeup_tx.send(pane_id);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crossbeam_channel::RecvTimeoutError;

    /// End-to-end: spawn the parser thread, push PTY bytes through `data_rx`,
    /// wait for the publish wakeup, and assert the published snapshot reflects the
    /// parsed content — proving parse + snapshot happen OFF this (test) thread.
    #[test]
    fn parser_thread_processes_offthread_and_publishes_snapshot() {
        let (data_tx, data_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let (wake_tx, wake_rx) = crossbeam_channel::unbounded::<usize>();
        let vt = VTerm::new(20, 4);
        let h = spawn_offthread_parser(7, "test".to_string(), data_rx, vt, wake_tx)
            .expect("parser thread spawns");

        // Initial snapshot is blank.
        assert_eq!(h.load().cursor, (0, 0));

        data_tx.send(b"\x1b[2J\x1b[Hhello".to_vec()).unwrap();
        // Wait for the parser thread to publish (no arbitrary sleep).
        assert_eq!(
            wake_rx.recv_timeout(Duration::from_secs(2)),
            Ok(7),
            "parser thread must wake the render loop after publishing"
        );

        let snap = h.load();
        let row0: String = (0..snap.cols)
            .map(|c| snap.cells[c as usize].c)
            .collect::<String>();
        assert!(
            row0.starts_with("hello"),
            "published snapshot must reflect off-thread-parsed content; got {row0:?}"
        );
    }

    /// A resize routed through the handle reaches the parser thread (which owns the
    /// VTerm) and the next snapshot carries the new dims.
    #[test]
    fn resize_routes_to_parser_thread_and_updates_snapshot_dims() {
        let (data_tx, data_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let (wake_tx, wake_rx) = crossbeam_channel::unbounded::<usize>();
        let vt = VTerm::new(20, 4);
        let h = spawn_offthread_parser(1, "t".to_string(), data_rx, vt, wake_tx)
            .expect("parser thread spawns");

        assert!(h.request_resize(30, 6), "first resize must send");
        assert!(!h.request_resize(30, 6), "duplicate resize must be deduped");
        // drive a publish via the resize, then via data to be sure
        data_tx.send(b"x".to_vec()).unwrap();
        // drain wakeups until the snapshot shows the new width (bounded).
        let mut got = false;
        for _ in 0..5 {
            if wake_rx.recv_timeout(Duration::from_secs(2)).is_err() {
                break;
            }
            if h.load().cols == 30 {
                got = true;
                break;
            }
        }
        assert!(
            got,
            "snapshot must reflect resized cols routed to parser thread"
        );
    }

    /// Agent-death path: with the handle still alive, closing the data channel
    /// (the agent's broadcast ended + forwarder exited) makes the parser exit. The
    /// parser's `wakeup_tx` clone drops on exit, so the wakeup channel disconnects.
    #[test]
    fn parser_thread_exits_when_data_channel_closes() {
        let (data_tx, data_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let (wake_tx, wake_rx) = crossbeam_channel::unbounded::<usize>();
        let vt = VTerm::new(10, 3);
        // Keep `h` alive (resize/cancel channels open) so this exercises the
        // data-close exit path, NOT the handle-drop cancel path.
        let _h = spawn_offthread_parser(2, "t".to_string(), data_rx, vt, wake_tx)
            .expect("parser thread spawns");
        drop(data_tx); // close data channel → parser exits → its wake_tx drops
        let mut disconnected = false;
        for _ in 0..50 {
            match wake_rx.recv_timeout(Duration::from_millis(100)) {
                Err(RecvTimeoutError::Disconnected) => {
                    disconnected = true;
                    break;
                }
                _ => continue,
            }
        }
        assert!(
            disconnected,
            "parser thread must exit when its data channel closes"
        );
    }

    /// #2404 r6 ① regression — the thread-leak fix. When the pane is closed while
    /// the agent is STILL ALIVE, the data channel stays open (the forwarder keeps
    /// `fwd_tx`, the parser keeps the `rx` clone), so the parser would never exit
    /// on its own. Dropping the `OffthreadHandle` must reap it anyway via the
    /// cancel signal + join. We hold `data_tx` (= agent alive) for the whole test,
    /// drop the handle, then assert the parser exited (its `wakeup_tx` dropped →
    /// wakeup channel disconnected). The old code (no cancel) would hang here.
    #[test]
    fn parser_exits_on_handle_drop_even_with_data_channel_open() {
        let (data_tx, data_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let (wake_tx, wake_rx) = crossbeam_channel::unbounded::<usize>();
        let vt = VTerm::new(10, 3);
        let h = spawn_offthread_parser(3, "t".to_string(), data_rx, vt, wake_tx)
            .expect("parser thread spawns");
        // Agent alive: data channel stays open for the whole test.
        drop(h); // Drop signals cancel + joins → parser is reaped before this returns.
        assert!(
            wake_rx.recv().is_err(),
            "handle drop must reap the parser even with the data channel open; \
             its wakeup_tx clone should have dropped"
        );
        drop(data_tx); // keep `data_tx` alive until here so the test reflects an alive agent
    }

    /// #2404 r6 ① — close + re-attach must NOT accumulate ghost parser threads.
    /// The agent stays alive for the whole test (one data channel held open), and
    /// each attach/close cycle drops its handle → the parser is reaped (cancel +
    /// join in `Drop`) before the next cycle. Per-cycle reap is proven by each
    /// parser's own wakeup channel disconnecting once its thread exits.
    #[test]
    fn close_then_reattach_does_not_accumulate_ghost_parsers() {
        // Agent alive for the whole test: a single data channel, never closed.
        let (data_tx, data_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        for cycle in 0..3 {
            // A fresh wakeup channel per attach so we observe THIS parser's reap.
            let (wake_tx, wake_rx) = crossbeam_channel::unbounded::<usize>();
            let h = spawn_offthread_parser(
                cycle,
                "t".to_string(),
                data_rx.clone(),
                VTerm::new(10, 3),
                wake_tx,
            )
            .expect("parser thread spawns");
            // "Close the pane": drop the handle while the agent (data_tx) is alive.
            // The join in Drop reaps the parser before this returns, so its
            // wakeup_tx clone is gone and this cycle's wake channel disconnects.
            drop(h);
            assert!(
                wake_rx.recv().is_err(),
                "cycle {cycle}: parser must be reaped on close even with the agent alive"
            );
        }
        drop(data_tx);
    }

    /// #2404 r6 re-review ① — the CRITICAL anti-freeze guarantee: dropping the
    /// handle must reap the parser within a bounded time even under a CONTINUOUS
    /// data flood (a forwarder refilling faster than the parser parses). With the
    /// old drain-until-empty + unbiased select, a resize during the flood spun
    /// `drain_pending_data` forever and the render-thread Drop-join would freeze.
    /// The fix (len-snapshot-bounded drain + cancel-first) keeps the join bounded.
    #[test]
    fn handle_drop_reaps_parser_within_deadline_under_continuous_flood() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let (data_tx, data_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let (wake_tx, _wake_rx) = crossbeam_channel::unbounded::<usize>();
        let h =
            spawn_offthread_parser(9, "flood".to_string(), data_rx, VTerm::new(40, 10), wake_tx)
                .expect("parser thread spawns");

        // Continuous producer: floods data as fast as possible (agent alive + busy).
        let stop = Arc::new(AtomicBool::new(false));
        let producer = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if data_tx.send(vec![b'x'; 512]).is_err() {
                        break; // parser/pane gone
                    }
                }
            })
        };
        // Drive a resize into the flood so the parser enters `drain_pending_data`
        // while data is continuously available — the exact spot the old code spun.
        assert!(h.request_resize(20, 8), "resize sends");

        // Drop the handle off-thread; assert the Drop-join completes within a strict
        // deadline. A regression that makes the join unbounded leaves `done` false →
        // the assert fails in bounded time (no infinite test hang).
        let done = Arc::new(AtomicBool::new(false));
        let dropper = {
            let done = Arc::clone(&done);
            std::thread::spawn(move || {
                drop(h); // Drop sends cancel + joins the parser
                done.store(true, Ordering::Relaxed);
            })
        };
        let mut reaped = false;
        for _ in 0..200 {
            if done.load(Ordering::Relaxed) {
                reaped = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        stop.store(true, Ordering::Relaxed);
        let _ = producer.join();
        let _ = dropper.join();
        assert!(
            reaped,
            "handle drop must reap the parser within the deadline even under a continuous flood"
        );
    }

    /// #2404 r6 ② / re-review ① — `drain_pending_data` flushes the queued backlog at
    /// the current dims in order AND is bounded to the `len()` snapshot: it returns
    /// even though the sender stays open, so a continuous producer can't wedge it.
    #[test]
    fn drain_pending_data_flushes_snapshot_and_is_bounded() {
        let (data_tx, data_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let (_cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();
        let mut vt = VTerm::new(20, 4);
        data_tx.send(b"\x1b[2J\x1b[Hab".to_vec()).unwrap();
        data_tx.send(b"cd".to_vec()).unwrap();
        // Sender stays open (agent alive) — drain must still return (bounded by len).
        let cancelled = drain_pending_data(&data_rx, &cancel_rx, &mut vt);
        assert!(!cancelled, "no cancel was sent");
        assert!(
            data_rx.is_empty(),
            "the queued snapshot must be fully drained"
        );
        let snap = vt.snapshot();
        let row0: String = (0..snap.cols).map(|c| snap.cells[c as usize].c).collect();
        assert!(
            row0.starts_with("abcd"),
            "queued bytes parsed in order at current dims; got {row0:?}"
        );
    }

    /// #2404 re-review ① — `drain_pending_data` stops early and returns `true` when a
    /// cancel arrives, so a pane close during a large backlog still returns promptly.
    #[test]
    fn drain_pending_data_returns_early_on_cancel() {
        let (data_tx, data_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let (cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();
        let mut vt = VTerm::new(20, 4);
        for _ in 0..100 {
            data_tx.send(vec![b'x']).unwrap();
        }
        cancel_tx.send(()).unwrap(); // cancel pending before the drain runs
        let cancelled = drain_pending_data(&data_rx, &cancel_rx, &mut vt);
        assert!(cancelled, "drain must observe the cancel and return true");
        assert!(
            !data_rx.is_empty(),
            "drain must stop early on cancel, leaving the backlog"
        );
    }

    /// #2404 r6 ② regression (DETERMINISTIC) — ordering: queued data must be parsed
    /// at the OLD dims BEFORE a resize is applied. Exercises `parser_loop`'s exact
    /// resize handling (`drain_pending_data` then `vterm.resize`) SYNCHRONOUSLY, so
    /// it does not depend on thread / `select!` timing — the prior thread-driven
    /// version was racy (r6 round-2: no barrier guaranteeing both arms pending; it
    /// passed locally + on ubuntu but flaked on macos CI). Width-sensitive probe:
    /// 20 `X` then cursor-home + erase-line. Parsed at cols=20 the 20 X's fill
    /// exactly row 0, so erase-line blanks the whole line → empty grid. Had the
    /// resize to cols=5 been applied first (the bug), the 20 X's would wrap to 4
    /// rows and erase-line would clear only row 0, leaving X's in rows 1-3.
    #[test]
    fn queued_data_is_parsed_at_old_dims_before_a_resize() {
        let (data_tx, data_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let (_cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();
        let mut vt = VTerm::new(20, 6);
        // Bytes are queued BEFORE the resize is handled — exactly the state the
        // parser is in when it dequeues a resize while data is still in the channel.
        data_tx
            .send(b"XXXXXXXXXXXXXXXXXXXX\x1b[H\x1b[K".to_vec())
            .unwrap();
        // parser_loop's resize handling, in order: flush queued data at the OLD
        // dims, then resize.
        let cancelled = drain_pending_data(&data_rx, &cancel_rx, &mut vt);
        assert!(!cancelled, "no cancel was sent");
        vt.resize(5, 6);
        // The 20 X's were parsed at cols=20 (one full row, then erase-line cleared
        // it), so the resized snapshot has NO stray X.
        let snap = vt.snapshot();
        assert!(
            !snap.cells.iter().any(|c| c.c == 'X'),
            "queued bytes must be parsed at the OLD width before the resize; \
             a stray 'X' means the resize jumped ahead of queued data"
        );
    }
}
