//! Core rendering: main entry point, tab bar, status bar, pane tree.

use std::collections::HashMap;

use super::border::{render_border_grid, render_pane_titles};
use crate::agent::{self, AgentRegistry};
use crate::channel::TelegramStatus;
use crate::layout::{DragTabTarget, Layout, PaneNode};
use crate::state::AgentState;
use ratatui::layout::{Alignment, Constraint, Direction, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
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
        AgentState::Idle => Color::DarkGray,
        AgentState::Thinking => Color::Yellow,
        AgentState::ToolUse => Color::Blue,
        AgentState::InteractivePrompt => Color::Indexed(214),
        AgentState::PermissionPrompt => Color::Magenta,
        // Phase A Piece-1: GitConflict shares the magenta band with
        // PermissionPrompt — both are work-blocked states needing
        // external intervention, surfaced together in the TUI status
        // band.
        AgentState::GitConflict => Color::Magenta,
        AgentState::ContextFull | AgentState::RateLimit | AgentState::ServerRateLimit => {
            Color::Indexed(208)
        }
        AgentState::UsageLimit | AgentState::AuthError | AgentState::ApiError => Color::Red,
        // #1634: model-unsupported is a permanent config fault — red like the
        // other error states.
        AgentState::ModelUnsupported => Color::Red,
        AgentState::Hang | AgentState::Crashed | AgentState::Restarting => Color::Red,
    }
}

/// #freeze-2 (t-…74503): max bytes of queued PTY output a pane drains into its
/// VTerm per frame, inside `terminal.draw` on the main thread (see
/// `Pane::drain_output`). Caps per-frame CPU so a boot/restart backlog can't stall
/// the draw (and thus input); the remainder drains over the next frames. Tunable —
/// smaller = snappier input under flood / slower visual catch-up. Calibrated to
/// keep `terminal.draw` responsive (a few ms); verify with the `#freeze-drain`
/// probe (`AGEND_FREEZE_INSTRUMENT`).
const DRAIN_OUTPUT_BUDGET_BYTES: usize = 32 * 1024;

/// #freeze-2: does any pane the render path actually DRAINS in the active tab
/// still have queued PTY output? The render loop re-arms `dirty` on this so a
/// budget-capped `drain_output` finishes draining over subsequent frames. Mirrors
/// `render_pane_tree`'s pane selection (zoom = only the focused pane is drawn).
pub fn active_tab_has_pending_output(layout: &Layout) -> bool {
    let Some(tab) = layout.tabs.get(layout.active) else {
        return false;
    };
    if tab.zoomed {
        tab.root()
            .find_pane(tab.focus_id)
            .is_some_and(|p| !p.rx.is_empty())
    } else {
        tab.root()
            .pane_ids()
            .iter()
            .any(|id| tab.root().find_pane(*id).is_some_and(|p| !p.rx.is_empty()))
    }
}

/// #freeze-3 (t-…50793): total bytes drained across ALL panes per frame, shared
/// active-tab-first. Caps per-frame main-thread VTerm work regardless of pane
/// count — the boot/restart flood is every pane dumping its screen at once, so a
/// naive per-pane budget would scale to `N × DRAIN_OUTPUT_BUDGET_BYTES` and
/// re-create the #freeze-2 long-draw that #2385 bounded for the active pane alone.
/// Sized at 2× the per-pane budget: the active tab keeps its full snappy budget
/// (zero draw-time regression when the background is idle) and the background
/// panes share the remainder.
const DRAIN_ALL_TOTAL_BUDGET_BYTES: usize = 2 * DRAIN_OUTPUT_BUDGET_BYTES;

