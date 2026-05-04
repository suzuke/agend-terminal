//! Overlay widgets: menu, rename, tab list, confirm, help, command palette.

use crate::app::MenuItem;
use crate::layout::Layout;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

/// Clamp a desired overlay dimension by the available space minus padding.
pub(super) fn clamp_overlay_dim(desired: u16, available: u16, pad: u16) -> u16 {
    desired.min(available.saturating_sub(pad))
}

/// Centred coordinate for an overlay of `overlay_dim` within `area_dim`.
pub(super) fn center_overlay(area_dim: u16, overlay_dim: u16) -> u16 {
    area_dim.saturating_sub(overlay_dim) / 2
}

/// Compute a centred overlay `Rect` with saturating arithmetic.
pub fn centered_overlay_rect(
    area: Rect,
    content_h: u16,
    content_w: u16,
    h_pad: u16,
    w_pad: u16,
) -> Rect {
    let height = clamp_overlay_dim(content_h, area.height, h_pad);
    let width = clamp_overlay_dim(content_w, area.width, w_pad);
    let x = center_overlay(area.width, width);
    let y = center_overlay(area.height, height);
    Rect::new(x, y, width, height)
}

/// Render a centered overlay frame with border and title. Returns the inner area.
pub(super) fn render_overlay_frame(frame: &mut Frame, color: Color, title: &str) -> Rect {
    let area = frame.area();
    let h = area.height.saturating_sub(4).max(10);
    let w = area.width.saturating_sub(6).max(40);
    let x = (area.width.saturating_sub(w)) / 2;
    let y = (area.height.saturating_sub(h)) / 2;
    let oa = Rect::new(x, y, w, h);
    frame.render_widget(Clear, oa);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color))
        .title(Span::styled(
            title,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(oa);
    frame.render_widget(block, oa);
    inner
}

pub fn render_menu(frame: &mut Frame, items: &[MenuItem], selected: usize) {
    let area = frame.area();
    let item_count = u16::try_from(items.len()).unwrap_or(u16::MAX);
    let menu_area = centered_overlay_rect(area, item_count.saturating_add(4), 50, 2, 4);
    frame.render_widget(Clear, menu_area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            " New Tab (Enter to select, Esc to cancel) ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(menu_area);
    frame.render_widget(block, menu_area);
    let lines: Vec<Line> = items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let style = if i == selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let prefix = if i == selected { "> " } else { "  " };
            Line::from(Span::styled(format!("{prefix}{}", item.label), style))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

pub fn render_rename(frame: &mut Frame, input: &str) {
    let area = frame.area();
    let w = clamp_overlay_dim(40, area.width, 4);
    let x = center_overlay(area.width, w);
    let y = (area.height / 2).saturating_sub(1);
    let ra = Rect::new(x, y, w, 3);
    frame.render_widget(Clear, ra);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(Span::styled(
            " Rename (Enter, Esc) ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(ra);
    frame.render_widget(block, ra);
    frame.render_widget(
        Paragraph::new(input.to_string()).style(Style::default().fg(Color::White)),
        inner,
    );
    let cursor_x = inner.x + input.width() as u16;
    if cursor_x < inner.x + inner.width {
        frame.set_cursor_position(ratatui::layout::Position::new(cursor_x, inner.y));
    }
}

pub fn render_tab_list(frame: &mut Frame, layout: &Layout, selected: usize) {
    let area = frame.area();
    let content_h = (layout.tabs.len() as u16).saturating_add(4);
    let la = centered_overlay_rect(area, content_h, 50, 2, 4);
    frame.render_widget(Clear, la);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            " Windows (Enter, Esc) ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(la);
    frame.render_widget(block, la);
    let lines: Vec<Line> = layout
        .tabs
        .iter()
        .enumerate()
        .map(|(i, tab)| {
            let is_sel = i == selected;
            let style = if is_sel {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let marker = if i == layout.active { "*" } else { " " };
            let pc = tab.root().pane_count();
            Line::from(vec![
                Span::styled(format!("{marker} {i}: "), style),
                Span::styled(tab.name.as_str(), style),
                Span::styled(
                    format!("  ({pc} pane{s})", s = if pc > 1 { "s" } else { "" }),
                    Style::default().fg(Color::DarkGray),
                ),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

pub fn render_move_pane_target(
    frame: &mut Frame,
    layout: &Layout,
    selected: usize,
    source_tab_idx: usize,
    split_dir: crate::layout::SplitDir,
) {
    let area = frame.area();
    let list_len = (layout.tabs.len() as u16).saturating_add(1);
    let content_h = list_len.saturating_add(4);
    let la = centered_overlay_rect(area, content_h, 54, 2, 4);
    frame.render_widget(Clear, la);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta))
        .title(Span::styled(
            format!(" Move pane to... (Split: {:?}) (Tab to toggle) ", split_dir),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(la);
    frame.render_widget(block, la);

    let mut lines: Vec<Line> = Vec::with_capacity(list_len.into());
    for (i, tab) in layout.tabs.iter().enumerate() {
        let is_sel = i == selected;
        let is_source = i == source_tab_idx;
        let style = if is_sel {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Magenta)
                .add_modifier(Modifier::BOLD)
        } else if is_source {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::White)
        };
        let marker = if is_source { "(source)" } else { "" };
        let pc = tab.root().pane_count();
        lines.push(Line::from(vec![
            Span::styled(format!(" {i}: "), style),
            Span::styled(tab.name.as_str(), style),
            Span::styled(
                format!(
                    "  ({pc} pane{s}) {marker}",
                    s = if pc > 1 { "s" } else { "" }
                ),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }
    let new_sel = selected == layout.tabs.len();
    let style = if new_sel {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Magenta)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Green)
    };
    lines.push(Line::from(vec![Span::styled(" [+] New tab", style)]));

    frame.render_widget(Paragraph::new(lines), inner);
}

pub fn render_confirm(frame: &mut Frame, message: &str) {
    let area = frame.area();
    let content_w = u16::try_from(message.len())
        .unwrap_or(u16::MAX)
        .saturating_add(4);
    let w = clamp_overlay_dim(content_w, area.width, 4);
    let x = center_overlay(area.width, w);
    let y = (area.height / 2).saturating_sub(1);
    let ca = Rect::new(x, y, w, 3);
    frame.render_widget(Clear, ca);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Red));
    let inner = block.inner(ca);
    frame.render_widget(block, ca);
    frame.render_widget(
        Paragraph::new(Span::styled(
            message,
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )),
        inner,
    );
}

pub fn render_help(frame: &mut Frame) {
    let help = vec![
        "",
        "  Tab Management",
        "    Ctrl+B c       New tab",
        "    Ctrl+B n / p   Next / previous tab",
        "    Ctrl+B l       Last used tab",
        "    Ctrl+B 0-9     Go to tab N",
        "    Ctrl+B &       Close tab",
        "    Ctrl+B ,       Rename tab",
        "    Ctrl+B w       List all tabs",
        "",
        "  Pane Management",
        "    Ctrl+B \"       Split horizontal",
        "    Ctrl+B %       Split vertical",
        "    Ctrl+B o       Cycle pane focus",
        "    Ctrl+B arrows  Directional focus",
        "    Ctrl+B A-arrow Resize pane (Alt-arrow)",
        "    Ctrl+B H/J/K/L Resize pane (portable)",
        "    Drag border    Resize pane",
        "    Drag title     Swap pane position",
        "    Drag → tab bar Move pane across tabs (drop on tab or [+])",
        "    Ctrl+B x       Close pane",
        "    Ctrl+B z       Toggle zoom",
        "    Ctrl+B Space   Next layout preset",
        "    Ctrl+B .       Rename pane",
        "    Ctrl+B !       Move pane to another tab (menu)",
        "",
        "  Scroll",
        "    Mouse wheel    Scroll focused pane",
        "    Ctrl+B [       Keyboard scroll mode",
        "    Shift+drag     Select text (native)",
        "",
        "  Panels & Commands",
        "    Ctrl+B :       Command palette",
        "      :spawn <n> [backend]  New tab",
        "      :vsplit <n> [backend] V-split",
        "      :hsplit <n> [backend] H-split",
        "      :layout [name]        Arrange panes",
        "      :kill <name>          Kill agent",
        "      :restart [name]       Restart agent",
        "      :send <to> <msg>      Send message",
        "      :broadcast <msg>      Broadcast",
        "      :status               Log agent states",
        "    Ctrl+B D       Decisions panel",
        "    Ctrl+B T       Task board",
        "",
        "  Other",
        "    Ctrl+J         Newline (no submit, works everywhere)",
        "    Shift+Enter    Newline (requires modern terminal)",
        "    Ctrl+B Ctrl+B  Send Ctrl+B to pane",
        "    Ctrl+B ~       Scratch shell (Esc to close)",
        "    Ctrl+B d       Detach (exit)",
        "    Ctrl+B ?       This help",
        "",
        "  Press any key to close",
    ];
    let area = frame.area();
    let h = (help.len() as u16 + 2).min(area.height.saturating_sub(2));
    let content_w = help.iter().map(|l| l.len() as u16).max().unwrap_or(48) + 2;
    let w = content_w.min(area.width.saturating_sub(4));
    let x = (area.width.saturating_sub(w)) / 2;
    let y = (area.height.saturating_sub(h)) / 2;
    let ha = Rect::new(x, y, w, h);
    frame.render_widget(Clear, ha);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(Span::styled(
            " Keybindings ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(ha);
    frame.render_widget(block, ha);
    let lines: Vec<Line> = help
        .iter()
        .map(|l| Line::from(Span::styled(*l, Style::default().fg(Color::White))))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

pub fn render_scroll_indicator(frame: &mut Frame, offset: usize) {
    let area = frame.area();
    let s = format!(" [scroll] line +{offset} | j/k PgUp/PgDn | q exit ");
    let w = s.len() as u16;
    let ba = Rect::new(area.width.saturating_sub(w), 0, w, 1);
    frame.render_widget(
        Paragraph::new(Span::styled(
            s,
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        ba,
    );
}

pub fn render_command_palette(frame: &mut Frame, input: &str) {
    let area = frame.area();
    let w = 60u16.min(area.width.saturating_sub(4));
    let x = (area.width.saturating_sub(w)) / 2;
    let y = area.height / 3;
    let ra = Rect::new(x, y, w, 3);
    frame.render_widget(Clear, ra);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            " : Command (Enter, Esc) ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(ra);
    frame.render_widget(block, ra);
    frame.render_widget(
        Paragraph::new(format!(":{input}")).style(Style::default().fg(Color::White)),
        inner,
    );
    let cursor_x = inner.x + 1 + input.width() as u16;
    if cursor_x < inner.x + inner.width {
        frame.set_cursor_position(ratatui::layout::Position::new(cursor_x, inner.y));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn clamp_overlay_dim_saturates_on_zero_area() {
        assert_eq!(clamp_overlay_dim(40, 0, 4), 0);
        assert_eq!(clamp_overlay_dim(40, 1, 4), 0);
        assert_eq!(clamp_overlay_dim(40, 4, 4), 0);
        assert_eq!(clamp_overlay_dim(40, 5, 4), 1);
    }

    #[test]
    fn clamp_overlay_dim_normal_case_returns_min_of_desired_and_available_minus_pad() {
        assert_eq!(clamp_overlay_dim(40, 200, 4), 40);
        assert_eq!(clamp_overlay_dim(200, 50, 4), 46);
        assert_eq!(clamp_overlay_dim(46, 50, 4), 46);
    }

    #[test]
    fn center_overlay_saturates_when_overlay_exceeds_area() {
        assert_eq!(center_overlay(0, 10), 0);
        assert_eq!(center_overlay(20, 50), 0);
        assert_eq!(center_overlay(100, 50), 25);
    }

    #[test]
    fn centered_overlay_rect_tiny_terminal_does_not_panic() {
        let r0 = centered_overlay_rect(Rect::new(0, 0, 0, 0), 10, 50, 2, 4);
        assert_eq!((r0.width, r0.height), (0, 0));

        let r1 = centered_overlay_rect(Rect::new(0, 0, 1, 1), 10, 50, 2, 4);
        assert_eq!((r1.width, r1.height), (0, 0));

        let r2 = centered_overlay_rect(Rect::new(0, 0, 4, 2), 10, 50, 2, 4);
        assert_eq!((r2.width, r2.height), (0, 0));
    }

    #[test]
    fn centered_overlay_rect_centers_within_area() {
        let r = centered_overlay_rect(Rect::new(0, 0, 100, 40), 10, 50, 2, 4);
        assert_eq!((r.x, r.y, r.width, r.height), (25, 15, 50, 10));
    }
}
