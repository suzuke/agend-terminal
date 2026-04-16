//! Keyboard input handling with Ctrl+B prefix mode (tmux-compatible).
//!
//! Repeatable keys (arrows, o, n, p) keep prefix active after dispatch,
//! so you can press Ctrl+B → ↑ ↑ ↑ without re-pressing Ctrl+B.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::time::{Duration, Instant};

/// How long repeat mode stays active after a repeatable key.
const REPEAT_TIMEOUT: Duration = Duration::from_millis(1500);

/// Actions that can be triggered by keybindings.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Forward(KeyEvent),
    NewTab,
    NextTab,
    PrevTab,
    LastTab,
    GotoTab(usize),
    SplitHorizontal,
    SplitVertical,
    CycleFocus,
    FocusUp,
    FocusDown,
    FocusLeft,
    FocusRight,
    ClosePane,
    CloseTab,
    ToggleZoom,
    NextLayout,
    RenameTab,
    RenamePane,
    ListTabs,
    ScrollMode,
    CommandPalette,
    ShowDecisions,
    ShowTasks,
    Detach,
    ShowHelp,
    None,
}

/// Prefix state machine.
enum PrefixState {
    /// Normal mode — keys forwarded to PTY.
    Normal,
    /// Waiting for first key after Ctrl+B (no timeout).
    WaitingFirst,
    /// Repeat mode — prefix stays active with timeout.
    Repeat { since: Instant },
}

/// Input state machine for prefix-key handling.
pub struct KeyHandler {
    state: PrefixState,
}

impl KeyHandler {
    pub fn new() -> Self {
        Self {
            state: PrefixState::Normal,
        }
    }

    /// Whether we're in repeat mode (prefix stays active for rapid presses).
    pub fn in_repeat(&self) -> bool {
        matches!(self.state, PrefixState::Repeat { .. })
    }

    /// Process a key event and return the resulting action.
    pub fn handle(&mut self, key: KeyEvent) -> Action {
        match &self.state {
            PrefixState::Normal => {
                // Check for Ctrl+B (prefix key)
                if key.code == KeyCode::Char('b') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    self.state = PrefixState::WaitingFirst;
                    return Action::None;
                }
                Action::Forward(key)
            }
            PrefixState::WaitingFirst => {
                // Ctrl+B Ctrl+B → forward Ctrl+B to PTY
                if key.code == KeyCode::Char('b') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    self.state = PrefixState::Normal;
                    return Action::Forward(key);
                }
                let action = dispatch_prefix(key);
                self.state = if is_repeatable(&action) {
                    PrefixState::Repeat {
                        since: Instant::now(),
                    }
                } else {
                    PrefixState::Normal
                };
                action
            }
            PrefixState::Repeat { since } => {
                // Enter or Esc exits repeat mode immediately
                if key.code == KeyCode::Enter || key.code == KeyCode::Esc {
                    self.state = PrefixState::Normal;
                    return Action::None;
                }
                if since.elapsed() > REPEAT_TIMEOUT {
                    self.state = PrefixState::Normal;
                    Action::Forward(key)
                } else {
                    let action = dispatch_prefix(key);
                    self.state = if is_repeatable(&action) {
                        PrefixState::Repeat {
                            since: Instant::now(),
                        }
                    } else {
                        PrefixState::Normal
                    };
                    action
                }
            }
        }
    }
}

/// Map the key after prefix (Ctrl+B) to an action.
fn dispatch_prefix(key: KeyEvent) -> Action {
    match key.code {
        // Window (tab) management
        KeyCode::Char('c') => Action::NewTab,
        KeyCode::Char('n') => Action::NextTab,
        KeyCode::Char('p') => Action::PrevTab,
        KeyCode::Char('l') => Action::LastTab,
        KeyCode::Char('&') => Action::CloseTab,
        KeyCode::Char(',') => Action::RenameTab,
        KeyCode::Char('w') => Action::ListTabs,
        KeyCode::Char(c) if c.is_ascii_digit() => {
            Action::GotoTab(c.to_digit(10).unwrap_or(0) as usize)
        }

        // Pane management
        KeyCode::Char('"') => Action::SplitHorizontal,
        KeyCode::Char('%') => Action::SplitVertical,
        KeyCode::Char('o') => Action::CycleFocus,
        KeyCode::Char('x') => Action::ClosePane,
        KeyCode::Char('z') => Action::ToggleZoom,
        KeyCode::Char(' ') => Action::NextLayout,
        KeyCode::Char('.') => Action::RenamePane,

        // Directional pane focus
        KeyCode::Up => Action::FocusUp,
        KeyCode::Down => Action::FocusDown,
        KeyCode::Left => Action::FocusLeft,
        KeyCode::Right => Action::FocusRight,

        // Modes
        KeyCode::Char('[') => Action::ScrollMode,
        KeyCode::Char(':') => Action::CommandPalette,
        KeyCode::Char('D') => Action::ShowDecisions,
        KeyCode::Char('T') => Action::ShowTasks,
        KeyCode::Char('d') => Action::Detach,
        KeyCode::Char('?') => Action::ShowHelp,

        _ => Action::None,
    }
}

/// Keys that keep prefix active for repeat presses.
fn is_repeatable(action: &Action) -> bool {
    matches!(
        action,
        Action::CycleFocus
            | Action::FocusUp
            | Action::FocusDown
            | Action::FocusLeft
            | Action::FocusRight
            | Action::NextTab
            | Action::PrevTab
            | Action::NextLayout
    )
}
