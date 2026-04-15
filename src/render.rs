//! Rendering: tab bar, status bar, pane tree, and overlay widgets.

use crate::agent::{self, AgentRegistry};
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

fn get_agent_state(registry: &AgentRegistry, name: &str) -> AgentState {
    let reg = agent::lock_registry(registry);
    reg.get(name)
        .and_then(|h| h.core.lock().ok())
        .map(|c| c.state.get_state())
        .unwrap_or(AgentState::Idle)
}

pub fn render(
    frame: &mut Frame,
    layout: &mut Layout,
    repeat_mode: bool,
    registry: &AgentRegistry,
) {
    let chunks = ratatui::layout::Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(frame.area());

    render_pane_tree(frame, chunks[1], layout, repeat_mode, registry);
    render_tab_bar(frame, chunks[0], layout, registry);
    render_status_bar(frame, chunks[2], layout);
}

fn render_tab_bar(frame: &mut Frame, area: Rect, layout: &Layout, registry: &AgentRegistry) {
    let mut spans = Vec::new();

    for (i, tab) in layout.tabs.iter().enumerate() {
        let is_active = i == layout.active;
        let first = tab.root().first_pane();
        let state = if first.backend.is_some() {
            get_agent_state(registry, &first.agent_name)
        } else {
            AgentState::Idle
        };
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

        let has_backend = first.backend.is_some();
        let has_notif = has_notification_in_tree(tab.root());
        let badge = if has_notif && !is_active { " !" } else { "" };

        let label = if has_backend {
            format!(" {} [{}]{badge} ", tab.name, state.display_name())
        } else {
            format!(" {}{badge} ", tab.name)
        };

        spans.push(dot);
        spans.push(Span::styled(label, style));
    }

    spans.push(Span::styled(" [+] ", Style::default().fg(Color::DarkGray)));
    let tabs = Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::DarkGray));
    frame.render_widget(tabs, area);
}

fn has_notification_in_tree(node: &PaneNode) -> bool {
    match node {
        PaneNode::Leaf(p) => p.has_notification,
        PaneNode::Split { first, second, .. } => {
            has_notification_in_tree(first) || has_notification_in_tree(second)
        }
    }
}

fn render_pane_tree(
    frame: &mut Frame,
    area: Rect,
    layout: &mut Layout,
    repeat_mode: bool,
    registry: &AgentRegistry,
) {
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

    if tab.zoomed {
        if let Some(pane) = tab.root_mut().find_pane_mut(focus_id) {
            render_pane(frame, area, pane, true, false, registry);
        }
        return;
    }

    let mut rects = std::collections::HashMap::new();
    render_node(frame, area, tab.root_mut(), focus_id, &mut rects, repeat_mode, registry);
    tab.pane_rects = rects;
}

fn render_node(
    frame: &mut Frame,
    area: Rect,
    node: &mut PaneNode,
    focus_id: usize,
    rects: &mut std::collections::HashMap<usize, (u16, u16, u16, u16)>,
    repeat_mode: bool,
    registry: &AgentRegistry,
) {
    match node {
        PaneNode::Leaf(pane) => {
            rects.insert(pane.id, (area.x, area.y, area.width, area.height));
            let focused = pane.id == focus_id;
            let inner_w = area.width.saturating_sub(2);
            let inner_h = area.height.saturating_sub(2);
            if inner_w > 0 && inner_h > 0
                && (inner_w != pane.vterm.cols() || inner_h != pane.vterm.rows())
            {
                pane.vterm.resize(inner_w, inner_h);
                let reg = agent::lock_registry(registry);
                if let Some(handle) = reg.get(&pane.agent_name) {
                    if let Ok(master) = handle.pty_master.lock() {
                        let _ = master.resize(portable_pty::PtySize {
                            rows: inner_h, cols: inner_w, pixel_width: 0, pixel_height: 0,
                        });
                    }
                }
            }
            render_pane(frame, area, pane, focused, repeat_mode, registry);
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

            render_node(frame, chunks[0], first, focus_id, rects, repeat_mode, registry);
            render_node(frame, chunks[1], second, focus_id, rects, repeat_mode, registry);
        }
    }
}

