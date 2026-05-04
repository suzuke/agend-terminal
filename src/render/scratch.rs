//! Scratch shell overlay.

use crate::agent::AgentRegistry;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, Clear};
use ratatui::Frame;

/// Centered box sized to 60% of `area`, clamped so a tiny window still gets
/// a readable box.
pub fn scratch_shell_rect(area: Rect) -> Rect {
    let w = ((area.width as u32 * 60 / 100) as u16).max(20);
    let h = ((area.height as u32 * 60 / 100) as u16).max(8);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}

/// Render the scratch shell overlay.
pub fn render_scratch_shell(
    frame: &mut Frame,
    pane: &mut crate::layout::Pane,
    registry: &AgentRegistry,
) {
    let oa = scratch_shell_rect(frame.area());
    frame.render_widget(Clear, oa);

    let title = format!(" Scratch Shell [{}] | Esc close ", pane.label());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta))
        .title(Span::styled(
            title,
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(oa);
    frame.render_widget(block, oa);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    pane.drain_output();

    if inner.width != pane.vterm.cols() || inner.height != pane.vterm.rows() {
        pane.vterm.resize(inner.width, inner.height);
        pane.resize_pty(registry, inner.width, inner.height);
    }

    pane.vterm
        .render_to_buffer(frame.buffer_mut(), inner, 0, false);

    let (cursor_line, cursor_col) = pane.vterm.cursor_pos();
    let cx = inner.x + cursor_col;
    let cy = inner.y + cursor_line;
    if cx < inner.x + inner.width && cy < inner.y + inner.height {
        frame.set_cursor_position(ratatui::layout::Position::new(cx, cy));
    }
}
