//! Mouse event handling — clicks / drags on the tab bar, pane title bars,
//! split borders, and text selection inside panes. Shared drag state that
//! outlives a single event lives in `MouseState`; per-tab drag state
//! (`dragging_pane`, `drag_target`, `selecting_pane`, `pane.selection`)
//! stays on `Layout::Tab` so it survives renders.

use std::path::Path;

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::agent::AgentRegistry;
use crate::layout::{Layout, SplitBorderHit, SplitDir};

use super::overlay::Overlay;

/// Drag state that persists across mouse events inside a single gesture.
///
/// Per-tab drag state (pane drag source/target, text selection) lives on
/// `Tab` so it is correctly scoped per tab and accessible from render.
#[derive(Default)]
pub(super) struct MouseState {
    /// Active split-border resize. `None` unless the user is mid-drag on a
    /// split border. The cached `Rect` is the pane-area rect at drag start;
    /// reusing it keeps drag math stable even if the terminal is resized
    /// mid-gesture.
    pub border_drag: Option<(SplitBorderHit, Rect)>,
}

/// Signals from mouse handling back to `run_app`. Fields are `Option` /
/// `bool` so callers can check once and fall through without extra state.
#[derive(Default)]
pub(super) struct MouseOutcome {
    /// Layout changed in a way that requires a resize pass.
    pub needs_resize: bool,
    /// Open an overlay (currently only `NewTabMenu` from the `[+]` button).
    pub new_overlay: Option<Overlay>,
    /// Record the previously-active tab for toggle-back support.
    pub new_last_tab: Option<usize>,
}

/// Dispatch one mouse event. Caller must ensure no overlay is active —
/// mouse events while modal is open should be swallowed, not routed here.
pub(super) fn handle(
    mouse: MouseEvent,
    layout: &mut Layout,
    state: &mut MouseState,
    fleet_path: &Path,
    registry: &AgentRegistry,
) -> MouseOutcome {
    let mut out = MouseOutcome::default();
    match mouse.kind {
        MouseEventKind::ScrollUp => super::scroll_focused(layout, 3),
        MouseEventKind::ScrollDown => super::scroll_focused(layout, -3),
        MouseEventKind::Down(MouseButton::Left) => {
            handle_down(mouse, layout, state, fleet_path, registry, &mut out);
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            handle_drag(mouse, layout, state);
        }
        MouseEventKind::Up(MouseButton::Left) => {
            handle_up(mouse, layout, state, &mut out);
        }
        _ => {}
    }
    out
}

fn handle_down(
    mouse: MouseEvent,
    layout: &mut Layout,
    state: &mut MouseState,
    fleet_path: &Path,
    registry: &AgentRegistry,
    out: &mut MouseOutcome,
) {
    if mouse.row == 0 {
        match tab_bar_hit_test(layout, mouse.column) {
            Some(TabBarClick::Tab(idx)) => {
                out.new_last_tab = Some(layout.active);
                layout.goto_tab(idx);
                out.needs_resize = true;
            }
            Some(TabBarClick::NewTab) => {
                out.new_overlay = Some(Overlay::NewTabMenu {
                    items: super::build_menu_items(fleet_path, registry),
                    selected: 0,
                });
            }
            None => {}
        }
        return;
    }

    // Title-bar hit is checked before split-border so that horizontally-stacked
    // panes (whose top border coincides with the split line) can be grabbed
    // for drag-to-swap. Horizontal-split borders must be resized via keyboard.
    let (c, r) = crossterm::terminal::size().unwrap_or((120, 40));
    let pa = Rect::new(0, 1, c, r.saturating_sub(2));
    // When a tab is zoomed, only the focused pane is visible; the split tree
    // still exists but its borders aren't rendered. Disable title-bar AND
    // border hit-tests so users can't drag invisible borders.
    let zoomed = layout.active_tab().is_some_and(|t| t.zoomed);
    let title_hit = (!zoomed)
        .then(|| {
            layout
                .active_tab()
                .and_then(|tab| tab.title_bar_at(mouse.column, mouse.row))
        })
        .flatten();
    if let Some(pane_id) = title_hit {
        if let Some(tab) = layout.active_tab_mut() {
            tab.focus_id = pane_id;
            // Only start a drag when there's a possible swap target;
            // otherwise the source pane briefly flashes magenta for a no-op.
            if tab.root().pane_count() > 1 {
                tab.dragging_pane = Some(pane_id);
                tab.drag_target = None;
            }
        }
    } else if !zoomed {
        let hit = layout.active_tab().and_then(|tab| {
            crate::layout::find_split_border(
                tab.root(),
                (pa.x, pa.y, pa.width, pa.height),
                mouse.column,
                mouse.row,
            )
        });
        if let Some(h) = hit {
            state.border_drag = Some((h, pa));
        } else {
            handle_selection(layout, &mouse);
        }
    } else {
        // Zoomed: only selection inside the one visible pane.
        handle_selection(layout, &mouse);
    }
}

fn handle_drag(mouse: MouseEvent, layout: &mut Layout, state: &mut MouseState) {
    if let Some((ref hit, ref pa)) = state.border_drag {
        let mouse_pos = match hit.dir {
            SplitDir::Horizontal => mouse.row,
            SplitDir::Vertical => mouse.column,
        };
        if let Some(tab) = layout.active_tab_mut() {
            crate::layout::adjust_split_ratio(
                tab.root_mut(),
                (pa.x, pa.y, pa.width, pa.height),
                hit.split_area,
                mouse_pos,
                hit.dir,
            );
        }
        // Don't fire PTY resize per-tick: the render loop recomputes
        // pane_rects from the updated ratio so the drag is visually smooth,
        // but resizing the PTY every mouse cell triggers the backend
        // (Claude/etc.) to reflow its entire UI and floods us with redraw
        // data. Defer the single PTY resize to mouse-up.
    } else if layout.active_tab().is_some_and(|t| t.dragging_pane.is_some()) {
        let target = layout.active_tab().and_then(|tab| {
            let source = tab.dragging_pane?;
            tab.pane_at(mouse.column, mouse.row)
                .filter(|&id| id != source)
        });
        if let Some(tab) = layout.active_tab_mut() {
            tab.drag_target = target;
        }
    } else {
        handle_selection(layout, &mouse);
    }
}

