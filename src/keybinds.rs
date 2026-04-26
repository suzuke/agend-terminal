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
    ResizeUp,
    ResizeDown,
    ResizeLeft,
    ResizeRight,
    ClosePane,
    CloseTab,
    ToggleZoom,
    NextLayout,
    RenameTab,
    RenamePane,
    /// Open the move-pane target menu (cross-tab relocation of the focused pane).
    MovePaneMenu,
    ListTabs,
    ScrollMode,
    CommandPalette,
    ShowDecisions,
    ShowTasks,
    ShowStatus,
    ShowMonitor,
    Detach,
    ShowHelp,
    /// Summon a floating scratch shell overlay (Ctrl+B ~). Esc closes & kills it.
    ScratchShell,
    /// Copy the focused pane's selection to clipboard (Cmd+C).
    CopySelection,
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
                // Cmd+C (macOS) → copy selection to clipboard
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::SUPER) {
                    return Action::CopySelection;
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
                if (key.code == KeyCode::Enter && !key.modifiers.contains(KeyModifiers::SHIFT))
                    || key.code == KeyCode::Esc
                {
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
        KeyCode::Char('l') if !key.modifiers.contains(KeyModifiers::SHIFT) => Action::LastTab,
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
        // tmux uses `!` for break-pane (split pane to new window). We reuse
        // the same key, but open a menu that lets the user pick an EXISTING
        // tab OR spawn a new one — covering both move-pane and break-pane.
        KeyCode::Char('!') => Action::MovePaneMenu,

        // Directional pane focus (plain arrows) / resize (Alt+arrows).
        // Alt+Arrow encoding depends on the terminal (macOS Terminal, Ghostty,
        // iTerm2 all disagree on whether Option is Meta), so we also accept
        // uppercase H/J/K/L as a portable tmux-style resize fallback.
        // Match both uppercase (legacy) and lowercase+SHIFT (Kitty protocol).
        KeyCode::Up if key.modifiers.contains(KeyModifiers::ALT) => Action::ResizeUp,
        KeyCode::Down if key.modifiers.contains(KeyModifiers::ALT) => Action::ResizeDown,
        KeyCode::Left if key.modifiers.contains(KeyModifiers::ALT) => Action::ResizeLeft,
        KeyCode::Right if key.modifiers.contains(KeyModifiers::ALT) => Action::ResizeRight,
        KeyCode::Char('K') => Action::ResizeUp,
        KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::SHIFT) => Action::ResizeUp,
        KeyCode::Char('J') => Action::ResizeDown,
        KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::SHIFT) => Action::ResizeDown,
        KeyCode::Char('H') => Action::ResizeLeft,
        KeyCode::Char('h') if key.modifiers.contains(KeyModifiers::SHIFT) => Action::ResizeLeft,
        KeyCode::Char('L') => Action::ResizeRight,
        KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::SHIFT) => Action::ResizeRight,
        KeyCode::Up => Action::FocusUp,
        KeyCode::Down => Action::FocusDown,
        KeyCode::Left => Action::FocusLeft,
        KeyCode::Right => Action::FocusRight,

        // Modes
        KeyCode::Char('[') => Action::ScrollMode,
        KeyCode::Char(':') => Action::CommandPalette,
        KeyCode::Char('D') => Action::ShowDecisions,
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::SHIFT) => Action::ShowDecisions,
        KeyCode::Char('s') => Action::ShowStatus,
        KeyCode::Char('m') | KeyCode::Char('M') => Action::ShowMonitor,
        KeyCode::Char('T') | KeyCode::Char('t') => Action::ShowTasks,
        KeyCode::Char('d') if !key.modifiers.contains(KeyModifiers::SHIFT) => Action::Detach,
        KeyCode::Char('?') => Action::ShowHelp,
        KeyCode::Char('~') => Action::ScratchShell,

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
            | Action::ResizeUp
            | Action::ResizeDown
            | Action::ResizeLeft
            | Action::ResizeRight
            | Action::NextTab
            | Action::PrevTab
            | Action::NextLayout
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn prefix_action(code: KeyCode, modifiers: KeyModifiers) -> Action {
        dispatch_prefix(KeyEvent::new(code, modifiers))
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    // --- Shift+L: ResizeRight (not LastTab) ---

    #[test]
    fn keybind_ctrl_b_shift_l_resize_right_kitty() {
        assert_eq!(
            prefix_action(KeyCode::Char('l'), KeyModifiers::SHIFT),
            Action::ResizeRight
        );
    }

    #[test]
    fn keybind_ctrl_b_shift_l_resize_right_legacy() {
        assert_eq!(
            prefix_action(KeyCode::Char('L'), KeyModifiers::empty()),
            Action::ResizeRight
        );
    }

    #[test]
    fn keybind_ctrl_b_l_last_tab_no_shift() {
        assert_eq!(
            prefix_action(KeyCode::Char('l'), KeyModifiers::empty()),
            Action::LastTab
        );
    }

    // --- Shift+D: ShowDecisions (not Detach) ---

    #[test]
    fn keybind_ctrl_b_shift_d_show_decisions_kitty() {
        assert_eq!(
            prefix_action(KeyCode::Char('d'), KeyModifiers::SHIFT),
            Action::ShowDecisions
        );
    }

    #[test]
    fn keybind_ctrl_b_shift_d_show_decisions_legacy() {
        assert_eq!(
            prefix_action(KeyCode::Char('D'), KeyModifiers::empty()),
            Action::ShowDecisions
        );
    }

    #[test]
    fn keybind_ctrl_b_d_detach_no_shift() {
        assert_eq!(
            prefix_action(KeyCode::Char('d'), KeyModifiers::empty()),
            Action::Detach
        );
    }

    // --- Shift+H: ResizeLeft ---

    #[test]
    fn keybind_ctrl_b_shift_h_resize_left_kitty() {
        assert_eq!(
            prefix_action(KeyCode::Char('h'), KeyModifiers::SHIFT),
            Action::ResizeLeft
        );
    }

    #[test]
    fn keybind_ctrl_b_shift_h_resize_left_legacy() {
        assert_eq!(
            prefix_action(KeyCode::Char('H'), KeyModifiers::empty()),
            Action::ResizeLeft
        );
    }

    // --- Shift+J: ResizeDown ---

    #[test]
    fn keybind_ctrl_b_shift_j_resize_down_kitty() {
        assert_eq!(
            prefix_action(KeyCode::Char('j'), KeyModifiers::SHIFT),
            Action::ResizeDown
        );
    }

    #[test]
    fn keybind_ctrl_b_shift_j_resize_down_legacy() {
        assert_eq!(
            prefix_action(KeyCode::Char('J'), KeyModifiers::empty()),
            Action::ResizeDown
        );
    }

    // --- Shift+K: ResizeUp ---

    #[test]
    fn keybind_ctrl_b_shift_k_resize_up_kitty() {
        assert_eq!(
            prefix_action(KeyCode::Char('k'), KeyModifiers::SHIFT),
            Action::ResizeUp
        );
    }

    #[test]
    fn keybind_ctrl_b_shift_k_resize_up_legacy() {
        assert_eq!(
            prefix_action(KeyCode::Char('K'), KeyModifiers::empty()),
            Action::ResizeUp
        );
    }

    // --- Cmd+C / Normal mode ---

    #[test]
    fn cmd_c_returns_copy_selection() {
        let mut handler = KeyHandler::new();
        let action = handler.handle(key(KeyCode::Char('c'), KeyModifiers::SUPER));
        assert_eq!(action, Action::CopySelection);
    }

    #[test]
    fn ctrl_b_still_enters_prefix() {
        let mut handler = KeyHandler::new();
        let action = handler.handle(key(KeyCode::Char('b'), KeyModifiers::CONTROL));
        assert_eq!(action, Action::None);
        let action2 = handler.handle(key(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(action2, Action::NextTab);
    }

    #[test]
    fn plain_c_forwards() {
        let mut handler = KeyHandler::new();
        let action = handler.handle(key(KeyCode::Char('c'), KeyModifiers::NONE));
        assert!(matches!(action, Action::Forward(_)));
    }
}
