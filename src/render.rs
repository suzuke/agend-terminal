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
use unicode_width::UnicodeWidthStr;

/// Telegram connection status for status bar display.
#[derive(Clone, Copy)]
pub enum TelegramStatus {
    /// No Telegram channel config in fleet.yaml.
    NotConfigured,
    /// Configured but token env var is missing.
    NoToken,
    /// Configured and token present (polling should be active).
    Connected,
}

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
    telegram: TelegramStatus,
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
    render_status_bar(frame, chunks[2], layout, telegram);
}

/// Get the highest-priority state across all panes in a tab.
fn highest_priority_state(tab: &crate::layout::Tab, registry: &AgentRegistry) -> AgentState {
    let mut best = AgentState::Idle;
    for id in tab.root().pane_ids() {
        if let Some(pane) = tab.root().find_pane(id) {
            if pane.backend.is_some() {
                let s = get_agent_state(registry, &pane.agent_name);
                if s.priority() > best.priority() {
                    best = s;
                }
            }
        }
    }
    best
}

/// SYNC: layout math (label width, spacing) must match tab_bar_hit_test() in app.rs.
fn render_tab_bar(frame: &mut Frame, area: Rect, layout: &Layout, registry: &AgentRegistry) {
    let mut spans = Vec::new();

    for (i, tab) in layout.tabs.iter().enumerate() {
        let is_active = i == layout.active;
        // Show the highest-priority state across all panes in this tab
        let state = highest_priority_state(tab, registry);
        let sc = state_color(state);

        let style = if is_active {
            Style::default()
                .fg(Color::Black)
                .bg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };

        if i > 0 {
            spans.push(Span::raw(" "));
        }

        let blink = matches!(
            state,
            AgentState::PermissionPrompt | AgentState::Hang | AgentState::Restarting
        );
        let dot = if blink {
            Span::styled(
                "*",
                Style::default().fg(sc).add_modifier(Modifier::SLOW_BLINK),
            )
        } else {
            Span::styled("*", Style::default().fg(sc))
        };

        let has_notif = tab.root().has_notification();
        let badge = if has_notif && !is_active { " !" } else { "" };
        let label = format!(" {}{badge} ", tab.name);

        spans.push(dot);
        spans.push(Span::styled(label, style));
    }

    spans.push(Span::styled(" [+] ", Style::default().fg(Color::DarkGray)));
    let tabs = Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::DarkGray));
    frame.render_widget(tabs, area);
}

fn split_chunks(area: Rect, dir: &SplitDir) -> [Rect; 2] {
    let direction = match dir {
        SplitDir::Horizontal => Direction::Vertical,
        SplitDir::Vertical => Direction::Horizontal,
    };
    let chunks = ratatui::layout::Layout::default()
        .direction(direction)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    [chunks[0], chunks[1]]
}

/// Resize pass — sync VTerm + PTY sizes to match render layout.
/// Call this after terminal resize, split/close, zoom toggle, or tab switch.
pub fn resize_panes(pane_area: Rect, layout: &mut Layout, registry: &AgentRegistry) {
    let tab = match layout.tabs.get_mut(layout.active) {
        Some(t) => t,
        None => return,
    };
    let mut resizes = Vec::new();
    if tab.zoomed {
        let focus_id = tab.focus_id;
        if let Some(pane) = tab.root_mut().find_pane_mut(focus_id) {
            let w = pane_area.width.saturating_sub(2);
            let h = pane_area.height.saturating_sub(2);
            if w > 0 && h > 0 && (w != pane.vterm.cols() || h != pane.vterm.rows()) {
                pane.vterm.resize(w, h);
                resizes.push((pane.agent_name.clone(), w, h));
            }
        }
    } else {
        let mut rects = std::mem::take(&mut tab.pane_rects);
        rects.clear();
        collect_resize_needs(pane_area, tab.root_mut(), &mut rects, &mut resizes);
        tab.pane_rects = rects;
    }
    apply_pty_resizes(&resizes, registry);
}

/// Collect (pane_name, width, height) for all panes that need resizing.
fn collect_resize_needs(
    area: Rect,
    node: &mut PaneNode,
    rects: &mut std::collections::HashMap<usize, (u16, u16, u16, u16)>,
    resizes: &mut Vec<(String, u16, u16)>,
) {
    match node {
        PaneNode::Leaf(pane) => {
            rects.insert(pane.id, (area.x, area.y, area.width, area.height));
            let w = area.width.saturating_sub(2);
            let h = area.height.saturating_sub(2);
            if w > 0 && h > 0 && (w != pane.vterm.cols() || h != pane.vterm.rows()) {
                pane.vterm.resize(w, h);
                resizes.push((pane.agent_name.clone(), w, h));
            }
        }
        PaneNode::Split { dir, first, second } => {
            let [c0, c1] = split_chunks(area, dir);
            collect_resize_needs(c0, first, rects, resizes);
            collect_resize_needs(c1, second, rects, resizes);
        }
    }
}