/// #freeze-3 (t-…50793) ROOT FIX: drain queued PTY output for EVERY pane (both the
/// active tab's and the background tabs') into its own `Pane.vterm`, within the
/// single shared per-frame `DRAIN_ALL_TOTAL_BUDGET_BYTES`, spending the ACTIVE
/// tab's panes first so the visible catch-up keeps priority. Returns `true` if any
/// pane still has queued output after this pass.
///
/// This fixes the residual freeze #2385 left: `render_pane` only ever drained the
/// ACTIVE tab, so a backgrounded busy tab's `pane.rx` grew UNBOUNDED and switching
/// to it replayed `ceil(backlog / budget)` frames of catch-up — proportional to
/// how long the tab was backgrounded (the operator's multi-second "一直刷新").
/// Draining every pane every frame keeps each `rx` bounded → the switch is
/// instant and memory is bounded.
///
/// All work is on the MAIN thread against `Pane.vterm` (owned, NOT behind
/// core.lock — the PTY read loops feed the SEPARATE `AgentCore.vterm`), so there
/// is zero contention with the per-agent core locks (perf-R1 safe).
///
/// Re-arm: the render loop re-arms its redraw on the ACTIVE tab's backlog only
/// (`active_tab_has_pending_output`) — background draining needs no redraw and is
/// guaranteed a next pass by the loop's ≤50ms idle cadence plus per-output
/// wakeups (both set `dirty` → frame-due → this runs again).
///
/// Limitation: a single background agent sustaining > one pane's drain rate
/// (~`DRAIN_OUTPUT_BUDGET_BYTES`/frame) indefinitely can delay OTHER background
/// panes' drain (active-first + the shared cap) — they still drain once it pauses,
/// and the active tab plus that agent are never starved. KISS: no cross-frame
/// round-robin cursor.
pub fn drain_all_panes(layout: &mut Layout) -> bool {
    let active = layout.active;
    let mut remaining = DRAIN_ALL_TOTAL_BUDGET_BYTES;
    let mut more = false;
    let mut probe_panes_with_backlog = 0usize;
    let mut probe_max_rx_chunks = 0usize;
    // Active tab first (visible catch-up priority), then the rest in tab order.
    let order = std::iter::once(active).chain((0..layout.tabs.len()).filter(move |&i| i != active));
    for tab_idx in order {
        let Some(tab) = layout.tabs.get_mut(tab_idx) else {
            continue;
        };
        for id in tab.root().pane_ids() {
            let Some(pane) = tab.root_mut().find_pane_mut(id) else {
                continue;
            };
            let budget = DRAIN_OUTPUT_BUDGET_BYTES.min(remaining);
            remaining = remaining.saturating_sub(pane.drain_output(budget));
            let rx_chunks = pane.rx.len();
            if rx_chunks > 0 {
                more = true;
                probe_panes_with_backlog += 1;
                probe_max_rx_chunks = probe_max_rx_chunks.max(rx_chunks);
            }
        }
    }
    // #freeze-3 probe (env-gated, `AGEND_FREEZE_INSTRUMENT`): summarize residual
    // backlog so an operator restart-repro can confirm background rx stays bounded.
    // Off by default → zero behavior change.
    if probe_panes_with_backlog > 0 && freeze_backlog_probe_enabled() {
        tracing::info!(
            tag = "#freeze-backlog",
            panes_with_backlog = probe_panes_with_backlog,
            max_rx_chunks = probe_max_rx_chunks,
            budget_spent = DRAIN_ALL_TOTAL_BUDGET_BYTES - remaining,
            "drain_all_panes residual backlog"
        );
    }
    more
}

/// #freeze-3 probe gate: the `#freeze-backlog` summary in `drain_all_panes` fires
/// only when `AGEND_FREEZE_INSTRUMENT` is set (any non-empty, non-`"0"` value),
/// mirroring `Pane::drain_output`'s `#freeze-drain` probe. Read once, cached.
fn freeze_backlog_probe_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("AGEND_FREEZE_INSTRUMENT").is_ok_and(|v| !v.is_empty() && v != "0")
    })
}

fn build_agent_state_snapshot(
    layout: &Layout,
    registry: &AgentRegistry,
) -> HashMap<String, AgentState> {
    let reg = agent::lock_registry(registry);
    let mut snapshot = HashMap::new();
    for tab in &layout.tabs {
        for id in tab.root().pane_ids() {
            if let Some(pane) = tab.root().find_pane(id) {
                if pane.backend.is_some() {
                    snapshot
                        .entry(pane.agent_name.to_string())
                        .or_insert_with(|| {
                            // Read the lock-free published mirror — NO core.lock().
                            // Under the boot PTY flood the per-agent core lock is
                            // held 2–6 ms by each `pty_read_loop` feed; taking it
                            // here (once per pane, every frame) made the render
                            // snapshot wait up to ~10 ms and starved input. The
                            // AtomicU8 is written in lockstep by `record_set`.
                            reg.get(&pane.instance_id)
                                .map(|h| {
                                    AgentState::from_u8(
                                        h.published_state
                                            .load(std::sync::atomic::Ordering::Relaxed),
                                    )
                                })
                                .unwrap_or(AgentState::Idle)
                        });
                }
            }
        }
    }
    snapshot
}

pub fn render(
    frame: &mut Frame,
    layout: &mut Layout,
    repeat_mode: bool,
    registry: &AgentRegistry,
    telegram: TelegramStatus,
    binary_stale: bool,
) {
    let chunks = ratatui::layout::Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(crate::layout::TAB_BAR_HEIGHT),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let snapshot = build_agent_state_snapshot(layout, registry);
    render_pane_tree(frame, chunks[1], layout, repeat_mode, registry, &snapshot);
    render_tab_bar(frame, chunks[0], layout, &snapshot);
    render_status_bar(frame, chunks[2], layout, telegram, binary_stale);
}

/// Get the highest-priority state across all panes in a tab.
pub fn highest_priority_state(
    tab: &crate::layout::Tab,
    snapshot: &HashMap<String, AgentState>,
) -> AgentState {
    let mut best = AgentState::Idle;
    for id in tab.root().pane_ids() {
        if let Some(pane) = tab.root().find_pane(id) {
            if pane.backend.is_some() {
                let s = snapshot
                    .get(pane.agent_name.as_str())
                    .copied()
                    .unwrap_or(AgentState::Idle);
                if s.priority() > best.priority() {
                    best = s;
                }
            }
        }
    }
    best
}

