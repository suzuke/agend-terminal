//! Core rendering: main entry point, tab bar, status bar, pane tree.

use super::border::{render_border_grid, render_pane_titles};
use crate::agent::{self, AgentRegistry};
use crate::channel::TelegramStatus;
use crate::layout::{DragTabTarget, Layout, PaneNode};
use crate::state::AgentState;
use ratatui::layout::{Alignment, Constraint, Direction, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

/// Sprint 20.5 Track 7 transient state badge (per dev-reviewer
/// cross-validation B↔C peer-pass): for tab-bar agents in transient
/// lifecycle states (Restarting / Crashed), surface a short text badge
/// alongside the colour dot so the registry-vs-process divergence window
/// is visually announced rather than silent.
///
/// Returns `None` for terminal / steady states — most ticks emit no badge,
/// keeping the tab bar uncluttered.
pub(super) fn transient_state_badge(state: AgentState) -> Option<&'static str> {
    match state {
        AgentState::Restarting => Some(" [respawning]"),
        AgentState::Crashed => Some(" [crashed]"),
        _ => None,
    }
}

pub fn state_color(state: AgentState) -> Color {
    match state {
        AgentState::Starting => Color::White,
        AgentState::AwaitingOperator => Color::Indexed(214),
        AgentState::Ready => Color::Green,
        AgentState::Idle => Color::DarkGray,
        AgentState::Thinking => Color::Yellow,
        AgentState::ToolUse => Color::Blue,
        AgentState::InteractivePrompt => Color::Indexed(214),
        AgentState::PermissionPrompt => Color::Magenta,
        AgentState::ContextFull | AgentState::RateLimit => Color::Indexed(208),
        AgentState::UsageLimit | AgentState::AuthError | AgentState::ApiError => Color::Red,
        AgentState::Hang | AgentState::Crashed | AgentState::Restarting => Color::Red,
    }
}

pub(super) fn get_agent_state(registry: &AgentRegistry, name: &str) -> AgentState {
    let reg = agent::lock_registry(registry);
    reg.get(name)
        .map(|h| h.core.lock().state.get_state())
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
            Constraint::Length(crate::layout::TAB_BAR_HEIGHT),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(frame.area());

    render_pane_tree(frame, chunks[1], layout, repeat_mode, registry);
    render_tab_bar(frame, chunks[0], layout, registry);
    render_status_bar(frame, chunks[2], layout, telegram);
}

/// Get the highest-priority state across all panes in a tab.
pub fn highest_priority_state(tab: &crate::layout::Tab, registry: &AgentRegistry) -> AgentState {
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

    let drag_tab_target = layout
        .active_tab()
        .and_then(|t| t.dragging_pane.and(t.drag_target_tab));

    for (i, tab) in layout.tabs.iter().enumerate() {
        let is_active = i == layout.active;
        let is_drag_drop =
            matches!(drag_tab_target, Some(DragTabTarget::ExistingTab(idx)) if idx == i);
        let is_reorder_target = layout
            .tab_reorder_target
            .is_some_and(|t| t == i && layout.tab_reorder_source.is_some_and(|s| s != i));
        let state = highest_priority_state(tab, registry);
        let sc = state_color(state);

        let style = if is_drag_drop || is_reorder_target {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Magenta)
                .add_modifier(Modifier::BOLD)
        } else if is_active {
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
            AgentState::PermissionPrompt
                | AgentState::InteractivePrompt
                | AgentState::Hang
                | AgentState::Restarting
                | AgentState::AwaitingOperator
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
        let notif_badge = if has_notif && !is_active { " !" } else { "" };
        let label = format!(" {}{notif_badge} ", tab.name);

        spans.push(dot);
        spans.push(Span::styled(label, style));

        if let Some(b) = transient_state_badge(state) {
            spans.push(Span::styled(b, Style::default().fg(Color::Yellow)));
        }
    }

    let new_tab_style = if matches!(drag_tab_target, Some(DragTabTarget::NewTab)) {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Magenta)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    spans.push(Span::styled(" [+] ", new_tab_style));
    let tabs = Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::DarkGray));
    frame.render_widget(tabs, area);
}

// split_chunks moved to layout/split.rs (Sprint 48 PR 1 — cross-dep resolution).

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
            let info = render_pane(frame, area, pane, true, false, registry, false, false);
            let infos = vec![info];
            render_border_grid(frame, &infos);
            render_pane_titles(frame, &infos);
        }
        tab.pane_rects.clear();
        tab.pane_rects
            .insert(focus_id, (area.x, area.y, area.width, area.height));
        return;
    }

    let drag_source = tab.dragging_pane;
    let drag_target = tab.drag_target;
    let mut rects = std::collections::HashMap::new();
    let mut border_infos: Vec<PaneBorderInfo> = Vec::new();
    render_node(
        frame,
        area,
        tab.root_mut(),
        focus_id,
        &mut rects,
        &mut border_infos,
        repeat_mode,
        registry,
        drag_source,
        drag_target,
    );
    tab.pane_rects = rects;
    render_border_grid(frame, &border_infos);
    render_pane_titles(frame, &border_infos);
}

