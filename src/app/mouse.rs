//! Mouse event handling — clicks / drags on the tab bar, pane title bars,
//! split borders, and text selection inside panes. Shared drag state that
//! outlives a single event lives in `MouseState`; per-tab drag state
//! (`dragging_pane`, `drag_target`, `selecting_pane`, `pane.selection`)
//! stays on `Layout::Tab` so it survives renders.

use std::path::Path;

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::agent::AgentRegistry;
use crate::layout::{DragTabTarget, Layout, MovePlacement, SplitBorderHit, SplitDir};

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
    if crate::layout::is_tab_bar_row(mouse.row) {
        match tab_bar_hit_test(layout, mouse.column) {
            Some(TabBarClick::Tab(idx)) => {
                out.new_last_tab = Some(layout.active);
                layout.goto_tab(idx);
                out.needs_resize = true;
                // Start tab drag for reorder
                layout.tab_reorder_source = Some(idx);
                layout.tab_reorder_target = None;
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
            // Allow drag even for single-pane tabs — the user can
            // cross-tab drag to move the pane to another tab.
            tab.dragging_pane = Some(pane_id);
            tab.drag_target = None;
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
    } else if layout.tab_reorder_source.is_some() && crate::layout::is_tab_bar_row(mouse.row) {
        // Tab reorder drag: update drop target
        layout.tab_reorder_target = match tab_bar_hit_test(layout, mouse.column) {
            Some(TabBarClick::Tab(idx)) => Some(idx),
            _ => None,
        };
    } else if layout
        .active_tab()
        .is_some_and(|t| t.dragging_pane.is_some())
    {
        // Pointer on the tab bar (row 0) is a cross-tab drop intent; pointer
        // inside the pane area is an intra-tab swap intent. Set exactly one
        // of `drag_target_tab` / `drag_target` per mouse position so that
        // render + mouse-up dispatch unambiguously.
        if crate::layout::is_tab_bar_row(mouse.row) {
            let hit = tab_bar_hit_test(layout, mouse.column);
            let active_idx = layout.active;
            let tab_target = match hit {
                // Hovering over the source's own tab is not a move target —
                // release on own tab is a no-op, matching Chrome's tab reorder UX.
                Some(TabBarClick::Tab(idx)) if idx != active_idx => {
                    Some(DragTabTarget::ExistingTab(idx))
                }
                Some(TabBarClick::Tab(_)) => None,
                Some(TabBarClick::NewTab) => Some(DragTabTarget::NewTab),
                None => None,
            };
            if let Some(tab) = layout.active_tab_mut() {
                tab.drag_target_tab = tab_target;
                tab.drag_target = None;
            }
        } else {
            let target = layout.active_tab().and_then(|tab| {
                let source = tab.dragging_pane?;
                tab.pane_at(mouse.column, mouse.row)
                    .filter(|&id| id != source)
            });
            if let Some(tab) = layout.active_tab_mut() {
                tab.drag_target = target;
                tab.drag_target_tab = None;
            }
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
    // Tab reorder: complete drag-to-reorder on mouse up
    if let Some(from) = layout.tab_reorder_source.take() {
        if let Some(to) = layout.tab_reorder_target.take() {
            if from != to && from < layout.tabs.len() && to < layout.tabs.len() {
                let tab = layout.tabs.remove(from);
                layout.tabs.insert(to, tab);
                layout.active = to;
                out.needs_resize = true;
            }
        }
    }
    if state.border_drag.is_some() {
        state.border_drag = None;
        // Ratio was updated live during drag but PTY resizes were deferred
        // — fire one now.
        out.needs_resize = true;
    } else if layout
        .active_tab()
        .is_some_and(|t| t.dragging_pane.is_some())
    {
        let source_id = layout.active_tab().and_then(|t| t.dragging_pane);
        let target_pane = layout.active_tab().and_then(|t| t.drag_target);
        let target_tab = layout.active_tab().and_then(|t| t.drag_target_tab);
        let from_tab_idx = layout.active;

        // Cross-tab drop takes precedence over intra-tab swap — the two are
        // populated mutually exclusively by `handle_drag`, but check in order
        // anyway so a stale field can't misfire.
        if let (Some(src), Some(tab_target)) = (source_id, target_tab) {
            match tab_target {
                DragTabTarget::ExistingTab(to_idx) => {
                    if let Some(new_idx) = layout.move_pane_across_tabs(
                        from_tab_idx,
                        src,
                        MovePlacement::SplitFocused {
                            to_tab: to_idx,
                            dir: SplitDir::Horizontal,
                        },
                    ) {
                        out.new_last_tab = Some(from_tab_idx);
                        // Follow the moved pane: the user's attention is on
                        // what they just dropped, so jump to its new tab.
                        layout.goto_tab(new_idx);
                        out.needs_resize = true;
                    }
                }
                DragTabTarget::NewTab => {
                    // Name the new tab after the pane's agent for a sensible
                    // default. User can rename later via `:rename-tab` etc.
                    let name = layout
                        .active_tab()
                        .and_then(|t| t.root().find_pane(src))
                        .map(|p| p.agent_name.clone())
                        .unwrap_or_else(|| "new".to_string());
                    if layout
                        .move_pane_across_tabs(from_tab_idx, src, MovePlacement::NewTab { name })
                        .is_some()
                    {
                        out.new_last_tab = Some(from_tab_idx);
                        out.needs_resize = true;
                    }
                }
            }
        } else if let (Some(src), Some(tgt)) = (source_id, target_pane) {
            if let Some(tab) = layout.active_tab_mut() {
                crate::layout::swap_panes(tab.root_mut(), src, tgt);
            }
            out.needs_resize = true;
        }
        // Clear drag state on the *current* active tab — the source tab may
        // have been removed (single-pane cross-tab move) so we can't clear by
        // from_tab_idx. No-target and intra-tab branches share this tail;
        // the cross-tab branch also lands here for the same reason.
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
                    if sel.start == sel.end {
                        // Click without drag — clear the zero-width selection
                        // to avoid a 1-cell white block artifact.
                        pane.selection = None;
                    } else {
                        let text = pane
                            .vterm
                            .extract_text(sel.start, sel.end, pane.scroll_offset);
                        if !text.is_empty() {
                            copy_to_clipboard(&text);
                        }
                    }
                }
                // Keep selection visible — cleared on next mouse down or keypress.
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
pub(super) fn copy_to_clipboard(text: &str) {
    match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text)) {
        Ok(()) => {}
        Err(e) => tracing::warn!(error = %e, "clipboard copy failed"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::layout::{DragTabTarget, Pane, PaneSource, SplitDir, Tab};
    use crate::vterm::VTerm;
    use crossterm::event::KeyModifiers;

    fn leaf(id: usize, agent: &str) -> Pane {
        Pane {
            agent_name: agent.to_string(),
            vterm: VTerm::new(10, 10),
            rx: crossbeam_channel::bounded(1).1,
            id,
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
        }
    }

    fn up_event() -> MouseEvent {
        // handle_up ignores `mouse` in the dragging_pane branch, so column/row
        // are placeholders. The Up(Left) kind is what routes the event.
        MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::empty(),
        }
    }

    /// Pins the dispatch order in `handle_up`: when both intra-tab swap and
    /// cross-tab drop targets are populated, cross-tab wins. `handle_drag`
    /// sets them mutually exclusively today, but this test is the belt to
    /// that suspender — a future refactor that re-introduces both fields
    /// simultaneously must not silently downgrade a tab-bar drop into an
    /// in-place swap.
    #[test]
    fn handle_up_cross_tab_drop_wins_over_intra_tab_swap() {
        let mut layout = Layout::new();
        layout.add_tab(Tab::new("src".to_string(), leaf(1, "a")));
        layout.tabs[0].split_focused(SplitDir::Vertical, leaf(2, "b"));
        layout.add_tab(Tab::new("dst".to_string(), leaf(3, "c")));
        layout.active = 0;
        layout.tabs[0].focus_id = 2;
        layout.tabs[0].dragging_pane = Some(2);
        // Stale intra-tab target — pane 1 lives in the same tab.
        layout.tabs[0].drag_target = Some(1);
        // Fresh cross-tab target — release over the "dst" tab label.
        layout.tabs[0].drag_target_tab = Some(DragTabTarget::ExistingTab(1));

        let mut state = MouseState::default();
        let mut out = MouseOutcome::default();
        handle_up(up_event(), &mut layout, &mut state, &mut out);

        // Pane 2 ("b") must have moved into "dst", NOT swapped with pane 1.
        assert_eq!(layout.tabs.len(), 2);
        assert_eq!(layout.tabs[0].root().pane_count(), 1);
        assert!(
            !layout.tabs[0].root().has_agent("b"),
            "pane b must leave src tab"
        );
        assert!(
            layout.tabs[0].root().has_agent("a"),
            "pane a must stay in src (not swapped)"
        );
        assert_eq!(layout.tabs[1].root().pane_count(), 2);
        assert!(
            layout.tabs[1].root().has_agent("b"),
            "pane b must land in dst"
        );
        // Focus follows the moved pane's new tab.
        assert_eq!(layout.active, 1);
        assert!(out.needs_resize);
        // Drag state cleared on the (now active) dest tab.
        assert!(layout.tabs[1].dragging_pane.is_none());
        assert!(layout.tabs[1].drag_target.is_none());
        assert!(layout.tabs[1].drag_target_tab.is_none());
    }

    #[test]
    fn test_drag_tab_reorder_updates_target() {
        let mut layout = Layout::new();
        layout.add_tab(Tab::new("a".to_string(), leaf(1, "a")));
        layout.add_tab(Tab::new("b".to_string(), leaf(2, "b")));

        let mut state = MouseState::default();
        layout.tab_reorder_source = Some(0);

        // Drag over tab 1. Tab 0 is " a* " (~5 cols).
        let event = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 6,
            row: 0,
            modifiers: KeyModifiers::empty(),
        };

        handle_drag(event, &mut layout, &mut state);
        assert_eq!(layout.tab_reorder_target, Some(1));
    }

    #[test]
    fn test_drag_pane_to_tab_bar_updates_target_tab() {
        let mut layout = Layout::new();
        layout.add_tab(Tab::new("src".to_string(), leaf(1, "a")));
        layout.add_tab(Tab::new("dst".to_string(), leaf(2, "b")));
        layout.active = 0;

        let mut state = MouseState::default();
        layout.tabs[0].dragging_pane = Some(1);

        // Drag over "dst" tab (idx 1) at row 0
        let event = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 10,
            row: 0,
            modifiers: KeyModifiers::empty(),
        };

        handle_drag(event, &mut layout, &mut state);
        assert_eq!(
            layout.tabs[0].drag_target_tab,
            Some(DragTabTarget::ExistingTab(1))
        );
        assert!(layout.tabs[0].drag_target.is_none());
    }

    #[test]
    fn test_drag_pane_to_new_tab_slot() {
        let mut layout = Layout::new();
        layout.add_tab(Tab::new("src".to_string(), leaf(1, "a")));
        layout.active = 0;

        let mut state = MouseState::default();
        layout.tabs[0].dragging_pane = Some(1);

        // Drag over "[+]" slot.
        let event = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 8,
            row: 0,
            modifiers: KeyModifiers::empty(),
        };

        handle_drag(event, &mut layout, &mut state);
        assert_eq!(layout.tabs[0].drag_target_tab, Some(DragTabTarget::NewTab));
    }

    #[test]
    fn test_drag_pane_within_tab_updates_swap_target() {
        let mut layout = Layout::new();
        layout.add_tab(Tab::new("tab".to_string(), leaf(1, "a")));
        layout.tabs[0].split_focused(SplitDir::Vertical, leaf(2, "b"));
        layout.active = 0;

        // Manually set pane rects to simulate a rendered state
        layout.tabs[0].pane_rects.insert(1, (0, 1, 10, 10));
        layout.tabs[0].pane_rects.insert(2, (10, 1, 10, 10));

        let mut state = MouseState::default();
        layout.tabs[0].dragging_pane = Some(1);

        // Drag over pane 2
        let event = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 15,
            row: 5,
            modifiers: KeyModifiers::empty(),
        };

        handle_drag(event, &mut layout, &mut state);
        assert_eq!(layout.tabs[0].drag_target, Some(2));
        assert!(layout.tabs[0].drag_target_tab.is_none());
    }
}
