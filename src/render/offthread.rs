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
use crossbeam_channel::{select, Receiver, RecvTimeoutError, Sender};
use std::cell::Cell;
use std::sync::Arc;
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

/// Spawn a per-pane parser thread that owns `vterm`, consumes `data_rx` (a clone
/// of the pane's PTY-output channel — the main-thread drain no-ops in off-thread
/// mode so this is the sole consumer), and publishes [`GridSnapshot`]s via
/// `ArcSwap`. Returns the main-thread [`OffthreadHandle`].
///
/// The thread is fire-and-forget: it exits when `data_rx` disconnects (the pane
/// was dropped → forwarder's `fwd_tx` send fails → channel closes). The initial
/// snapshot is a blank grid so render always has something to paint.
pub fn spawn_offthread_parser(
    pane_id: usize,
    name: String,
    data_rx: Receiver<Vec<u8>>,
    vterm: VTerm,
    wakeup_tx: Sender<usize>,
) -> OffthreadHandle {
    let cols = vterm.cols();
    let rows = vterm.rows();
    let snapshot = Arc::new(ArcSwap::from_pointee(GridSnapshot::blank(cols, rows)));
    let (resize_tx, resize_rx) = crossbeam_channel::unbounded::<(u16, u16)>();
    let publisher = Arc::clone(&snapshot);

    // fire-and-forget: per-pane parser thread; exits when data_rx disconnects
    // (pane dropped). No JoinHandle needed — pane teardown closes the channel.
    let _ = std::thread::Builder::new()
        .name(format!("{name}_parse"))
        .spawn(move || {
            parser_loop(pane_id, &name, data_rx, resize_rx, vterm, publisher, wakeup_tx);
        });

    OffthreadHandle {
        snapshot,
        resize_tx,
        last_sent_dims: Cell::new((cols, rows)),
    }
}

enum Event {
    Data(Result<Vec<u8>, crossbeam_channel::RecvError>),
    Resize(Result<(u16, u16), crossbeam_channel::RecvError>),
}

/// The parser thread body: block for an event, coalesce a burst within
/// [`SNAPSHOT_COALESCE_MS`], apply all events to the owned `VTerm`, then publish
/// ONE snapshot + one render wakeup. Exits when the data channel disconnects.
fn parser_loop(
    pane_id: usize,
    name: &str,
    data_rx: Receiver<Vec<u8>>,
    resize_rx: Receiver<(u16, u16)>,
    mut vterm: VTerm,
    publisher: Arc<ArcSwap<GridSnapshot>>,
    wakeup_tx: Sender<usize>,
) {
    let coalesce = Duration::from_millis(SNAPSHOT_COALESCE_MS);
    // Once the resize channel disconnects (handle dropped) we stop selecting on it
    // to avoid a busy-loop on the perpetually-ready Err; data_rx then drives exit.
    let mut resize_open = true;

    loop {
        // Phase 1 — block for the first event of a burst.
        let first = if resize_open {
            select! {
                recv(data_rx) -> m => Event::Data(m),
                recv(resize_rx) -> m => Event::Resize(m),
            }
        } else {
            Event::Data(data_rx.recv())
        };
        match first {
            Event::Data(Err(_)) => break, // pane gone → exit thread
            Event::Resize(Err(_)) => {
                resize_open = false;
                continue;
            }
            Event::Data(Ok(d)) => vterm.process(&d),
            Event::Resize(Ok((c, r))) => vterm.resize(c, r),
        }

        // Phase 2 — coalesce everything that arrives within the window.
        let deadline = Instant::now() + coalesce;
        loop {
            let timeout = deadline.saturating_duration_since(Instant::now());
            let ev = if resize_open {
                select! {
                    recv(data_rx) -> m => Some(Event::Data(m)),
                    recv(resize_rx) -> m => Some(Event::Resize(m)),
                    default(timeout) => None,
                }
            } else {
                match data_rx.recv_timeout(timeout) {
                    Ok(d) => Some(Event::Data(Ok(d))),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => {
                        Some(Event::Data(Err(crossbeam_channel::RecvError)))
                    }
                }
            };
            match ev {
                None => break, // coalesce window elapsed
                Some(Event::Data(Ok(d))) => vterm.process(&d),
                Some(Event::Data(Err(_))) => {
                    // pane gone mid-burst: publish what we have, then exit.
                    publish(&vterm, &publisher, &wakeup_tx, pane_id, name);
                    return;
                }
                Some(Event::Resize(Ok((c, r)))) => vterm.resize(c, r),
                Some(Event::Resize(Err(_))) => resize_open = false,
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
mod tests {
    use super::*;

    /// End-to-end: spawn the parser thread, push PTY bytes through `data_rx`,
    /// wait for the publish wakeup, and assert the published snapshot reflects the
    /// parsed content — proving parse + snapshot happen OFF this (test) thread.
    #[test]
    fn parser_thread_processes_offthread_and_publishes_snapshot() {
        let (data_tx, data_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let (wake_tx, wake_rx) = crossbeam_channel::unbounded::<usize>();
        let vt = VTerm::new(20, 4);
        let h = spawn_offthread_parser(7, "test".to_string(), data_rx, vt, wake_tx);

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
        let h = spawn_offthread_parser(1, "t".to_string(), data_rx, vt, wake_tx);

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
        assert!(got, "snapshot must reflect resized cols routed to parser thread");
    }

    /// Dropping the handle + closing the data channel makes the parser thread exit
    /// (no leak). We can't join a fire-and-forget thread directly; instead assert
    /// the wakeup channel disconnects once the thread returns and its `wakeup_tx`
    /// clone drops.
    #[test]
    fn parser_thread_exits_when_data_channel_closes() {
        let (data_tx, data_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let (wake_tx, wake_rx) = crossbeam_channel::unbounded::<usize>();
        let vt = VTerm::new(10, 3);
        let h = spawn_offthread_parser(2, "t".to_string(), data_rx, vt, wake_tx);
        drop(h); // drop resize_tx
        drop(data_tx); // close data channel → thread exits → its wake_tx drops
        // Once the thread returns, the only remaining wake_tx (moved into it) drops,
        // so recv returns Disconnected within a bounded time.
        let mut disconnected = false;
        for _ in 0..20 {
            match wake_rx.recv_timeout(Duration::from_millis(200)) {
                Err(RecvTimeoutError::Disconnected) => {
                    disconnected = true;
                    break;
                }
                _ => continue,
            }
        }
        assert!(disconnected, "parser thread must exit when data channel closes");
    }
}
