//! Split math — ratio calculation, hit-testing, resize, spatial navigation.

use super::tree::{ratio_bounds, PaneNode, SplitDir};
use ratatui::layout::Rect;

pub fn ratio_to_size(ratio: f32, total: u16) -> u16 {
    if total < 2 {
        return total / 2;
    }
    let (lo, hi) = ratio_bounds(total);
    let clamped = ratio.clamp(lo, hi);
    ((clamped * total as f32).round() as u16).clamp(1, total - 1)
}

/// Compute the (first, second) child areas of a split.
///
/// Siblings overlap by 1 cell on the split axis so they share a single border
/// column/row. This lets the render-time border grid merge adjacent pane borders
/// into joined box-drawing chars (`├┤┬┴┼`) across all terminals — macOS Terminal
/// in particular doesn't auto-join `┘┌` pairs into `┬` when drawn side-by-side.
///
/// Invariant (for non-degenerate sizes): `second_start = first_start + first_size - 1`
/// and `second_size = total - first_size + 1`. Total painted cells = `first_size + second_size - 1`.
#[allow(clippy::type_complexity)]
pub fn split_child_areas(
    area: (u16, u16, u16, u16),
    dir: SplitDir,
    ratio: f32,
) -> ((u16, u16, u16, u16), (u16, u16, u16, u16)) {
    let (ax, ay, aw, ah) = area;
    match dir {
        SplitDir::Horizontal => {
            let first_h = ratio_to_size(ratio, ah);
            let overlap = if first_h >= 1 && ah > first_h { 1 } else { 0 };
            let second_y = ay + first_h.saturating_sub(overlap);
            let second_h = ah + overlap - first_h;
            ((ax, ay, aw, first_h), (ax, second_y, aw, second_h))
        }
        SplitDir::Vertical => {
            let first_w = ratio_to_size(ratio, aw);
            let overlap = if first_w >= 1 && aw > first_w { 1 } else { 0 };
            let second_x = ax + first_w.saturating_sub(overlap);
            let second_w = aw + overlap - first_w;
            ((ax, ay, first_w, ah), (second_x, ay, second_w, ah))
        }
    }
}

/// Info about a split border detected at a mouse position.
#[derive(Clone, Copy)]
pub struct SplitBorderHit {
    /// The area of the split node that owns this border.
    pub split_area: (u16, u16, u16, u16),
    pub dir: SplitDir,
}

/// Mouse-hit tolerance for vertical-split borders, in columns. The
/// rendered separator (`│`) is one cell wide and dragging it precisely
/// is annoying — an off-by-one click silently fell through to the
/// text-selection path, leaving the user unable to resize horizontally
/// without pixel-level aim.
///
/// Applied only to `SplitDir::Vertical`. Horizontal splits are mouse-
/// resized via the bottom pane's title bar (see `app::mouse::handle`),
/// not the border itself, so widening that hit zone would just steal
/// clicks from pane content.
pub const VSPLIT_BORDER_HIT_TOLERANCE: u16 = 1;

/// Walk the pane tree with area info to find a split border at (col, row).
/// A border is the 1-cell boundary between the two children of a split,
/// optionally widened to a ±[`VSPLIT_BORDER_HIT_TOLERANCE`] zone for
/// vertical splits.
///
/// Children are checked before the parent so a click on a nested split's
/// exact border always resolves to the inner split, even when the outer's
/// tolerance zone reaches the same column. Without this order, the
/// parent would steal precise inner-border clicks the moment we widened
/// it.
pub fn find_split_border(
    node: &PaneNode,
    area: (u16, u16, u16, u16),
    col: u16,
    row: u16,
) -> Option<SplitBorderHit> {
    match node {
        PaneNode::Leaf(_) => None,
        PaneNode::Split {
            dir,
            ratio,
            first,
            second,
        } => {
            let (first_area, second_area) = split_child_areas(area, *dir, *ratio);
            if let Some(hit) = find_split_border(first, first_area, col, row) {
                return Some(hit);
            }
            if let Some(hit) = find_split_border(second, second_area, col, row) {
                return Some(hit);
            }
            let (ax, ay, aw, ah) = area;
            // Shared-border convention: with 1-cell overlap between siblings,
            // the border column/row is the last cell of `first` == first cell
            // of `second`. `split_child_areas` computes `second_{x,y}` as
            // `first_size - 1`, so we reuse that directly.
            let on_border = match dir {
                SplitDir::Horizontal => {
                    let border_row = second_area.1;
                    row == border_row && col >= ax && col < ax + aw
                }
                SplitDir::Vertical => {
                    let border_col = second_area.0;
                    col.abs_diff(border_col) <= VSPLIT_BORDER_HIT_TOLERANCE
                        && row >= ay
                        && row < ay + ah
                }
            };
            if on_border {
                Some(SplitBorderHit {
                    split_area: area,
                    dir: *dir,
                })
            } else {
                None
            }
        }
    }
}

