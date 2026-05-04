//! Layout tests — distributed from the original layout.rs mod tests block.

use super::pane::PaneSource;
use super::tree::{ratio_bounds, MIN_PANE_CELLS};
use super::*;
use crate::backend::Backend;
use crate::vterm::VTerm;
use std::time::Instant;
use unicode_width::UnicodeWidthStr;

// --- ratio_bounds invariants (covers Round A #3 + #6) ---

#[test]
fn ratio_bounds_symmetric_when_room() {
    let (lo, hi) = ratio_bounds(100);
    assert!(
        (lo + hi - 1.0).abs() < f32::EPSILON,
        "bounds should be symmetric"
    );
}

#[test]
fn ratio_bounds_degenerate_when_tiny() {
    // total < 2 * MIN_PANE_CELLS — no valid split honoring both minimums
    assert_eq!(ratio_bounds(5), (0.5, 0.5));
    assert_eq!(ratio_bounds(0), (0.5, 0.5));
}

#[test]
fn ratio_bounds_min_cells_enforced() {
    let (lo, _) = ratio_bounds(30);
    // first child at lo ratio ≈ 3 cells (MIN_PANE_CELLS)
    let first = (lo * 30.0).round() as u16;
    assert_eq!(first, MIN_PANE_CELLS);
}

// --- ratio_to_size guarantees (covers #3) ---

#[test]
fn ratio_to_size_no_zero_when_room() {
    // For any ratio in [0.0, 1.0] and total ≥ 2 * MIN_PANE_CELLS,
    // both sides get ≥ MIN_PANE_CELLS.
    for total in [6u16, 10, 40, 100, 500] {
        for ratio_int in 0..=100 {
            let r = ratio_int as f32 / 100.0;
            let first = ratio_to_size(r, total);
            let second = total - first;
            assert!(
                first >= MIN_PANE_CELLS && second >= MIN_PANE_CELLS,
                "total={total} ratio={r} -> first={first} second={second}"
            );
        }
    }
}

#[test]
fn ratio_to_size_degenerate_does_not_panic() {
    // total < 2: degrade gracefully instead of panicking
    assert_eq!(ratio_to_size(0.5, 0), 0);
    assert_eq!(ratio_to_size(0.5, 1), 0);
}

#[test]
fn ratio_to_size_sum_matches_total() {
    // split_child_areas relies on `first <= total` for safe overlap math
    // (second_size = total - first + overlap); verify no drift.
    for total in [2u16, 3, 10, 100, 1000] {
        for ratio_int in [0, 25, 50, 75, 100] {
            let r = ratio_int as f32 / 100.0;
            let first = ratio_to_size(r, total);
            assert!(first <= total, "first={first} > total={total}");
        }
    }
}

// --- split_child_areas overlap invariant (covers #1 joined borders) ---

#[test]
fn split_child_areas_vertical_siblings_share_one_cell() {
    // Siblings must overlap by exactly 1 cell on the split axis so the
    // border grid can merge their shared edge into a single glyph.
    let area = (0u16, 0u16, 20u16, 10u16);
    let (first, second) = split_child_areas(area, SplitDir::Vertical, 0.5);
    assert_eq!(first.0, 0, "first.x");
    assert!(first.2 > 0, "first.w must be > 0");
    assert_eq!(second.0, first.0 + first.2 - 1, "shared column");
    // Total painted width = first + second - 1 (the shared column is
    // counted once).
    assert_eq!(first.2 + second.2 - 1, area.2, "cells account for overlap");
}

#[test]
fn split_child_areas_horizontal_siblings_share_one_cell() {
    let area = (0u16, 0u16, 20u16, 10u16);
    let (first, second) = split_child_areas(area, SplitDir::Horizontal, 0.5);
    assert_eq!(second.1, first.1 + first.3 - 1, "shared row");
    assert_eq!(first.3 + second.3 - 1, area.3, "cells account for overlap");
}

