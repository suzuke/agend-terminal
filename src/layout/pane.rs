//! Pane types — single terminal pane with PTY or remote bridge.

use crate::agent::{self, AgentRegistry};
use crate::backend::Backend;
use crate::bridge_client::BridgeClient;
use crate::vterm::VTerm;
use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

/// How a pane's input/resize is delivered to the underlying process.
///
/// `Local` panes route through `AgentRegistry` keyed by `Pane::agent_name` —
/// the pane doesn't own the PTY, the registry does. `Remote` panes own a
/// `BridgeClient` that speaks to a daemon-hosted agent over TCP.
pub enum PaneSource {
    Local,
    Remote(Arc<Mutex<BridgeClient>>),
}

/// A single pane displaying one agent's terminal output.
/// PTY ownership is in AgentRegistry (Local) or BridgeClient (Remote) —
/// pane only holds a subscriber channel and a local VTerm.
pub struct Pane {
    pub agent_name: crate::types::AgentName,
    /// #1441: authoritative registry key, resolved once from fleet.yaml at
    /// pane construction (`create_pane` / `attach_pane`). All `Local`-pane
    /// registry lookups (input/resize inject, status display) route through
    /// this UUID rather than `agent_name`, so live-process identity matches
    /// inbox identity and cannot drift when a name is reused. Carries
    /// `InstanceId::default()` for non-fleet test panes (never routed).
    pub instance_id: crate::types::InstanceId,
    pub vterm: VTerm,
    pub rx: crossbeam_channel::Receiver<Vec<u8>>,
    pub id: usize,
    pub backend: Option<Backend>,
    /// Working directory this pane was spawned in.
    pub working_dir: Option<PathBuf>,
    /// User-defined display name (shown in pane border). agent_name is used if None.
    pub display_name: Option<String>,
    /// Scroll offset (lines from bottom). 0 = live view.
    pub scroll_offset: usize,
    /// True when an unread `[from:...]` message was detected.
    pub has_notification: bool,
    /// Fleet instance name (key in fleet.yaml). None for shell panes.
    pub fleet_instance_name: Option<String>,
    /// Last time the user typed into this pane from the TUI.
    pub last_input_at: Option<Instant>,
    /// Count of pending queued notifications for this pane.
    pub pending_notification_count: usize,
    /// Active text selection, in absolute scrollback logical coordinates.
    pub selection: Option<Selection>,
    /// Whether input/resize go to a local PTY (via registry) or a remote
    /// daemon-hosted agent (via `BridgeClient`).
    pub source: PaneSource,
    /// Option X (off-thread parse, `AGEND_OFFTHREAD_PARSE`): when `Some`, a
    /// per-pane parser thread owns a SEPARATE `VTerm` and publishes immutable
    /// grid snapshots via `ArcSwap`. The main thread then renders from the latest
    /// snapshot and does NOT drain `rx` / parse here (`drain_output` no-ops and
    /// `core_render` paints `snapshot` instead). `None` (default, flag OFF) = the
    /// byte-identical main-thread drain+parse path.
    pub offthread: Option<crate::render::offthread::OffthreadHandle>,
    /// #forwarder-reap: held ONLY for its `Drop`. The output-forwarder thread
    /// `select!`s on the agent `rx` AND the receiver paired with this sender, so
    /// dropping the pane (close) drops this sender → that receiver disconnects →
    /// a forwarder blocked on a QUIET agent's `rx.recv()` wakes and exits in
    /// bounded time, instead of lingering until the agent next speaks or dies.
    /// `None` for panes with no crossbeam forwarder: the placeholder before
    /// attach, remote `BridgeClient` panes (socket reader, own lifecycle), and
    /// test panes. Set to `Some` by `apply_attachment` / `attach_pane`.
    pub _fwd_cancel: Option<crossbeam_channel::Sender<()>>,
}

/// Text selection anchored to absolute scrollback line ids so it stays pinned to
/// its content under both new output and user scrolling.
///
/// `.0` is an ABSOLUTE line id (`grid_line + `[`selection_base`](Pane::selection_base));
/// `.1` is the column. Endpoints are produced by [`Pane::viewport_to_logical_line`]
/// and resolved at render / extract time via [`Pane::logical_line_to_viewport`] /
/// [`Pane::extract_selection_text`].
///
/// Edge cases (#offthread-selection):
/// - Live (flag-OFF): the id derives from the live `vterm.max_scroll()`, capped at
///   alacritty's 10000-row scrollback — a line evicted past that cap loses its anchor
///   and drifts. Unreachable within a single gesture (~10000 lines in the seconds a
///   drag takes), so left unhandled.
/// - Off-thread: the id is monotonic via the published snapshot's `history_origin`
///   (parser-stamped), so it tracks content across the snapshot window's
///   evict+append — the >1000-row drift r6 caught. The snapshot window is only
///   `SNAPSHOT_SCROLLBACK_ROWS` (1000) deep; an endpoint scrolled OUT of that window
///   resolves to blanks (clamped), NOT wrong content. The same 10000-cap saturation
///   limit applies (parity-with-live; alacritty exposes no monotonic counter, #2411).
#[derive(Clone)]
pub struct Selection {
    /// Start: (logical line, column). May be before or after `end`.
    pub start: (i64, u16),
    /// End: (logical line, column).
    pub end: (i64, u16),
}