/// SYNC: layout math (label width, spacing) must match tab_bar_hit_test() in app.rs.
fn render_tab_bar(
    frame: &mut Frame,
    area: Rect,
    layout: &Layout,
    snapshot: &HashMap<String, AgentState>,
) {
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
        let state = highest_priority_state(tab, snapshot);
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
    // #1071: Clear pre-render blanks any cells that retained chars from
    // a prior longer tab strip (e.g. tab close shrinks the bar). ratatui's
    // Paragraph only writes cells covered by span text + applies the area
    // style; cells outside spans keep their prior char.
    frame.render_widget(Clear, area);
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
    snapshot: &HashMap<String, AgentState>,
) {
    let tab = match layout.tabs.get_mut(layout.active) {
        Some(t) => t,
        None => {
            // #1071: Clear pre-render — the fallback Paragraph is reached
            // after the last tab closes; without Clear the prior frame's
            // pane-tree cells (border chars + VTerm content) leak through.
            frame.render_widget(Clear, area);
            let msg = Paragraph::new("No agents. Press Ctrl+B c to create a new tab.")
                .style(Style::default().fg(Color::DarkGray));
            frame.render_widget(msg, area);
            return;
        }
    };

    let focus_id = tab.focus_id;

    if tab.zoomed {
        if let Some(pane) = tab.root_mut().find_pane_mut(focus_id) {
            let info = render_pane(
                frame, area, pane, true, false, registry, snapshot, false, false,
            );
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
        snapshot,
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
    snapshot: &HashMap<String, AgentState>,
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
                snapshot,
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
                snapshot,
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
                snapshot,
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
    snapshot: &HashMap<String, AgentState>,
    is_drag_source: bool,
    is_drag_target: bool,
) -> PaneBorderInfo {
    // #freeze-3: draining moved OUT of the render path into the render loop's
    // `drain_all_panes`, which drains EVERY tab's panes (not just the active one)
    // so a backgrounded tab's `rx` stays bounded. `render_pane` is now a pure
    // VTerm read; the active tab's catch-up re-arm still rides on
    // `active_tab_has_pending_output` in the loop.
    if focused {
        pane.has_notification = false;
    }

    let state = if pane.backend.is_some() {
        snapshot
            .get(pane.agent_name.as_str())
            .copied()
            .unwrap_or(AgentState::Idle)
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

    let title_segments = pane_title_segments(
        pane,
        title_style,
        state,
        crate::runtime_config::get().show_pane_state,
    );

    // W2.6: the pane content rect is the authority for the vterm/PTY size, and
    // render is the authoritative chokepoint that corrects to it (layout
    // pre-computes an estimate). See `crate::render::resize`.
    let content = crate::render::resize::PaneContentRect::from_bordered_area(area);
    if content.is_empty() {
        return PaneBorderInfo {
            area,
            border_style,
            title_segments,
            priority,
        };
    }
    let inner = content.rect();
    if let Some(d) =
        crate::render::resize::ResizeDecision::needed(inner, pane.vterm.cols(), pane.vterm.rows())
    {
        pane.vterm.resize(d.cols, d.rows);
        pane.resize_pty(registry, d.cols, d.rows);
    }
    let render_offset = pane.scroll_offset;
    pane.vterm
        .render_to_buffer(frame.buffer_mut(), inner, render_offset, !focused);

    if let Some(ref sel) = pane.selection {
        // Selection is stored in absolute scrollback logical coords; map each
        // endpoint to the current viewport and clip to the visible window so
        // the highlight tracks its content as it scrolls (#1432).
        let (s, e) = if sel.start <= sel.end {
            (sel.start, sel.end)
        } else {
            (sel.end, sel.start)
        };
        let s_row = pane.logical_line_to_viewport(s.0);
        let e_row = pane.logical_line_to_viewport(e.0);
        let lo = s_row.max(0);
        let hi = e_row.min(inner.height as i64 - 1);
        let mut vrow = lo;
        while vrow <= hi {
            let col_start = if vrow == s_row { s.1 } else { 0 };
            let col_end = if vrow == e_row {
                e.1
            } else {
                inner.width.saturating_sub(1)
            };
            for col in col_start..=col_end {
                let x = inner.x + col;
                let y = inner.y + vrow as u16;
                if x < inner.x + inner.width && y < inner.y + inner.height {
                    let cell = &mut frame.buffer_mut()[(x, y)];
                    let style = cell.style().add_modifier(Modifier::REVERSED);
                    cell.set_style(style);
                }
            }
            vrow += 1;
        }
    }

    if focused {
        let (cursor_line, cursor_col) = pane.vterm.cursor_pos();
        let max_x = inner.x + inner.width.saturating_sub(1);
        let max_y = inner.y + inner.height.saturating_sub(1);
        let (cx, cy) = if render_offset == 0 {
            (
                (inner.x + cursor_col).min(max_x),
                (inner.y + cursor_line).min(max_y),
            )
        } else {
            (inner.x, max_y)
        };
        frame.set_cursor_position(ratatui::layout::Position::new(cx, cy));
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
    state: AgentState,
    show_state_badge: bool,
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
    // #1713/#1523 diagnostic (runtime_config.show_pane_state, default off): append
    // a `[<State>]` text badge of the DETECTED AgentState so the operator can
    // eyeball-verify detection against the live pane. Extra text only — the
    // pane's colour (state_color) is untouched, and it uses the same
    // `title_style` so it introduces no new colour. The transient
    // Restarting/Crashed tab-bar badge (`transient_state_badge`) is unaffected.
    if show_state_badge {
        segments.push((format!(" [{state:?}]"), title_style));
    }
    segments.push((" ".to_string(), title_style));
    segments
}

pub(super) fn render_status_bar(
    frame: &mut Frame,
    area: Rect,
    layout: &Layout,
    telegram: TelegramStatus,
    binary_stale: bool,
) {
    let mut spans = Vec::new();

    // #1027: operator-facing indicator for "running daemon's binary is
    // older than the on-disk binary; restart to pick up new code".
    // Replaces the previous inbox-emit path (which routed to agents
    // who cannot restart the daemon). Sticky-true until process
    // restart — see mcp_registry_watcher module-doc.
    if binary_stale {
        spans.push(Span::styled(
            " ! daemon binary stale (restart) ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }

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

    // #1071: Clear pre-render (single Clear before BOTH bars). The two
    // Paragraphs render to the same area but only cover cells where their
    // own span text falls; cells in the middle gap between them — and any
    // trailing cells beyond shorter content compared to a prior frame —
    // would otherwise retain prior chars.
    frame.render_widget(Clear, area);
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
            agent_name: "agent".into(),
            instance_id: crate::types::InstanceId::default(),
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
        let segments = pane_title_segments(&pane, Style::default(), AgentState::Idle, false);
        let joined = segments
            .into_iter()
            .map(|(text, _)| text)
            .collect::<String>();
        assert!(joined.contains("[3]"));
    }

    #[test]
    fn pane_title_no_state_suffix() {
        let pane = Pane {
            agent_name: "agent".into(),
            instance_id: crate::types::InstanceId::default(),
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
        // #1713 flag OFF (default): no state badge appended; only the base label
        // (+ the transient Restarting/Crashed tab badge, which lives elsewhere).
        let segments = pane_title_segments(&pane, Style::default(), AgentState::Idle, false);
        let joined: String = segments.iter().map(|(t, _)| t.as_str()).collect();
        assert!(
            !joined.contains("[Idle]") && !joined.contains("[idle]"),
            "flag-off: pane title must not contain a state badge, got: {joined}"
        );
    }

    /// #1713 flag ON: the pane title appends a `[<State>]` badge of the detected
    /// AgentState (all states), so the operator can eyeball-verify detection.
    #[test]
    fn pane_title_state_badge_when_flag_on_1713() {
        let pane = Pane {
            agent_name: "agent".into(),
            instance_id: crate::types::InstanceId::default(),
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
        for (state, want) in [
            (AgentState::ServerRateLimit, "[ServerRateLimit]"),
            (AgentState::PermissionPrompt, "[PermissionPrompt]"),
            (AgentState::Thinking, "[Thinking]"),
            (AgentState::Idle, "[Idle]"),
        ] {
            let segments = pane_title_segments(&pane, Style::default(), state, true);
            let joined: String = segments.iter().map(|(t, _)| t.as_str()).collect();
            assert!(
                joined.contains(want),
                "flag-on: title must contain {want}, got: {joined}"
            );
        }
    }

    #[test]
    fn state_color_returns_distinct_colors_for_key_states() {
        let idle = state_color(AgentState::Idle);
        let thinking = state_color(AgentState::Thinking);
        let tool_use = state_color(AgentState::ToolUse);
        assert_ne!(idle, thinking, "idle vs thinking must differ");
        assert_ne!(thinking, tool_use, "thinking vs tool_use must differ");
        assert_ne!(idle, tool_use, "idle vs tool_use must differ");
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
                agent_name: "test".into(),
                instance_id: crate::types::InstanceId::default(),
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
        let snapshot = HashMap::new();
        let result = highest_priority_state(&tab, &snapshot);
        assert_eq!(result, AgentState::Idle);
    }

    #[test]
    fn render_resizes_vterm_to_pane_content_rows_2046() {
        let backend = ratatui::backend::TestBackend::new(40, 20);
        let mut terminal =
            ratatui::Terminal::new(backend).expect("test terminal creation should succeed");
        let registry: AgentRegistry =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));

        let pane = Pane {
            agent_name: "agent".into(),
            instance_id: crate::types::InstanceId::default(),
            // 40x20 frame -> pane tree is 40x18 after tab/status chrome, and
            // pane border leaves a 38x16 terminal content area. Start 5 rows
            // short to reproduce #2046's floating backend footer symptom.
            vterm: VTerm::new(38, 11),
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
        };
        let mut layout = Layout::new();
        layout.add_tab(crate::layout::Tab::new("agent".to_string(), pane));

        terminal
            .draw(|frame| {
                render(
                    frame,
                    &mut layout,
                    false,
                    &registry,
                    TelegramStatus::NotConfigured,
                    false,
                );
            })
            .expect("test terminal draw should succeed");

        let pane = layout.active_tab().unwrap().focused_pane().unwrap();
        assert_eq!(pane.vterm.cols(), 38);
        assert_eq!(
            pane.vterm.rows(),
            16,
            "render must keep the VTerm/PTY rows equal to pane content rows"
        );
    }

    #[test]
    fn main_tui_footer_shows_help_hint() {
        let backend = ratatui::backend::TestBackend::new(100, 3);
        let mut terminal =
            ratatui::Terminal::new(backend).expect("test terminal creation should succeed");
        let layout = crate::layout::Layout::new();
        terminal
            .draw(|frame| {
                render_status_bar(
                    frame,
                    frame.area(),
                    &layout,
                    TelegramStatus::NotConfigured,
                    false,
                );
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

    /// #1140: a wide (2-cell) char replaced by narrow chars across frames must
    /// leave no ghost in the former spacer cell. Contract-lock for the
    /// wide→narrow render path our panes depend on.
    ///
    /// Note: the plain-CJK case already worked on ratatui 0.29 — the #1140
    /// ghost came from VS16-emoji width miscalculation, which the 0.30 upgrade
    /// fixes. That bug is layout-level (width-dependent placement), so it can't
    /// be cleanly discriminated cross-version in a unit test; the emoji case
    /// below documents the correct 0.30 behavior and the real-world ghost is
    /// confirmed by operator visual check (see PR).
    fn render_row(terminal: &mut ratatui::Terminal<ratatui::backend::TestBackend>, text: &str) {
        terminal
            .draw(|frame| {
                frame
                    .buffer_mut()
                    .set_string(0, 0, text, ratatui::style::Style::default());
            })
            .expect("test draw should succeed");
    }

    fn backend_row(
        terminal: &ratatui::Terminal<ratatui::backend::TestBackend>,
        width: u16,
    ) -> String {
        let buf = terminal.backend().buffer();
        (0..width)
            .map(|x| buf.cell((x, 0)).map(|c| c.symbol()).unwrap_or(" "))
            .collect()
    }

    #[test]
    fn wide_char_to_narrow_leaves_no_ghost() {
        let backend = ratatui::backend::TestBackend::new(4, 1);
        let mut terminal =
            ratatui::Terminal::new(backend).expect("test terminal creation should succeed");

        // Frame 1: wide "中" spans cols 0-1, "X" at col 2.
        render_row(&mut terminal, "中X ");
        // Frame 2: "中" replaced by narrow "ab".
        render_row(&mut terminal, "abX ");
        assert_eq!(
            backend_row(&terminal, 4).trim_end(),
            "abX",
            "spacer cell must not retain the wide char's right half"
        );

        // Issue's exact symptom: lone narrow char where a wide char was, rest empty.
        render_row(&mut terminal, "中X ");
        render_row(&mut terminal, "a   ");
        assert_eq!(
            backend_row(&terminal, 4).trim_end(),
            "a",
            "no lone-char ghost may persist in the former spacer cell"
        );

        // VS16 emoji (U+2764 U+FE0F) — width-2 on ratatui 0.30, the actual
        // #1140 ghost source. Documents correct post-upgrade clearing.
        render_row(&mut terminal, "\u{2764}\u{fe0f}X ");
        render_row(&mut terminal, "abX ");
        assert_eq!(
            backend_row(&terminal, 4).trim_end(),
            "abX",
            "VS16-emoji spacer must be cleared on wide→narrow"
        );
    }

    /// #1027 RED: when `binary_stale` is true, the status bar MUST show
    /// a "daemon binary stale" warning so the operator sees a stable
    /// TUI indicator (replacing the previous inbox-emit path which
    /// targeted agents who cannot act on it).
    #[test]
    fn status_bar_shows_warning_when_binary_stale() {
        let backend = ratatui::backend::TestBackend::new(120, 3);
        let mut terminal =
            ratatui::Terminal::new(backend).expect("test terminal creation should succeed");
        let layout = crate::layout::Layout::new();
        terminal
            .draw(|frame| {
                render_status_bar(
                    frame,
                    frame.area(),
                    &layout,
                    TelegramStatus::NotConfigured,
                    true,
                );
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
            text.contains("daemon binary stale"),
            "binary_stale=true must surface a warning in status bar, got: {text}"
        );
    }

    /// #1027 RED: when `binary_stale` is false, no warning is shown.
    #[test]
    fn status_bar_no_warning_when_binary_fresh() {
        let backend = ratatui::backend::TestBackend::new(120, 3);
        let mut terminal =
            ratatui::Terminal::new(backend).expect("test terminal creation should succeed");
        let layout = crate::layout::Layout::new();
        terminal
            .draw(|frame| {
                render_status_bar(
                    frame,
                    frame.area(),
                    &layout,
                    TelegramStatus::NotConfigured,
                    false,
                );
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
            !text.contains("daemon binary stale"),
            "binary_stale=false must NOT surface warning, got: {text}"
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
        assert_eq!(transient_state_badge(AgentState::Idle), None);
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

    // ── #1071 chrome Clear-widget pre-render tests ──
    //
    // Reviewer RC0 caught that `Block::default().style(...)` does NOT clear
    // cells (verified in ratatui-0.29 source: Block::render_ref calls
    // `buf.set_style` which is style-only). RC1 uses `Clear` widget which
    // calls `cell.reset()` per cell — properly blanking before Paragraph
    // overlay. Tests pin the production wiring at 3 chrome sites:
    //   1. render_tab_bar
    //   2. render_status_bar (single Clear before both left + right bars)
    //   3. "No agents" fallback Paragraph
    //
    // T1/T1c/T2 are STRUCTURAL pins (include_str + grep) — they FAIL on
    // pre-#1071 source and PASS post-fix. T3 is a behavioral sanity test
    // documenting the Clear-then-Paragraph contract (passes both ways).
    // T4 references #1064/#1066 VTerm pre-fill (already shipped). T5
    // documents the AGEND_RENDER_DEBUG diagnostic separately per reviewer.

    /// T1 (#1071 RED): render_tab_bar source must `render_widget(Clear, …)`
    /// before the Paragraph render. Pre-fix code only has the bare
    /// Paragraph render at this site.
    #[test]
    fn render_tab_bar_uses_clear_widget_pre_render() {
        let source = include_str!("core_render.rs");
        let prod_end = source
            .find("#[cfg(test)]")
            .expect("core_render.rs must have a #[cfg(test)] tests module");
        let prod_src = &source[..prod_end];
        let tab_bar_start = prod_src
            .find("fn render_tab_bar")
            .expect("fn render_tab_bar must exist");
        let tab_bar_end = prod_src[tab_bar_start..]
            .find("\nfn ")
            .map(|i| tab_bar_start + i)
            .unwrap_or(prod_src.len());
        let body = &prod_src[tab_bar_start..tab_bar_end];
        assert!(
            body.contains("Clear"),
            "#1071 invariant: render_tab_bar must render Clear widget before Paragraph"
        );
    }

    /// T1c (#1071 RED, dev-2 nit): "No agents" fallback Paragraph must be
    /// preceded by Clear render. Pre-fix code has only the fallback
    /// Paragraph render with no Clear.
    #[test]
    fn no_agents_fallback_uses_clear_widget_pre_render() {
        let source = include_str!("core_render.rs");
        let prod_end = source
            .find("#[cfg(test)]")
            .expect("core_render.rs must have a #[cfg(test)] tests module");
        let prod_src = &source[..prod_end];
        // The fallback Paragraph contains a distinctive literal.
        let no_agents_pos = prod_src
            .find("No agents. Press Ctrl+B c to create a new tab.")
            .expect("\"No agents.\" fallback must exist");
        // Search backwards for the preceding fn boundary to scope the body.
        let scope_start = prod_src[..no_agents_pos]
            .rfind("None => {")
            .expect("fallback must be inside the None branch");
        let scope_end =
            no_agents_pos + "No agents. Press Ctrl+B c to create a new tab.".len() + 200; // enough room past the Paragraph render to catch the call sequence
        let body = &prod_src[scope_start..scope_end.min(prod_src.len())];
        assert!(
            body.contains("Clear"),
            "#1071 invariant: \"No agents\" fallback must render Clear widget before fallback Paragraph"
        );
    }

    /// T2 (#1071 RED): render_status_bar source must `render_widget(Clear,
    /// …)` before any Paragraph render. Pre-fix code has only the
    /// left+right bar Paragraphs with no Clear.
    #[test]
    fn render_status_bar_uses_clear_widget_pre_render() {
        let source = include_str!("core_render.rs");
        let prod_end = source
            .find("#[cfg(test)]")
            .expect("core_render.rs must have a #[cfg(test)] tests module");
        let prod_src = &source[..prod_end];
        let status_start = prod_src
            .find("fn render_status_bar")
            .expect("fn render_status_bar must exist");
        let status_end = prod_src[status_start..]
            .find("\nfn ")
            .map(|i| status_start + i)
            .unwrap_or(prod_src.len());
        let body = &prod_src[status_start..status_end];
        assert!(
            body.contains("Clear"),
            "#1071 invariant: render_status_bar must render Clear widget before Paragraph(s)"
        );
        // Also pin: Clear must appear BEFORE the first Paragraph render.
        let clear_pos = body.find("Clear").unwrap_or(body.len());
        let first_para = body
            .find("Paragraph::new")
            .expect("status bar must construct a Paragraph");
        assert!(
            clear_pos < first_para,
            "#1071 invariant: Clear must precede Paragraph render in status_bar; \
             got Clear at {clear_pos}, first Paragraph at {first_para}"
        );
    }

    /// T3 (#1071 sanity): Clear widget followed by Paragraph blanks
    /// pre-poisoned cells outside the Paragraph's span content. Documents
    /// the contract; passes both pre-fix and post-fix.
    #[test]
    fn clear_then_paragraph_blanks_residual_outside_spans() {
        use ratatui::widgets::{Clear, Widget};
        let area = Rect::new(0, 0, 30, 1);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        for x in 0..30 {
            buf[(x, 0)].set_char('X');
        }
        Widget::render(Clear, area, &mut buf);
        let para = Paragraph::new(Line::from(vec![Span::raw("short")]))
            .style(Style::default().bg(Color::DarkGray));
        Widget::render(para, area, &mut buf);
        let tail: String = (5..30).map(|x| buf[(x, 0)].symbol()).collect();
        let expected: String = " ".repeat(25);
        assert_eq!(
            tail, expected,
            "Clear+Paragraph must blank trailing cells, got: {tail:?}"
        );
    }

    // T4 (#1071 reference): VTerm body residual is locked by PR #1066
    // (#1064 fix) via `src/vterm.rs::tests::area_taller_than_grid_*` + siblings.
    // Chrome layer (this PR) and VTerm body layer cover disjoint regions:
    // tab bar at top row, status bar at bottom row, VTerm body in the middle
    // Min(1) region. No new test added here.

    // T5 (#1071 separate concern per reviewer): AGEND_RENDER_DEBUG diagnostic
    // env flag is preserved as standalone debug-gate, NOT part of the main fix.
    // Operator can run the daemon with `AGEND_RENDER_DEBUG=1` to call
    // `terminal.clear()` before each draw, distinguishing chrome-buffer-level
    // residual (would disappear under the diagnostic) from alacritty-grid-level
    // residual (would persist; points at H8 backend partial-redraw class).
    #[test]
    #[ignore = "diagnostic env-gated; runs manually with AGEND_RENDER_DEBUG=1"]
    fn render_debug_env_diagnostic_documented() {
        // Placeholder: the diagnostic gate is implementation-tracked as a
        // separate concern. This test documents the protocol for future
        // wiring; runs only with `cargo test -- --ignored` against a daemon
        // spun up with the env set.
    }

    /// Regression for the post-#2346 residual freeze: the per-frame render state
    /// snapshot must read each agent's state via the lock-free published mirror,
    /// NOT `core.lock()`. Under the boot PTY flood the core lock is held multi-ms
    /// by each `pty_read_loop` feed; when the snapshot took it, the render loop
    /// (and thus input) stalled up to ~10 ms/frame. Here we hold an agent's core
    /// lock on a background thread and assert the snapshot still returns promptly
    /// AND with the correct published state. If `build_agent_state_snapshot` ever
    /// reverts to `core.lock().state.get_state()`, this blocks ~200 ms and fails.
    ///
    /// `#[cfg(unix)]`: the only registry-handle builder, `agent::mk_test_handle`,
    /// is `#[cfg(all(test, unix))]` (real openpty + `true`), so this test is
    /// unix-only. The lock-free property itself is platform-agnostic; the state
    /// unit tests (`agentstate_u8_roundtrip`, `published_mirror_tracks_current_…`)
    /// cover the mirror cross-platform.
    #[cfg(unix)]
    #[test]
    fn snapshot_reads_published_state_without_core_lock() {
        let id = crate::types::InstanceId::default();
        let handle = crate::agent::mk_test_handle("agent", id);
        // Drive a real transition through record_set so the published mirror moves
        // off its initial value.
        handle.core.lock().state.set_restarting();
        let core = std::sync::Arc::clone(&handle.core);

        let registry: AgentRegistry =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::from([
                (id, handle),
            ])));

        let pane = Pane {
            agent_name: "agent".into(),
            instance_id: id,
            vterm: VTerm::new(38, 11),
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
        let mut layout = Layout::new();
        layout.add_tab(crate::layout::Tab::new("agent".to_string(), pane));

        // Hold the agent's core lock on a background thread; signal once held so
        // the timing assertion is deterministic (no sleep-race).
        let (tx, rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _g = core.lock();
            tx.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(200));
        });
        rx.recv().unwrap(); // core lock is now held

        let t0 = std::time::Instant::now();
        let snap = build_agent_state_snapshot(&layout, &registry);
        let elapsed = t0.elapsed();

        assert_eq!(
            snap.get("agent"),
            Some(&AgentState::Restarting),
            "snapshot must report the published state"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "snapshot blocked {elapsed:?} on a held core.lock — it must read the \
             lock-free published mirror, not take core.lock()"
        );
        holder.join().unwrap();
    }

    /// #freeze-2: the render loop re-arms `dirty` on this when a budget-capped
    /// `drain_output` leaves a backlog — so it MUST report a visible pane's queued
    /// rx (else the backlog stalls; correctness rule ①). Cross-platform (no PTY).
    #[test]
    fn active_tab_has_pending_output_reflects_visible_pane_queue() {
        let (tx, rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let pane = Pane {
            agent_name: "agent".into(),
            instance_id: crate::types::InstanceId::default(),
            vterm: VTerm::new(38, 11),
            rx,
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
        let mut layout = Layout::new();
        layout.add_tab(crate::layout::Tab::new("agent".to_string(), pane));

        // Empty channel → nothing to re-arm.
        assert!(!active_tab_has_pending_output(&layout));
        // Queued output on the visible pane → re-arm so the next frame drains it.
        tx.send(b"backlog".to_vec()).unwrap();
        assert!(active_tab_has_pending_output(&layout));
    }

    fn pane_with_rx(id: usize, rx: crossbeam_channel::Receiver<Vec<u8>>) -> Pane {
        Pane {
            agent_name: "agent".into(),
            instance_id: crate::types::InstanceId::default(),
            vterm: VTerm::new(38, 11),
            rx,
            id,
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
        }
    }

    /// #freeze-3 H2 MECHANISM: the per-frame drain re-arm
    /// (`active_tab_has_pending_output`, the render loop's signal to keep the
    /// VISIBLE catch-up going) only sees the ACTIVE tab. A backgrounded tab's
    /// backlog therefore never re-arms a *redraw* — correct, since hidden tabs need
    /// no redraw. (Background draining is the job of `drain_all_panes`, gated below;
    /// this test pins the re-arm's active-only scope so the two stay decoupled.)
    /// Cross-platform.
    #[test]
    fn rearm_ignores_backgrounded_tab_backlog_freeze3() {
        let (_tx0, rx0) = crossbeam_channel::unbounded::<Vec<u8>>();
        let (tx1, rx1) = crossbeam_channel::unbounded::<Vec<u8>>();
        let mut layout = Layout::new();
        layout.add_tab(crate::layout::Tab::new(
            "t0".to_string(),
            pane_with_rx(1, rx0),
        ));
        layout.add_tab(crate::layout::Tab::new(
            "t1".to_string(),
            pane_with_rx(2, rx1),
        ));
        layout.goto_tab(0); // add_tab focuses the new tab; make t0 the active one.

        // active = 0 (t0). Backlog ONLY on the BACKGROUND tab (t1).
        tx1.send(b"backlog".to_vec()).unwrap();
        assert!(
            !active_tab_has_pending_output(&layout),
            "a backgrounded tab's backlog must NOT re-arm a redraw — its catch-up is \
             invisible; it is drained by `drain_all_panes`, not the redraw re-arm"
        );

        // Switch to t1 → its backlog is now the active tab's → the re-arm fires.
        layout.goto_tab(1);
        assert!(
            active_tab_has_pending_output(&layout),
            "switching to the backlogged tab makes it active → re-arm fires (redraw)"
        );
    }

    /// #freeze-3 ROOT-FIX GATE: `drain_all_panes` must drain a BACKGROUND tab's
    /// pane too (not just the active tab), so a backgrounded busy pane's `rx`
    /// converges to EMPTY over a BOUNDED number of frames instead of accumulating
    /// unbounded. This is the fix for the residual switch-time catch-up #2385 left:
    /// pre-fix only the active tab drained, so switching to a long-backgrounded tab
    /// replayed `ceil(backlog / budget)` frames (∝ background duration). RED if
    /// `drain_all_panes` skipped background tabs (the bug): the background `rx`
    /// would never drain and the loop below would not converge. Cross-platform.
    #[test]
    fn drain_all_panes_bounds_background_rx_freeze3() {
        const CHUNK: usize = 4 * 1024; // PTY-read-sized chunk

        let (_tx0, rx0) = crossbeam_channel::unbounded::<Vec<u8>>();
        let (tx1, rx1) = crossbeam_channel::unbounded::<Vec<u8>>();
        let mut layout = Layout::new();
        layout.add_tab(crate::layout::Tab::new(
            "t0".to_string(),
            pane_with_rx(1, rx0),
        ));
        layout.add_tab(crate::layout::Tab::new(
            "t1".to_string(),
            pane_with_rx(2, rx1),
        ));
        layout.goto_tab(0); // t0 ACTIVE (idle), t1 BACKGROUND (flooded).

        // Flood the BACKGROUND tab with a backlog the active render path never sees.
        let backlog = 512 * 1024;
        let chunks = backlog / CHUNK; // 128 chunks
        for _ in 0..chunks {
            tx1.send(vec![b'x'; CHUNK]).unwrap();
        }
        let bg_rx_len = |layout: &Layout| -> usize {
            layout.tabs[1]
                .root()
                .find_pane(2)
                .map(|p| p.rx.len())
                .unwrap_or(0)
        };
        assert_eq!(
            bg_rx_len(&layout),
            chunks,
            "precondition: the whole backlog is queued on the background pane"
        );

        // Each `drain_all_panes` call == one render frame. The background pane MUST
        // drain down to empty over a bounded number of frames (the fix), not stay
        // queued (the bug). The active tab is idle and leaves the shared budget, so
        // t1 drains DRAIN_OUTPUT_BUDGET_BYTES (32 KiB = 8 chunks) per frame.
        let mut frames = 0usize;
        let mut prev = bg_rx_len(&layout);
        loop {
            let more = drain_all_panes(&mut layout);
            frames += 1;
            assert!(
                frames < 1_000,
                "background backlog must converge to drained (it did not — the bug: \
                 a background tab is never drained, so its rx grows unbounded)"
            );
            let now = bg_rx_len(&layout);
            assert!(
                now < prev,
                "each frame must make progress draining the background pane \
                 (prev={prev} now={now})"
            );
            prev = now;
            if !more {
                break;
            }
        }
        assert_eq!(
            bg_rx_len(&layout),
            0,
            "after convergence the background rx is empty → switching to it shows \
             no catch-up"
        );
        assert_eq!(
            frames, 16,
            "512 KiB backlog drains in ceil(512KiB / 32KiB) = 16 bounded frames at \
             the per-pane budget (the backlog itself is now bounded because draining \
             runs every frame for background tabs too)"
        );
    }
}
