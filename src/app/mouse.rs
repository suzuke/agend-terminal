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

    // #901: pre-focus on Down(Left) BEFORE the forward decision below.
    // Without this, a click on a mouse-forwarded pane's body (OpenCode
    // and any future backend that enables SGR mouse) early-returns at
    // the forward arm and never reaches `handle_down`/`handle_selection`,
    // so `tab.focus_id` stays put — operator clicks pane body, focus
    // doesn't move. Pre-focusing at the top is backend-agnostic and
    // leaves the forward path itself untouched (Down still reaches the
    // backend so OpenCode's internal buttons still work).
    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        if let Some((pane_id, _, _)) = pane_for_mouse_forward(layout, &mouse) {
            if let Some(tab) = layout.active_tab_mut() {
                tab.focus_id = pane_id;
            }
        }
    }

    // #92758-3: a fresh left-button press dismisses any stale selection highlight,
    // fleet-wide in the active tab — so a mis-select can be cleared by clicking
    // ANYWHERE (a different pane, a border, the tab bar), not only by clicking
    // inside the same pane's text or via the keyboard copy key. Placed BEFORE the
    // mouse-forward gate below so it also fires when the click lands on a
    // mouse-forwarding pane (the click still forwards as usual). The normal
    // selection flow re-starts a fresh zero-width selection only on a real
    // pane-body click, so drag-to-copy and #2294 clear-on-copy are unchanged.
    // Selection state ONLY — focus is handled by the #901 pre-focus above.
    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        if let Some(tab) = layout.active_tab_mut() {
            tab.root_mut().clear_selections();
        }
    }

    // #700 + #783: If a pane's terminal wants mouse events AND shift is not
    // held, forward the event as SGR mouse report to the PTY instead of local
    // handling. The routing decision lives in `pane_for_mouse_forward` so it
    // can be unit-tested without the PTY write side-effect.
    // #783: route the SGR write to the cursor-target pane (which is not
    // necessarily the focused pane). `pane_for_mouse_forward` returns the
    // pane id under the cursor that wants mouse + SGR; we then call
    // `write_to_pane` (sibling of `write_to_focused`) to deliver bytes to
    // that specific pane's PTY.
    if let Some((pane_id, inner_x, inner_y)) = pane_for_mouse_forward(layout, &mouse) {
        if let Some(encoded) = crate::mouse_forward::encode_sgr(&mouse, inner_x, inner_y) {
            super::write_to_pane(&crate::home_dir(), layout, registry, pane_id, &encoded);
            return out;
        }
    }

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
                        .unwrap_or_else(|| "new".into());
                    if layout
                        .move_pane_across_tabs(
                            from_tab_idx,
                            src,
                            MovePlacement::NewTab {
                                name: name.to_string(),
                            },
                        )
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

#[derive(Debug, PartialEq)]
enum TabBarClick {
    Tab(usize),
    NewTab,
}