/// #freeze-2 probe (env-gated, `AGEND_FREEZE_INSTRUMENT`): per-frame `drain_output`
/// cost threshold in microseconds, or `None` when disabled (zero overhead). Read
/// once and cached. Lets an operator restart-repro confirm the freeze is in
/// `drain_output` and that the budget bounds it. Off by default → no behavior
/// change. `"1"` => the 2 ms default; `"<N>"` (N>1) => custom µs; else disabled.
fn drain_probe_threshold_us() -> Option<u64> {
    static PROBE: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *PROBE.get_or_init(|| match std::env::var("AGEND_FREEZE_INSTRUMENT") {
        Ok(v) if v == "1" => Some(2_000),
        Ok(v) => v.parse::<u64>().ok().filter(|n| *n > 1),
        Err(_) => None,
    })
}

/// #freeze-cputime probe: this thread's consumed CPU time in microseconds, or
/// `None` when unavailable. Pairs with the `#freeze-drain` wall-clock
/// (`drain_us`) so an operator restart-repro can tell a CPU-bound parse
/// (`drain_us ≈ cpu_us`) apart from a preempted / descheduled render thread
/// (`drain_us ≫ cpu_us`) — the former argues for off-thread parse, the latter
/// for a cheaper stagger-reattach. Unix uses `clock_gettime(CLOCK_THREAD_CPUTIME_ID)`
/// (macOS 10.12+ / Linux — the operator is on macOS); Windows is a best-effort
/// skip (`None`). Called ONLY when the probe is enabled, so the flag-off path
/// issues zero extra syscalls.
#[cfg(unix)]
fn thread_cpu_time_us() -> Option<u64> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `clock_gettime` writes a fully-initialized `timespec` through the
    // valid stack pointer; `CLOCK_THREAD_CPUTIME_ID` is a POSIX per-thread clock
    // id present on macOS and Linux. No aliasing or lifetime concerns.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    if rc != 0 {
        return None;
    }
    Some((ts.tv_sec as u64).wrapping_mul(1_000_000) + (ts.tv_nsec as u64) / 1_000)
}

#[cfg(not(unix))]
fn thread_cpu_time_us() -> Option<u64> {
    // Windows: best-effort skip — the `#freeze-drain` probe is exercised on the
    // operator's macOS box, so `cpu_us` logs as the `-1` "unavailable" sentinel.
    None
}

impl Pane {
    /// Max scroll-back offset for THIS pane's render path. Off-thread mode
    /// (`AGEND_OFFTHREAD_PARSE`) renders the parser thread's published snapshot and
    /// leaves the main-thread `vterm` idle (drain no-ops), so its `max_scroll()` is 0
    /// and clamping scroll to it would pin every off-thread pane to the bottom
    /// (#offthread-scroll regression). Use the snapshot's captured scrollback depth
    /// when off-thread, else the live vterm's full history.
    pub fn scroll_max(&self) -> usize {
        match &self.offthread {
            Some(handle) => handle.scroll_max(),
            None => self.vterm.max_scroll(),
        }
    }

    /// #offthread-mouse: whether the child terminal has enabled mouse reporting, for
    /// the mouse-forward gate. Off-thread the main-thread `vterm` is idle (processes
    /// no bytes → mode default → mouse OFF), so this reads the PARSER-stamped mode on
    /// the published snapshot, NOT `pane.vterm`. Reading `pane.vterm` off-thread is the
    /// dead-gate bug: a fullscreen claude-code (alt-screen + mouse-capture, self-
    /// scrolls) would never receive its forwarded wheel/click. Flag-OFF reads the live
    /// `vterm` (byte-identical to before).
    pub fn wants_mouse(&self) -> bool {
        match &self.offthread {
            Some(handle) => handle.load().wants_mouse,
            None => self.vterm.wants_mouse(),
        }
    }

    /// #offthread-mouse: whether SGR mouse encoding is active, paired with
    /// [`wants_mouse`](Self::wants_mouse) for the forward gate. Same off-thread
    /// snapshot vs flag-OFF `vterm` dispatch.
    pub fn mouse_sgr(&self) -> bool {
        match &self.offthread {
            Some(handle) => handle.load().mouse_sgr,
            None => self.vterm.mouse_sgr(),
        }
    }

    /// #offthread-selection: the ABSOLUTE-line base a selection endpoint is measured
    /// from. Off-thread it is the published snapshot's visible-top absolute id
    /// (`history_origin + history.len()`), read from the snapshot — NOT `pane.vterm`,
    /// which is idle off-thread (`max_scroll()` = 0, the bug root). Flag-OFF it is the
    /// live `vterm.max_scroll()`. Encoding an endpoint as `row - scroll_offset + base`
    /// makes it an absolute, monotonic line id (see `GridSnapshot::history_origin`)
    /// that stays pinned to its content as the snapshot window evicts+appends rows —
    /// fixing the >1000-row drift a relative depth had. Under-cap off-thread
    /// `history_origin == 0` so `base == history.len() == scroll_max()` and flag-OFF
    /// `base == vterm.max_scroll()` → both byte-identical to the pre-anchor form.
    fn selection_base(&self) -> i64 {
        match &self.offthread {
            Some(handle) => {
                let snap = handle.load();
                snap.history_origin as i64 + snap.history.len() as i64
            }
            None => self.vterm.max_scroll() as i64,
        }
    }

    /// Convert a viewport row (0-based within the pane interior) to an ABSOLUTE
    /// scrollback line id at the current scroll position. Inverse of
    /// [`Self::logical_line_to_viewport`].
    ///
    /// Derivation: render maps viewport row → `grid_line = row - scroll_offset`
    /// (see `VTerm::render_to_buffer`); adding [`selection_base`](Self::selection_base)
    /// (the absolute id of the visible top) yields an absolute id stable under append
    /// AND across snapshot-window eviction (#offthread-selection).
    pub fn viewport_to_logical_line(&self, row: u16) -> i64 {
        row as i64 - self.scroll_offset as i64 + self.selection_base()
    }

