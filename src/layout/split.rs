//! Split math — ratio calculation, hit-testing, resize, spatial navigation.

use super::pane::Pane;
use super::tree::{PaneNode, SplitDir, MIN_PANE_CELLS, ratio_bounds};
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
