//! Rendering: tab bar, status bar, pane tree, and overlay widgets.

use crate::app::MenuItem;
use crate::layout::{Layout, PaneNode, SplitDir};
use crate::state::AgentState;
use ratatui::layout::{Constraint, Direction, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

fn state_color(state: AgentState) -> Color {
    match state {
        AgentState::Starting => Color::White,
        AgentState::Ready => Color::Green,
        AgentState::Idle => Color::DarkGray,
        AgentState::Thinking => Color::Yellow,
        AgentState::ToolUse => Color::Blue,
        AgentState::PermissionPrompt => Color::Magenta,
        AgentState::ContextFull | AgentState::RateLimit => Color::Indexed(208),
        AgentState::UsageLimit | AgentState::AuthError | AgentState::ApiError => Color::Red,
        AgentState::Hang | AgentState::Crashed | AgentState::Restarting => Color::Red,
    }
}

pub fn render(frame: &mut Frame, layout: &mut Layout, scroll_offset: usize, repeat_mode: bool) {
    let chunks = ratatui::layout::Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(frame.area());

    render_tab_bar(frame, chunks[0], layout);
    render_pane_tree(frame, chunks[1], layout, scroll_offset, repeat_mode);
    render_status_bar(frame, chunks[2], layout);
}

fn render_tab_bar(frame: &mut Frame, area: Rect, layout: &Layout) {
    let mut spans = Vec::new();

    for (i, tab) in layout.tabs.iter().enumerate() {
        let is_active = i == layout.active;
        let state = tab.root().first_pane().state();
        let sc = state_color(state);

        let style = if is_active {
            Style::default().fg(Color::Black).bg(Color::White).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };

        if i > 0 {
            spans.push(Span::raw(" "));
        }

        let blink = matches!(state, AgentState::PermissionPrompt | AgentState::Hang | AgentState::Restarting);
        let dot = if blink {
            Span::styled("*", Style::default().fg(sc).add_modifier(Modifier::SLOW_BLINK))
        } else {
            Span::styled("*", Style::default().fg(sc))
        };

        let has_backend = tab.root().first_pane().backend.is_some();
        let label = if has_backend {
            format!(" {} [{}] ", tab.name, state.display_name())
        } else {
            format!(" {} ", tab.name)
        };

        spans.push(dot);
        spans.push(Span::styled(label, style));
    }

    spans.push(Span::styled(" [+] ", Style::default().fg(Color::DarkGray)));
    let tabs = Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::DarkGray));
    frame.render_widget(tabs, area);
}

fn render_pane_tree(frame: &mut Frame, area: Rect, layout: &mut Layout, scroll_offset: usize, repeat_mode: bool) {
    let tab = match layout.tabs.get_mut(layout.active) {
        Some(t) => t,
        None => {
            let msg = Paragraph::new("No agents. Press Ctrl+B c to create a new tab.")
                .style(Style::default().fg(Color::DarkGray));
            frame.render_widget(msg, area);
            return;
        }
    };

    let focus_id = tab.focus_id;

    // Zoomed: show only the focused pane fullscreen
    if tab.zoomed {
        if let Some(pane) = tab.root_mut().find_pane_mut(focus_id) {
            render_pane(frame, area, pane, true, scroll_offset, false);
        }
        return;
    }

    // Collect pane rects during render for spatial navigation
    let mut rects = std::collections::HashMap::new();
    render_node(frame, area, tab.root_mut(), focus_id, scroll_offset, &mut rects, repeat_mode);
    tab.pane_rects = rects;
}

/// Recursively render the pane tree and collect pane rects.
fn render_node(
    frame: &mut Frame,
    area: Rect,
    node: &mut PaneNode,
    focus_id: usize,
    scroll_offset: usize,
    rects: &mut std::collections::HashMap<usize, (u16, u16, u16, u16)>,
    repeat_mode: bool,
) {
    match node {
        PaneNode::Leaf(pane) => {
            rects.insert(pane.id, (area.x, area.y, area.width, area.height));
            let focused = pane.id == focus_id;
            let so = if focused { scroll_offset } else { 0 };
            let inner_w = area.width.saturating_sub(2);
            let inner_h = area.height.saturating_sub(2);
            if inner_w != pane.vterm.cols() || inner_h != pane.vterm.rows() {
                pane.resize(inner_w, inner_h);
            }
            render_pane(frame, area, pane, focused, so, repeat_mode);
        }
        PaneNode::Split { dir, first, second } => {
            let direction = match dir {
                SplitDir::Horizontal => Direction::Vertical,
                SplitDir::Vertical => Direction::Horizontal,
            };
            let chunks = ratatui::layout::Layout::default()
                .direction(direction)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(area);

            render_node(frame, chunks[0], first, focus_id, scroll_offset, rects, repeat_mode);
            render_node(frame, chunks[1], second, focus_id, scroll_offset, rects, repeat_mode);
        }
    }
}

