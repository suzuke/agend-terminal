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
    /// Active text selection (grid coordinates within this pane's VTerm).
    pub selection: Option<Selection>,
    /// `max_scroll()` snapshot at selection start. Used to compensate for
    /// new output during selection so the viewport stays pinned to the
    /// same content.
    pub selection_scroll_freeze: Option<usize>,
    /// Whether input/resize go to a local PTY (via registry) or a remote
    /// daemon-hosted agent (via `BridgeClient`).
    pub source: PaneSource,
}

/// Text selection within a pane's VTerm grid.
#[derive(Clone)]
pub struct Selection {
    /// Start position (row, col) in VTerm grid coordinates.
    pub start: (u16, u16),
    /// End position (row, col) — may be before or after start.
    pub end: (u16, u16),
}

impl Pane {
    /// Effective scroll offset: during active selection, compensates for
    /// new output since selection started so the viewport stays pinned.
    pub fn effective_scroll_offset(&self) -> usize {
        match self.selection_scroll_freeze {
            Some(frozen_max) => {
                let current_max = self.vterm.max_scroll();
                let drift = current_max.saturating_sub(frozen_max);
                self.scroll_offset.saturating_add(drift)
            }
            None => self.scroll_offset,
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

    /// Drain pending output into the local VTerm.
    pub fn drain_output(&mut self) {
        // #1432: while a mouse selection gesture is active, freeze the grid by
        // not draining new output. `selection_scroll_freeze` is set on MouseDown
        // and cleared on MouseUp, so this pauses only for the gesture's duration.
        // The scroll-offset drift compensation (#1358) only handles output that
        // *appends* to scrollback; agent CLIs frequently redraw in place (cursor
        // movement, spinners) without growing `max_scroll()`, which the offset
        // approach cannot pin. Freezing the grid covers every case. Output stays
        // queued in the unbounded `rx` channel and drains on the next call once
        // the gesture ends.
        if self.selection_scroll_freeze.is_some() {
            return;
        }
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
                let reg = agent::lock_registry(registry);
                if let Some(handle) = reg.get(self.agent_name.as_str()) {
                    let _ = agent::write_to_agent(handle, bytes);
                }
                drop(reg);
                // Clear reply_to on TUI keyboard input (Sprint 52).
                crate::daemon::heartbeat_pair::update_with(&self.agent_name, |p| {
                    p.reply_to_channel = None;
                    p.reply_to_input_id = None;
                });
            }
            PaneSource::Remote(client) => {
                let mut c = client.lock();
                let _ = c.send_input(bytes);
            }
        }
    }

    /// Resize this pane's underlying PTY / remote agent.
    pub fn resize_pty(&self, registry: &AgentRegistry, cols: u16, rows: u16) {
        match &self.source {
            PaneSource::Local => {
                let reg = agent::lock_registry(registry);
                if let Some(handle) = reg.get(self.agent_name.as_str()) {
                    let master = handle.pty_master.lock();
                    let _ = master.resize(portable_pty::PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
            }
            PaneSource::Remote(client) => {
                let mut c = client.lock();
                let _ = c.send_resize(cols, rows);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vterm::VTerm;

    fn leaf(id: usize, name: &str) -> Pane {
        Pane {
            agent_name: name.into(),
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
            selection_scroll_freeze: None,
            source: PaneSource::Local,
        }
    }

    /// #1432: output must not reach the VTerm while a selection gesture is
    /// active (grid frozen), and must catch up once the gesture ends.
    #[test]
    fn drain_output_frozen_during_selection_then_resumes() {
        let (tx, rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let mut pane = Pane {
            agent_name: "agent".into(),
            vterm: VTerm::new(20, 5),
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
            selection_scroll_freeze: None,
            source: PaneSource::Local,
        };

        tx.send(b"hello".to_vec()).expect("send baseline output");
        pane.drain_output();
        let before = pane.vterm.read_scrollback(100);
        assert!(before.contains("hello"), "baseline: {before:?}");

        // Selection gesture active → freeze: new output must not be drained.
        pane.selection_scroll_freeze = Some(pane.vterm.max_scroll());
        tx.send(b" world".to_vec())
            .expect("send frozen-window output");
        pane.drain_output();
        let during = pane.vterm.read_scrollback(100);
        assert_eq!(during, before, "grid must stay frozen during selection");

        // Gesture ends → drain resumes and catches up.
        pane.selection_scroll_freeze = None;
        pane.drain_output();
        let after = pane.vterm.read_scrollback(100);
        assert!(
            after.contains("hello world"),
            "output must catch up after selection: {after:?}"
        );
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