/// Apply PTY resizes with a single registry lock.
fn apply_pty_resizes(resizes: &[(String, u16, u16)], registry: &AgentRegistry) {
    if resizes.is_empty() {
        return;
    }
    let reg = agent::lock_registry(registry);
    for (name, cols, rows) in resizes {
        if let Some(handle) = reg.get(name.as_str()) {
            if let Ok(master) = handle.pty_master.lock() {
                let _ = master.resize(portable_pty::PtySize {
                    rows: *rows,
                    cols: *cols,
                    pixel_width: 0,
                    pixel_height: 0,
                });
            }
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
        tab.pane_rects.clear();
        tab.pane_rects
            .insert(focus_id, (area.x, area.y, area.width, area.height));
        return;
    }

    let mut rects = std::collections::HashMap::new();
    render_node(
        frame,
        area,
        tab.root_mut(),
        focus_id,
        &mut rects,
        repeat_mode,
        registry,
    );
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
            render_pane(frame, area, pane, focused, repeat_mode, registry);
        }
        PaneNode::Split { dir, first, second } => {
            let [c0, c1] = split_chunks(area, dir);
            render_node(frame, c0, first, focus_id, rects, repeat_mode, registry);
            render_node(frame, c1, second, focus_id, rects, repeat_mode, registry);
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
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else if focused {
        Style::default()
            .fg(border_color)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(title, title_style));

    let inner = block.inner(area);
    frame.render_widget(block, area);
    pane.vterm
        .render_to_buffer(frame.buffer_mut(), inner, pane.scroll_offset, !focused);

    // Highlight text selection
    if let Some(ref sel) = pane.selection {
        let (s, e) = if sel.start <= sel.end {
            (sel.start, sel.end)
        } else {
            (sel.end, sel.start)
        };
        for row in s.0..=e.0 {
            let col_start = if row == s.0 { s.1 } else { 0 };
            let col_end = if row == e.0 {
                e.1
            } else {
                inner.width.saturating_sub(1)
            };
            for col in col_start..=col_end {
                let x = inner.x + col;
                let y = inner.y + row;
                if x < inner.x + inner.width && y < inner.y + inner.height {
                    let cell = &mut frame.buffer_mut()[(x, y)];
                    let style = cell.style().add_modifier(Modifier::REVERSED);
                    cell.set_style(style);
                }
            }
        }
    }

    if focused && pane.scroll_offset == 0 {
        let (cursor_line, cursor_col) = pane.vterm.cursor_pos();
        let cx = inner.x + cursor_col;
        let cy = inner.y + cursor_line;
        if cx < inner.x + inner.width && cy < inner.y + inner.height {
            frame.set_cursor_position(ratatui::layout::Position::new(cx, cy));
        }
    }
}

fn render_status_bar(frame: &mut Frame, area: Rect, layout: &Layout, telegram: TelegramStatus) {
    let mut spans = Vec::new();

    let mut agent_count = 0;
    let mut total = 0;
    for tab in &layout.tabs {
        total += tab.root().pane_count();
        agent_count += tab.root().agent_count();
    }

    if agent_count > 0 {
        spans.push(Span::styled(
            format!(" {agent_count} agent(s) "),
            Style::default().fg(Color::Cyan),
        ));
    }
    if total > agent_count {
        spans.push(Span::styled(
            format!(" {total} pane(s) "),
            Style::default().fg(Color::White),
        ));
    }

    if let Some(tab) = layout.active_tab() {
        if let Some(preset) = tab.last_layout {
            spans.push(Span::styled(
                format!(" [{}] ", preset.name()),
                Style::default().fg(Color::Yellow),
            ));
        }
    }

    match telegram {
        TelegramStatus::Connected => {
            spans.push(Span::styled(
                " TG ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        TelegramStatus::NoToken => {
            spans.push(Span::styled(
                " TG(no token) ",
                Style::default().fg(Color::Yellow),
            ));
        }
        TelegramStatus::NotConfigured => {}
    }

    spans.push(Span::styled(
        " | Ctrl+B c new | : cmd | n/p switch | d detach | ? help ",
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
    let w = 40u16.min(area.width - 4);
    let x = (area.width.saturating_sub(w)) / 2;
    let y = area.height / 2 - 1;
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
        Paragraph::new(format!("{input}_")).style(Style::default().fg(Color::White)),
        inner,
    );
    let cursor_x = inner.x + input.width() as u16;
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

pub fn render_confirm(frame: &mut Frame, message: &str) {
    let area = frame.area();
    let w = (message.len() as u16 + 4).min(area.width - 4);
    let x = (area.width.saturating_sub(w)) / 2;
    let y = area.height / 2 - 1;
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
        "    Ctrl+B x       Close pane",
        "    Ctrl+B z       Toggle zoom",
        "    Ctrl+B Space   Next layout preset",
        "    Ctrl+B .       Rename pane",
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
        "    Ctrl+B D       Decisions panel",
        "    Ctrl+B T       Task board",
        "",
        "  Other",
        "    Ctrl+B Ctrl+B  Send Ctrl+B to pane",
        "    Ctrl+B d       Detach (exit)",
        "    Ctrl+B ?       This help",
        "",
        "  Press any key to close",
    ];
    let area = frame.area();
    let h = (help.len() as u16 + 2).min(area.height.saturating_sub(2));
    let w = 48u16.min(area.width.saturating_sub(4));
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

/// Render a centered overlay frame with border and title. Returns the inner area.
fn render_overlay_frame(frame: &mut Frame, color: Color, title: &str) -> Rect {
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
        Paragraph::new(format!(":{input}_")).style(Style::default().fg(Color::White)),
        inner,
    );
    let cursor_x = inner.x + 1 + input.width() as u16;
    if cursor_x < inner.x + inner.width {
        frame.set_cursor_position(ratatui::layout::Position::new(cursor_x, inner.y));
    }
}

pub fn render_decisions(frame: &mut Frame, items: &[crate::decisions::Decision], scroll: usize) {
    let count = items.len();
    let title = format!(" Decisions ({count}) | j/k scroll | q close ");
    let inner = render_overlay_frame(frame, Color::Yellow, &title);

    if items.is_empty() {
        frame.render_widget(
            Paragraph::new("  No decisions yet.").style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    for (i, d) in items.iter().enumerate() {
        let marker = if i == scroll { "> " } else { "  " };
        let style = if i == scroll {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(Span::styled(
            format!("{marker}[{}] {}", d.scope, d.title),
            style,
        )));
        if i == scroll {
            // Show detail for selected decision
            lines.push(Line::from(Span::styled(
                format!(
                    "    by {} | {}",
                    d.author,
                    &d.created_at.get(..10).unwrap_or(&d.created_at)
                ),
                Style::default().fg(Color::DarkGray),
            )));
            for line in d.content.lines() {
                lines.push(Line::from(Span::styled(
                    format!("    {line}"),
                    Style::default().fg(Color::Gray),
                )));
            }
            if !d.tags.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("    tags: {}", d.tags.join(", ")),
                    Style::default().fg(Color::Cyan),
                )));
            }
            lines.push(Line::from(""));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

pub fn render_tasks(frame: &mut Frame, items: &[crate::tasks::Task], scroll: usize) {
    let count = items.len();
    let title = format!(" Tasks ({count}) | j/k scroll | q close ");
    let inner = render_overlay_frame(frame, Color::Blue, &title);

    if items.is_empty() {
        frame.render_widget(
            Paragraph::new("  No tasks yet.").style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    for (i, t) in items.iter().enumerate() {
        let marker = if i == scroll { "> " } else { "  " };
        let status_color = match t.status.as_str() {
            "open" => Color::Green,
            "claimed" => Color::Yellow,
            "done" => Color::DarkGray,
            "blocked" => Color::Red,
            _ => Color::White,
        };
        let pri_color = match t.priority.as_str() {
            "urgent" => Color::Red,
            "high" => Color::Yellow,
            _ => Color::White,
        };
        let style = if i == scroll {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Blue)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let assignee = t.assignee.as_deref().unwrap_or("-");
        lines.push(Line::from(vec![
            Span::styled(marker, style),
            Span::styled(
                format!("[{}] ", t.status),
                Style::default().fg(status_color),
            ),
            Span::styled(
                &t.title,
                if i == scroll {
                    style
                } else {
                    Style::default().fg(Color::White)
                },
            ),
            Span::styled(
                format!("  ({}) @{assignee}", t.priority),
                Style::default().fg(pri_color),
            ),
        ]));
        if i == scroll {
            if !t.description.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("    {}", t.description),
                    Style::default().fg(Color::Gray),
                )));
            }
            if let Some(ref result) = t.result {
                lines.push(Line::from(Span::styled(
                    format!("    result: {result}"),
                    Style::default().fg(Color::Green),
                )));
            }
            lines.push(Line::from(Span::styled(
                format!(
                    "    by {} | {}",
                    t.created_by,
                    &t.created_at.get(..10).unwrap_or(&t.created_at)
                ),
                Style::default().fg(Color::DarkGray),
            )));
            lines.push(Line::from(""));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}