    /// Convert an absolute scrollback line id back to a viewport row at the current
    /// scroll position. The result may be negative or `>=` viewport height when the
    /// anchored content has scrolled off-screen; callers clip.
    pub fn logical_line_to_viewport(&self, logical: i64) -> i64 {
        logical + self.scroll_offset as i64 - self.selection_base()
    }

    /// Extract the selected text for THIS pane's render path (#offthread-selection).
    /// Off-thread mode (`offthread = Some`) reads the parser's published
    /// `GridSnapshot` — the live `vterm` is idle/blank there, so reading it would
    /// copy nothing. Flag-OFF reads the live `vterm` (byte-identical to before).
    /// Coordinates are absolute scrollback logical coords from `viewport_to_logical_line`.
    pub fn extract_selection_text(&self, start: (i64, u16), end: (i64, u16)) -> String {
        match &self.offthread {
            Some(handle) => handle.load().extract_text(start, end),
            None => self.vterm.extract_text(start, end),
        }
    }

    /// #t-97931 (off-thread draft-gate, F-A): the last `n` visible rows of THIS pane's
    /// render path, for the #1944 draft-protection input-box probe. Off-thread the
    /// main-thread `vterm` is idle (blank) → read the parser's published snapshot, so
    /// the gate sees the REAL input box and never mis-reads an unsent draft as empty
    /// (which would clobber it). Flag-OFF reads the live `vterm` (byte-identical).
    pub fn tail_lines(&self, n: usize) -> String {
        match &self.offthread {
            Some(handle) => handle.load().tail_lines(n),
            None => self.vterm.tail_lines(n),
        }
    }

    /// #t-97931: text + per-char DIM mask of the last `n` visible rows, paired with
    /// [`tail_lines`](Self::tail_lines) for the codex dim-ghost draft probe. Same
    /// off-thread snapshot vs flag-OFF `vterm` dispatch.
    pub fn tail_lines_with_dim(&self, n: usize) -> (String, Vec<bool>) {
        match &self.offthread {
            Some(handle) => handle.load().tail_lines_with_dim(n),
            None => self.vterm.tail_lines_with_dim(n),
        }
    }

