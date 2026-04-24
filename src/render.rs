//! Rendering: tab bar, status bar, pane tree, and overlay widgets.

use crate::agent::{self, AgentRegistry};
use crate::app::MenuItem;
use crate::layout::{DragTabTarget, Layout, PaneNode, SplitDir};
use crate::state::AgentState;
use ratatui::layout::{Alignment, Constraint, Direction, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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
        AgentState::AwaitingOperator => Color::Indexed(214), // orange — needs human attention
        AgentState::Ready => Color::Green,
        AgentState::Idle => Color::DarkGray,
        AgentState::Thinking => Color::Yellow,
        AgentState::ToolUse => Color::Blue,
        AgentState::InteractivePrompt => Color::Indexed(214), // orange — matches AwaitingOperator semantics (needs human OK)
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

    // Cross-tab drag drop target (if the active tab is currently dragging a
    // pane over the tab bar). Used to highlight the hovered tab / `[+]`.
    let drag_tab_target = layout
        .active_tab()
        .and_then(|t| t.dragging_pane.and(t.drag_target_tab));

    for (i, tab) in layout.tabs.iter().enumerate() {
        let is_active = i == layout.active;
        let is_drag_drop =
            matches!(drag_tab_target, Some(DragTabTarget::ExistingTab(idx)) if idx == i);
        // Tab reorder: highlight the drop target during tab-to-tab drag
        let is_reorder_target = layout
            .tab_reorder_target
            .is_some_and(|t| t == i && layout.tab_reorder_source.is_some_and(|s| s != i));
        // Show the highest-priority state across all panes in this tab
        let state = highest_priority_state(tab, registry);
        let sc = state_color(state);

        let style = if is_drag_drop || is_reorder_target {
            // Drop-target highlight wins over active styling so the user sees
            // where the pane will land. Magenta matches the intra-tab
            // drag-swap highlight (see is_drag_target below).
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
        let badge = if has_notif && !is_active { " !" } else { "" };
        let label = format!(" {}{badge} ", tab.name);

        spans.push(dot);
        spans.push(Span::styled(label, style));
    }

    // `[+]` doubles as the "drop here to spawn a new tab" zone during a
    // cross-tab drag. Highlight it so the user sees that releasing here is
    // a meaningful action, not a miss.
    let new_tab_style = if matches!(drag_tab_target, Some(DragTabTarget::NewTab)) {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Magenta)
            .add_modifier(Modifier::BOLD)
    } else {
        // fg must differ from the tab bar's bg (also DarkGray) — use Gray to match
        // the unselected-tab label color.
        Style::default().fg(Color::Gray)
    };
    spans.push(Span::styled(" [+] ", new_tab_style));
    let tabs = Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::DarkGray));
    frame.render_widget(tabs, area);
}

/// Split an area into two child rects that overlap by 1 cell on the split axis
/// so siblings share a border column/row. Mirrors `layout::split_child_areas`
/// — keep the two in sync (both produce overlap-by-1 results).
fn split_chunks(area: Rect, dir: &SplitDir, ratio: f32) -> [Rect; 2] {
    let total = match dir {
        SplitDir::Horizontal => area.height,
        SplitDir::Vertical => area.width,
    };
    let first_size = crate::layout::ratio_to_size(ratio, total);
    let overlap: u16 = if first_size >= 1 && total > first_size {
        1
    } else {
        0
    };
    match dir {
        SplitDir::Horizontal => {
            let second_y = area.y + first_size.saturating_sub(overlap);
            let second_h = area.height + overlap - first_size;
            [
                Rect::new(area.x, area.y, area.width, first_size),
                Rect::new(area.x, second_y, area.width, second_h),
            ]
        }
        SplitDir::Vertical => {
            let second_x = area.x + first_size.saturating_sub(overlap);
            let second_w = area.width + overlap - first_size;
            [
                Rect::new(area.x, area.y, first_size, area.height),
                Rect::new(second_x, area.y, second_w, area.height),
            ]
        }
    }
}

/// Resize pass — sync VTerm + PTY sizes to match render layout.
/// Call this after terminal resize, split/close, zoom toggle, or tab switch.
pub fn resize_panes(pane_area: Rect, layout: &mut Layout, registry: &AgentRegistry) {
    let tab = match layout.tabs.get_mut(layout.active) {
        Some(t) => t,
        None => return,
    };
    let mut resizes: Vec<(usize, u16, u16)> = Vec::new();
    if tab.zoomed {
        let focus_id = tab.focus_id;
        if let Some(pane) = tab.root_mut().find_pane_mut(focus_id) {
            let w = pane_area.width.saturating_sub(2);
            let h = pane_area.height.saturating_sub(2);
            if w > 0 && h > 0 && (w != pane.vterm.cols() || h != pane.vterm.rows()) {
                pane.vterm.resize(w, h);
                resizes.push((pane.id, w, h));
            }
        }
    } else {
        let mut rects = std::mem::take(&mut tab.pane_rects);
        rects.clear();
        collect_resize_needs(pane_area, tab.root_mut(), &mut rects, &mut resizes);
        tab.pane_rects = rects;
    }
    // Dispatch PTY / bridge resize via `Pane::resize_pty` — local panes hit
    // the registry, remote panes push through their BridgeClient.
    for (id, cols, rows) in &resizes {
        if let Some(pane) = tab.root().find_pane(*id) {
            pane.resize_pty(registry, *cols, *rows);
        }
    }
}