/// Adjust the ratio of the split whose area matches `split_area`,
/// setting a new ratio based on mouse position.
pub fn adjust_split_ratio(
    node: &mut PaneNode,
    area: (u16, u16, u16, u16),
    split_area: (u16, u16, u16, u16),
    mouse_pos: u16,
    dir: SplitDir,
) -> bool {
    match node {
        PaneNode::Leaf(_) => false,
        PaneNode::Split {
            dir: d,
            ratio,
            first,
            second,
        } => {
            if area == split_area && *d == dir {
                let (start, total) = match dir {
                    SplitDir::Horizontal => (area.1, area.3),
                    SplitDir::Vertical => (area.0, area.2),
                };
                if total > 1 {
                    // With 1-cell overlap between siblings, the border sits at
                    // `first_size - 1` cells from `start`. So when the user
                    // drags the border to `mouse_pos`, the desired first_size
                    // is `(mouse_pos - start) + 1`.
                    let desired_first =
                        (mouse_pos.saturating_sub(start) as f32 + 1.0) / (total as f32);
                    let (lo, hi) = ratio_bounds(total);
                    *ratio = desired_first.clamp(lo, hi);
                }
                return true;
            }
            let (first_area, second_area) = split_child_areas(area, *d, *ratio);
            if adjust_split_ratio(first, first_area, split_area, mouse_pos, dir) {
                return true;
            }
            adjust_split_ratio(second, second_area, split_area, mouse_pos, dir)
        }
    }
}

/// Adjust the split ratio containing the focused pane in a given direction.
/// `step` is the ratio delta (positive = grow first child). `area` is the
/// rect enclosing `node`; it's tracked through recursion so the final clamp
/// uses bounds derived from the target split's actual cell count.
pub fn resize_focused(
    node: &mut PaneNode,
    area: (u16, u16, u16, u16),
    focus_id: usize,
    dir: Direction,
    step: f32,
) -> bool {
    match node {
        PaneNode::Leaf(_) => false,
        PaneNode::Split {
            dir: split_dir,
            ratio,
            first,
            second,
        } => {
            let first_has = first.find_pane(focus_id).is_some();
            let second_has = second.find_pane(focus_id).is_some();
            if !first_has && !second_has {
                return false;
            }
            let dir_matches = matches!(
                (*split_dir, dir),
                (SplitDir::Vertical, Direction::Left | Direction::Right)
                    | (SplitDir::Horizontal, Direction::Up | Direction::Down)
            );
            if dir_matches {
                // Absolute direction: Right/Down pushes the split boundary
                // right/down regardless of which side is focused (tmux-style).
                let delta = match dir {
                    Direction::Right | Direction::Down => step,
                    Direction::Left | Direction::Up => -step,
                };
                let total = match *split_dir {
                    SplitDir::Horizontal => area.3,
                    SplitDir::Vertical => area.2,
                };
                let (lo, hi) = ratio_bounds(total);
                *ratio = (*ratio + delta).clamp(lo, hi);
                return true;
            }
            // Recurse into the child containing focus, tracking its area.
            let (first_area, second_area) = split_child_areas(area, *split_dir, *ratio);
            if first_has {
                resize_focused(first, first_area, focus_id, dir, step)
            } else {
                resize_focused(second, second_area, focus_id, dir, step)
            }
        }
    }
}