#[allow(clippy::too_many_arguments)]
fn render_node(
    frame: &mut Frame,
    area: Rect,
    node: &mut PaneNode,
    focus_id: usize,
    rects: &mut std::collections::HashMap<usize, (u16, u16, u16, u16)>,
    border_infos: &mut Vec<PaneBorderInfo>,
    repeat_mode: bool,
    registry: &AgentRegistry,
    drag_source: Option<usize>,
    drag_target: Option<usize>,
) {
    match node {
        PaneNode::Leaf(pane) => {
            rects.insert(pane.id, (area.x, area.y, area.width, area.height));
            let focused = pane.id == focus_id;
            let is_drag_source = drag_source == Some(pane.id);
            let is_drag_target = drag_target == Some(pane.id);
            let info = render_pane(
                frame,
                area,
                pane,
                focused,
                repeat_mode,
                registry,
                is_drag_source,
                is_drag_target,
            );
            border_infos.push(info);
        }
        PaneNode::Split {
            dir,
            ratio,
            first,
            second,
        } => {
            let [c0, c1] = crate::layout::split_chunks(area, dir, *ratio);
            render_node(
                frame,
                c0,
                first,
                focus_id,
                rects,
                border_infos,
                repeat_mode,
                registry,
                drag_source,
                drag_target,
            );
            render_node(
                frame,
                c1,
                second,
                focus_id,
                rects,
                border_infos,
                repeat_mode,
                registry,
                drag_source,
                drag_target,
            );
        }
    }
}

/// One leaf pane's contribution to the border grid.
pub(super) struct PaneBorderInfo {
    pub(super) area: Rect,
    pub(super) border_style: Style,
    pub(super) title_segments: Vec<(String, Style)>,
    pub(super) priority: u8,
}

#[allow(clippy::too_many_arguments)]
fn render_pane(
    frame: &mut Frame,
    area: Rect,
    pane: &mut crate::layout::Pane,
    focused: bool,
    repeat_mode: bool,
    registry: &AgentRegistry,
    is_drag_source: bool,
    is_drag_target: bool,
) -> PaneBorderInfo {
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

    let (border_style, title_style, priority) = if is_drag_source {
        let s = Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::REVERSED);
        (s, s.add_modifier(Modifier::BOLD), 5u8)
    } else if is_drag_target {
        let s = Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::REVERSED);
        (s, s.add_modifier(Modifier::BOLD), 4u8)
    } else if focused && repeat_mode {
        let border = Style::default().fg(Color::Yellow);
        let title = Style::default()
            .bg(Color::Yellow)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD);
        (border, title, 3u8)
    } else if focused {
        let c = match sc {
            Color::DarkGray | Color::White => Color::Cyan,
            _ => sc,
        };
        let border = Style::default().fg(c);
        let title = Style::default()
            .bg(c)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD);
        (border, title, 2u8)
    } else {
        let s = Style::default().fg(Color::DarkGray);
        (s, s, 1u8)
    };

    let title_segments = pane_title_segments(pane, title_style);

    let inner = Rect::new(
        area.x + 1,
        area.y + 1,
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );
    if inner.width == 0 || inner.height == 0 {
        return PaneBorderInfo {
            area,
            border_style,
            title_segments,
            priority,
        };
    }
    pane.vterm
        .render_to_buffer(frame.buffer_mut(), inner, pane.scroll_offset, !focused);

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

    PaneBorderInfo {
        area,
        border_style,
        title_segments,
        priority,
    }
}

pub(super) fn pane_title_segments(
    pane: &crate::layout::Pane,
    title_style: Style,
) -> Vec<(String, Style)> {
    let mut segments = Vec::new();
    let base = format!(" {}", pane.label());
    segments.push((base, title_style));
    if pane.pending_notification_count > 0 {
        segments.push((
            format!(" [{}]", pane.pending_notification_count),
            Style::default()
                .bg(Color::Yellow)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        ));
    }
    segments.push((" ".to_string(), title_style));
    segments
}