fn render_pane(
    frame: &mut Frame,
    area: Rect,
    pane: &mut crate::layout::Pane,
    focused: bool,
    scroll_offset: usize,
    repeat_mode: bool,
) {
    pane.drain_output();

    let state = pane.state();
    let sc = state_color(state);

    // In repeat mode, focused pane border turns yellow to indicate active navigation
    let border_color = if focused && repeat_mode {
        Color::Yellow
    } else if focused {
        sc
    } else {
        Color::DarkGray
    };

    let border_style = Style::default().fg(border_color);

    let title = if pane.backend.is_some() {
        format!(" {} [{}] ", pane.agent_name, state.display_name())
    } else {
        format!(" {} ", pane.agent_name)
    };

    let title_style = if focused && repeat_mode {
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else if focused {
        Style::default().fg(sc).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(Span::styled(title, title_style));

    let inner = block.inner(area);
    frame.render_widget(block, area);
    pane.vterm.render_to_buffer(frame.buffer_mut(), inner, scroll_offset);
}

fn render_status_bar(frame: &mut Frame, area: Rect, layout: &Layout) {
    let mut spans = Vec::new();

    let mut agent_count = 0;
    let mut total = 0;
    for tab in &layout.tabs {
        let root = tab.root();
        total += root.pane_count();
        agent_count += root.agent_count();
    }

    if agent_count > 0 {
        spans.push(Span::styled(format!(" {agent_count} agent(s) "), Style::default().fg(Color::Cyan)));
    }
    if total > agent_count {
        spans.push(Span::styled(format!(" {total} pane(s) "), Style::default().fg(Color::White)));
    }

    spans.push(Span::styled(
        " | Ctrl+B c new | n/p switch | d detach | ? help ",
        Style::default().fg(Color::DarkGray),
    ));

    let bar = Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::DarkGray));
    frame.render_widget(bar, area);
}

pub fn render_menu(frame: &mut Frame, items: &[MenuItem], selected: usize) {
    let area = frame.area();
    let menu_height = (items.len() as u16 + 4).min(area.height - 2);
    let menu_width = 50u16.min(area.width - 4);
    let x = (area.width.saturating_sub(menu_width)) / 2;
    let y = (area.height.saturating_sub(menu_height)) / 2;
    let menu_area = Rect::new(x, y, menu_width, menu_height);

    frame.render_widget(Clear, menu_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            " New Tab (Enter to select, Esc to cancel) ",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(menu_area);
    frame.render_widget(block, menu_area);

    let lines: Vec<Line> = items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let style = if i == selected {
                Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
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
    let w = 40u16.min(area.width - 4);
    let x = (area.width.saturating_sub(w)) / 2;
    let y = area.height / 2 - 1;
    let rename_area = Rect::new(x, y, w, 3);

    frame.render_widget(Clear, rename_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(Span::styled(
            " Rename tab (Enter to confirm, Esc to cancel) ",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rename_area);
    frame.render_widget(block, rename_area);

    let text = Paragraph::new(format!("{input}_")).style(Style::default().fg(Color::White));
    frame.render_widget(text, inner);
}

pub fn render_tab_list(frame: &mut Frame, layout: &Layout, selected: usize) {
    let area = frame.area();
    let count = layout.tabs.len();
    let h = (count as u16 + 4).min(area.height - 2);
    let w = 50u16.min(area.width - 4);
    let x = (area.width.saturating_sub(w)) / 2;
    let y = (area.height.saturating_sub(h)) / 2;
    let list_area = Rect::new(x, y, w, h);

    frame.render_widget(Clear, list_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            " Windows (Enter to switch, Esc to cancel) ",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(list_area);
    frame.render_widget(block, list_area);

    let lines: Vec<Line> = layout
        .tabs
        .iter()
        .enumerate()
        .map(|(i, tab)| {
            let state = tab.root().first_pane().state();
            let sc = state_color(state);
            let is_sel = i == selected;
            let is_active = i == layout.active;

            let style = if is_sel {
                Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };

            let marker = if is_active { "*" } else { " " };
            let has_backend = tab.root().first_pane().backend.is_some();
            let state_label = if has_backend {
                format!(" [{}]", state.display_name())
            } else {
                String::new()
            };

            let pane_count = tab.root().pane_count();
            Line::from(vec![
                Span::styled(format!("{marker} {i}: "), style),
                Span::styled(tab.name.as_str(), style),
                Span::styled(state_label, Style::default().fg(sc)),
                Span::styled(
                    format!("  ({pane_count} pane{})", if pane_count > 1 { "s" } else { "" }),
                    Style::default().fg(Color::DarkGray),
                ),
            ])
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
}

pub fn render_help(frame: &mut Frame) {
    let help_lines = vec![
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
        "    Ctrl+B arrows  Directional pane focus",
        "    Ctrl+B x       Close pane",
        "    Ctrl+B z       Toggle zoom",
        "",
        "  Other",
        "    Ctrl+B [       Scroll mode (q to exit)",
        "    Ctrl+B d       Detach (exit)",
        "    Ctrl+B ?       This help",
        "",
        "  Press any key to close",
    ];

    let area = frame.area();
    let h = (help_lines.len() as u16 + 2).min(area.height - 2);
    let w = 48u16.min(area.width - 4);
    let x = (area.width.saturating_sub(w)) / 2;
    let y = (area.height.saturating_sub(h)) / 2;
    let help_area = Rect::new(x, y, w, h);

    frame.render_widget(Clear, help_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(Span::styled(
            " Keybindings ",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(help_area);
    frame.render_widget(block, help_area);

    let lines: Vec<Line> = help_lines
        .iter()
        .map(|l| Line::from(Span::styled(*l, Style::default().fg(Color::White))))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

pub fn render_scroll_indicator(frame: &mut Frame, offset: usize) {
    let area = frame.area();
    let indicator = format!(" [scroll] line +{offset} | j/k PgUp/PgDn | q exit ");
    let w = indicator.len() as u16;
    let x = area.width.saturating_sub(w);
    let bar_area = Rect::new(x, 0, w, 1);

    let text = Paragraph::new(Span::styled(
        indicator,
        Style::default()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(text, bar_area);
}