fn handle_up(
    mouse: MouseEvent,
    layout: &mut Layout,
    state: &mut MouseState,
    out: &mut MouseOutcome,
) {
    if state.border_drag.is_some() {
        state.border_drag = None;
        // Ratio was updated live during drag but PTY resizes were deferred
        // — fire one now.
        out.needs_resize = true;
    } else if layout.active_tab().is_some_and(|t| t.dragging_pane.is_some()) {
        let source_id = layout.active_tab().and_then(|t| t.dragging_pane);
        let target_id = layout.active_tab().and_then(|t| t.drag_target);
        if let (Some(src), Some(tgt)) = (source_id, target_id) {
            if let Some(tab) = layout.active_tab_mut() {
                crate::layout::swap_panes(tab.root_mut(), src, tgt);
            }
            out.needs_resize = true;
        }
        if let Some(tab) = layout.active_tab_mut() {
            tab.clear_drag();
        }
    } else {
        handle_selection(layout, &mouse);
    }
}

enum TabBarClick {
    Tab(usize),
    NewTab,
}

/// Hit-test the tab bar at the given column.
/// SYNC: layout math must match render_tab_bar() in render.rs.
fn tab_bar_hit_test(layout: &Layout, col: u16) -> Option<TabBarClick> {
    use unicode_width::UnicodeWidthStr;
    let mut x: u16 = 0;
    for (i, tab) in layout.tabs.iter().enumerate() {
        if i > 0 {
            x += 1;
        } // separator space
        let is_active = i == layout.active;
        let has_notif = tab.root().has_notification();
        let badge = if has_notif && !is_active { " !" } else { "" };
        let label = format!(" {}{badge} ", tab.name);
        let tab_w = 1 + label.width() as u16; // "*" + label
        if col >= x && col < x + tab_w {
            return Some(TabBarClick::Tab(i));
        }
        x += tab_w;
    }
    // " [+] " button
    if col >= x && col < x + 5 {
        return Some(TabBarClick::NewTab);
    }
    None
}

/// Handle mouse selection: down starts, drag extends, up copies to clipboard.
/// Works on any pane (not just focused) by finding the pane under the cursor.
fn handle_selection(layout: &mut Layout, mouse: &MouseEvent) {
    let tab = match layout.active_tab_mut() {
        Some(t) => t,
        None => return,
    };

    // Down: hit-test pane_rects. Drag/Up: use cached selecting_pane.
    // When zoomed, only the focused pane is visible — skip hit-test.
    let target_id = match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if tab.zoomed {
                Some(tab.focus_id)
            } else {
                tab.pane_rects
                    .iter()
                    .find(|(_, &(px, py, pw, ph))| {
                        mouse.column >= px
                            && mouse.column < px + pw
                            && mouse.row >= py
                            && mouse.row < py + ph
                    })
                    .map(|(&id, _)| id)
            }
        }
        _ => tab.selecting_pane,
    };
    let target_id = match target_id {
        Some(id) => id,
        None => return,
    };

    // Focus clicked pane and cache selection target on Down.
    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        tab.focus_id = target_id;
        tab.selecting_pane = Some(target_id);
    }

    let rect = tab.pane_rects.get(&target_id).copied();
    let (px, py, pw, ph) = match rect {
        Some(r) => r,
        None => return,
    };
    let inner_x = px + 1;
    let inner_y = py + 1;
    let inner_w = pw.saturating_sub(2);
    let inner_h = ph.saturating_sub(2);
    if inner_w == 0 || inner_h == 0 {
        return;
    }

    // Scope the pane borrow so we can touch tab.selecting_pane afterwards.
    let finished = {
        let pane = match tab.root_mut().find_pane_mut(target_id) {
            Some(p) => p,
            None => return,
        };
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if mouse.column >= inner_x
                    && mouse.column < inner_x + inner_w
                    && mouse.row >= inner_y
                    && mouse.row < inner_y + inner_h
                {
                    let col = mouse.column - inner_x;
                    let row = mouse.row - inner_y;
                    pane.selection = Some(crate::layout::Selection {
                        start: (row, col),
                        end: (row, col),
                    });
                }
                false
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let col = mouse.column.max(inner_x).min(inner_x + inner_w - 1) - inner_x;
                let row = mouse.row.max(inner_y).min(inner_y + inner_h - 1) - inner_y;
                if let Some(ref mut sel) = pane.selection {
                    sel.end = (row, col);
                }
                false
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(ref sel) = pane.selection {
                    let text = pane
                        .vterm
                        .extract_text(sel.start, sel.end, pane.scroll_offset);
                    if !text.is_empty() {
                        copy_to_clipboard(&text);
                    }
                }
                pane.selection = None;
                true
            }
            _ => false,
        }
    };

    if finished {
        tab.selecting_pane = None;
    }
}

/// Copy text to system clipboard (macOS / Linux / Windows).
fn copy_to_clipboard(text: &str) {
    match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text)) {
        Ok(()) => {}
        Err(e) => tracing::warn!(error = %e, "clipboard copy failed"),
    }
}
