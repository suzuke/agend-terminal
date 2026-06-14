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
}

/// Text selection anchored to absolute scrollback logical coordinates so it
/// stays pinned to its content under both new output and user scrolling.
///
/// `.0` is the logical line index counted from the oldest line currently in
/// the buffer (`grid_line + max_scroll()`); `.1` is the column. Endpoints are
/// converted to viewport rows at render / extract time via
/// [`Pane::logical_line_to_viewport`].
///
/// Edge case: a line evicted from the 10000-line history cap loses its anchor
/// and the selection drifts. Unreachable within a single gesture (would need
/// ~10000 lines of output in the seconds a drag takes), so left unhandled.
#[derive(Clone)]
pub struct Selection {
    /// Start: (logical line, column). May be before or after `end`.
    pub start: (i64, u16),
    /// End: (logical line, column).
    pub end: (i64, u16),
}

impl Pane {
    /// Convert a viewport row (0-based within the pane interior) to an absolute
    /// scrollback logical line at the current scroll position. Inverse of
    /// [`Self::logical_line_to_viewport`].
    ///
    /// Derivation: render maps viewport row → `grid_line = row - scroll_offset`
    /// (see `VTerm::render_to_buffer`), and the oldest buffer line sits at
    /// `grid_line = -max_scroll()`. Counting from that oldest line gives a
    /// reference stable under append: `logical = grid_line + max_scroll()`.
    pub fn viewport_to_logical_line(&self, row: u16) -> i64 {
        row as i64 - self.scroll_offset as i64 + self.vterm.max_scroll() as i64
    }

    /// Convert an absolute scrollback logical line back to a viewport row at
    /// the current scroll position. The result may be negative or `>=` viewport
    /// height when the anchored content has scrolled off-screen; callers clip.
    pub fn logical_line_to_viewport(&self, logical: i64) -> i64 {
        logical + self.scroll_offset as i64 - self.vterm.max_scroll() as i64
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

    /// Drain pending output into the local VTerm.
    pub fn drain_output(&mut self) {
        while let Ok(data) = self.rx.try_recv() {
            self.vterm.process(&data);
            if self.backend.is_some() {
                let text = String::from_utf8_lossy(&data);
                if text.contains("[from:") {
                    self.has_notification = true;
                }
            }
        }
        // Don't auto-scroll if user has scrolled back (they're reading history).
        // User scrolls back to bottom manually via mouse or Ctrl+B [ → j.
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
        }
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
}
