//! SGR mouse event encoder — converts crossterm MouseEvent to xterm SGR format.
//! Used to forward mouse events to backends that enable mouse tracking.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

/// Encode a crossterm MouseEvent as SGR mouse report bytes.
/// `pane_col`/`pane_row` are the pane's top-left offset (subtracted from event coords).
/// Returns None if the event kind is not forwardable.
pub fn encode_sgr(event: &MouseEvent, pane_col: u16, pane_row: u16) -> Option<Vec<u8>> {
    let x = event.column.saturating_sub(pane_col) + 1; // 1-based
    let y = event.row.saturating_sub(pane_row) + 1;

    let (button, suffix) = match event.kind {
        MouseEventKind::Down(btn) => (button_code(btn), 'M'),
        MouseEventKind::Up(btn) => (button_code(btn), 'm'),
        MouseEventKind::Drag(btn) => (button_code(btn) + 32, 'M'),
        MouseEventKind::ScrollUp => (64, 'M'),
        MouseEventKind::ScrollDown => (65, 'M'),
        MouseEventKind::ScrollLeft => (66, 'M'),
        MouseEventKind::ScrollRight => (67, 'M'),
        MouseEventKind::Moved => (35, 'M'), // motion with no button
    };

    Some(format!("\x1b[<{button};{x};{y}{suffix}").into_bytes())
}

fn button_code(btn: MouseButton) -> u8 {
    match btn {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn encode_sgr_left_click() {
        let event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 10,
            row: 5,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let result = encode_sgr(&event, 0, 0).unwrap();
        assert_eq!(result, b"\x1b[<0;11;6M");
    }

    #[test]
    fn encode_sgr_scroll_up() {
        let event = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 3,
            row: 2,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let result = encode_sgr(&event, 0, 0).unwrap();
        assert_eq!(result, b"\x1b[<64;4;3M");
    }

    #[test]
    fn encode_sgr_with_pane_offset() {
        let event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: 15,
            row: 10,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let result = encode_sgr(&event, 5, 3).unwrap();
        // x = 15-5+1 = 11, y = 10-3+1 = 8
        assert_eq!(result, b"\x1b[<2;11;8M");
    }

    #[test]
    fn encode_sgr_button_release() {
        let event = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 1,
            row: 1,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let result = encode_sgr(&event, 0, 0).unwrap();
        assert_eq!(result, b"\x1b[<0;2;2m"); // lowercase m for release
    }
}