#[test]
fn find_split_border_matches_shared_column() {
    // A vertical split's border should be detectable at exactly the
    // shared column (first.x + first.w - 1), and NOT at the old
    // pre-overlap position (first.x + first.w).
    let root = PaneNode::Split {
        dir: SplitDir::Vertical,
        ratio: 0.5,
        first: Box::new(PaneNode::Leaf(Box::new(Pane {
            agent_name: "a".to_string(),
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
        }))),
        second: Box::new(PaneNode::Leaf(Box::new(Pane {
            agent_name: "b".to_string(),
            vterm: VTerm::new(10, 10),
            rx: crossbeam_channel::bounded(1).1,
            id: 2,
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
        }))),
    };
    let area = (0u16, 0u16, 20u16, 10u16);
    let (first, second) = split_child_areas(area, SplitDir::Vertical, 0.5);
    let shared_col = first.0 + first.2 - 1;
    assert_eq!(shared_col, second.0);
    assert!(find_split_border(&root, area, shared_col, 5).is_some());
    // ±VSPLIT_BORDER_HIT_TOLERANCE is now a hit too: clicking exactly
    // one column off the border still grabs it, fixing the
    // pixel-perfect-aim regression where off-by-one fell through to
    // text selection.
    assert!(find_split_border(&root, area, shared_col + 1, 5).is_some());
    assert!(find_split_border(&root, area, shared_col - 1, 5).is_some());
    // Two columns out is well into pane content and must NOT hit.
    assert!(
        find_split_border(&root, area, shared_col + 2, 5).is_none(),
        "tolerance must not bleed past ±1 column"
    );
    assert!(
        find_split_border(&root, area, shared_col.saturating_sub(2), 5).is_none(),
        "tolerance must not bleed past ±1 column"
    );
}

#[test]
fn nested_vsplit_inner_border_wins_over_outer_tolerance() {
    // Outer vertical split contains an inner vertical split as its
    // first child. With the children-first recursion order, a click
    // on the inner border resolves to the inner split — even if the
    // outer's ±1 tolerance zone happens to overlap. Regression pin
    // for the recursion-order change introduced with hit tolerance.
    fn leaf(id: usize, name: &str) -> PaneNode {
        PaneNode::Leaf(Box::new(Pane {
            agent_name: name.to_string(),
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
        }))
    }
    let inner = PaneNode::Split {
        dir: SplitDir::Vertical,
        ratio: 0.5,
        first: Box::new(leaf(1, "a")),
        second: Box::new(leaf(2, "b")),
    };
    let outer = PaneNode::Split {
        dir: SplitDir::Vertical,
        ratio: 0.5,
        first: Box::new(inner),
        second: Box::new(leaf(3, "c")),
    };
    let outer_area = (0u16, 0u16, 40u16, 10u16);
    let (inner_area, outer_second_area) = split_child_areas(outer_area, SplitDir::Vertical, 0.5);
    let outer_border = outer_second_area.0;
    let (inner_first, inner_second) = split_child_areas(inner_area, SplitDir::Vertical, 0.5);
    let inner_border = inner_second.0;
    // Sanity: both borders exist at distinct columns
    assert_ne!(inner_border, outer_border);
    assert_eq!(inner_border, inner_first.0 + inner_first.2 - 1);

    // Click on inner border → inner split's area, not outer's.
    let hit =
        find_split_border(&outer, outer_area, inner_border, 5).expect("inner border must hit");
    assert_eq!(
        hit.split_area, inner_area,
        "click on inner border must resolve to inner split"
    );

    // Click on outer border → outer.
    let hit =
        find_split_border(&outer, outer_area, outer_border, 5).expect("outer border must hit");
    assert_eq!(hit.split_area, outer_area);
}

#[test]
fn hsplit_border_hit_is_not_widened() {
    // Horizontal splits are NOT mouse-resizable via the border (the
    // bottom pane's title bar lives at the same row and gets
    // grabbed first by the mouse handler), so applying the column
    // tolerance to rows would only steal pane content clicks.
    // Pin the asymmetry so a future "make it consistent" refactor
    // doesn't accidentally regress horizontal selection.
    let root = PaneNode::Split {
        dir: SplitDir::Horizontal,
        ratio: 0.5,
        first: Box::new(PaneNode::Leaf(Box::new(Pane {
            agent_name: "top".to_string(),
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
        }))),
        second: Box::new(PaneNode::Leaf(Box::new(Pane {
            agent_name: "bot".to_string(),
            vterm: VTerm::new(10, 10),
            rx: crossbeam_channel::bounded(1).1,
            id: 2,
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
        }))),
    };
    let area = (0u16, 0u16, 20u16, 10u16);
    let (_first, second) = split_child_areas(area, SplitDir::Horizontal, 0.5);
    let border_row = second.1;
    assert!(find_split_border(&root, area, 5, border_row).is_some());
    // ±1 row must NOT hit on horizontal splits — pane content lives
    // on those rows and the keyboard is the supported resize path.
    assert!(find_split_border(&root, area, 5, border_row + 1).is_none());
    assert!(find_split_border(&root, area, 5, border_row.saturating_sub(1)).is_none());
}