/// Hit-test the tab bar at the given column.
/// SYNC: layout math must match render_tab_bar() in render.rs.
/// M4: This is O(n_tabs) string-width scan, not a full layout re-render.
/// Caching tab positions would save ~5 iterations but adds invalidation
/// complexity. Kept as-is — n_tabs is typically 1-5.
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
                    // #1432: anchor in absolute scrollback logical coordinates
                    // so the selection survives new output and user scrolling.
                    let logical = pane.viewport_to_logical_line(row);
                    pane.selection = Some(crate::layout::Selection {
                        start: (logical, col),
                        end: (logical, col),
                    });
                }
                false
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                // #1432: dragging past the top/bottom edge auto-scrolls so the
                // selection can extend beyond the visible viewport. Overshoot
                // distance sets the scroll step (drag further → scroll faster).
                let row = if mouse.row < inner_y {
                    let overshoot = (inner_y - mouse.row) as usize;
                    pane.scroll_offset =
                        (pane.scroll_offset + overshoot).min(pane.vterm.max_scroll());
                    0
                } else if mouse.row >= inner_y + inner_h {
                    let overshoot = (mouse.row - (inner_y + inner_h - 1)) as usize;
                    pane.scroll_offset = pane.scroll_offset.saturating_sub(overshoot);
                    inner_h - 1
                } else {
                    mouse.row - inner_y
                };
                let col = mouse.column.max(inner_x).min(inner_x + inner_w - 1) - inner_x;
                let logical = pane.viewport_to_logical_line(row);
                if let Some(ref mut sel) = pane.selection {
                    sel.end = (logical, col);
                }
                false
            }
            MouseEventKind::Up(MouseButton::Left) => {
                // Releasing a real drag KEEPS the selection (no auto-copy, no
                // clear) — copying is an explicit act via the copy key
                // (CopySelection → Cmd+C / Ctrl+Shift+C), which copies + clears.
                // A pure click leaves a zero-width selection; clear that so a click
                // still dismisses the highlight (#2296). `copy_to_clipboard` stays —
                // the CopySelection path (dispatch) is its remaining caller.
                if let Some(ref sel) = pane.selection {
                    if sel.start == sel.end {
                        pane.selection = None;
                    }
                }
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

/// #783: pick the pane that should receive an SGR mouse-forward for this
/// event, returning the pane id plus the inner top-left of that pane in
/// terminal coordinates. Returns `None` when the event should fall
/// through to local TUI handling (shift held, no pane under cursor, the
/// pane's terminal does not want mouse, or cursor sits on the border).
///
/// Pre-fix (#700) consulted only the focused pane, which broke #783's
/// multi-pane case (opencode in a non-focused split). The §3.10 GREEN
/// commit switches the lookup to `tab.pane_at(col, row)` so the pane
/// UNDER the cursor handles its own mouse events regardless of focus.
fn pane_for_mouse_forward(layout: &Layout, mouse: &MouseEvent) -> Option<(usize, u16, u16)> {
    if mouse
        .modifiers
        .contains(crossterm::event::KeyModifiers::SHIFT)
    {
        return None;
    }
    let tab = layout.active_tab()?;
    // #783: look up the pane UNDER THE CURSOR, not the focused one.
    // Pre-fix used `tab.focus_id`, which broke multi-pane scenarios where
    // the wants_mouse pane (opencode) was not focused.
    let pane_id = tab.pane_at(mouse.column, mouse.row)?;
    let pane = tab.root().find_pane(pane_id)?;
    if !pane.vterm.wants_mouse() || !pane.vterm.mouse_sgr() {
        return None;
    }
    let &(px, py, pw, ph) = tab.pane_rects.get(&pane_id)?;
    let inner_x = px + 1;
    let inner_y = py + 1;
    let inner_w = pw.saturating_sub(2);
    let inner_h = ph.saturating_sub(2);
    if mouse.column >= inner_x
        && mouse.column < inner_x + inner_w
        && mouse.row >= inner_y
        && mouse.row < inner_y + inner_h
    {
        Some((pane_id, inner_x, inner_y))
    } else {
        None
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
            agent_name: agent.into(),
            instance_id: crate::types::InstanceId::default(),
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

    fn scroll_up_at(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column,
            row,
            modifiers: KeyModifiers::empty(),
        }
    }

    /// Opencode mouse-on startup: 1000h + 1002h + 1003h enable a tracking
    /// mode (`wants_mouse`); 1006h selects SGR encoding (`mouse_sgr`).
    /// Matches the sequence pinned in `vterm::tests::wants_mouse_matches_opencode_startup_sequence`.
    fn enable_opencode_mouse(pane: &mut Pane) {
        pane.vterm
            .process(b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h");
    }

    /// #783 §3.10 anchor — in a multi-pane tab, mouse-forwarding must
    /// target the pane UNDER the cursor, not the focused pane. The
    /// pre-fix `tab.focused_pane()` lookup skipped SGR forwarding when
    /// the wants_mouse pane (opencode in right split) was not focused;
    /// the operator's scroll then fell through to the local TUI scroll
    /// handler. This test pins the contract by asserting the routing
    /// decision itself.
    ///
    /// RED on the §3.10 RED commit: `pane_for_mouse_forward` still uses
    /// `tab.focus_id`, so the focused (no-mouse) pane gets the lookup
    /// and returns `None`. Asserting `Some(2, _, _)` fails.
    /// GREEN after the cursor-lookup refactor.
    #[test]
    fn pane_for_mouse_forward_targets_pane_under_cursor_not_focused() {
        let mut layout = Layout::new();
        layout.add_tab(Tab::new("multi".to_string(), leaf(1, "left")));
        layout.tabs[0].split_focused(SplitDir::Vertical, leaf(2, "opencode"));
        layout.active = 0;
        // Focus the LEFT pane (id=1); opencode lives in RIGHT pane (id=2).
        layout.tabs[0].focus_id = 1;

        // Layout rects: Pane 1 at (0,1)-(10,11), Pane 2 at (10,1)-(20,11).
        layout.tabs[0].pane_rects.insert(1, (0, 1, 10, 10));
        layout.tabs[0].pane_rects.insert(2, (10, 1, 10, 10));

        // Pane 2 (right, NOT focused) is the opencode pane wanting mouse.
        enable_opencode_mouse(layout.tabs[0].root_mut().find_pane_mut(2).unwrap());

        // Mouse scroll over the RIGHT pane interior (column 15, row 5).
        let mouse = scroll_up_at(15, 5);

        let routed = super::pane_for_mouse_forward(&layout, &mouse);
        assert_eq!(
            routed.map(|(id, _, _)| id),
            Some(2),
            "mouse over right opencode pane must route there regardless of focus; got {routed:?}"
        );
    }

    /// #783 invariant — when no pane under the cursor wants mouse,
    /// `pane_for_mouse_forward` returns `None` so the event falls through
    /// to local TUI handling. Pins that the cursor-lookup refactor does
    /// not change behavior for non-opencode panes.
    #[test]
    fn pane_for_mouse_forward_returns_none_when_target_pane_lacks_mouse() {
        let mut layout = Layout::new();
        layout.add_tab(Tab::new("multi".to_string(), leaf(1, "left")));
        layout.tabs[0].split_focused(SplitDir::Vertical, leaf(2, "right"));
        layout.active = 0;
        layout.tabs[0].focus_id = 1;
        layout.tabs[0].pane_rects.insert(1, (0, 1, 10, 10));
        layout.tabs[0].pane_rects.insert(2, (10, 1, 10, 10));
        // No `enable_opencode_mouse` — neither pane wants mouse.

        let mouse = scroll_up_at(15, 5);
        assert!(
            super::pane_for_mouse_forward(&layout, &mouse).is_none(),
            "no pane wants mouse → fall through to local scroll handling"
        );
    }

    /// #783 regression-guard — single-pane case must keep working
    /// (post-#700/#739/#741/#744 wants_mouse path). The cursor-lookup
    /// refactor must not regress the single-pane scenario where focused
    /// == under-cursor.
    #[test]
    fn pane_for_mouse_forward_single_pane_with_mouse_still_routes() {
        let mut layout = Layout::new();
        layout.add_tab(Tab::new("single".to_string(), leaf(1, "opencode")));
        layout.active = 0;
        layout.tabs[0].focus_id = 1;
        layout.tabs[0].pane_rects.insert(1, (0, 1, 20, 10));

        enable_opencode_mouse(layout.tabs[0].root_mut().find_pane_mut(1).unwrap());

        let mouse = scroll_up_at(10, 5);
        let routed = super::pane_for_mouse_forward(&layout, &mouse);
        assert_eq!(
            routed.map(|(id, _, _)| id),
            Some(1),
            "single-pane opencode tab must still route mouse to that pane"
        );
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

    // --- Sprint 41 T-3: tab_bar_hit_test coverage ---

    #[test]
    fn tab_bar_hit_test_first_tab() {
        let mut layout = Layout::new();
        layout.add_tab(Tab::new("tab0".to_string(), leaf(1, "a")));
        // First tab starts at col 0: "*" (1) + " tab0 " (6) = 7 cols
        let result = tab_bar_hit_test(&layout, 0);
        assert_eq!(result, Some(TabBarClick::Tab(0)));
    }

    #[test]
    fn tab_bar_hit_test_second_tab() {
        let mut layout = Layout::new();
        layout.add_tab(Tab::new("a".to_string(), leaf(1, "x")));
        layout.add_tab(Tab::new("b".to_string(), leaf(2, "y")));
        // Tab "a": "*" + " a " = 4 cols (0..3). Separator: 1 col (4).
        // Tab "b": "*" + " b " = 4 cols (5..8).
        let result = tab_bar_hit_test(&layout, 5);
        assert_eq!(result, Some(TabBarClick::Tab(1)));
    }

    #[test]
    fn tab_bar_hit_test_new_tab_button() {
        let mut layout = Layout::new();
        layout.add_tab(Tab::new("x".to_string(), leaf(1, "a")));
        // Tab "x": 4 cols (0..3). Then " [+] " at col 4..8.
        let result = tab_bar_hit_test(&layout, 4);
        assert_eq!(result, Some(TabBarClick::NewTab));
    }

    #[test]
    fn tab_bar_hit_test_beyond_all_tabs_returns_none() {
        let mut layout = Layout::new();
        layout.add_tab(Tab::new("t".to_string(), leaf(1, "a")));
        // Way past all tabs + [+] button
        let result = tab_bar_hit_test(&layout, 100);
        assert_eq!(result, None);
    }

    #[test]
    fn tab_bar_hit_test_empty_layout_returns_new_tab() {
        let layout = Layout::new();
        // No tabs → only [+] button at col 0
        let result = tab_bar_hit_test(&layout, 0);
        assert_eq!(result, Some(TabBarClick::NewTab));
    }

    // --- Sprint 41 T-3 r2: drag state machine tests ---

    #[test]
    fn drag_start_sets_dragging_pane_on_title_hit() {
        // Drive the REAL `handle_down`: a Down(Left) on pane 2's title-bar text
        // must begin dragging pane 2 (and focus it). The earlier version set
        // `dragging_pane = Some(2)` by hand and asserted it back — a tautology
        // that never exercised handle_down's title-hit branch.
        let mut layout = two_pane_layout("right");
        layout.tabs[0].focus_id = 1; // focus the OTHER pane to prove the move

        // pane 2 rect = (10, 1, 10, 10); its " right " title text spans cols
        // [11, 17). Click at col 12, row 1 (the pane's top row, not the tab bar).
        let click = down_left_at(12, 1);
        assert_eq!(
            layout.tabs[0].title_bar_at(12, 1),
            Some(2),
            "test precondition: synthetic click must land on pane 2's title text"
        );

        let mut state = MouseState::default();
        let mut out = MouseOutcome::default();
        let fleet_path = std::path::Path::new("/nonexistent/fleet.yaml");
        handle_down(
            click,
            &mut layout,
            &mut state,
            fleet_path,
            &empty_registry(),
            &mut out,
        );

        assert_eq!(
            layout.tabs[0].dragging_pane,
            Some(2),
            "handle_down on a pane title bar must begin dragging that pane"
        );
        assert_eq!(
            layout.tabs[0].focus_id, 2,
            "title-bar hit also focuses the clicked pane"
        );
        assert_eq!(layout.tabs[0].drag_target, None);
    }

    #[test]
    fn drag_end_clears_dragging_pane() {
        let mut layout = Layout::new();
        layout.add_tab(Tab::new("src".to_string(), leaf(1, "a")));
        layout.tabs[0].split_focused(SplitDir::Vertical, leaf(2, "b"));
        layout.active = 0;
        layout.tabs[0].focus_id = 2;
        layout.tabs[0].dragging_pane = Some(2);
        layout.tabs[0].drag_target = Some(1);

        let mut state = MouseState::default();
        let mut out = MouseOutcome::default();
        handle_up(up_event(), &mut layout, &mut state, &mut out);

        assert_eq!(
            layout.tabs[0].dragging_pane, None,
            "drag-end must clear dragging_pane"
        );
    }

    #[test]
    fn border_drag_state_cleared_on_mouse_up() {
        let mut state = MouseState {
            border_drag: Some((
                crate::layout::SplitBorderHit {
                    split_area: (0, 1, 60, 38),
                    dir: SplitDir::Vertical,
                },
                ratatui::layout::Rect::new(0, 1, 120, 38),
            )),
        };
        let mut layout = Layout::new();
        layout.add_tab(Tab::new("t".to_string(), leaf(1, "a")));
        let mut out = MouseOutcome::default();
        handle_up(up_event(), &mut layout, &mut state, &mut out);

        assert!(
            state.border_drag.is_none(),
            "border_drag must be cleared on mouse-up"
        );
    }

    // ----- #901: Down(Left) on mouse-forwarded pane must focus that pane -----

    use serial_test::serial;

    /// AGEND_HOME isolation for tests that call `handle()` and hit the
    /// forward path (which writes a metadata file via
    /// `notification_queue::record_input_activity`). `#[serial]` callers
    /// guarantee env-var swaps don't race.
    struct ScopedHome {
        prev: Option<String>,
        dir: std::path::PathBuf,
    }
    impl ScopedHome {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("agend-901-{}-{}", tag, std::process::id()));
            std::fs::create_dir_all(&dir).ok();
            let prev = std::env::var("AGEND_HOME").ok();
            std::env::set_var("AGEND_HOME", &dir);
            Self { prev, dir }
        }
    }
    impl Drop for ScopedHome {
        fn drop(&mut self) {
            match &self.prev {
                Some(p) => std::env::set_var("AGEND_HOME", p),
                None => std::env::remove_var("AGEND_HOME"),
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn empty_registry() -> crate::agent::AgentRegistry {
        std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()))
    }

    fn down_left_at(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::empty(),
        }
    }

    fn drag_left_at(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::empty(),
        }
    }

    fn up_left_at(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::empty(),
        }
    }

    /// Build a two-pane vertical split with left at id=1, right at id=2.
    /// Pane 1 occupies columns 0..10, pane 2 occupies columns 10..20.
    /// Both panes are 10 rows tall starting at row 1 (row 0 is the tab bar).
    fn two_pane_layout(right_agent: &str) -> Layout {
        let mut layout = Layout::new();
        layout.add_tab(Tab::new("multi".to_string(), leaf(1, "left")));
        layout.tabs[0].split_focused(SplitDir::Vertical, leaf(2, right_agent));
        layout.active = 0;
        layout.tabs[0].pane_rects.insert(1, (0, 1, 10, 10));
        layout.tabs[0].pane_rects.insert(2, (10, 1, 10, 10));
        layout
    }

    /// #901 contract: clicking the body of a mouse-forwarded pane (e.g.
    /// OpenCode) MUST focus that pane, even though the click is forwarded
    /// to the backend's PTY. Pre-fix, `mouse::handle` early-returned after
    /// the SGR forward (lines 62-67) and never ran `handle_down`, leaving
    /// `tab.focus_id` pointing at the previously-focused pane. The
    /// operator workaround was to click the pane title bar instead.
    ///
    /// RED at parent SHA: focus stays on the left pane.
    /// GREEN after the top-level pre-focus fix: focus moves to the pane
    /// under the cursor BEFORE the forward early-return runs.
    #[test]
    #[serial]
    fn pane_body_down_focuses_pane_even_when_mouse_forwarded() {
        let _home = ScopedHome::new("focus-forwarded");

        let mut layout = two_pane_layout("opencode");
        layout.tabs[0].focus_id = 1;
        enable_opencode_mouse(layout.tabs[0].root_mut().find_pane_mut(2).unwrap());

        // Precondition: opencode pane IS the mouse-forward target — without
        // this the test wouldn't be exercising the bug path.
        let click = down_left_at(15, 5);
        assert!(
            super::pane_for_mouse_forward(&layout, &click).is_some(),
            "test precondition: opencode pane under cursor must be the forward target"
        );

        let mut state = MouseState::default();
        let fleet_path = std::path::Path::new("/nonexistent/fleet.yaml");
        super::handle(
            click,
            &mut layout,
            &mut state,
            fleet_path,
            &empty_registry(),
        );

        assert_eq!(
            layout.tabs[0].focus_id, 2,
            "Down(Left) on mouse-forwarded pane body MUST update focus_id \
             (started at 1, expected 2 after fix); operator-visible symptom \
             is clicking OpenCode pane body doesn't focus it"
        );
    }

    /// #783/#700 invariant regression-guard: non-mouse panes
    /// (claude/codex/kiro/gemini) must still route Down(Left) through the
    /// local `handle_down` path so `handle_selection` updates both
    /// `focus_id` AND `selecting_pane`. The top-level pre-focus block
    /// only runs when `pane_for_mouse_forward` matches; for non-mouse
    /// panes it is a no-op and the legacy local-dispatch path must
    /// remain intact, including selection-cache state.
    #[test]
    #[serial]
    fn pane_body_down_on_non_mouse_pane_still_updates_focus_via_local_path() {
        let _home = ScopedHome::new("focus-local");

        let mut layout = two_pane_layout("right");
        layout.tabs[0].focus_id = 1;
        // No `enable_opencode_mouse` — neither pane wants mouse.

        let click = down_left_at(15, 5);
        assert!(
            super::pane_for_mouse_forward(&layout, &click).is_none(),
            "test precondition: no pane wants mouse → forward returns None"
        );

        let mut state = MouseState::default();
        let fleet_path = std::path::Path::new("/nonexistent/fleet.yaml");
        super::handle(
            click,
            &mut layout,
            &mut state,
            fleet_path,
            &empty_registry(),
        );

        assert_eq!(
            layout.tabs[0].focus_id, 2,
            "non-mouse pane body click must still update focus via \
             handle_down → handle_selection (legacy path)"
        );
        assert_eq!(
            layout.tabs[0].selecting_pane,
            Some(2),
            "handle_selection MUST cache selecting_pane on Down so drag \
             continues against the same pane"
        );
    }

    // ----- #92758-3: a left-click dismisses a stale selection anywhere -----

    fn wide_selection() -> crate::layout::Selection {
        // A non-zero-width (visible) selection — the mis-select the operator
        // wants to clear by clicking.
        crate::layout::Selection {
            start: (0, 0),
            end: (0, 5),
        }
    }

    /// G1: clicking a DIFFERENT pane clears the stale highlight in the pane that
    /// held the selection (pre-fix the Down only touched the clicked pane).
    #[test]
    #[serial]
    fn left_click_clears_selection_in_other_pane_2158() {
        let _home = ScopedHome::new("clear-g1");
        let mut layout = two_pane_layout("right");
        layout.tabs[0].focus_id = 1;
        layout.tabs[0]
            .root_mut()
            .find_pane_mut(1)
            .unwrap()
            .selection = Some(wide_selection());

        // Click inside pane 2's body (cols 10..20) — the OTHER pane.
        let mut state = MouseState::default();
        super::handle(
            down_left_at(15, 5),
            &mut layout,
            &mut state,
            std::path::Path::new("/nonexistent/fleet.yaml"),
            &empty_registry(),
        );

        assert!(
            layout.tabs[0]
                .root_mut()
                .find_pane_mut(1)
                .unwrap()
                .selection
                .is_none(),
            "clicking pane 2 must clear pane 1's stale selection (G1)"
        );
    }

    /// G3: a click that lands OUTSIDE any pane body (the tab bar at row 0) still
    /// clears a stale selection — the clear runs before the tab/border branching.
    #[test]
    #[serial]
    fn left_click_on_tab_bar_clears_selection_2158() {
        let _home = ScopedHome::new("clear-g3");
        let mut layout = two_pane_layout("right");
        layout.tabs[0]
            .root_mut()
            .find_pane_mut(2)
            .unwrap()
            .selection = Some(wide_selection());

        // Row 0 is the tab bar — not a pane body.
        let mut state = MouseState::default();
        super::handle(
            down_left_at(3, 0),
            &mut layout,
            &mut state,
            std::path::Path::new("/nonexistent/fleet.yaml"),
            &empty_registry(),
        );

        assert!(
            layout.tabs[0]
                .root_mut()
                .find_pane_mut(2)
                .unwrap()
                .selection
                .is_none(),
            "clicking the tab bar must still clear a stale selection (G3)"
        );
    }

    /// Drag-start regression (#2296): a pane-body click clears the stale selection
    /// AND anchors a fresh zero-width one, so a drag can still begin.
    #[test]
    #[serial]
    fn left_click_on_pane_body_anchors_fresh_zero_width_selection_2158() {
        let _home = ScopedHome::new("clear-restart");
        let mut layout = two_pane_layout("right");
        layout.tabs[0]
            .root_mut()
            .find_pane_mut(2)
            .unwrap()
            .selection = Some(wide_selection());

        // Click inside pane 2's INNER text area (rect 10,1,10,10 → inner 11..19).
        let mut state = MouseState::default();
        super::handle(
            down_left_at(13, 4),
            &mut layout,
            &mut state,
            std::path::Path::new("/nonexistent/fleet.yaml"),
            &empty_registry(),
        );

        let pane = layout.tabs[0].root_mut().find_pane_mut(2).unwrap();
        let sel = pane
            .selection
            .as_ref()
            .expect("a pane-body click anchors a fresh selection for drag");
        assert_eq!(
            sel.start, sel.end,
            "the stale wide selection is replaced by a fresh zero-width one (drag-to-copy intact)"
        );
    }

    // ----- #43783: releasing a drag keeps the selection (copy is the copy key) -----

    /// Operator semantics: releasing a REAL drag KEEPS the selection — no auto-copy
    /// and no auto-clear. Copying is an explicit act via the copy key
    /// (CopySelection → Cmd+C / Ctrl+Shift+C). Replaces #2294's release-auto-copy.
    #[test]
    fn drag_release_keeps_selection_no_autocopy_43783() {
        let mut layout = two_pane_layout("right");
        // Down anchors a zero-width selection inside pane 1's inner area...
        handle_selection(&mut layout, &down_left_at(3, 3));
        // ...drag extends it to a real (non-zero) span...
        handle_selection(&mut layout, &drag_left_at(6, 3));
        // ...and releasing must KEEP it (not copy, not clear).
        handle_selection(&mut layout, &up_left_at(6, 3));

        let pane = layout.tabs[0].root_mut().find_pane_mut(1).unwrap();
        let sel = pane
            .selection
            .as_ref()
            .expect("a released drag must keep its selection");
        assert_ne!(
            sel.start, sel.end,
            "the released selection must remain a real (non-zero) span"
        );
    }

    /// A pure click (Down then Up at the same spot, zero-width) still dismisses the
    /// highlight on release — the click-to-clear path is preserved.
    #[test]
    fn pure_click_release_clears_zero_width_selection_43783() {
        let mut layout = two_pane_layout("right");
        handle_selection(&mut layout, &down_left_at(3, 3));
        handle_selection(&mut layout, &up_left_at(3, 3));

        assert!(
            layout.tabs[0]
                .root_mut()
                .find_pane_mut(1)
                .unwrap()
                .selection
                .is_none(),
            "a pure click must not leave a zero-width selection"
        );
    }
}