    /// Display label: display_name if set, otherwise agent_name.
    pub fn label(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.agent_name)
    }

    pub fn mark_input_activity(&mut self) {
        self.last_input_at = Some(Instant::now());
    }

    #[cfg(test)]
    pub fn is_composing(&self) -> bool {
        self.last_input_at.is_some_and(|instant| {
            instant.elapsed() < crate::notification_queue::COMPOSE_IDLE_TIMEOUT
        })
    }

    /// Drain up to `budget_bytes` of pending PTY output into the local VTerm,
    /// leaving any remainder queued in the (unbounded) channel. Returns the number
    /// of BYTES drained this call. Callers that share one per-frame budget across
    /// many panes (`render::drain_all_panes`) subtract it from the remaining budget;
    /// whether output still remains is `!pane.rx.is_empty()`.
    ///
    /// #freeze-2 (t-…74503): this `vterm.process` loop runs on the MAIN thread
    /// inside `terminal.draw`. Un-bounded, a boot/restart backlog (every agent
    /// re-attaching + dumping its screen) made one draw take ~100 ms+ → the loop
    /// couldn't service input → freeze. (CPU, not a lock — #2380's lock-free
    /// snapshot addressed a different, smaller consumer.) Bounding the per-frame
    /// work keeps `terminal.draw` short so input stays responsive while the pane
    /// visually catches up across a few frames. Lossless + FIFO: chunks are
    /// processed in receive order; unprocessed chunks stay queued.
    pub fn drain_output(&mut self, budget_bytes: usize) -> usize {
        // Option X (off-thread parse): when a parser thread owns this pane's VTerm
        // it is the SOLE consumer of `rx` (a clone), so the main thread must NOT
        // drain here — doing so would steal chunks from the parser (crossbeam is
        // MPMC). Render reads the published snapshot instead. Flag OFF (default) →
        // `offthread` is `None` → no-op and the path below is byte-identical.
        if self.offthread.is_some() {
            return 0;
        }
        let probe_threshold = drain_probe_threshold_us();
        let probe_start = probe_threshold.map(|_| Instant::now());
        // #freeze-cputime: snapshot this thread's CPU time alongside the wall
        // clock, but ONLY when the probe is enabled — `and_then` is the gate, so
        // a `None` `probe_threshold` (flag off) issues no `clock_gettime` here.
        let probe_cpu_start = probe_threshold.and_then(|_| thread_cpu_time_us());
        let mut drained = 0usize;
        // Process whole chunks until the byte budget is met (per-chunk granularity:
        // the last chunk may carry us slightly over — chunks are PTY-read-bounded).
        while drained < budget_bytes {
            match self.rx.try_recv() {
                Ok(data) => {
                    drained += data.len();
                    self.vterm.process(&data);
                    if self.backend.is_some() {
                        let text = String::from_utf8_lossy(&data);
                        if text.contains("[from:") {
                            self.has_notification = true;
                        }
                    }
                }
                Err(_) => break, // channel empty (or disconnected)
            }
        }
        // Don't auto-scroll if user has scrolled back (they're reading history).
        // User scrolls back to bottom manually via mouse or Ctrl+B [ → j.
        if let (Some(threshold), Some(start)) = (probe_threshold, probe_start) {
            let us = start.elapsed().as_micros() as u64;
            if us >= threshold {
                // #freeze-cputime: thread CPU time consumed across this drain
                // cycle (same span as `drain_us`). `drain_us ≈ cpu_us` ⇒ CPU-bound
                // parse (off-thread parse worth it); `drain_us ≫ cpu_us` ⇒ render
                // thread was preempted / descheduled (cheaper stagger-reattach
                // fix). `-1` = unavailable (Windows / clock failure).
                let cpu_us: i64 = match (probe_cpu_start, thread_cpu_time_us()) {
                    (Some(s), Some(e)) => e.saturating_sub(s) as i64,
                    _ => -1,
                };
                tracing::info!(
                    tag = "#freeze-drain",
                    drain_us = us,
                    cpu_us = cpu_us,
                    bytes = drained,
                    more = !self.rx.is_empty(),
                    "drain_output budget cycle"
                );
            }
        }
        drained
    }

    /// Write bytes (keystrokes, paste) to this pane's underlying process.
    /// Dispatches on `source`: Local goes through the registry, Remote goes
    /// through the pane's BridgeClient. Errors are swallowed — a broken pane
    /// surfaces via its output channel closing, which the app handles at the
    /// next drain.
    pub fn write_input(&mut self, registry: &AgentRegistry, bytes: &[u8]) {
        self.mark_input_activity();
        match &self.source {
            PaneSource::Local => {
                // #1530/F1: snapshot the writer under the registry lock, release
                // it, THEN write — never hold the registry across the (up to 5s)
                // blocking PTY write.
                let writer_snap = {
                    let reg = agent::lock_registry(registry);
                    reg.get(&self.instance_id)
                        .map(|h| agent::InjectTarget::from_handle(h).pty_writer)
                };
                if let Some(writer) = writer_snap {
                    let _ = agent::write_to_pty(&writer, bytes);
                }
                // Clear reply_to on TUI keyboard input (Sprint 52).
                crate::daemon::heartbeat_pair::update_with(&self.agent_name, |p| {
                    p.reply_to_channel = None;
                    p.reply_to_input_id = None;
                });
                // #1665 reply-ledger: operator took over in the TUI — can't tell
                // "user abandoned" from "operator handled it out-of-band", so
                // clear the audited turn without warning.
                crate::reply_ledger::clear_turn(&self.agent_name);
            }
            PaneSource::Remote(client) => {
                let mut c = client.lock();
                let _ = c.send_input(bytes);
            }
        }
    }

    /// Resize this pane's underlying PTY / remote agent.
    ///
    /// W2.6 resize contract (see [`crate::render::resize`]): this is the shared
    /// primitive both chokepoints drive. LAYOUT pre-computes `(cols, rows)` from
    /// the split geometry and calls this before the first frame (an estimate);
    /// RENDER then recomputes the actual content rect and calls this again as the
    /// authoritative correction. `(cols, rows)` here is always a
    /// [`crate::render::resize::PaneContentRect`] dimension.
    pub fn resize_pty(&self, registry: &AgentRegistry, cols: u16, rows: u16) {
        match &self.source {
            PaneSource::Local => {
                // #7: some backends' TUIs don't repaint on the SIGWINCH that
                // `master.resize` delivers (kiro-cli 2.1.x), so we follow the
                // resize with an explicit redraw trigger for those backends only.
                let redraw = redraw_seq_after_resize(self.backend.as_ref());
                let reg = agent::lock_registry(registry);
                if let Some(handle) = reg.get(&self.instance_id) {
                    {
                        let master = handle.pty_master.lock();
                        let _ = master.resize(portable_pty::PtySize {
                            rows,
                            cols,
                            pixel_width: 0,
                            pixel_height: 0,
                        });
                    }
                    // #7: send the redraw trigger AFTER the resize, on the input
                    // writer (not the master). Clone the writer and release the
                    // registry lock before the blocking write (#1530/F1: never
                    // hold the registry lock across a PTY write).
                    if let Some(seq) = redraw {
                        let writer = handle.pty_writer.clone();
                        drop(reg);
                        let _ = agent::write_to_pty(&writer, seq);
                    }
                }
            }
            PaneSource::Remote(client) => {
                let mut c = client.lock();
                let _ = c.send_resize(cols, rows);
            }
        }
    }
}

