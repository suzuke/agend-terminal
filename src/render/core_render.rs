//! Core rendering: main entry point, tab bar, status bar, pane tree.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::border::{render_border_grid, render_pane_titles};
use crate::agent::{self, AgentRegistry};
use crate::channel::TelegramStatus;
use crate::layout::{DragTabTarget, Layout, PaneNode};
use crate::state::AgentState;
use crate::team_view::{LeadBadge, TeamView};
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

fn render_state_color(state: Option<AgentState>) -> Color {
    state.map(state_color).unwrap_or(Color::LightCyan)
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
) -> HashMap<String, Option<AgentState>> {
    build_agent_state_snapshot_with_remote(layout, registry, None)
}

fn build_agent_state_snapshot_with_remote(
    layout: &Layout,
    registry: &AgentRegistry,
    remote_states: Option<&HashMap<String, Option<AgentState>>>,
) -> HashMap<String, Option<AgentState>> {
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
                            if let Some(remote_states) = remote_states {
                                remote_states
                                    .get(pane.agent_name.as_str())
                                    .cloned()
                                    .unwrap_or(None)
                            } else {
                                reg.get(&pane.instance_id)
                                    .map(|h| Some(observed_or_raw_state(h, show_observed)))
                                    .unwrap_or(None)
                            }
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

#[allow(clippy::too_many_arguments)]
#[allow(dead_code)]
pub fn render(
    frame: &mut Frame,
    layout: &mut Layout,
    repeat_mode: bool,
    registry: &AgentRegistry,
    telegram: TelegramStatus,
    binary_stale: bool,
    pending_decisions: usize,
    daemon_list_mode: crate::runtime::AgentListMode,
    remote_states: Option<&HashMap<String, Option<AgentState>>>,
) {
    render_with_team(
        frame,
        layout,
        repeat_mode,
        registry,
        telegram,
        binary_stale,
        pending_decisions,
        daemon_list_mode,
        remote_states,
        None,
    );
}

#[allow(clippy::too_many_arguments)]
pub fn render_with_team(
    frame: &mut Frame,
    layout: &mut Layout,
    repeat_mode: bool,
    registry: &AgentRegistry,
    telegram: TelegramStatus,
    binary_stale: bool,
    pending_decisions: usize,
    daemon_list_mode: crate::runtime::AgentListMode,
    remote_states: Option<&HashMap<String, Option<AgentState>>>,
    team_view: Option<&TeamView>,
) {
    let chunks = ratatui::layout::Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(crate::layout::TAB_BAR_HEIGHT),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let snapshot = match remote_states {
        Some(remote_states) => {
            build_agent_state_snapshot_with_remote(layout, registry, Some(remote_states))
        }
        None => build_agent_state_snapshot(layout, registry),
    };
    render_pane_tree(
        frame,
        chunks[1],
        layout,
        repeat_mode,
        registry,
        &snapshot,
        team_view,
    );
    render_tab_bar(frame, chunks[0], layout, &snapshot, team_view);
    render_status_bar_with_team(
        frame,
        chunks[2],
        layout,
        telegram,
        binary_stale,
        pending_decisions,
        daemon_list_mode,
        team_view,
    );
}

/// Get the highest-priority state across all panes in a tab.
pub fn highest_priority_state(
    tab: &crate::layout::Tab,
    snapshot: &HashMap<String, Option<AgentState>>,
) -> Option<AgentState> {
    let mut best: Option<AgentState> = None;
    for id in tab.root().pane_ids() {
        if let Some(pane) = tab.root().find_pane(id) {
            if pane.backend.is_some() {
                let Some(s) = snapshot.get(pane.agent_name.as_str()).copied().flatten() else {
                    continue;
                };
                if best.is_none_or(|current| s.priority() > current.priority()) {
                    best = Some(s);
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
    snapshot: &HashMap<String, Option<AgentState>>,
    team_view: Option<&TeamView>,
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
        let sc = render_state_color(state);

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
            Some(
                AgentState::PermissionPrompt
                    | AgentState::InteractivePrompt
                    | AgentState::Hang
                    | AgentState::Restarting
                    | AgentState::AwaitingOperator,
            )
        );
        let dot = if blink {
            Span::styled(
                "*",
                Style::default().fg(sc).add_modifier(Modifier::SLOW_BLINK),
            )
        } else {
            Span::styled("*", Style::default().fg(sc))
        };

        let label = tab.tab_bar_label_with_team(is_active, team_view);

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
    snapshot: &HashMap<String, Option<AgentState>>,
    team_view: Option<&TeamView>,
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
                frame, area, pane, true, false, registry, snapshot, false, false, team_view,
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
        team_view,
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
    snapshot: &HashMap<String, Option<AgentState>>,
    drag_source: Option<usize>,
    drag_target: Option<usize>,
    team_view: Option<&TeamView>,
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
                team_view,
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
                team_view,
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
                team_view,
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
    snapshot: &HashMap<String, Option<AgentState>>,
    is_drag_source: bool,
    is_drag_target: bool,
    team_view: Option<&TeamView>,
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
        snapshot.get(pane.agent_name.as_str()).copied().flatten()
    } else {
        Some(AgentState::Idle)
    };
    let sc = render_state_color(state);

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

    let title_segments = pane_title_segments_with_team(
        pane,
        title_style,
        state,
        crate::runtime_config::get().show_pane_state,
        team_view,
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
    // AUDIT2-017: clamp to the current history depth so a stale offset (after
    // alt-screen entry / zoom / resize / clear) renders the deepest available
    // line instead of blank rows.
    let render_offset = pane.clamped_scroll_offset();
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

#[allow(dead_code)]
pub(super) fn pane_title_segments(
    pane: &crate::layout::Pane,
    title_style: Style,
    state: Option<AgentState>,
    show_state_badge: bool,
) -> Vec<(String, Style)> {
    pane_title_segments_with_team(pane, title_style, state, show_state_badge, None)
}

fn pane_title_segments_with_team(
    pane: &crate::layout::Pane,
    title_style: Style,
    state: Option<AgentState>,
    show_state_badge: bool,
    team_view: Option<&TeamView>,
) -> Vec<(String, Style)> {
    let mut segments = Vec::new();
    let base = format!(" {}", pane.label());
    segments.push((base, title_style));
    if let Some(badge) = pane
        .fleet_instance_name
        .as_deref()
        .zip(team_view)
        .map(|(name, view)| view.badge(name, pane.instance_ref().as_ref()))
    {
        let (text, style) = match badge {
            LeadBadge::Lead => (
                " [LEAD]".to_string(),
                title_style
                    .fg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD),
            ),
            LeadBadge::Uncertain => (
                " [LEAD?]".to_string(),
                title_style.fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            LeadBadge::None => (String::new(), title_style),
        };
        if !text.is_empty() {
            segments.push((text, style));
        }
    }
    if pane.is_disconnected() {
        segments.push((
            " [DISCONNECTED]".to_string(),
            Style::default()
                .bg(Color::Red)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(error) = pane.restart_error() {
        segments.push((
            format!(" [RESTART FAILED: {error}]"),
            Style::default()
                .bg(Color::Red)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if pane.pending_notification_count > 0 {
        segments.push((
            format!(" [{}]", pane.pending_notification_count),
            Style::default()
                .bg(Color::Yellow)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        ));
    }
    // #2313 P2b: marker on the pane of the agent that AUTHORED a
    // still-unanswered decision-board question — lets the operator see WHERE
    // a pending question came from, distinct from the notification badge
    // above (which is about messages TO this pane, not FROM it).
    if pane.pending_decision_count > 0 {
        segments.push((
            " 🔴".to_string(),
            // pane-label-bg-lost: inherit the surrounding title's background
            // (and any other modifier, e.g. the repeat-mode/drag-state
            // overrides above) instead of Style::default() — the marker is
            // one segment in a visually continuous label band, not a
            // standalone badge like the `[N]` notification count above,
            // which deliberately uses its own contrasting bg.
            title_style.fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    // #1713/#1523 diagnostic (runtime_config.show_pane_state, default off): append
    // a `[<State>]` text badge of the DETECTED AgentState so the operator can
    // eyeball-verify detection against the live pane. Extra text only — the
    // pane's colour (state_color) is untouched, and it uses the same
    // `title_style` so it introduces no new colour.
    if show_state_badge || state.is_none() {
        let badge = state.map_or_else(|| "[?]".to_string(), |state| format!("[{state:?}]"));
        segments.push((format!(" {badge}"), title_style));
    }
    segments.push((" ".to_string(), title_style));
    segments
}

#[allow(dead_code)]
pub(super) fn render_status_bar(
    frame: &mut Frame,
    area: Rect,
    layout: &Layout,
    telegram: TelegramStatus,
    binary_stale: bool,
    pending_decisions: usize,
    daemon_list_mode: crate::runtime::AgentListMode,
) {
    render_status_bar_with_team(
        frame,
        area,
        layout,
        telegram,
        binary_stale,
        pending_decisions,
        daemon_list_mode,
        None,
    );
}

#[allow(clippy::too_many_arguments)]
pub(super) fn render_status_bar_with_team(
    frame: &mut Frame,
    area: Rect,
    layout: &Layout,
    telegram: TelegramStatus,
    binary_stale: bool,
    pending_decisions: usize,
    daemon_list_mode: crate::runtime::AgentListMode,
    team_view: Option<&TeamView>,
) {
    super::team_render::render_status_bar_with_team(
        frame,
        area,
        layout,
        telegram,
        binary_stale,
        pending_decisions,
        daemon_list_mode,
        team_view,
    );
}

#[cfg(test)]
#[path = "core_render_team_tests.rs"]
mod team_tests;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "core_render_tests.rs"]
mod tests;