fn render_pane(
    frame: &mut Frame,
    area: Rect,
    pane: &mut crate::layout::Pane,
    focused: bool,
    repeat_mode: bool,
    registry: &AgentRegistry,
) {
    pane.drain_output();

    if focused {
        pane.has_notification = false;
    }

    let state = if pane.backend.is_some() {
        get_agent_state(registry, &pane.agent_name)
    } else {
        AgentState::Idle
    };
    let sc = state_color(state);

    // Focused pane: always use a bright border so it's clearly visible.
    // Yellow for repeat mode, Green for active agents, Cyan for idle/shell.
    let border_color = if focused && repeat_mode {
        Color::Yellow
    } else if focused {
        match sc {
            Color::DarkGray | Color::White => Color::Cyan, // idle, starting, shell
            _ => sc, // thinking=yellow, tooluse=blue, error=red, etc.
        }
    } else {
        Color::DarkGray
    };

    let title = if pane.backend.is_some() {
        format!(" {} [{}] ", pane.label(), state.display_name())
    } else {
        format!(" {} ", pane.label())
    };

    let title_style = if focused && repeat_mode {
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else if focused {
        Style::default().fg(border_color).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(title, title_style));

    let inner = block.inner(area);
    frame.render_widget(block, area);
    pane.vterm.render_to_buffer(frame.buffer_mut(), inner, pane.scroll_offset, focused);

    // Set terminal cursor at VTerm cursor position for the focused pane.
    // This is needed for IME (input method) to show candidates at the right place.
    if focused && pane.scroll_offset == 0 {
        let (cursor_line, cursor_col) = pane.vterm.cursor_pos();
        let cx = inner.x + cursor_col;
        let cy = inner.y + cursor_line;
        if cx < inner.x + inner.width && cy < inner.y + inner.height {
            frame.set_cursor_position(ratatui::layout::Position::new(cx, cy));
        }
    }
}

fn render_status_bar(frame: &mut Frame, area: Rect, layout: &Layout) {
    let mut spans = Vec::new();

    let mut agent_count = 0;
    let mut total = 0;
    for tab in &layout.tabs {
        total += tab.root().pane_count();
        agent_count += tab.root().agent_count();
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

// --- Overlay renderers ---

pub fn render_menu(frame: &mut Frame, items: &[MenuItem], selected: usize) {
    let area = frame.area();
    let menu_height = (items.len() as u16 + 4).min(area.height - 2);
    let menu_width = 50u16.min(area.width - 4);
    let x = (area.width.saturating_sub(menu_width)) / 2;
    let y = (area.height.saturating_sub(menu_height)) / 2;
    let menu_area = Rect::new(x, y, menu_width, menu_height);
    frame.render_widget(Clear, menu_area);
    let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(" New Tab (Enter to select, Esc to cancel) ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)));
    let inner = block.inner(menu_area);
    frame.render_widget(block, menu_area);
    let lines: Vec<Line> = items.iter().enumerate().map(|(i, item)| {
        let style = if i == selected { Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD) } else { Style::default().fg(Color::White) };
        let prefix = if i == selected { "> " } else { "  " };
        Line::from(Span::styled(format!("{prefix}{}", item.label), style))
    }).collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

pub fn render_rename(frame: &mut Frame, input: &str) {
    let area = frame.area();
    let w = 40u16.min(area.width - 4);
    let x = (area.width.saturating_sub(w)) / 2;
    let y = area.height / 2 - 1;
    let ra = Rect::new(x, y, w, 3);
    frame.render_widget(Clear, ra);
    let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Yellow))
        .title(Span::styled(" Rename (Enter, Esc) ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)));
    let inner = block.inner(ra);
    frame.render_widget(block, ra);
    frame.render_widget(Paragraph::new(format!("{input}_")).style(Style::default().fg(Color::White)), inner);
    let cursor_x = inner.x + input.len() as u16;
    if cursor_x < inner.x + inner.width {
        frame.set_cursor_position(ratatui::layout::Position::new(cursor_x, inner.y));
    }
}

pub fn render_tab_list(frame: &mut Frame, layout: &Layout, selected: usize) {
    let area = frame.area();
    let h = (layout.tabs.len() as u16 + 4).min(area.height - 2);
    let w = 50u16.min(area.width - 4);
    let x = (area.width.saturating_sub(w)) / 2;
    let y = (area.height.saturating_sub(h)) / 2;
    let la = Rect::new(x, y, w, h);
    frame.render_widget(Clear, la);
    let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(" Windows (Enter, Esc) ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)));
    let inner = block.inner(la);
    frame.render_widget(block, la);
    let lines: Vec<Line> = layout.tabs.iter().enumerate().map(|(i, tab)| {
        let is_sel = i == selected;
        let style = if is_sel { Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD) } else { Style::default().fg(Color::White) };
        let marker = if i == layout.active { "*" } else { " " };
        let pc = tab.root().pane_count();
        Line::from(vec![
            Span::styled(format!("{marker} {i}: "), style),
            Span::styled(tab.name.as_str(), style),
            Span::styled(format!("  ({pc} pane{s})", s = if pc > 1 { "s" } else { "" }), Style::default().fg(Color::DarkGray)),
        ])
    }).collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

pub fn render_confirm(frame: &mut Frame, message: &str) {
    let area = frame.area();
    let w = (message.len() as u16 + 4).min(area.width - 4);
    let x = (area.width.saturating_sub(w)) / 2;
    let y = area.height / 2 - 1;
    let ca = Rect::new(x, y, w, 3);
    frame.render_widget(Clear, ca);
    let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Red));
    let inner = block.inner(ca);
    frame.render_widget(block, ca);
    frame.render_widget(
        Paragraph::new(Span::styled(message, Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))),
        inner,
    );
}

pub fn render_help(frame: &mut Frame) {
    let help = vec!["", "  Tab Management", "    Ctrl+B c       New tab", "    Ctrl+B n / p   Next / previous tab",
        "    Ctrl+B l       Last used tab", "    Ctrl+B 0-9     Go to tab N", "    Ctrl+B &       Close tab",
        "    Ctrl+B ,       Rename tab", "    Ctrl+B w       List all tabs", "", "  Pane Management",
        "    Ctrl+B \"       Split horizontal", "    Ctrl+B %       Split vertical", "    Ctrl+B o       Cycle pane focus",
        "    Ctrl+B arrows  Directional focus", "    Ctrl+B x       Close pane", "    Ctrl+B z       Toggle zoom",
        "    Ctrl+B .       Rename pane",
        "", "  Scroll", "    Mouse wheel    Scroll focused pane",
        "    Ctrl+B [       Keyboard scroll mode", "    Shift+drag     Select text (native)",
        "", "  Other", "    Ctrl+B d       Detach (exit)",
        "    Ctrl+B ?       This help", "", "  Press any key to close"];
    let area = frame.area();
    let h = (help.len() as u16 + 2).min(area.height - 2);
    let w = 48u16.min(area.width - 4);
    let x = (area.width.saturating_sub(w)) / 2;
    let y = (area.height.saturating_sub(h)) / 2;
    let ha = Rect::new(x, y, w, h);
    frame.render_widget(Clear, ha);
    let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Yellow))
        .title(Span::styled(" Keybindings ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)));
    let inner = block.inner(ha);
    frame.render_widget(block, ha);
    let lines: Vec<Line> = help.iter().map(|l| Line::from(Span::styled(*l, Style::default().fg(Color::White)))).collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

pub fn render_scroll_indicator(frame: &mut Frame, offset: usize) {
    let area = frame.area();
    let s = format!(" [scroll] line +{offset} | j/k PgUp/PgDn | q exit ");
    let w = s.len() as u16;
    let ba = Rect::new(area.width.saturating_sub(w), 0, w, 1);
    frame.render_widget(Paragraph::new(Span::styled(s, Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD))), ba);
}