/// #7: the byte sequence to emit after a PTY resize to force a repaint, for
/// backends whose preset opts in via [`crate::backend::BackendPreset::redraw_after_resize`].
///
/// Returns `Some(Ctrl+L)` only for opted-in backends (kiro-cli 2.1.x TUI v2,
/// which ignores the resize SIGWINCH and shows a blank pane until the next
/// keystroke); `None` for every other backend — so they emit NOTHING extra on
/// resize and are provably untouched. `Ctrl+L` (`0x0c`, form-feed) is the
/// conventional TUI/readline "redraw" key, distinct from the submit key (`\r`).
fn redraw_seq_after_resize(backend: Option<&Backend>) -> Option<&'static [u8]> {
    backend
        .filter(|b| b.preset().redraw_after_resize)
        .map(|_| b"\x0c".as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vterm::VTerm;

    // #7: the redraw trigger must fire for Kiro ONLY and emit Ctrl+L; every
    // other backend (and a backend-less pane) must emit NOTHING, so resize
    // behavior for Claude/Codex/OpenCode/Gemini/Agy/Shell/Raw is unchanged.
    #[test]
    fn redraw_seq_after_resize_is_kiro_only_ctrl_l_7() {
        assert_eq!(
            redraw_seq_after_resize(Some(&Backend::KiroCli)),
            Some(b"\x0c".as_slice()),
            "Kiro must get Ctrl+L after resize"
        );
        for b in [
            Backend::ClaudeCode,
            Backend::Codex,
            Backend::OpenCode,
            Backend::Agy,
            Backend::Shell,
            Backend::Raw("whatever".into()),
        ] {
            assert_eq!(
                redraw_seq_after_resize(Some(&b)),
                None,
                "{b:?} must emit nothing after resize (provably untouched)"
            );
        }
        assert_eq!(
            redraw_seq_after_resize(None),
            None,
            "a backend-less pane must emit nothing"
        );
    }

    fn leaf(id: usize, name: &str) -> Pane {
        Pane {
            agent_name: name.into(),
            instance_id: crate::types::InstanceId::default(),
            vterm: VTerm::new(10, 10),
            rx: crossbeam_channel::bounded(1).1,
            id,
            backend: None,
            working_dir: None,
            display_name: None,
            scroll_offset: 0,
            has_notification: false,
            fleet_instance_name: None,
            last_input_at: None,
            pending_notification_count: 0,
            selection: None,
            source: PaneSource::Local,
            offthread: None,
            _fwd_cancel: None,
        }
    }

    /// #t-97931 (F-A, off-thread draft-gate) guard ③: the path-aware `Pane::tail_lines*`
    /// must read the parser's PUBLISHED snapshot off-thread, NOT the idle main-thread
    /// `pane.vterm` (blank). Else the #1944 draft-protection gate reads an empty input
    /// box and clobbers a real unsent draft. NEUTER: revert the accessor body to
    /// `self.vterm.tail_lines*` → off-thread it returns blank → this RED.
    #[test]
    fn tail_lines_offthread_reads_snapshot_not_idle_vterm() {
        let (data_tx, data_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let (wake_tx, wake_rx) = crossbeam_channel::unbounded::<usize>();
        let parser_vt = VTerm::new(40, 6);
        let handle = crate::render::offthread::spawn_offthread_parser(
            1,
            "claude".to_string(),
            data_rx,
            parser_vt,
            wake_tx,
        )
        .expect("parser thread spawns");
        // The agent's input box carries an UNSENT draft + a DIM ghost above it.
        data_tx
            .send(b"\x1b[2J\x1b[H\x1b[2mghost\x1b[0m\r\n> draft in progress".to_vec())
            .expect("parser data channel live");
        wake_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("parser must publish the input-box snapshot");

        let mut pane = leaf(1, "claude");
        pane.offthread = Some(handle);
        // The idle main-thread vterm is blank — the dead-gate surface the F-A bug read.
        assert!(
            pane.vterm.tail_lines(4).trim().is_empty(),
            "off-thread pane.vterm is idle → tail is blank (the F-A mis-read source)"
        );
        // The path-aware accessor reads the parser-published snapshot → sees the draft.
        let tail = pane.tail_lines(4);
        assert!(
            tail.contains("draft in progress"),
            "off-thread tail_lines must read the snapshot's real input box, got {tail:?}"
        );
        let (dtext, dim) = pane.tail_lines_with_dim(4);
        assert!(
            dtext.contains("draft in progress") && dim.iter().any(|&d| d),
            "off-thread tail_lines_with_dim must read snapshot text + DIM mask, got {dtext:?}"
        );
    }

    /// #1432: a fixed content line keeps the same logical anchor regardless of
    /// scroll offset — the basis for drag-scroll not drifting the selection.
    #[test]
    fn logical_anchor_is_offset_invariant() {
        let (_tx, rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let mut pane = test_pane(rx, 20, 5);
        for i in 0..20 {
            pane.vterm.process(format!("line{i}\r\n").as_bytes());
        }
        // Same grid line (e.g. one line above the screen top) viewed at two
        // different scroll offsets maps to the same logical anchor.
        pane.scroll_offset = 0;
        let a0 = pane.viewport_to_logical_line(1);
        pane.scroll_offset = 3;
        // Scrolling back by 3 moves that content down by 3 rows on screen.
        let a3 = pane.viewport_to_logical_line(1 + 3);
        assert_eq!(a0, a3, "logical anchor must be offset-invariant");
        // Round-trip back to a viewport row.
        assert_eq!(pane.logical_line_to_viewport(a3), 1 + 3);
    }

    /// #1432: appending output shifts the on-screen row of anchored content but
    /// leaves its logical anchor unchanged (selection tracks content, no drift).
    #[test]
    fn logical_anchor_stable_across_appended_output() {
        let (_tx, rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let mut pane = test_pane(rx, 20, 5);
        for i in 0..20 {
            pane.vterm.process(format!("line{i}\r\n").as_bytes());
        }
        pane.scroll_offset = 0;
        let anchor = pane.viewport_to_logical_line(2);
        let row_before = pane.logical_line_to_viewport(anchor);
        // 4 new lines scroll the grid up.
        for i in 20..24 {
            pane.vterm.process(format!("line{i}\r\n").as_bytes());
        }
        let row_after = pane.logical_line_to_viewport(anchor);
        assert_eq!(
            row_after,
            row_before - 4,
            "anchored content must move up by exactly the appended line count"
        );
    }

    fn test_pane(rx: crossbeam_channel::Receiver<Vec<u8>>, cols: u16, rows: u16) -> Pane {
        Pane {
            agent_name: "agent".into(),
            instance_id: crate::types::InstanceId::default(),
            vterm: VTerm::new(cols, rows),
            rx,
            id: 1,
            backend: None,
            working_dir: None,
            display_name: None,
            scroll_offset: 0,
            has_notification: false,
            fleet_instance_name: None,
            last_input_at: None,
            pending_notification_count: 0,
            selection: None,
            source: PaneSource::Local,
            offthread: None,
            _fwd_cancel: None,
        }
    }

    /// #offthread-selection END-TO-END: an off-thread pane's live `vterm` is idle/
    /// blank (drain no-ops; the parser thread owns the real grid), so text selection
    /// must read the published snapshot. Drives the REAL parser, builds an off-thread
    /// Pane, and asserts `extract_selection_text` (the path mouse copy-on-select +
    /// Cmd+C use) returns the on-screen content via the snapshot — NOT the empty live
    /// vterm. Neuter: route `extract_selection_text` back to `pane.vterm` → empty → RED.
    #[test]
    fn offthread_pane_extracts_selection_from_snapshot_not_idle_vterm() {
        let (data_tx, data_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let (wake_tx, wake_rx) = crossbeam_channel::unbounded::<usize>();
        let parser_vt = VTerm::new(20, 4);
        let handle = crate::render::offthread::spawn_offthread_parser(
            9,
            "sel".to_string(),
            data_rx,
            parser_vt,
            wake_tx,
        )
        .expect("parser thread spawns");

        // 8 lines into a 4-row screen → 4 rows scroll into the captured history.
        let mut payload = String::from("\x1b[2J\x1b[H");
        for i in 1..=8 {
            payload.push_str(&format!("row{i:02}\r\n"));
        }
        data_tx
            .send(payload.into_bytes())
            .expect("parser data channel is live");
        assert_eq!(
            wake_rx.recv_timeout(std::time::Duration::from_secs(2)),
            Ok(9),
            "parser must publish a snapshot"
        );

        // Off-thread pane: the parser owns the content; the pane's own vterm is idle.
        let mut pane = test_pane(crossbeam_channel::bounded(1).1, 20, 4);
        pane.offthread = Some(handle);
        pane.scroll_offset = 0;

        // The bug: the OLD path (idle vterm) copies nothing.
        assert!(
            pane.vterm.extract_text((0, 0), (3, 19)).trim().is_empty(),
            "precondition: an off-thread pane's live vterm is blank"
        );

        // Full content (oldest history row .. bottom visible) via the off-thread-aware
        // depth: must contain the FIRST (scrolled into history) + LAST (visible) lines.
        let depth = pane.scroll_max() as i64;
        assert!(
            depth > 0,
            "8 lines / 4 rows must produce captured scrollback"
        );
        let full = pane.extract_selection_text((0, 0), (depth + 3, 19));
        assert!(
            full.contains("row01") && full.contains("row08"),
            "off-thread selection must copy history + visible content; got {full:?}"
        );

        // Viewport-mapped selection (the path the mouse uses): the visible screen
        // rows 0..3 extract real on-screen content, not blanks.
        let start = (pane.viewport_to_logical_line(0), 0u16);
        let end = (pane.viewport_to_logical_line(3), 19u16);
        let visible = pane.extract_selection_text(start, end);
        assert!(
            visible.contains("row") && !visible.trim().is_empty(),
            "viewport selection must copy on-screen content; got {visible:?}"
        );
    }

    /// Wait for the parser's first publish, then drain any further publishes until
    /// quiet, so the loaded snapshot reflects the WHOLE fed burst (not a mid-burst one).
    fn wait_settled(wake_rx: &crossbeam_channel::Receiver<usize>) {
        wake_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("parser must publish at least once");
        while wake_rx
            .recv_timeout(std::time::Duration::from_millis(150))
            .is_ok()
        {}
    }

    /// #offthread-selection ANCHOR STABILITY (r6 REJECT @03bf21af). A selection
    /// endpoint must stay pinned to its content AFTER the snapshot's 1000-row history
    /// window saturates and shifts — the rejected impl used a RELATIVE depth
    /// (`grid_line + history.len()`) which drifts there because `history.len()` pins at
    /// 1000 while content evicts+appends. The absolute `history_origin` fix tracks it.
    /// Neuter (the rejected coord): drop the `- history_origin` in `extract_text` AND
    /// the origin in `selection_base` → after the window shift the endpoint drifts →
    /// `after != before` → RED.
    #[test]
    fn offthread_selection_anchor_stable_past_snapshot_window() {
        let (data_tx, data_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let (wake_tx, wake_rx) = crossbeam_channel::unbounded::<usize>();
        let parser_vt = VTerm::new(20, 4);
        let handle = crate::render::offthread::spawn_offthread_parser(
            2,
            "anchor".to_string(),
            data_rx,
            parser_vt,
            wake_tx,
        )
        .expect("parser thread spawns");

        // 1100 distinct lines → the 1000-row window SATURATES + shifts, and the parser
        // max_scroll (~1096) ≫ window → history_origin advances past 0.
        let mut p = String::from("\x1b[2J\x1b[H");
        for i in 1..=1100 {
            p.push_str(&format!("L{i:05}\r\n"));
        }
        data_tx.send(p.into_bytes()).expect("send burst");
        wait_settled(&wake_rx);

        let mut pane = test_pane(crossbeam_channel::bounded(1).1, 20, 4);
        pane.offthread = Some(handle);
        pane.scroll_offset = 0;

        // The regime r6 flagged: window saturated + origin advanced.
        let snap0 = pane.offthread.as_ref().expect("offthread set").load();
        assert_eq!(
            snap0.history.len(),
            1000,
            "1100 lines must saturate the 1000-row snapshot window"
        );
        assert!(
            snap0.history_origin > 0,
            "history_origin must advance past the window: {}",
            snap0.history_origin
        );
        drop(snap0);

        // Capture the absolute id of the TOP visible row + its content NOW.
        let cap_top = pane.viewport_to_logical_line(0);
        let before = pane.extract_selection_text((cap_top, 0), (cap_top, 19));
        assert!(
            before.starts_with('L'),
            "captured a content line: {before:?}"
        );

        // Publish 10 MORE lines: the captured content scrolls back 10 rows (still in
        // the 1000-row window). The absolute id is fixed; origin advances by 10.
        let mut p2 = String::new();
        for i in 1101..=1110 {
            p2.push_str(&format!("L{i:05}\r\n"));
        }
        data_tx.send(p2.into_bytes()).expect("send more");
        wait_settled(&wake_rx);

        let after = pane.extract_selection_text((cap_top, 0), (cap_top, 19));
        assert_eq!(
            after, before,
            "absolute anchor must track content across a window shift past the cap (the r6 drift)"
        );
    }

    /// #offthread-selection: a resize BETWEEN selection capture and copy must not
    /// drift the anchor (r6 required). A rows-only resize (cols unchanged → history
    /// does NOT reflow) shifts the screen/scrollback split → both `max_scroll` and
    /// `history_origin` move, but the absolute id stays pinned to the same content.
    #[test]
    fn offthread_selection_anchor_survives_resize_between_capture_and_copy() {
        let (data_tx, data_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let (wake_tx, wake_rx) = crossbeam_channel::unbounded::<usize>();
        let parser_vt = VTerm::new(20, 6);
        let handle = crate::render::offthread::spawn_offthread_parser(
            3,
            "resize".to_string(),
            data_rx,
            parser_vt,
            wake_tx,
        )
        .expect("parser thread spawns");

        let mut p = String::from("\x1b[2J\x1b[H");
        for i in 1..=40 {
            p.push_str(&format!("R{i:04}\r\n"));
        }
        data_tx.send(p.into_bytes()).expect("send burst");
        wait_settled(&wake_rx);

        let mut pane = test_pane(crossbeam_channel::bounded(1).1, 20, 6);
        pane.offthread = Some(handle);
        pane.scroll_offset = 0;

        let cap = pane.viewport_to_logical_line(1); // a visible content row
        let before = pane.extract_selection_text((cap, 0), (cap, 19));
        assert!(
            before.starts_with('R'),
            "captured a content line: {before:?}"
        );

        // Resize rows only (cols stay 20 → no history reflow), via the real handle →
        // the parser re-publishes a fresh snapshot at the new dims with a new origin.
        assert!(
            pane.offthread
                .as_ref()
                .expect("offthread set")
                .request_resize(20, 10),
            "resize must be routed to the parser"
        );
        wait_settled(&wake_rx);

        let after = pane.extract_selection_text((cap, 0), (cap, 19));
        assert_eq!(
            after, before,
            "absolute anchor must survive a resize between capture and copy"
        );
    }

    /// #freeze-2: `drain_output` must (a) cap per call at the byte budget and
    /// return the bytes drained, (b) leave the remainder queued (`!rx.is_empty()`)
    /// so the loop re-arms, (c) over repeated calls drain the WHOLE backlog, (d)
    /// losslessly and in FIFO order.
    #[test]
    fn drain_output_budget_caps_and_drains_losslessly_in_fifo_order() {
        let (tx, rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let mut pane = test_pane(rx, 80, 4);
        // 10 distinct single-byte chunks: '0'..='9' (1 byte each).
        for c in b'0'..=b'9' {
            let _ = tx.send(vec![c]); // unbounded + live rx → never fails
        }
        drop(tx); // disconnect so try_recv ends the loop once the queue is empty

        // First call, 3-byte budget: processes exactly chunks '0','1','2' (3 bytes,
        // the while-budget check trips at drained==3), leaving '3'..='9' queued.
        let drained = pane.drain_output(3);
        assert_eq!(
            drained, 3,
            "a 3-byte budget drains exactly 3 one-byte chunks"
        );
        assert!(
            !pane.rx.is_empty(),
            "a 3-byte budget over 10 queued bytes must leave a remainder"
        );
        let after_first = pane.vterm.tail_lines(4);
        assert!(
            after_first.contains("012"),
            "budgeted prefix processed: {after_first:?}"
        );
        assert!(
            !after_first.contains('3'),
            "budget cap: the 4th chunk must NOT be processed yet: {after_first:?}"
        );

        // Drain the rest over subsequent "frames" until dry (a call drains 0 once
        // the queue is empty).
        let mut frames = 0;
        while pane.drain_output(3) > 0 {
            frames += 1;
            assert!(frames < 50, "must converge to drained");
        }
        // Lossless + FIFO: every chunk processed, in order.
        let final_lines = pane.vterm.tail_lines(4);
        assert!(
            final_lines.contains("0123456789"),
            "all chunks must be drained in FIFO order: {final_lines:?}"
        );
    }

    /// Option X (S3 wiring): when a parser thread owns this pane's VTerm
    /// (`offthread = Some`), `drain_output` MUST be a no-op. This is the freeze-fix
    /// invariant — the main render thread parses ZERO bytes per frame regardless of
    /// backlog size, so a restart flood (the boot-race) can never stall the draw /
    /// input loop. It is also a correctness requirement: the parser is the SOLE
    /// consumer of the pane's `rx` clone (crossbeam is MPMC), so a main-thread drain
    /// here would STEAL chunks from the parser.
    #[test]
    fn drain_output_is_noop_when_offthread_owns_parsing() {
        // `pane.rx` carries a flood; the parser gets a SEPARATE idle channel so the
        // assertions are deterministic (the parser never touches `pane.rx`).
        let (flood_tx, flood_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let (_parser_tx, parser_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let (wake_tx, _wake_rx) = crossbeam_channel::unbounded::<usize>();
        let handle = crate::render::offthread::spawn_offthread_parser(
            1,
            "t".to_string(),
            parser_rx,
            VTerm::new(20, 4),
            wake_tx,
        )
        .expect("parser thread spawns");
        let mut pane = test_pane(flood_rx, 20, 4);
        pane.offthread = Some(handle);
        for _ in 0..100 {
            let _ = flood_tx.send(b"flood".to_vec());
        }

        let drained = pane.drain_output(10_000_000);
        assert_eq!(
            drained, 0,
            "offthread pane: main-thread drain must be a no-op"
        );
        assert_eq!(
            pane.rx.len(),
            100,
            "main must NOT consume any rx chunks when the parser owns parsing"
        );
        assert!(
            pane.vterm.tail_lines(4).trim().is_empty(),
            "main-thread VTerm must stay untouched (zero parse on main): {:?}",
            pane.vterm.tail_lines(4)
        );
    }

    // ── #freeze-3 H2 (background-tab catch-up) deterministic proofs ────────

    /// #freeze-3 H2 MEMORY: the pane's `rx` is an UNBOUNDED channel — left undrained
    /// (the pre-fix background-tab case) it queues EVERY chunk, growing ∝ the
    /// agent's output. This pins the hazard the render loop's `drain_all_panes` must
    /// bound: it now drains every pane every frame, so a backgrounded pane's `rx`
    /// never reaches this state in practice (see
    /// `core_render::tests::drain_all_panes_bounds_background_rx_freeze3`).
    #[test]
    fn backgrounded_pane_rx_accumulates_unbounded_freeze3() {
        let (tx, rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let pane = test_pane(rx, 80, 24);
        for _ in 0..1000 {
            let _ = tx.send(vec![b'y'; 1024]); // live unbounded rx → never fails
        }
        assert_eq!(
            pane.rx.len(),
            1000,
            "an un-drained pane queues every chunk — unbounded memory (~1 MiB here; \
             grows without limit). The fix is to drain it every frame regardless of \
             which tab is active."
        );
    }

    /// #freeze-3 H2 SIZING: a budget-capped drain processes
    /// `DRAIN_OUTPUT_BUDGET_BYTES` (32 KiB) per frame, so draining a backlog takes
    /// `ceil(backlog / 32KiB)` frames — LINEAR in the backlog. At `FRAME_INTERVAL`
    /// = 33 ms: 1 MiB ≈ 32 frames ≈ 1 s, 16 MiB ≈ 512 ≈ 17 s. This is exactly the
    /// residual #2385 left: it turned "one 107 ms input-stall" into a non-blocking
    /// but UNBOUNDED-LENGTH catch-up — unbounded because the *backlog* was unbounded
    /// (background tabs never drained). The #freeze-3 fix bounds the backlog by
    /// draining every pane every frame; this test sizes the per-frame budget.
    #[test]
    fn drain_catchup_frames_scale_linearly_with_backlog_h2_freeze3() {
        const BUDGET: usize = 32 * 1024; // mirror DRAIN_OUTPUT_BUDGET_BYTES
        const CHUNK: usize = 4 * 1024; // PTY-read-sized chunk
        for &(kib, want_frames) in &[(128usize, 4usize), (256, 8), (512, 16)] {
            let (tx, rx) = crossbeam_channel::unbounded::<Vec<u8>>();
            let mut pane = test_pane(rx, 80, 24);
            let total = kib * 1024;
            for _ in 0..(total / CHUNK) {
                let _ = tx.send(vec![b'x'; CHUNK]); // live unbounded rx → never fails
            }
            // Count per-frame bounded drains until dry (the render loop's re-arm).
            let mut frames = 0usize;
            loop {
                frames += 1;
                assert!(frames < 100_000, "must converge to drained");
                pane.drain_output(BUDGET);
                if pane.rx.is_empty() {
                    break;
                }
            }
            assert_eq!(
                frames, want_frames,
                "{kib} KiB backlog must take ceil({kib}KiB / 32KiB) = {want_frames} \
                 bounded-drain frames (catch-up is LINEAR in backlog)"
            );
        }
    }

    #[test]
    fn pane_composing_after_input() {
        let mut pane = leaf(1, "agent");
        pane.mark_input_activity();
        assert!(pane.is_composing());
    }
    #[test]
    fn pane_not_composing_after_idle() {
        let mut pane = leaf(1, "agent");
        pane.last_input_at = Some(
            std::time::Instant::now()
                - crate::notification_queue::COMPOSE_IDLE_TIMEOUT
                - std::time::Duration::from_millis(1),
        );
        assert!(!pane.is_composing());
    }

    // #freeze-cputime: the probe's thread-CPU-time source must work on the
    // operator's platform (macOS) and Linux, and advance monotonically as the
    // thread burns CPU — otherwise the `drain_us` vs `cpu_us` comparison the
    // confirm-first probe rests on would be meaningless. Uses real CPU work (no
    // sleep) so it measures thread CPU time, not wall time.
    #[cfg(unix)]
    #[test]
    fn thread_cpu_time_us_is_some_and_monotonic() {
        let t0 = thread_cpu_time_us().expect("CLOCK_THREAD_CPUTIME_ID available on unix");
        let mut acc = 0u64;
        for i in 0..5_000_000u64 {
            acc = acc.wrapping_add(i.wrapping_mul(2_654_435_761));
        }
        std::hint::black_box(acc);
        let t1 = thread_cpu_time_us().expect("second CPU-time sample");
        assert!(
            t1 >= t0,
            "thread CPU time must be monotonic non-decreasing: {t0} -> {t1}"
        );
    }
}
