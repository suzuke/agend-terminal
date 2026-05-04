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
    pub agent_name: String,
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
                let reg = agent::lock_registry(registry);
                if let Some(handle) = reg.get(&self.agent_name) {
                    let _ = agent::write_to_agent(handle, bytes);
                }
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
                if let Some(handle) = reg.get(&self.agent_name) {
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
            agent_name: name.to_string(),
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