#[test]
fn adjust_split_ratio_border_lands_where_user_clicked() {
    // When the user drags the border to col X, adjust_split_ratio must
    // produce a ratio such that the new border sits at col X (modulo
    // ratio_to_size rounding within 1 cell).
    let mut root = PaneNode::Split {
        dir: SplitDir::Vertical,
        ratio: 0.5,
        first: Box::new(PaneNode::Leaf(Box::new(Pane {
            agent_name: "a".to_string(),
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
        }))),
        second: Box::new(PaneNode::Leaf(Box::new(Pane {
            agent_name: "b".to_string(),
            vterm: VTerm::new(10, 10),
            rx: crossbeam_channel::bounded(1).1,
            id: 2,
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
        }))),
    };
    let area = (0u16, 0u16, 100u16, 20u16);
    // User drags border to col 60. Must settle at col 60 (± 0 cells
    // since 100-cell total gives exact pixel mapping).
    assert!(adjust_split_ratio(
        &mut root,
        area,
        area,
        60,
        SplitDir::Vertical,
    ));
    if let PaneNode::Split { ratio, .. } = &root {
        let (first, _second) = split_child_areas(area, SplitDir::Vertical, *ratio);
        let new_border_col = first.0 + first.2 - 1;
        assert!(
            (new_border_col as i32 - 60).abs() <= 1,
            "border at col {new_border_col}, expected ~60"
        );
    } else {
        panic!("root should still be a Split");
    }
}

// --- Unicode title hit-test (covers #4) ---
//
// title_bar_at uses UnicodeWidthStr::width for the label, matching the
// rendered ` {label} ` width. We test the width calc directly since
// building a full Tab with Pane requires a VTerm + agent registry.

#[test]
fn unicode_width_for_title_matches_terminal_cells() {
    // ASCII: 1 cell per char
    assert_eq!(UnicodeWidthStr::width("alice") as u16, 5);
    // CJK: 2 cells per char
    assert_eq!(UnicodeWidthStr::width("代理") as u16, 4);
    // Mixed
    assert_eq!(UnicodeWidthStr::width("a代") as u16, 3);
}

/// Helper: create a single-pane Tab with a given agent name and pane rect.
fn tab_with_pane(name: &str, id: usize, rect: (u16, u16, u16, u16)) -> Tab {
    let pane = Pane {
        agent_name: name.to_string(),
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
    };
    let mut tab = Tab::new("test".into(), pane);
    tab.pane_rects.insert(id, rect);
    tab
}

#[test]
fn title_bar_at_hits_within_name_text() {
    // Pane "alice" at (0,0) width 40. Label " alice " occupies cols 1..8.
    let tab = tab_with_pane("alice", 1, (0, 0, 40, 10));
    // Col 1 = leading space of " alice " → hit
    assert_eq!(tab.title_bar_at(1, 0), Some(1));
    // Col 6 = last char of "alice" → hit
    assert_eq!(tab.title_bar_at(6, 0), Some(1));
    // Col 7 = trailing space → hit
    assert_eq!(tab.title_bar_at(7, 0), Some(1));
}

#[test]
fn title_bar_at_misses_outside_name_text() {
    // Pane "alice" at (0,0) width 40. Label ends at col 8.
    let tab = tab_with_pane("alice", 1, (0, 0, 40, 10));
    // Col 0 = border glyph → miss
    assert_eq!(tab.title_bar_at(0, 0), None);
    // Col 8 = past label → miss (falls through to border check)
    assert_eq!(tab.title_bar_at(8, 0), None);
    // Col 30 = far right → miss
    assert_eq!(tab.title_bar_at(30, 0), None);
}

#[test]
fn title_bar_at_name_fills_pane_width() {
    // Pane name exactly fills the pane: name width 8 + 2 padding = 10 = pane width - 1 (border)
    // Pane at (0,0) width 11. Label " longname " = cols 1..11.
    let tab = tab_with_pane("longname", 1, (0, 0, 11, 10));
    // All cols 1..10 should hit
    for col in 1..11 {
        assert_eq!(tab.title_bar_at(col, 0), Some(1), "col {col} should hit");
    }
}

// --- Tab / Layout mutation tests ---
//
// The helper below builds a minimal Pane that is cheap to construct and
// does not drive any PTY. Callers that need the pane to count as an agent
// (e.g. agent_count) should override `backend` via `leaf_agent`.