pub(super) fn render_status_bar(
    frame: &mut Frame,
    area: Rect,
    layout: &Layout,
    telegram: TelegramStatus,
) {
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

    let left_bar = Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::DarkGray));
    frame.render_widget(left_bar, area);

    let right_hint = Line::from(vec![
        Span::styled(
            "Ctrl+B c new | : cmd | n/p switch | d detach | ",
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            "Ctrl+B ? help ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    let right_bar = Paragraph::new(right_hint)
        .alignment(Alignment::Right)
        .style(Style::default().bg(Color::DarkGray));
    frame.render_widget(right_bar, area);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::layout::{Pane, PaneSource};
    use crate::vterm::VTerm;

    #[test]
    fn badge_shows_pending_count() {
        let pane = Pane {
            agent_name: "agent".to_string(),
            vterm: VTerm::new(10, 10),
            rx: crossbeam_channel::bounded(1).1,
            id: 1,
            backend: None,
            working_dir: None,
            display_name: None,
            scroll_offset: 0,
            has_notification: false,
            fleet_instance_name: None,
            last_input_at: None,
            pending_notification_count: 3,
            selection: None,
            source: PaneSource::Local,
        };
        let segments = pane_title_segments(&pane, Style::default());
        let joined = segments
            .into_iter()
            .map(|(text, _)| text)
            .collect::<String>();
        assert!(joined.contains("[3]"));
    }

    #[test]
    fn pane_title_no_state_suffix() {
        let pane = Pane {
            agent_name: "agent".to_string(),
            vterm: VTerm::new(10, 10),
            rx: crossbeam_channel::bounded(1).1,
            id: 1,
            backend: Some(crate::backend::Backend::ClaudeCode),
            working_dir: None,
            display_name: None,
            scroll_offset: 0,
            has_notification: false,
            fleet_instance_name: None,
            last_input_at: None,
            pending_notification_count: 0,
            selection: None,
            source: PaneSource::Local,
        };
        let segments = pane_title_segments(&pane, Style::default());
        let joined: String = segments.iter().map(|(t, _)| t.as_str()).collect();
        assert!(
            !joined.contains("[idle]"),
            "pane title must not contain state suffix, got: {joined}"
        );
    }

    #[test]
    fn state_color_returns_distinct_colors_for_key_states() {
        let idle = state_color(AgentState::Idle);
        let thinking = state_color(AgentState::Thinking);
        let tool_use = state_color(AgentState::ToolUse);
        let ready = state_color(AgentState::Ready);
        assert_ne!(idle, thinking, "idle vs thinking must differ");
        assert_ne!(thinking, tool_use, "thinking vs tool_use must differ");
        assert_ne!(idle, ready, "idle vs ready must differ");
    }

    #[test]
    fn state_color_error_states_are_red() {
        assert_eq!(state_color(AgentState::Crashed), Color::Red);
        assert_eq!(state_color(AgentState::Restarting), Color::Red);
    }

    #[test]
    fn highest_priority_state_returns_idle_for_empty_tab() {
        let tab = crate::layout::Tab::new(
            "empty".to_string(),
            crate::layout::Pane {
                agent_name: "test".to_string(),
                vterm: VTerm::new(10, 10),
                rx: crossbeam_channel::bounded(1).1,
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
                source: PaneSource::Local,
            },
        );
        let registry: crate::agent::AgentRegistry =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let result = highest_priority_state(&tab, &registry);
        assert_eq!(result, AgentState::Idle);
    }

    #[test]
    fn main_tui_footer_shows_help_hint() {
        let backend = ratatui::backend::TestBackend::new(100, 3);
        let mut terminal =
            ratatui::Terminal::new(backend).expect("test terminal creation should succeed");
        let layout = crate::layout::Layout::new();
        terminal
            .draw(|frame| {
                render_status_bar(frame, frame.area(), &layout, TelegramStatus::NotConfigured);
            })
            .expect("test terminal draw should succeed");
        let buf = terminal.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                text.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
            }
        }
        assert!(
            text.contains("Ctrl+B ?"),
            "status bar should contain 'Ctrl+B ?' hint, got: {text}"
        );
    }

    #[test]
    fn transient_state_badge_emits_for_restarting_and_crashed_only() {
        assert_eq!(
            transient_state_badge(AgentState::Restarting),
            Some(" [respawning]")
        );
        assert_eq!(
            transient_state_badge(AgentState::Crashed),
            Some(" [crashed]")
        );
        assert_eq!(transient_state_badge(AgentState::Idle), None);
        assert_eq!(transient_state_badge(AgentState::Ready), None);
        assert_eq!(transient_state_badge(AgentState::ToolUse), None);
        assert_eq!(transient_state_badge(AgentState::Hang), None);
        assert_eq!(transient_state_badge(AgentState::PermissionPrompt), None);
    }

    #[test]
    fn split_chunks_tiny_terminal_no_underflow() {
        use ratatui::layout::Rect;
        let area = Rect::new(0, 0, 1, 1);
        let [_a, b] = crate::layout::split_chunks(area, &crate::layout::SplitDir::Horizontal, 0.9);
        assert!(
            b.height >= 1,
            "second chunk height must be ≥1, got {}",
            b.height
        );
        let [_c, d] = crate::layout::split_chunks(area, &crate::layout::SplitDir::Vertical, 0.9);
        assert!(
            d.width >= 1,
            "second chunk width must be ≥1, got {}",
            d.width
        );
    }
}
