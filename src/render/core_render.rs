//! Core rendering: main entry point, tab bar, status bar, pane tree.

use std::collections::HashMap;
use std::time::{Duration, Instant};

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

pub fn state_color(state: AgentState) -> Color {
    match state {
        AgentState::Starting => Color::White,
        AgentState::AwaitingOperator => Color::Indexed(214),
        AgentState::Idle => Color::DarkGray,
        AgentState::Active => Color::Yellow,
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

// ── dim non-focused panes (t-…50430, version b) ────────────────────────────────────────
//
// Blend every cell of a NON-focused pane's content toward the (dark) terminal background so
// the focused pane stands out. RGB blend (not `Modifier::DIM`, which is terminal-dependent
// and weak) → guaranteed visible + cross-terminal consistent. Applied as a post-render
// per-cell buffer rewrite, mirroring the selection-highlight loop in `render_pane`.

/// Fraction each non-focused cell's colours move toward the terminal background. 0.5 = a
/// clear "dimmed" look that stays readable (white text → mid-grey). Tunable.
const DIM_BLEND: f32 = 0.5;
/// Assumed terminal background (dark) — the blend target. agend fleet terminals are dark;
/// blending toward black makes non-focused content recede uniformly.
const TERM_BG_RGB: (u8, u8, u8) = (0, 0, 0);
/// Assumed default foreground for a `Reset` fg cell (the dominant case — most terminal text
/// uses the default fg). Resolving it lets that text actually dim, not just the rarer
/// explicitly-coloured cells.
const TERM_FG_RGB: (u8, u8, u8) = (204, 204, 204);

/// Linear blend of `c` a `factor` of the way toward `target` (per channel).
fn blend_rgb(c: (u8, u8, u8), target: (u8, u8, u8), factor: f32) -> (u8, u8, u8) {
    let f = factor.clamp(0.0, 1.0);
    let lerp = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * f).round() as u8;
    (
        lerp(c.0, target.0),
        lerp(c.1, target.1),
        lerp(c.2, target.2),
    )
}

/// Resolve a 256-colour index to RGB (standard xterm palette: 0-15 system, 16-231 the
/// 6×6×6 cube, 232-255 the grayscale ramp).
fn indexed_rgb(i: u8) -> (u8, u8, u8) {
    const SYS: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (128, 0, 0),
        (0, 128, 0),
        (128, 128, 0),
        (0, 0, 128),
        (128, 0, 128),
        (0, 128, 128),
        (192, 192, 192),
        (128, 128, 128),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (0, 0, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    match i {
        0..=15 => SYS[i as usize],
        16..=231 => {
            let i = i - 16;
            let conv = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            (conv(i / 36), conv((i % 36) / 6), conv(i % 6))
        }
        232..=255 => {
            let v = 8 + (i - 232) * 10;
            (v, v, v)
        }
    }
}

/// Resolve a ratatui [`Color`] to RGB for blending. `is_fg` decides how `Reset` resolves:
/// a `Reset` fg is the terminal's default foreground (so it can dim), while a `Reset` bg is
/// already the terminal background (the blend target) → `None`, leave it untouched.
fn color_to_rgb(c: Color, is_fg: bool) -> Option<(u8, u8, u8)> {
    Some(match c {
        Color::Reset => {
            if is_fg {
                TERM_FG_RGB
            } else {
                return None;
            }
        }
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Indexed(i) => indexed_rgb(i),
        Color::Black => (0, 0, 0),
        Color::Red => (128, 0, 0),
        Color::Green => (0, 128, 0),
        Color::Yellow => (128, 128, 0),
        Color::Blue => (0, 0, 128),
        Color::Magenta => (128, 0, 128),
        Color::Cyan => (0, 128, 128),
        Color::Gray => (192, 192, 192),
        Color::DarkGray => (128, 128, 128),
        Color::LightRed => (255, 0, 0),
        Color::LightGreen => (0, 255, 0),
        Color::LightYellow => (255, 255, 0),
        Color::LightBlue => (0, 0, 255),
        Color::LightMagenta => (255, 0, 255),
        Color::LightCyan => (0, 255, 255),
        Color::White => (255, 255, 255),
    })
}

/// Dim one colour toward the terminal background, or leave it unchanged when it is already
/// the background (a `Reset` bg).
fn dim_color(c: Color, is_fg: bool) -> Color {
    match color_to_rgb(c, is_fg) {
        Some(rgb) => {
            let (r, g, b) = blend_rgb(rgb, TERM_BG_RGB, DIM_BLEND);
            Color::Rgb(r, g, b)
        }
        None => c,
    }
}

/// Blend every cell in `inner` toward the terminal background — the version-b dim for a
/// non-focused pane. Per-cell buffer rewrite (same shape as the selection-highlight loop).
/// Takes `&mut Buffer` (not `&mut Frame`) so it is directly unit-testable with
/// `Buffer::empty`; the call site passes `frame.buffer_mut()`.
fn dim_pane_content(buf: &mut ratatui::buffer::Buffer, inner: Rect) {
    for y in inner.y..inner.y.saturating_add(inner.height) {
        for x in inner.x..inner.x.saturating_add(inner.width) {
            let cell = &mut buf[(x, y)];
            let fg = dim_color(cell.fg, true);
            let bg = dim_color(cell.bg, false);
            cell.set_fg(fg);
            cell.set_bg(bg);
        }
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

/// #freeze-4: per-pane byte budget for the BOOT-phase drain. Generous (8× the
/// steady per-pane budget) so a restart flood clears in few loading frames; the
/// real limiter is the per-frame TIME cap below, this just bounds how far a single
/// pane can run past that cap (≈ one pane's worth of `DUMP_CHUNK_BYTES` chunks).
const DRAIN_BOOT_PER_PANE_BUDGET_BYTES: usize = 8 * DRAIN_OUTPUT_BUDGET_BYTES;

/// #freeze-4 (t-…2324) BOOT-phase drain: drain panes active-first until everything
/// is empty OR `time_cap` elapses; returns `true` if any backlog remains. Unlike
/// the steady-state byte-capped [`drain_all_panes`] (#2385/#2393, left untouched),
/// this uses a per-frame TIME budget — the clock is checked between panes and the
/// pass stops once `time_cap` is spent.
///
/// The TIME cap is the load-bearing safety: it GUARANTEES the render loop returns
/// to `select!` to service input every frame, so a restart flood can NEVER hard-
/// freeze input regardless of backlog size — worst case the bounded boot/loading
/// phase just lasts a few more frames. Used only inside the bounded boot window
/// (see the render loop's `booting` state); steady state is unchanged.
pub fn drain_all_panes_until(layout: &mut Layout, time_cap: Duration) -> bool {
    let start = Instant::now();
    let active = layout.active;
    let mut more = false;
    let order = std::iter::once(active).chain((0..layout.tabs.len()).filter(move |&i| i != active));
    for tab_idx in order {
        let Some(tab) = layout.tabs.get_mut(tab_idx) else {
            continue;
        };
        for id in tab.root().pane_ids() {
            let Some(pane) = tab.root_mut().find_pane_mut(id) else {
                continue;
            };
            pane.drain_output(DRAIN_BOOT_PER_PANE_BUDGET_BYTES);
            if !pane.rx.is_empty() {
                more = true;
            }
            // Yield after each pane once the per-frame time budget is spent so the
            // loop services input; remaining panes/backlog drain next boot frame.
            if start.elapsed() >= time_cap {
                return true;
            }
        }
    }
    more
}

/// #freeze-4: a small top-centered "loading" notice shown during the bounded boot
/// catch-up phase, so the restart flood reads as a load (with progress) rather than
/// a freeze. `applied`/`expected` = attaches completed / total deferred attaches.
pub fn render_boot_indicator(frame: &mut Frame, applied: usize, expected: usize) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let text = if expected > 0 {
        format!(" loading — attaching {applied}/{expected} agents… ")
    } else {
        " loading… ".to_string()
    };
    let w = (text.chars().count() as u16).min(area.width);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y,
        width: w,
        height: 1,
    };
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(text).alignment(Alignment::Center).style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        rect,
    );
}

fn build_agent_state_snapshot(
    layout: &Layout,
    registry: &AgentRegistry,
) -> HashMap<String, AgentState> {
    let reg = agent::lock_registry(registry);
    // #2413 (A): show the Shadow Observer's high-confidence badge correction in place
    // of the raw screen state, unless the operator turned it off (`:set observed_badge
    // off`) or the whole observer is killed (`AGEND_SHADOW_OBSERVER=0`). Computed ONCE
    // per frame (not per pane); both reads are cheap but neither belongs in the loop.
    let show_observed =
        crate::runtime_config::get().observed_badge && crate::daemon::shadow::enabled();
    let mut snapshot = HashMap::new();
    for tab in &layout.tabs {
        for id in tab.root().pane_ids() {
            if let Some(pane) = tab.root().find_pane(id) {
                if pane.backend.is_some() {
                    snapshot
                        .entry(pane.agent_name.to_string())
                        .or_insert_with(|| {
                            reg.get(&pane.instance_id)
                                .map(|h| observed_or_raw_state(h, show_observed))
                                .unwrap_or(AgentState::Idle)
                        });
                }
            }
        }
    }
    snapshot
}

/// #2413 (A): the badge state for one agent — the Shadow Observer's high-confidence
/// correction (`published_observed`) when `show_observed` AND a correction is published,
/// else the raw screen-scrape state (`published_state`).
///
/// Both reads are lock-free `Relaxed` `AtomicU8` loads — NO `core.lock()`. Under the
/// boot PTY flood the per-agent core lock is held 2–6 ms by each `pty_read_loop` feed;
/// taking it here (once per pane, every frame) made the render snapshot wait up to
/// ~10 ms and starved input. Both atomics are written in lockstep by their writers
/// (`record_set` for `published_state`; the per-tick `shadow_observe` driver for
/// `published_observed`), so the render path stays contention-free.
fn observed_or_raw_state(h: &agent::AgentHandle, show_observed: bool) -> AgentState {
    use std::sync::atomic::Ordering::Relaxed;
    if show_observed {
        if let Some(corrected) = AgentState::from_observed_u8(h.published_observed.load(Relaxed)) {
            return corrected;
        }
    }
    AgentState::from_u8(h.published_state.load(Relaxed))
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

/// SYNC: per-tab width must match `tab_bar_hit_test()` in app/mouse.rs. Both derive
/// the label from `Tab::tab_bar_label`; the only other widths are the `*` dot (1), the
/// inter-tab separator (1), and the trailing ` [+] ` (5).
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

        let label = tab.tab_bar_label(is_active);

        spans.push(dot);
        spans.push(Span::styled(label, style));
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
    let render_offset = pane.scroll_offset;
    // The cursor source differs by render path (live VTerm vs off-thread
    // snapshot); capture it here so the focused-cursor block below stays shared.
    let cursor = if let Some(handle) = &pane.offthread {
        // Option X (off-thread parse, flag AGEND_OFFTHREAD_PARSE): the parser
        // thread owns the VTerm and publishes an immutable snapshot. The main
        // thread does ZERO parse here — it loads the latest snapshot, paints it,
        // and routes any resize to the parser thread (which owns the VTerm). The
        // snapshot carries a bounded scrollback window, so `render_offset` scrolls
        // it like the live path (#offthread-scroll; depth = SNAPSHOT_SCROLLBACK_ROWS).
        let snap = handle.load();
        if let Some(d) = crate::render::resize::ResizeDecision::needed(inner, snap.cols, snap.rows)
        {
            // #2419 (r6 Finding 2): fire SIGWINCH only when the resize actually changed
            // the parser's dims. `request_resize` is a synchronous barrier (blocks until
            // the parser is at the new dims) AND deduped on `last_sent_dims` — when dims
            // are unchanged it returns `false` WITHOUT sending or blocking, so a
            // steady-state frame neither stalls here nor re-SIGWINCHes the child. The old
            // unconditional `resize_pty` re-triggered a child redraw EVERY frame until the
            // snapshot caught up, repeatedly feeding fresh full-width output into the
            // resize race it was meant to settle.
            if handle.request_resize(d.cols, d.rows) {
                pane.resize_pty(registry, d.cols, d.rows);
            }
        }
        snap.render_to_buffer(frame.buffer_mut(), inner, render_offset, !focused);
        snap.cursor
    } else {
        if let Some(d) = crate::render::resize::ResizeDecision::needed(
            inner,
            pane.vterm.cols(),
            pane.vterm.rows(),
        ) {
            pane.vterm.resize(d.cols, d.rows);
            pane.resize_pty(registry, d.cols, d.rows);
        }
        pane.vterm
            .render_to_buffer(frame.buffer_mut(), inner, render_offset, !focused);
        pane.vterm.cursor_pos()
    };

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

    // Dim a NON-focused pane's content (version b) so the focused pane stands out at a
    // glance. Toggle: runtime_config.dim_unfocused_panes (default ON). Zoomed mode draws
    // only the focused pane, so `!focused` naturally skips it — no special case needed.
    if !focused && crate::runtime_config::get().dim_unfocused_panes {
        dim_pane_content(frame.buffer_mut(), inner);
    }

    if focused {
        let (cursor_line, cursor_col) = cursor;
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
    // `title_style` so it introduces no new colour.
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
            offthread: None,
            _fwd_cancel: None,
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
            offthread: None,
            _fwd_cancel: None,
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
            offthread: None,
            _fwd_cancel: None,
        };
        for (state, want) in [
            (AgentState::ServerRateLimit, "[ServerRateLimit]"),
            (AgentState::PermissionPrompt, "[PermissionPrompt]"),
            (AgentState::Active, "[Active]"),
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
        let active = state_color(AgentState::Active);
        assert_ne!(idle, active, "idle vs active must differ");
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
                offthread: None,
                _fwd_cancel: None,
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
            offthread: None,
            _fwd_cancel: None,
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

    /// Option X (S3 wiring): when `pane.offthread = Some`, `render_pane` paints the
    /// parser thread's published snapshot — NOT the (idle) main-thread `pane.vterm`.
    /// Proven by leaving `pane.vterm` blank and asserting the snapshot's content
    /// reaches the frame buffer. (Pairs with `drain_output_is_noop_when_offthread...`
    /// in layout::pane: together they show the off-thread path renders correctly
    /// while the main thread does zero parse.)
    #[test]
    fn render_paints_offthread_snapshot_not_main_vterm() {
        // Spawn a parser, push known content, and wait for it to publish a snapshot.
        let (data_tx, data_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let (wake_tx, wake_rx) = crossbeam_channel::unbounded::<usize>();
        let handle = crate::render::offthread::spawn_offthread_parser(
            1,
            "t".to_string(),
            data_rx,
            VTerm::new(38, 16),
            wake_tx,
        )
        .expect("parser thread spawns");
        data_tx
            .send(b"\x1b[2J\x1b[HOFFTHREAD_SNAP".to_vec())
            .unwrap();
        wake_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("parser thread must publish a snapshot");

        let backend = ratatui::backend::TestBackend::new(40, 20);
        let mut terminal =
            ratatui::Terminal::new(backend).expect("test terminal creation should succeed");
        let registry: AgentRegistry =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));

        let pane = Pane {
            agent_name: "agent".into(),
            instance_id: crate::types::InstanceId::default(),
            // Blank/idle — the render source MUST be the snapshot, not this VTerm.
            vterm: VTerm::new(38, 16),
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
            offthread: Some(handle),
            _fwd_cancel: None,
        };
        assert!(
            pane.vterm.tail_lines(16).trim().is_empty(),
            "sanity: the main-thread VTerm starts blank"
        );

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

        let buf = terminal.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                text.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
            }
        }
        assert!(
            text.contains("OFFTHREAD_SNAP"),
            "render must paint the off-thread snapshot content, not the blank main VTerm; frame: {text:?}"
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

    /// #2413 (A): the badge picks the Shadow Observer correction over the raw state
    /// ONLY when `show_observed` AND a correction is published; otherwise the raw
    /// `published_state` wins. Pins all three branches of `observed_or_raw_state` —
    /// the lock-free read the render snapshot uses. `#[cfg(unix)]` (mk_test_handle).
    #[cfg(unix)]
    #[test]
    fn observed_or_raw_state_prefers_correction_only_when_enabled() {
        let id = crate::types::InstanceId::default();
        let handle = crate::agent::mk_test_handle("agent", id);
        // Raw screen state = Restarting (via record_set); a published badge override.
        handle.core.lock().state.set_restarting();
        handle
            .core
            .lock()
            .state
            .publish_observed(Some(AgentState::Active));

        // Toggle ON ⇒ the high-confidence correction wins.
        assert_eq!(observed_or_raw_state(&handle, true), AgentState::Active);
        // Toggle OFF ⇒ the raw state wins (operator opted out / observer killed).
        assert_eq!(
            observed_or_raw_state(&handle, false),
            AgentState::Restarting
        );

        // No correction published (sentinel) ⇒ raw state even when enabled.
        handle.core.lock().state.publish_observed(None);
        assert_eq!(observed_or_raw_state(&handle, true), AgentState::Restarting);
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
            offthread: None,
            _fwd_cancel: None,
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
            offthread: None,
            _fwd_cancel: None,
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
            offthread: None,
            _fwd_cancel: None,
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

    // ── #freeze-4 restart-flood boot-phase drain ──────────────────────────

    /// Build N tabs, each pane flooded with `chunks_per_pane` × 4 KiB (a restart
    /// flood). Senders are returned so the caller keeps them alive (rx stays
    /// connected). Tab 0 is active.
    fn flooded_layout(
        n: usize,
        chunks_per_pane: usize,
    ) -> (Layout, Vec<crossbeam_channel::Sender<Vec<u8>>>) {
        const CHUNK: usize = 4 * 1024;
        let mut layout = Layout::new();
        let mut txs = Vec::new();
        for id in 1..=n {
            let (tx, rx) = crossbeam_channel::unbounded::<Vec<u8>>();
            for _ in 0..chunks_per_pane {
                tx.send(vec![b'x'; CHUNK]).unwrap();
            }
            layout.add_tab(crate::layout::Tab::new(
                format!("t{id}"),
                pane_with_rx(id, rx),
            ));
            txs.push(tx);
        }
        layout.goto_tab(0);
        (layout, txs)
    }

    fn rx_len_of(layout: &Layout, tab_idx: usize, pane_id: usize) -> usize {
        layout.tabs[tab_idx]
            .root()
            .find_pane(pane_id)
            .map(|p| p.rx.len())
            .unwrap_or(0)
    }

    /// #freeze-4 LOAD-BEARING SAFETY: the per-frame TIME cap MUST stop the boot
    /// drain mid-pass so the render loop returns to `select!` to service input every
    /// frame — a restart flood can never hard-freeze input regardless of backlog.
    /// A `Duration::ZERO` cap must yield after the FIRST pane, leaving later panes
    /// UNTOUCHED. RED if the time-cap check is removed (neutered): the pass would
    /// drain every pane in one call and the untouched assertion fails. Cross-platform.
    #[test]
    fn drain_all_panes_until_time_cap_yields_after_bounded_work_freeze4() {
        // 100 × 4 KiB = 400 KiB/pane, larger than the boot per-pane budget so a pane
        // can't be drained to empty "for free" in a single visit.
        let (mut layout, txs) = flooded_layout(3, 100);

        let more = drain_all_panes_until(&mut layout, Duration::ZERO);
        assert!(
            more,
            "a ZERO time-cap with backlog remaining must report more pending"
        );
        // The LAST pane in drain order (tab 2, id 3) must still hold its FULL backlog
        // — the ZERO cap stopped the pass long before reaching it.
        assert_eq!(
            rx_len_of(&layout, 2, 3),
            100,
            "ZERO time-cap must yield before draining every pane (without the cap, \
             all panes drain in one call → input would be starved)"
        );
        drop(txs);
    }

    /// #freeze-4: given time (a generous cap, as the bounded boot window provides),
    /// the boot drain clears the WHOLE restart flood across active + background tabs
    /// in a bounded number of frames → after the boot phase no pane carries backlog
    /// into interactive use.
    #[test]
    fn drain_all_panes_until_clears_whole_flood_when_uncapped_freeze4() {
        let (mut layout, txs) = flooded_layout(3, 100);

        let mut frames = 0usize;
        loop {
            let more = drain_all_panes_until(&mut layout, Duration::from_secs(30));
            frames += 1;
            assert!(frames < 1000, "boot catch-up must converge");
            if !more {
                break;
            }
        }
        for (tab_idx, pane_id) in [(0usize, 1usize), (1, 2), (2, 3)] {
            assert_eq!(
                rx_len_of(&layout, tab_idx, pane_id),
                0,
                "every pane's restart backlog must be fully drained in the boot phase \
                 (tab {tab_idx} pane {pane_id})"
            );
        }
        drop(txs);
    }

    // ── dim non-focused panes (version b) ──────────────────────────────────────────────

    /// blend-fn + colour-resolution determinism, incl. the confirm-first readability point
    /// (a dimmed default-fg cell is a clearly-visible mid-grey, not black).
    #[test]
    fn dim_blend_and_color_resolution_are_deterministic() {
        // RGB blend toward black at 0.5 = halfway (rounded).
        assert_eq!(
            blend_rgb((204, 204, 204), TERM_BG_RGB, 0.5),
            (102, 102, 102)
        );
        assert_eq!(
            blend_rgb((255, 255, 255), TERM_BG_RGB, 0.5),
            (128, 128, 128)
        );
        assert_eq!(blend_rgb((200, 100, 50), TERM_BG_RGB, 0.5), (100, 50, 25));
        assert_eq!(blend_rgb((10, 20, 30), TERM_BG_RGB, 0.0), (10, 20, 30)); // factor 0 = identity

        // Named/Indexed/Reset → RGB resolution.
        assert_eq!(color_to_rgb(Color::White, true), Some((255, 255, 255)));
        assert_eq!(color_to_rgb(Color::Blue, true), Some((0, 0, 128)));
        assert_eq!(color_to_rgb(Color::Indexed(0), true), Some((0, 0, 0)));
        assert_eq!(
            color_to_rgb(Color::Indexed(15), true),
            Some((255, 255, 255))
        );
        assert_eq!(color_to_rgb(Color::Indexed(196), true), Some((255, 0, 0)));
        assert_eq!(color_to_rgb(Color::Indexed(232), true), Some((8, 8, 8)));
        assert_eq!(
            color_to_rgb(Color::Indexed(255), true),
            Some((238, 238, 238))
        );
        // Reset fg resolves to the assumed default fg (so the dominant text dims); Reset bg
        // is already the background → None (left untouched).
        assert_eq!(color_to_rgb(Color::Reset, true), Some(TERM_FG_RGB));
        assert_eq!(color_to_rgb(Color::Reset, false), None);

        // dim_color end-to-end.
        assert_eq!(
            dim_color(Color::Rgb(200, 100, 50), true),
            Color::Rgb(100, 50, 25)
        );
        assert_eq!(dim_color(Color::Reset, false), Color::Reset); // bg untouched
                                                                  // confirm-first: default text dims to a visible mid-grey, NOT black/invisible.
        let dimmed_default = dim_color(Color::Reset, true);
        assert_eq!(dimmed_default, Color::Rgb(102, 102, 102));
        assert_ne!(
            dimmed_default,
            Color::Rgb(0, 0, 0),
            "must stay readable, not vanish"
        );
    }

    /// Buffer wiring (models a 2-pane layout): `dim_pane_content` blends ONLY the region it
    /// is given (the non-focused pane's inner rect), leaving every other cell — i.e. the
    /// focused pane, which never receives the call — byte-identical. Mirrors how `render_pane`
    /// calls it solely under `!focused`.
    #[test]
    fn dim_pane_content_blends_only_the_nonfocused_region() {
        use ratatui::buffer::Buffer;
        // Two side-by-side 5×3 pane regions: left = focused (x 0..5), right = non-focused.
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 3));
        for y in 0..3u16 {
            for x in 0..10u16 {
                buf[(x, y)].set_fg(Color::White);
                buf[(x, y)].set_bg(Color::Reset);
            }
        }
        // One non-focused cell carries an explicit coloured bg to exercise the bg path.
        buf[(7, 1)].set_bg(Color::Blue);

        let nonfocused = Rect::new(5, 0, 5, 3);
        dim_pane_content(&mut buf, nonfocused);

        // Focused (left) region: untouched.
        for y in 0..3u16 {
            for x in 0..5u16 {
                assert_eq!(
                    buf[(x, y)].fg,
                    Color::White,
                    "focused fg unchanged at {x},{y}"
                );
                assert_eq!(
                    buf[(x, y)].bg,
                    Color::Reset,
                    "focused bg unchanged at {x},{y}"
                );
            }
        }
        // Non-focused (right) region: fg blended toward black; Reset bg left as-is.
        for y in 0..3u16 {
            for x in 5..10u16 {
                assert_eq!(
                    buf[(x, y)].fg,
                    Color::Rgb(128, 128, 128),
                    "non-focused fg blended at {x},{y}"
                );
            }
        }
        assert_eq!(
            buf[(5, 0)].bg,
            Color::Reset,
            "Reset bg stays the terminal background"
        );
        // The explicit Blue bg blended toward black.
        assert_eq!(buf[(7, 1)].bg, Color::Rgb(0, 0, 64), "coloured bg dimmed");
    }
}