pub(super) fn center(rect: (u16, u16, u16, u16)) -> (i32, i32) {
    let (x, y, w, h) = rect;
    (x as i32 + w as i32 / 2, y as i32 + h as i32 / 2)
}

pub(super) fn overlaps_y(a: (u16, u16, u16, u16), b: (u16, u16, u16, u16)) -> bool {
    let a_top = a.1 as i32;
    let a_bot = a.1 as i32 + a.3 as i32;
    let b_top = b.1 as i32;
    let b_bot = b.1 as i32 + b.3 as i32;
    a_top < b_bot && b_top < a_bot
}

pub(super) fn overlaps_x(a: (u16, u16, u16, u16), b: (u16, u16, u16, u16)) -> bool {
    let a_left = a.0 as i32;
    let a_right = a.0 as i32 + a.2 as i32;
    let b_left = b.0 as i32;
    let b_right = b.0 as i32 + b.2 as i32;
    a_left < b_right && b_left < a_right
}

#[derive(Clone, Copy)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

/// Split an area into two sub-rects along the given direction, with 1-cell
/// overlap at the border (so the border character is shared between both
/// children). Mirror of `render::render_pane_tree`'s split logic
/// — keep the two in sync (both produce overlap-by-1 results).
pub(crate) fn split_chunks(area: Rect, dir: &SplitDir, ratio: f32) -> [Rect; 2] {
    let total = match dir {
        SplitDir::Horizontal => area.height,
        SplitDir::Vertical => area.width,
    };
    let first_size = ratio_to_size(ratio, total);
    let overlap: u16 = if first_size >= 1 && total > first_size {
        1
    } else {
        0
    };
    match dir {
        SplitDir::Horizontal => {
            let second_y = area.y + first_size.saturating_sub(overlap);
            // H4: saturating_sub prevents underflow on tiny terminals
            let second_h = (area.height + overlap).saturating_sub(first_size).max(1);
            [
                Rect::new(area.x, area.y, area.width, first_size),
                Rect::new(area.x, second_y, area.width, second_h),
            ]
        }
        SplitDir::Vertical => {
            let second_x = area.x + first_size.saturating_sub(overlap);
            // H4: saturating_sub prevents underflow on tiny terminals
            let second_w = (area.width + overlap).saturating_sub(first_size).max(1);
            [
                Rect::new(area.x, area.y, first_size, area.height),
                Rect::new(second_x, area.y, second_w, area.height),
            ]
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::layout::pane::{Pane, PaneSource};
    use crate::layout::tree::{PaneNode, SplitDir, MIN_PANE_CELLS};
    use crate::vterm::VTerm;

    fn mk_pane(id: usize, name: &str) -> Pane {
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
    fn mk_leaf(id: usize, name: &str) -> PaneNode {
        PaneNode::Leaf(Box::new(mk_pane(id, name)))
    }

    #[test]
    fn ratio_to_size_no_zero_when_room() {
        for total in [6u16, 10, 40, 100, 500] {
            for ri in 0..=100 {
                let r = ri as f32 / 100.0;
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
        assert_eq!(ratio_to_size(0.5, 0), 0);
        assert_eq!(ratio_to_size(0.5, 1), 0);
    }
    #[test]
    fn ratio_to_size_sum_matches_total() {
        for total in [2u16, 3, 10, 100, 1000] {
            for ri in [0, 25, 50, 75, 100] {
                let r = ri as f32 / 100.0;
                let first = ratio_to_size(r, total);
                assert!(first <= total);
            }
        }
    }
    #[test]
    fn split_child_areas_vertical_siblings_share_one_cell() {
        let area = (0u16, 0u16, 20u16, 10u16);
        let (first, second) = split_child_areas(area, SplitDir::Vertical, 0.5);
        assert_eq!(second.0, first.0 + first.2 - 1);
        assert_eq!(first.2 + second.2 - 1, area.2);
    }
    #[test]
    fn split_child_areas_horizontal_siblings_share_one_cell() {
        let area = (0u16, 0u16, 20u16, 10u16);
        let (first, second) = split_child_areas(area, SplitDir::Horizontal, 0.5);
        assert_eq!(second.1, first.1 + first.3 - 1);
        assert_eq!(first.3 + second.3 - 1, area.3);
    }
    #[test]
    fn find_split_border_matches_shared_column() {
        let root = PaneNode::Split {
            dir: SplitDir::Vertical,
            ratio: 0.5,
            first: Box::new(mk_leaf(1, "a")),
            second: Box::new(mk_leaf(2, "b")),
        };
        let area = (0u16, 0u16, 20u16, 10u16);
        let (first, second) = split_child_areas(area, SplitDir::Vertical, 0.5);
        let shared_col = first.0 + first.2 - 1;
        assert_eq!(shared_col, second.0);
        assert!(find_split_border(&root, area, shared_col, 5).is_some());
        assert!(find_split_border(&root, area, shared_col + 1, 5).is_some());
        assert!(find_split_border(&root, area, shared_col - 1, 5).is_some());
        assert!(find_split_border(&root, area, shared_col + 2, 5).is_none());
        assert!(find_split_border(&root, area, shared_col.saturating_sub(2), 5).is_none());
    }
    #[test]
    fn nested_vsplit_inner_border_wins_over_outer_tolerance() {
        let inner = PaneNode::Split {
            dir: SplitDir::Vertical,
            ratio: 0.5,
            first: Box::new(mk_leaf(1, "a")),
            second: Box::new(mk_leaf(2, "b")),
        };
        let outer = PaneNode::Split {
            dir: SplitDir::Vertical,
            ratio: 0.5,
            first: Box::new(inner),
            second: Box::new(mk_leaf(3, "c")),
        };
        let outer_area = (0u16, 0u16, 40u16, 10u16);
        let (inner_area, outer_second_area) =
            split_child_areas(outer_area, SplitDir::Vertical, 0.5);
        let outer_border = outer_second_area.0;
        let (inner_first, _) = split_child_areas(inner_area, SplitDir::Vertical, 0.5);
        let inner_border = inner_first.0 + inner_first.2 - 1;
        assert_ne!(inner_border, outer_border);
        let hit = find_split_border(&outer, outer_area, inner_border, 5).unwrap();
        assert_eq!(hit.split_area, inner_area);
        let hit = find_split_border(&outer, outer_area, outer_border, 5).unwrap();
        assert_eq!(hit.split_area, outer_area);
    }
    #[test]
    fn hsplit_border_hit_is_not_widened() {
        let root = PaneNode::Split {
            dir: SplitDir::Horizontal,
            ratio: 0.5,
            first: Box::new(mk_leaf(1, "top")),
            second: Box::new(mk_leaf(2, "bot")),
        };
        let area = (0u16, 0u16, 20u16, 10u16);
        let (_, second) = split_child_areas(area, SplitDir::Horizontal, 0.5);
        let border_row = second.1;
        assert!(find_split_border(&root, area, 5, border_row).is_some());
        assert!(find_split_border(&root, area, 5, border_row + 1).is_none());
        assert!(find_split_border(&root, area, 5, border_row.saturating_sub(1)).is_none());
    }
    #[test]
    fn adjust_split_ratio_border_lands_where_user_clicked() {
        let mut root = PaneNode::Split {
            dir: SplitDir::Vertical,
            ratio: 0.5,
            first: Box::new(mk_leaf(1, "a")),
            second: Box::new(mk_leaf(2, "b")),
        };
        let area = (0u16, 0u16, 100u16, 20u16);
        assert!(adjust_split_ratio(
            &mut root,
            area,
            area,
            60,
            SplitDir::Vertical
        ));
        if let PaneNode::Split { ratio, .. } = &root {
            let (first, _) = split_child_areas(area, SplitDir::Vertical, *ratio);
            let new_border = first.0 + first.2 - 1;
            assert!((new_border as i32 - 60).abs() <= 1);
        } else {
            panic!("root should be Split");
        }
    }
}