/// Collect (pane_id, width, height) for all panes that need resizing.
fn collect_resize_needs(
    area: Rect,
    node: &mut PaneNode,
    rects: &mut std::collections::HashMap<usize, (u16, u16, u16, u16)>,
    resizes: &mut Vec<(usize, u16, u16)>,
) {
    match node {
        PaneNode::Leaf(pane) => {
            rects.insert(pane.id, (area.x, area.y, area.width, area.height));
            let w = area.width.saturating_sub(2);
            let h = area.height.saturating_sub(2);
            if w > 0 && h > 0 && (w != pane.vterm.cols() || h != pane.vterm.rows()) {
                pane.vterm.resize(w, h);
                resizes.push((pane.id, w, h));
            }
        }
        PaneNode::Split {
            dir,
            ratio,
            first,
            second,
        } => {
            let [c0, c1] = split_chunks(area, dir, *ratio);
            collect_resize_needs(c0, first, rects, resizes);
            collect_resize_needs(c1, second, rects, resizes);
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
            let info = render_pane(frame, area, pane, true, false, registry, false, false);
            // Single-pane zoom: just draw its own 4 edges, no neighbors to merge with.
            let infos = vec![info];
            render_border_grid(frame, &infos);
            // Titles paint on top of borders so they read as text, not `─`.
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
    // Two-pass border rendering: first merge every pane's 4 edges into a single
    // grid (shared cells OR'd together → ┬/┴/├/┤/┼), then overlay each pane's
    // title on its top border row.
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
            let [c0, c1] = split_chunks(area, dir, *ratio);
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

/// One leaf pane's contribution to the border grid: its rect + resolved
/// border/title styles + a merge priority (drag_source, then drag_target,
/// then focused+repeat, then focused, then default). Two adjacent panes'
/// shared border cells pick the higher priority's style so drag highlights
/// always win over neighbor focus.
struct PaneBorderInfo {
    area: Rect,
    border_style: Style,
    title_segments: Vec<(String, Style)>,
    priority: u8,
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

    // Drag source/target use REVERSED modifier so the highlight is unambiguous
    // even when the agent's state color happens to match (Magenta =
    // PermissionPrompt, Green = Ready). Reversed swaps fg/bg on the border
    // cells, which no state-color path uses — so drag is always visually
    // distinct from any agent state.
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
        // Focused title gets a filled band (bg = state color, fg = black) so the
        // input focus is unambiguous at a glance in multi-pane layouts. Border
        // keeps fg-only so joined borders between panes stay visually clean.
        let title = Style::default()
            .bg(c)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD);
        (border, title, 2u8)
    } else {
        let s = Style::default().fg(Color::DarkGray);
        (s, s, 1u8)
    };

    let title_segments = pane_title_segments(pane, state, title_style);

    // Inner (content) area = outer shrunk 1 cell on every side, matching the
    // former Block::ALL inner. Borders are drawn later in `render_border_grid`
    // from every pane's collected rect — neighboring panes' shared edges merge
    // into joined box-drawing chars. Titles paint on top of the top border in
    // `render_pane_titles`.
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

    PaneBorderInfo {
        area,
        border_style,
        title_segments,
        priority,
    }
}

fn pane_title_segments(
    pane: &crate::layout::Pane,
    state: AgentState,
    title_style: Style,
) -> Vec<(String, Style)> {
    let mut segments = Vec::new();
    let base = if pane.backend.is_some() {
        format!(" {} [{}]", pane.label(), state.display_name())
    } else {
        format!(" {}", pane.label())
    };
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

// --- Pane border grid (joined box-drawing across shared edges) ---
//
// A single BorderCell encodes which of the four neighbor directions have a
// line continuing from this cell. Merging the per-pane edge contributions
// into one grid is what makes adjacent corners resolve into ├┤┬┴┼ on
// terminals that don't auto-join `┘┌` glyphs (e.g. macOS Terminal.app).

const DIR_N: u8 = 0b0001;
const DIR_E: u8 = 0b0010;
const DIR_S: u8 = 0b0100;
const DIR_W: u8 = 0b1000;

#[derive(Clone, Copy, Default)]
struct BorderCell {
    mask: u8,
    style: Style,
    priority: u8,
}

fn border_char(mask: u8) -> Option<char> {
    let c = match mask {
        0 => return None,
        m if m == DIR_N => '│',
        m if m == DIR_S => '│',
        m if m == DIR_E => '─',
        m if m == DIR_W => '─',
        m if m == DIR_N | DIR_S => '│',
        m if m == DIR_E | DIR_W => '─',
        m if m == DIR_N | DIR_E => '└',
        m if m == DIR_N | DIR_W => '┘',
        m if m == DIR_S | DIR_E => '┌',
        m if m == DIR_S | DIR_W => '┐',
        m if m == DIR_N | DIR_S | DIR_E => '├',
        m if m == DIR_N | DIR_S | DIR_W => '┤',
        m if m == DIR_N | DIR_E | DIR_W => '┴',
        m if m == DIR_S | DIR_E | DIR_W => '┬',
        m if m == DIR_N | DIR_E | DIR_S | DIR_W => '┼',
        _ => return None,
    };
    Some(c)
}

/// Add a pane's 4 edges to the grid. Corners carry bits from two perpendicular
/// edges so the merged mask naturally yields the right glyph. Interior edge
/// cells get only `N|S` or `E|W`; when a sibling pane adds its own edge at the
/// same cell (they overlap by 1 on the split axis), the OR is idempotent.
fn add_pane_borders(
    cells: &mut std::collections::HashMap<(u16, u16), BorderCell>,
    info: &PaneBorderInfo,
) {
    let area = info.area;
    if area.width < 2 || area.height < 2 {
        return;
    }
    let x0 = area.x;
    let y0 = area.y;
    let x1 = area.x + area.width - 1;
    let y1 = area.y + area.height - 1;

    let merge = |cells: &mut std::collections::HashMap<(u16, u16), BorderCell>,
                 x: u16,
                 y: u16,
                 mask: u8| {
        let slot = cells.entry((x, y)).or_default();
        slot.mask |= mask;
        if info.priority > slot.priority {
            slot.style = info.border_style;
            slot.priority = info.priority;
        }
    };

    // Top + bottom edges (corners carry vertical bits pointing inward).
    for x in x0..=x1 {
        let mut top = 0u8;
        let mut bot = 0u8;
        if x > x0 {
            top |= DIR_W;
            bot |= DIR_W;
        }
        if x < x1 {
            top |= DIR_E;
            bot |= DIR_E;
        }
        if x == x0 || x == x1 {
            top |= DIR_S;
            bot |= DIR_N;
        }
        merge(cells, x, y0, top);
        merge(cells, x, y1, bot);
    }
    // Left + right edge interiors (corners already handled above).
    if y1 > y0 + 1 {
        for y in (y0 + 1)..y1 {
            merge(cells, x0, y, DIR_N | DIR_S);
            merge(cells, x1, y, DIR_N | DIR_S);
        }
    }
}

fn render_border_grid(frame: &mut Frame, infos: &[PaneBorderInfo]) {
    let mut cells: std::collections::HashMap<(u16, u16), BorderCell> =
        std::collections::HashMap::new();
    for info in infos {
        add_pane_borders(&mut cells, info);
    }
    let buf = frame.buffer_mut();
    let buf_area = buf.area;
    for ((x, y), cell) in cells {
        if x < buf_area.x
            || x >= buf_area.x + buf_area.width
            || y < buf_area.y
            || y >= buf_area.y + buf_area.height
        {
            continue;
        }
        if let Some(ch) = border_char(cell.mask) {
            let b = &mut buf[(x, y)];
            b.set_char(ch);
            b.set_style(cell.style);
        }
    }
}

/// Overlay each pane's title on its top border row, starting at `area.x + 1`.
/// `title_bar_at` (in layout.rs) mirrors this position for mouse hit-testing
/// — any change here must stay in sync.
fn render_pane_titles(frame: &mut Frame, infos: &[PaneBorderInfo]) {
    let buf = frame.buffer_mut();
    let buf_area = buf.area;
    for info in infos {
        let area = info.area;
        if area.width < 3 || area.height == 0 {
            continue;
        }
        let y = area.y;
        // Reserve the right-most cell for the top-right corner glyph. All
        // bounds arithmetic here uses `saturating_*` because u16 addition of
        // two real Rect fields can overflow on pathological sizes (e.g. an
        // area abutting u16::MAX) — wraparound would then silently bypass
        // the `if x + w > last_usable_x` bound check and corrupt memory
        // outside our Rect (P2-architecture sweep).
        let last_usable_x = area.x.saturating_add(area.width).saturating_sub(1);
        let buf_right = buf_area.x.saturating_add(buf_area.width);
        let buf_bottom = buf_area.y.saturating_add(buf_area.height);
        let mut x = area.x.saturating_add(1);
        for (segment, style) in &info.title_segments {
            for g in segment.chars() {
                // Unicode width fits in a `u8` in practice; clamp to u16 so a
                // future width > 65535 would still terminate the loop safely.
                let w = u16::try_from(UnicodeWidthChar::width(g).unwrap_or(0)).unwrap_or(u16::MAX);
                if w == 0 {
                    continue;
                }
                if x.saturating_add(w) > last_usable_x {
                    break;
                }
                if x >= buf_right || y >= buf_bottom {
                    break;
                }
                let cell = &mut buf[(x, y)];
                cell.set_char(g);
                cell.set_style(*style);
                // For wide chars, blank out the trailing cells so residual border
                // glyphs don't peek through.
                for off in 1..w {
                    let tx = x.saturating_add(off);
                    if tx >= buf_right {
                        break;
                    }
                    let trail = &mut buf[(tx, y)];
                    trail.set_char(' ');
                    trail.set_style(*style);
                }
                x = x.saturating_add(w);
            }
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

    // Left side: agent / pane / layout / TG status (the spans above).
    // Right side: keybinding cheatsheet ending in the help hotkey so it
    // hugs the bottom-right corner regardless of terminal width. Rendered
    // as a separate right-aligned Paragraph over the same area — when
    // the terminal is narrow enough that the two overlap, the right hint
    // wins (kept short specifically for that case), so the user never
    // loses the "how do I open help?" affordance.
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

// --- Overlay renderers ---

pub fn render_menu(frame: &mut Frame, items: &[MenuItem], selected: usize) {
    let area = frame.area();
    // `items.len() as u16` can silently truncate; `area.height - 2` panics
    // if height < 2. Use saturating arithmetic throughout so a tiny
    // terminal renders a (clipped) menu instead of underflow-panicking.
    let item_count = u16::try_from(items.len()).unwrap_or(u16::MAX);
    let menu_height = item_count
        .saturating_add(4)
        .min(area.height.saturating_sub(2));
    let menu_width = 50u16.min(area.width.saturating_sub(4));
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
    // Use the real terminal cursor; don't print a literal '_'.
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

/// Render the move-pane destination picker. Lists all tabs plus a trailing
/// "[+] New tab" option. `selected == layout.tabs.len()` corresponds to the
/// new-tab slot; `source_tab_idx` is dimmed as an invalid target so the user
/// knows releasing Enter there is a no-op.
pub fn render_move_pane_target(
    frame: &mut Frame,
    layout: &Layout,
    selected: usize,
    source_tab_idx: usize,
    split_dir: crate::layout::SplitDir,
) {
    let area = frame.area();
    let list_len = layout.tabs.len() + 1; // tabs + "New tab"
    let h = (list_len as u16 + 4).min(area.height - 2);
    let w = 54u16.min(area.width - 4);
    let x = (area.width.saturating_sub(w)) / 2;
    let y = (area.height.saturating_sub(h)) / 2;
    let la = Rect::new(x, y, w, h);
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

    let mut lines: Vec<Line> = Vec::with_capacity(list_len);
    for (i, tab) in layout.tabs.iter().enumerate() {
        let is_sel = i == selected;
        let is_source = i == source_tab_idx;
        let style = if is_sel {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Magenta)
                .add_modifier(Modifier::BOLD)
        } else if is_source {
            // Source tab is not a meaningful target — dim it so selection
            // visually skips past without needing a hard-disable.
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
    // Trailing "New tab" slot.
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
    // Use the real terminal cursor for insertion point; don't print a literal
    // '_' after the input or it shows as a phantom character next to the cursor.
    frame.render_widget(
        Paragraph::new(format!(":{input}")).style(Style::default().fg(Color::White)),
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

pub fn render_tasks(
    frame: &mut Frame,
    items: &[crate::tasks::Task],
    sel_col: usize,
    sel_row: usize,
    mode: &crate::app::TaskBoardMode,
    view: crate::app::BoardView,
    home: &std::path::Path,
) {
    use crate::app::{BoardView, TaskBoardMode};
    let count = items.len();
    let view_tabs = match view {
        BoardView::Tasks => "[t] tasks  [f] fleet",
        BoardView::Fleet => " [t] tasks [f] fleet",
    };
    let title = format!(" Board ({count}) | {view_tabs} | Tab switch | q close ");
    let inner = render_overlay_frame(frame, Color::Blue, &title);

    if matches!(view, BoardView::Fleet) {
        render_fleet_view(frame, items, inner, home);
        return;
    }

    if items.is_empty() {
        frame.render_widget(
            Paragraph::new("  No tasks yet.").style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }

    let columns = task_board_columns(items);

    // Detail view for selected task
    if matches!(mode, TaskBoardMode::Detail) {
        if let Some(task) = columns[sel_col].get(sel_row) {
            let mut lines = vec![
                Line::from(Span::styled(
                    &task.title,
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    format!(
                        "Status: {}  Priority: {}  Assignee: {}",
                        task.status,
                        task.priority,
                        task.assignee.as_deref().unwrap_or("-")
                    ),
                    Style::default().fg(Color::Gray),
                )),
                Line::from(""),
            ];
            if !task.description.is_empty() {
                lines.push(Line::from(Span::styled(
                    &task.description,
                    Style::default().fg(Color::White),
                )));
                lines.push(Line::from(""));
            }
            if !task.depends_on.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("Depends on: {}", task.depends_on.join(", ")),
                    Style::default().fg(Color::Yellow),
                )));
            }
            if let Some(ref result) = task.result {
                lines.push(Line::from(Span::styled(
                    format!("Result: {result}"),
                    Style::default().fg(Color::Green),
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Esc to go back",
                Style::default().fg(Color::DarkGray),
            )));
            frame.render_widget(Paragraph::new(lines), inner);
            return;
        }
    }

    // 4-column kanban layout
    let col_areas = ratatui::layout::Layout::horizontal([
        Constraint::Ratio(1, 4),
        Constraint::Ratio(1, 4),
        Constraint::Ratio(1, 4),
        Constraint::Ratio(1, 4),
    ])
    .split(inner);

    let col_titles = ["Backlog", "Open", "In Progress", "Done"];
    let done_visible = columns.get(3).map(|c| c.len()).unwrap_or(0);
    let col_title_strs: Vec<String> = col_titles
        .iter()
        .enumerate()
        .map(|(i, t)| {
            if i == 3 {
                format!(" {} ({}/14d) ", t, done_visible)
            } else {
                format!(" {} ({}) ", t, columns.get(i).map(|c| c.len()).unwrap_or(0))
            }
        })
        .collect();
    let col_colors = [Color::Gray, Color::Green, Color::Yellow, Color::DarkGray];

    for (ci, (tasks, area)) in columns.iter().zip(col_areas.iter()).enumerate() {
        let is_active = ci == sel_col;
        let border_color = if is_active {
            Color::Blue
        } else {
            col_colors[ci]
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .title(Span::styled(
                &col_title_strs[ci],
                Style::default()
                    .fg(if is_active {
                        Color::Blue
                    } else {
                        col_colors[ci]
                    })
                    .add_modifier(Modifier::BOLD),
            ));
        let block_inner = block.inner(*area);
        frame.render_widget(block, *area);

        let mut lines: Vec<Line> = Vec::new();
        for (ri, t) in tasks.iter().enumerate() {
            let is_selected = is_active && ri == sel_row;
            let pri_badge = match t.priority.as_str() {
                "urgent" => "🔴",
                "high" => "🟠",
                "normal" => "🔵",
                _ => "⚪",
            };
            let blocked = if t.status == "blocked" { " 🔴" } else { "" };
            let assignee = t
                .assignee
                .as_deref()
                .map(|a| format!(" @{a}"))
                .unwrap_or_default();
            let text = format!("{pri_badge} {}{blocked}{assignee}", t.title);
            let style = if is_selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Blue)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            lines.push(Line::from(Span::styled(text, style)));
        }
        frame.render_widget(Paragraph::new(lines), block_inner);
    }

    // Status line: active/idle agent summary from In Progress column
    if inner.height > 2 && inner.width > 10 {
        let active_agents: std::collections::HashSet<&str> = columns[2]
            .iter()
            .filter_map(|t| t.assignee.as_deref())
            .collect();
        let status_text = if active_agents.is_empty() {
            "all idle".to_string()
        } else {
            let names: Vec<&str> = active_agents.into_iter().collect();
            format!("active: {}", names.join(", "))
        };
        let status_y = inner.y + inner.height.saturating_sub(1);
        let status_area = Rect::new(
            inner.x + 1,
            status_y,
            inner.width.saturating_sub(2).min(status_text.len() as u16),
            1,
        );
        frame.render_widget(
            Paragraph::new(Span::styled(status_text, Style::default().fg(Color::Cyan))),
            status_area,
        );
    }

    // Help hint at bottom-right (hidden in Help mode)
    if !matches!(mode, TaskBoardMode::Help) && inner.height > 1 && inner.width > 6 {
        let hint = "? help";
        let hint_x = inner.x + inner.width.saturating_sub(hint.len() as u16 + 1);
        let hint_y = inner.y + inner.height.saturating_sub(1);
        let hint_area = Rect::new(hint_x, hint_y, hint.len() as u16, 1);
        frame.render_widget(
            Paragraph::new(Span::styled(hint, Style::default().fg(Color::DarkGray))),
            hint_area,
        );
    }

    // Overlay sub-modes rendered on top of the kanban
    match mode {
        TaskBoardMode::NewTask { input } => {
            let w = 50u16.min(inner.width.saturating_sub(4));
            let popup = Rect::new(
                inner.x + (inner.width.saturating_sub(w)) / 2,
                inner.y + inner.height / 3,
                w,
                3,
            );
            frame.render_widget(Clear, popup);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Green))
                .title(Span::styled(
                    " New Task ",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ));
            let inner_popup = block.inner(popup);
            frame.render_widget(block, popup);
            let cursor = format!("{input}▏");
            frame.render_widget(
                Paragraph::new(cursor).style(Style::default().fg(Color::White)),
                inner_popup,
            );
        }
        TaskBoardMode::Assign { choices, selected } => {
            let h = (choices.len() as u16 + 2).min(inner.height.saturating_sub(2));
            let w = 40u16.min(inner.width.saturating_sub(4));
            let popup = Rect::new(
                inner.x + (inner.width.saturating_sub(w)) / 2,
                inner.y + (inner.height.saturating_sub(h)) / 2,
                w,
                h,
            );
            frame.render_widget(Clear, popup);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(Span::styled(
                    " Assign to ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ));
            let inner_popup = block.inner(popup);
            frame.render_widget(block, popup);
            let mut lines: Vec<Line> = Vec::new();
            for (i, (display, _value)) in choices.iter().enumerate() {
                let style = if i == *selected {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                lines.push(Line::from(Span::styled(display.as_str(), style)));
            }
            frame.render_widget(Paragraph::new(lines), inner_popup);
        }
        TaskBoardMode::Help => {
            let text = vec![
                Line::from(Span::styled(
                    "Task Board Help",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from("───────────────"),
                Line::from("  n           New task"),
                Line::from("  ↑↓ / j k    Select task in column"),
                Line::from("  ←→ / h l    Switch column"),
                Line::from("  H / L       Move task left/right (change status)"),
                Line::from("  Shift+D     Mark done from any column"),
                Line::from("  a           Assign to agent or team"),
                Line::from("  d           Cancel task"),
                Line::from("  Enter       View task detail"),
                Line::from("  Esc / q     Close Task Board"),
                Line::from(""),
                Line::from(Span::styled(
                    "Press ? or Esc to close help",
                    Style::default().fg(Color::DarkGray),
                )),
            ];
            let h = (text.len() as u16 + 2).min(inner.height.saturating_sub(2));
            let w = 52u16.min(inner.width.saturating_sub(4));
            let popup = Rect::new(
                inner.x + (inner.width.saturating_sub(w)) / 2,
                inner.y + (inner.height.saturating_sub(h)) / 2,
                w,
                h,
            );
            frame.render_widget(Clear, popup);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow))
                .title(Span::styled(
                    " ? Help ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ));
            let inner_popup = block.inner(popup);
            frame.render_widget(block, popup);
            frame.render_widget(Paragraph::new(text), inner_popup);
        }
        _ => {}
    }
}
/// Sorted by priority desc then created_at asc within each column.
/// Cancelled tasks are excluded.
/// Group tasks into 4 kanban columns: Backlog, Open, In Progress, Done.
/// Sorted by priority desc then created_at asc within each column.
/// Cancelled tasks are excluded.
pub fn task_board_columns(items: &[crate::tasks::Task]) -> [Vec<&crate::tasks::Task>; 4] {
    let mut backlog: Vec<&crate::tasks::Task> = Vec::new();
    let mut open: Vec<&crate::tasks::Task> = Vec::new();
    let mut in_progress: Vec<&crate::tasks::Task> = Vec::new();
    let mut done: Vec<&crate::tasks::Task> = Vec::new();

    for t in items {
        match t.status.as_str() {
            "cancelled" => {} // excluded
            "open" | "blocked" if t.priority == "low" => backlog.push(t),
            "open" | "blocked" => open.push(t),
            "claimed" => in_progress.push(t),
            "done" => done.push(t),
            _ => open.push(t),
        }
    }

    fn sort_col(col: &mut Vec<&crate::tasks::Task>) {
        col.sort_by(|a, b| {
            let pri_ord = |p: &str| -> u8 {
                match p {
                    "urgent" => 0,
                    "high" => 1,
                    "normal" => 2,
                    "low" => 3,
                    _ => 4,
                }
            };
            pri_ord(&a.priority)
                .cmp(&pri_ord(&b.priority))
                .then(a.created_at.cmp(&b.created_at))
        });
    }

    sort_col(&mut backlog);
    sort_col(&mut open);
    sort_col(&mut in_progress);
    // Secondary sort: group In Progress by assignee for visual grouping
    in_progress.sort_by(|a, b| {
        a.assignee
            .as_deref()
            .unwrap_or("")
            .cmp(b.assignee.as_deref().unwrap_or(""))
    });
    sort_col(&mut done);

    [backlog, open, in_progress, done]
}

/// Render the Fleet View — agent-centric dashboard grouped by team.
fn render_fleet_view(
    frame: &mut Frame,
    tasks: &[crate::tasks::Task],
    area: Rect,
    home: &std::path::Path,
) {
    // Build agent → claimed task mapping
    let mut agent_tasks: std::collections::HashMap<&str, Vec<&crate::tasks::Task>> =
        std::collections::HashMap::new();
    for t in tasks {
        if t.status == "claimed" {
            if let Some(ref a) = t.assignee {
                agent_tasks.entry(a.as_str()).or_default().push(t);
            }
        }
    }

    // Load teams for grouping
    let teams = crate::teams::list_all(home);
    let fleet = crate::fleet::FleetConfig::load(&home.join("fleet.yaml")).ok();
    let all_instances: Vec<String> = fleet.map(|c| c.instance_names()).unwrap_or_default();

    let mut lines: Vec<Line> = Vec::new();

    // Group by team
    let mut assigned_agents: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for team in &teams {
        lines.push(Line::from(Span::styled(
            format!(
                "═══ {} (orchestrator: {}) ═══",
                team.name,
                team.orchestrator.as_deref().unwrap_or("none")
            ),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        for member in &team.members {
            assigned_agents.insert(member.as_str());
            let symbol = if agent_tasks.contains_key(member.as_str()) {
                "🟠"
            } else {
                "🟢"
            };
            let task_info = agent_tasks
                .get(member.as_str())
                .and_then(|ts| ts.first())
                .map(|t| format!(" → {} ({})", t.title, t.id))
                .unwrap_or_else(|| " idle".to_string());
            lines.push(Line::from(Span::styled(
                format!("  {symbol} {member}{task_info}"),
                Style::default().fg(Color::White),
            )));
        }
        lines.push(Line::from(""));
    }

    // Unassigned agents
    let unassigned: Vec<&str> = all_instances
        .iter()
        .filter(|n| !assigned_agents.contains(n.as_str()))
        .map(String::as_str)
        .collect();
    if !unassigned.is_empty() {
        lines.push(Line::from(Span::styled(
            "═══ unassigned ═══",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )));
        for name in &unassigned {
            let symbol = if agent_tasks.contains_key(name) {
                "🟠"
            } else {
                "🟢"
            };
            let task_info = agent_tasks
                .get(name)
                .and_then(|ts| ts.first())
                .map(|t| format!(" → {} ({})", t.title, t.id))
                .unwrap_or_else(|| " idle".to_string());
            lines.push(Line::from(Span::styled(
                format!("  {symbol} {name}{task_info}"),
                Style::default().fg(Color::White),
            )));
        }
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "No agents configured. Add instances to fleet.yaml.",
            Style::default().fg(Color::DarkGray),
        )));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

/// Centered box sized to 60% of `area`, clamped so a tiny window still gets
/// a readable box. Shared by the spawn path (`Action::ScratchShell` in
/// `app::dispatch`) and the render path so both agree on geometry.
pub fn scratch_shell_rect(area: Rect) -> Rect {
    let w = ((area.width as u32 * 60 / 100) as u16).max(20);
    let h = ((area.height as u32 * 60 / 100) as u16).max(8);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}

/// Render the scratch shell overlay — a centered floating box (~60% of the
/// terminal) containing the pane's VTerm output. Drains pending output and
/// adapts VTerm/PTY size to the current box so a resized window stays usable.
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

    // Drain pending shell output before reading VTerm.
    pane.drain_output();

    // Resize VTerm + PTY if the floating box shape changed. Panes in the
    // layout go through `render::resize_panes` on Event::Resize; overlay
    // panes aren't in the layout, so we reconcile here on each draw.
    if inner.width != pane.vterm.cols() || inner.height != pane.vterm.rows() {
        pane.vterm.resize(inner.width, inner.height);
        pane.resize_pty(registry, inner.width, inner.height);
    }

    pane.vterm
        .render_to_buffer(frame.buffer_mut(), inner, 0, false);

    // Cursor — overlay is always focused, no scroll.
    let (cursor_line, cursor_col) = pane.vterm.cursor_pos();
    let cx = inner.x + cursor_col;
    let cy = inner.y + cursor_line;
    if cx < inner.x + inner.width && cy < inner.y + inner.height {
        frame.set_cursor_position(ratatui::layout::Position::new(cx, cy));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Pane, PaneSource};
    use crate::vterm::VTerm;
    use std::collections::HashMap;

    fn info(x: u16, y: u16, w: u16, h: u16) -> PaneBorderInfo {
        PaneBorderInfo {
            area: Rect::new(x, y, w, h),
            border_style: Style::default(),
            title_segments: Vec::new(),
            priority: 1,
        }
    }

    #[test]
    fn border_char_corner_and_junction_masks() {
        assert_eq!(border_char(0), None);
        assert_eq!(border_char(DIR_S | DIR_E), Some('┌'));
        assert_eq!(border_char(DIR_S | DIR_W), Some('┐'));
        assert_eq!(border_char(DIR_N | DIR_E), Some('└'));
        assert_eq!(border_char(DIR_N | DIR_W), Some('┘'));
        assert_eq!(border_char(DIR_E | DIR_W), Some('─'));
        assert_eq!(border_char(DIR_N | DIR_S), Some('│'));
        assert_eq!(border_char(DIR_N | DIR_S | DIR_E), Some('├'));
        assert_eq!(border_char(DIR_N | DIR_S | DIR_W), Some('┤'));
        assert_eq!(border_char(DIR_S | DIR_E | DIR_W), Some('┬'));
        assert_eq!(border_char(DIR_N | DIR_E | DIR_W), Some('┴'));
        assert_eq!(border_char(DIR_N | DIR_S | DIR_E | DIR_W), Some('┼'));
    }

    #[test]
    fn single_pane_renders_outer_frame() {
        // One pane at (0,0,10,5) → corners are ┌┐└┘, top/bottom are ─, sides │.
        let mut cells: HashMap<(u16, u16), BorderCell> = HashMap::new();
        add_pane_borders(&mut cells, &info(0, 0, 10, 5));
        assert_eq!(border_char(cells[&(0, 0)].mask), Some('┌'));
        assert_eq!(border_char(cells[&(9, 0)].mask), Some('┐'));
        assert_eq!(border_char(cells[&(0, 4)].mask), Some('└'));
        assert_eq!(border_char(cells[&(9, 4)].mask), Some('┘'));
        assert_eq!(border_char(cells[&(5, 0)].mask), Some('─'));
        assert_eq!(border_char(cells[&(5, 4)].mask), Some('─'));
        assert_eq!(border_char(cells[&(0, 2)].mask), Some('│'));
        assert_eq!(border_char(cells[&(9, 2)].mask), Some('│'));
    }

    #[test]
    fn adjacent_vertical_panes_produce_t_junctions() {
        // Two panes sharing col 9 (overlap-by-1 model):
        //   A = (0, 0, 10, 5),  B = (9, 0, 11, 5)
        // Shared-column corners merge to ┬ (top) and ┴ (bottom); middle is │.
        let mut cells: HashMap<(u16, u16), BorderCell> = HashMap::new();
        add_pane_borders(&mut cells, &info(0, 0, 10, 5));
        add_pane_borders(&mut cells, &info(9, 0, 11, 5));
        assert_eq!(
            border_char(cells[&(9, 0)].mask),
            Some('┬'),
            "top of shared column must be ┬, not two stacked ┐┌"
        );
        assert_eq!(border_char(cells[&(9, 4)].mask), Some('┴'));
        assert_eq!(border_char(cells[&(9, 2)].mask), Some('│'));
        // Outer corners preserved.
        assert_eq!(border_char(cells[&(0, 0)].mask), Some('┌'));
        assert_eq!(border_char(cells[&(19, 0)].mask), Some('┐'));
    }

    #[test]
    fn four_way_grid_produces_cross_junction() {
        // 2x2 grid (horizontal split above+below a vertical split):
        //   A=(0,0,10,5) B=(9,0,11,5) C=(0,4,10,6) D=(9,4,11,6)
        // The center cell (9, 4) is the 4-way junction — must render ┼.
        let mut cells: HashMap<(u16, u16), BorderCell> = HashMap::new();
        add_pane_borders(&mut cells, &info(0, 0, 10, 5));
        add_pane_borders(&mut cells, &info(9, 0, 11, 5));
        add_pane_borders(&mut cells, &info(0, 4, 10, 6));
        add_pane_borders(&mut cells, &info(9, 4, 11, 6));
        assert_eq!(
            border_char(cells[&(9, 4)].mask),
            Some('┼'),
            "4-way junction must be ┼"
        );
        assert_eq!(
            border_char(cells[&(0, 4)].mask),
            Some('├'),
            "left-edge T must be ├"
        );
        assert_eq!(
            border_char(cells[&(19, 4)].mask),
            Some('┤'),
            "right-edge T must be ┤"
        );
    }

    #[test]
    fn higher_priority_wins_shared_cell_style() {
        // When a drag_source pane (priority 5) and a default pane (priority 1)
        // share a border cell, the drag style must win regardless of insertion order.
        let mut a = info(0, 0, 10, 5);
        a.priority = 5;
        a.border_style = Style::default().fg(Color::Magenta);
        let b = info(9, 0, 11, 5);
        let mut cells: HashMap<(u16, u16), BorderCell> = HashMap::new();
        add_pane_borders(&mut cells, &b);
        add_pane_borders(&mut cells, &a);
        let shared = cells[&(9, 2)];
        assert_eq!(shared.priority, 5);
        assert_eq!(shared.style.fg, Some(Color::Magenta));
    }

    fn make_task(
        title: &str,
        status: &str,
        priority: &str,
        created_at: &str,
    ) -> crate::tasks::Task {
        crate::tasks::Task {
            id: title.to_string(),
            title: title.to_string(),
            description: String::new(),
            status: status.to_string(),
            priority: priority.to_string(),
            assignee: None,
            routed_to: None,
            created_by: String::new(),
            depends_on: Vec::new(),
            result: None,
            created_at: created_at.to_string(),
            updated_at: String::new(),
            due_at: None,
        }
    }

    #[test]
    fn task_board_groups_by_status() {
        let tasks = vec![
            make_task("backlog-item", "open", "low", "2026-01-01"),
            make_task("open-item", "open", "high", "2026-01-02"),
            make_task("wip", "claimed", "normal", "2026-01-03"),
            make_task("finished", "done", "normal", "2026-01-04"),
            make_task("cancelled-item", "cancelled", "normal", "2026-01-05"),
            make_task("blocked-low", "blocked", "low", "2026-01-06"),
        ];
        let [backlog, open, in_progress, done] = task_board_columns(&tasks);
        assert_eq!(backlog.len(), 2, "open+low and blocked+low → backlog");
        assert_eq!(open.len(), 1, "open+high → open");
        assert_eq!(in_progress.len(), 1, "claimed → in progress");
        assert_eq!(done.len(), 1, "done → done");
        // cancelled excluded from all columns
    }

    #[test]
    fn task_board_sorts_by_priority_then_created() {
        let tasks = vec![
            make_task("normal-old", "open", "normal", "2026-01-01"),
            make_task("urgent-new", "open", "urgent", "2026-01-03"),
            make_task("normal-new", "open", "normal", "2026-01-02"),
            make_task("high-old", "open", "high", "2026-01-01"),
        ];
        let [_, open, _, _] = task_board_columns(&tasks);
        assert_eq!(open[0].title, "urgent-new", "urgent first");
        assert_eq!(open[1].title, "high-old", "high second");
        assert_eq!(open[2].title, "normal-old", "normal oldest third");
        assert_eq!(open[3].title, "normal-new", "normal newest last");
    }

    #[test]
    fn badge_shows_pending_count() {
        let pane = Pane {
            agent_name: "agent".to_string(),
            vterm: VTerm::new(10, 10),
            rx: crossbeam::channel::bounded(1).1,
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
        let segments = pane_title_segments(&pane, AgentState::Idle, Style::default());
        let joined = segments
            .into_iter()
            .map(|(text, _)| text)
            .collect::<String>();
        assert!(joined.contains("[3]"));
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

    fn render_task_board_to_string(mode: &crate::app::TaskBoardMode) -> String {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal =
            ratatui::Terminal::new(backend).expect("test terminal creation should succeed");
        let tasks = vec![crate::tasks::Task {
            id: "t-1".into(),
            title: "test task".into(),
            description: String::new(),
            status: "open".into(),
            priority: "normal".into(),
            assignee: None,
            routed_to: None,
            depends_on: Vec::new(),
            result: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            created_by: "test".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            due_at: None,
        }];
        terminal
            .draw(|frame| {
                render_tasks(
                    frame,
                    &tasks,
                    0,
                    0,
                    mode,
                    crate::app::BoardView::Tasks,
                    std::path::Path::new("/tmp"),
                )
            })
            .expect("test terminal draw should succeed");
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn task_board_footer_shows_help_hint() {
        let output = render_task_board_to_string(&crate::app::TaskBoardMode::Board);
        assert!(
            output.contains("? help"),
            "Board mode should show '? help' hint, got:\n{output}"
        );
    }

    #[test]
    fn task_board_help_mode_hides_footer() {
        let output = render_task_board_to_string(&crate::app::TaskBoardMode::Help);
        assert!(
            !output.contains("? help"),
            "Help mode should NOT show '? help' hint, got:\n{output}"
        );
    }
}