fn leaf(id: usize, name: &str) -> Pane {
    Pane {
        agent_name: name.to_string(),
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

fn leaf_agent(id: usize, name: &str) -> Pane {
    let mut p = leaf(id, name);
    p.backend = Some(Backend::ClaudeCode);
    p
}

#[test]
fn pane_composing_after_input() {
    let mut pane = leaf(1, "agent");
    pane.mark_input_activity();
    assert!(pane.is_composing());
}

#[test]
fn pane_not_composing_after_idle() {
    let mut pane = leaf(1, "agent");
    pane.last_input_at = Some(
        Instant::now()
            - crate::notification_queue::COMPOSE_IDLE_TIMEOUT
            - std::time::Duration::from_millis(1),
    );
    assert!(!pane.is_composing());
}

#[test]
fn split_at_pane_targets_non_focused_pane() {
    // split_at_pane must honor the explicit target_id even when a
    // different pane is focused — this is what `target_pane` in
    // create_instance relies on.
    let mut tab = Tab::new("t".to_string(), leaf(1, "a"));
    assert!(tab.split_focused(SplitDir::Vertical, leaf(2, "b")));
    // Focus stays on pane 1, but we target pane 2.
    assert_eq!(tab.focus_id, 1);
    assert!(tab.split_at_pane(2, SplitDir::Horizontal, leaf(3, "c")));
    assert_eq!(tab.root().pane_count(), 3);
    assert!(tab.root().has_agent("c"));
}

#[test]
fn split_at_pane_returns_false_when_target_missing() {
    // Nonexistent target_id must leave the tree intact.
    let mut tab = Tab::new("t".to_string(), leaf(1, "a"));
    assert!(!tab.split_at_pane(999, SplitDir::Vertical, leaf(2, "b")));
    assert_eq!(tab.root().pane_count(), 1);
}

#[test]
fn pane_count_and_agent_count_across_split() {
    // Tab with one agent + one shell, split-right. pane_count counts all
    // leaves; agent_count counts only panes whose backend is Some.
    let mut tab = Tab::new("mixed".to_string(), leaf_agent(1, "alice"));
    assert!(tab.split_focused(SplitDir::Vertical, leaf(2, "shell")));
    assert_eq!(tab.root().pane_count(), 2);
    assert_eq!(tab.root().agent_count(), 1);
}

#[test]
fn close_focused_updates_focus_to_sibling() {
    // Closing the focused pane must move focus_id to the remaining pane
    // so subsequent actions target a real pane (not a stale id).
    let mut tab = Tab::new("t".to_string(), leaf(1, "a"));
    assert!(tab.split_focused(SplitDir::Vertical, leaf(2, "b")));
    // After split_focused, focus_id still points at pane 1.
    assert_eq!(tab.focus_id, 1);
    let removed = tab.close_focused();
    assert_eq!(removed.as_deref(), Some("a"));
    assert_eq!(tab.root().pane_count(), 1);
    assert_eq!(tab.focus_id, 2, "focus must move to surviving pane");
}

#[test]
fn close_pane_by_id_returns_none_when_last() {
    // A tab with a single pane cannot close its last pane — the caller
    // should close the tab instead.
    let mut tab = Tab::new("t".to_string(), leaf(1, "only"));
    assert!(tab.close_pane_by_id(1).is_none());
    assert_eq!(tab.root().pane_count(), 1);
}

#[test]
fn cycle_focus_wraps_around_three_panes() {
    // With 3 panes, three cycles return focus to the origin.
    let mut tab = Tab::new("t".to_string(), leaf(1, "a"));
    assert!(tab.split_focused(SplitDir::Vertical, leaf(2, "b")));
    assert!(tab.split_focused(SplitDir::Vertical, leaf(3, "c")));
    let start = tab.focus_id;
    tab.cycle_focus();
    tab.cycle_focus();
    tab.cycle_focus();
    assert_eq!(tab.focus_id, start);
}

#[test]
fn apply_layout_even_horizontal_preserves_pane_count() {
    // Rebuilding a tree from a preset must not lose panes, and must reset
    // pane_rects (cached hit-test data is stale after re-tile).
    let mut tab = Tab::new("t".to_string(), leaf(1, "a"));
    assert!(tab.split_focused(SplitDir::Vertical, leaf(2, "b")));
    assert!(tab.split_focused(SplitDir::Horizontal, leaf(3, "c")));
    tab.pane_rects.insert(1, (0, 0, 10, 10));
    tab.apply_layout(LayoutPreset::EvenHorizontal);
    assert_eq!(tab.root().pane_count(), 3);
    assert_eq!(tab.last_layout, Some(LayoutPreset::EvenHorizontal));
    assert!(tab.pane_rects.is_empty(), "pane_rects must be cleared");
}

#[test]
fn next_layout_cycles_from_none_to_even_horizontal() {
    // First next_layout call (with last_layout = None) must land on
    // EvenHorizontal — the start of the cycle.
    let mut tab = Tab::new("t".to_string(), leaf(1, "a"));
    assert!(tab.split_focused(SplitDir::Vertical, leaf(2, "b")));
    assert!(tab.last_layout.is_none());
    tab.next_layout();
    assert_eq!(tab.last_layout, Some(LayoutPreset::EvenHorizontal));
}

#[test]
fn layout_next_tab_wraps_at_boundary() {
    // next_tab from the last tab wraps to the first.
    let mut layout = Layout::new();
    layout.add_tab(Tab::new("t1".to_string(), leaf(1, "a")));
    layout.add_tab(Tab::new("t2".to_string(), leaf(2, "b")));
    layout.add_tab(Tab::new("t3".to_string(), leaf(3, "c")));
    assert_eq!(layout.active, 2, "add_tab switches to new tab");
    layout.next_tab();
    assert_eq!(layout.active, 0, "wrap from last to first");
}

#[test]
fn swap_panes_across_nested_split() {
    // Build a tree where the two panes to swap live under different
    // sub-splits, forcing find_two_panes to recurse into (true, true) /
    // (false, false) branches. swap_panes uses mem::swap so the whole
    // Pane (id included) travels: post-swap, the two physical positions
    // hold each other's ids.
    let mut tab = Tab::new("t".to_string(), leaf(1, "a"));
    assert!(tab.split_focused(SplitDir::Vertical, leaf(2, "b")));
    tab.focus_id = 1;
    assert!(tab.split_focused(SplitDir::Horizontal, leaf(3, "c")));
    tab.focus_id = 2;
    assert!(tab.split_focused(SplitDir::Horizontal, leaf(4, "d")));

    let pre = tab.root().pane_ids();
    let first_id = pre[0];
    let last_id = *pre.last().expect("non-empty");
    assert!(swap_panes(tab.root_mut(), first_id, last_id));

    let post = tab.root().pane_ids();
    assert_eq!(post.len(), pre.len(), "no panes lost");
    assert_eq!(post[0], last_id, "first slot now holds the last pane");
    assert_eq!(
        *post.last().expect("non-empty"),
        first_id,
        "last slot now holds the first pane"
    );
}

// --- detach_pane / move_pane_across_tabs ---

#[test]
fn detach_pane_refuses_sole_pane() {
    // A lone pane can't be detached in-place because it would leave the
    // tab with an empty root. Callers must consume the whole tab via
    // Layout::move_pane_across_tabs instead.
    let mut tab = Tab::new("t".to_string(), leaf(1, "a"));
    assert!(tab.detach_pane(1).is_none());
    assert_eq!(tab.root().pane_count(), 1);
}

#[test]
fn detach_pane_missing_id_returns_none() {
    // Unknown pane id leaves the tree untouched.
    let mut tab = Tab::new("t".to_string(), leaf(1, "a"));
    assert!(tab.split_focused(SplitDir::Vertical, leaf(2, "b")));
    assert!(tab.detach_pane(999).is_none());
    assert_eq!(tab.root().pane_count(), 2);
}

#[test]
fn detach_pane_returns_pane_and_moves_focus() {
    // Detaching the focused pane hands it back to the caller and moves
    // focus to the sibling so subsequent input isn't orphaned.
    let mut tab = Tab::new("t".to_string(), leaf(1, "a"));
    assert!(tab.split_focused(SplitDir::Vertical, leaf(2, "b")));
    tab.focus_id = 1;
    let detached = tab.detach_pane(1).expect("pane 1 should detach");
    assert_eq!(detached.agent_name, "a");
    assert_eq!(tab.root().pane_count(), 1);
    assert_eq!(tab.focus_id, 2, "focus must move to the sibling pane");
}

#[test]
fn detach_pane_clears_transient_state() {
    // Drag/selection state that names the departing pane must be reset
    // so a half-finished interaction doesn't resume against a gone pane.
    let mut tab = Tab::new("t".to_string(), leaf(1, "a"));
    assert!(tab.split_focused(SplitDir::Vertical, leaf(2, "b")));
    tab.dragging_pane = Some(1);
    tab.drag_target = Some(1);
    tab.selecting_pane = Some(1);
    let _ = tab.detach_pane(1).expect("detaches");
    assert!(tab.dragging_pane.is_none());
    assert!(tab.drag_target.is_none());
    assert!(tab.selecting_pane.is_none());
}

#[test]
fn move_pane_across_tabs_same_tab_rejected() {
    // from == to would collapse into an in-place move; swap_panes covers
    // intra-tab reorder, so the cross-tab API refuses.
    let mut layout = Layout::new();
    layout.add_tab(Tab::new("t0".to_string(), leaf(1, "a")));
    layout.tabs[0].split_focused(SplitDir::Vertical, leaf(2, "b"));
    assert!(layout
        .move_pane_across_tabs(
            0,
            1,
            MovePlacement::SplitFocused {
                to_tab: 0,
                dir: SplitDir::Vertical
            }
        )
        .is_none());
    assert_eq!(layout.tabs[0].root().pane_count(), 2);
}

#[test]
fn move_pane_across_tabs_split_focused_preserves_both_tabs() {
    let mut layout = Layout::new();
    layout.add_tab(Tab::new("src".to_string(), leaf(1, "a")));
    layout.tabs[0].split_focused(SplitDir::Vertical, leaf(2, "b"));
    layout.add_tab(Tab::new("dst".to_string(), leaf(3, "c")));

    let dest = layout
        .move_pane_across_tabs(
            0,
            2,
            MovePlacement::SplitFocused {
                to_tab: 1,
                dir: SplitDir::Horizontal,
            },
        )
        .expect("move succeeds");
    assert_eq!(dest, 1);
    assert_eq!(layout.tabs.len(), 2);
    assert_eq!(layout.tabs[0].root().pane_count(), 1);
    assert!(!layout.tabs[0].root().has_agent("b"));
    assert_eq!(layout.tabs[1].root().pane_count(), 2);
    assert!(layout.tabs[1].root().has_agent("b"));
    assert_eq!(layout.tabs[1].focus_id, 2);
}

#[test]
fn move_pane_across_tabs_single_pane_source_removes_tab() {
    // Source tab disappears; returned dest index reflects the shift.
    let mut layout = Layout::new();
    layout.add_tab(Tab::new("solo".to_string(), leaf(1, "a")));
    layout.add_tab(Tab::new("dst".to_string(), leaf(2, "b")));
    let dest = layout
        .move_pane_across_tabs(
            0,
            1,
            MovePlacement::SplitFocused {
                to_tab: 1,
                dir: SplitDir::Horizontal,
            },
        )
        .expect("move succeeds");
    assert_eq!(dest, 0, "destination shifted left after source removed");
    assert_eq!(layout.tabs.len(), 1);
    assert_eq!(layout.tabs[0].name, "dst");
    assert_eq!(layout.tabs[0].root().pane_count(), 2);
    assert!(layout.tabs[0].root().has_agent("a"));
}

#[test]
fn move_pane_across_tabs_new_tab_placement() {
    let mut layout = Layout::new();
    layout.add_tab(Tab::new("src".to_string(), leaf(1, "a")));
    layout.tabs[0].split_focused(SplitDir::Vertical, leaf(2, "b"));

    let dest = layout
        .move_pane_across_tabs(
            0,
            2,
            MovePlacement::NewTab {
                name: "popped".to_string(),
            },
        )
        .expect("move succeeds");
    assert_eq!(dest, 1);
    assert_eq!(layout.tabs.len(), 2);
    assert_eq!(layout.tabs[1].name, "popped");
    assert_eq!(layout.tabs[1].root().pane_count(), 1);
    assert!(layout.tabs[1].root().has_agent("b"));
    assert_eq!(layout.active, 1);
}

#[test]
fn find_agent_pane_returns_location() {
    let mut layout = Layout::new();
    layout.add_tab(Tab::new("t0".to_string(), leaf(1, "a")));
    layout.add_tab(Tab::new("t1".to_string(), leaf(2, "b")));
    layout.tabs[1].split_focused(SplitDir::Vertical, leaf(3, "c"));

    assert_eq!(layout.find_agent_pane("a"), Some((0, 1)));
    assert_eq!(layout.find_agent_pane("c"), Some((1, 3)));
    assert_eq!(layout.find_agent_pane("ghost"), None);
}
