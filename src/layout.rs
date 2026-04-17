//! Tab and pane layout management — tree-based nested splits.

use crate::backend::Backend;
use crate::vterm::VTerm;
use std::path::PathBuf;
use unicode_width::UnicodeWidthStr;

/// A single pane displaying one agent's terminal output.
/// PTY ownership is in AgentRegistry — pane only has subscriber channel + local VTerm.
pub struct Pane {
    pub agent_name: String,
    pub vterm: VTerm,
    pub rx: crossbeam::channel::Receiver<Vec<u8>>,
    pub id: usize,
    pub backend: Option<Backend>,
    /// Working directory this pane was spawned in.
    pub working_dir: Option<PathBuf>,
    /// User-defined display name (shown in pane border). agent_name is used if None.
    pub display_name: Option<String>,
    /// Scroll offset (lines from bottom). 0 = live view.
    pub scroll_offset: usize,
    /// True when an unread `[from:...]` message was detected.
    pub has_notification: bool,
    /// Fleet instance name (key in fleet.yaml). None for shell panes.
    pub fleet_instance_name: Option<String>,
    /// Active text selection (grid coordinates within this pane's VTerm).
    pub selection: Option<Selection>,
}

/// Text selection within a pane's VTerm grid.
#[derive(Clone)]
pub struct Selection {
    /// Start position (row, col) in VTerm grid coordinates.
    pub start: (u16, u16),
    /// End position (row, col) — may be before or after start.
    pub end: (u16, u16),
}

impl Pane {
    /// Display label: display_name if set, otherwise agent_name.
    pub fn label(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.agent_name)
    }

    /// Drain pending output into the local VTerm.
    pub fn drain_output(&mut self) {
        while let Ok(data) = self.rx.try_recv() {
            self.vterm.process(&data);
            if self.backend.is_some() {
                let text = String::from_utf8_lossy(&data);
                if text.contains("[from:") {
                    self.has_notification = true;
                }
            }
        }
        // Don't auto-scroll if user has scrolled back (they're reading history).
        // User scrolls back to bottom manually via mouse or Ctrl+B [ → j.
    }
}

/// Split direction.
#[derive(Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum SplitDir {
    Horizontal,
    Vertical,
}

impl SplitDir {
    pub fn opposite(self) -> Self {
        match self {
            Self::Horizontal => Self::Vertical,
            Self::Vertical => Self::Horizontal,
        }
    }
}

/// Tree node: either a leaf (pane) or a split containing two children.
pub enum PaneNode {
    Leaf(Box<Pane>),
    Split {
        dir: SplitDir,
        ratio: f32,
        first: Box<PaneNode>,
        second: Box<PaneNode>,
    },
}

const DEFAULT_RATIO: f32 = 0.5;

/// Minimum cells (columns or rows) required per pane child to remain usable.
/// All ratio clamps derive from this so the bounds scale with terminal size —
/// a 400-cell area allows near-full drag range, while a 10-cell area still
/// guarantees both sides are visible.
const MIN_PANE_CELLS: u16 = 3;

/// Valid ratio bounds for a split of `total` cells. Returns `(min, max)` such
/// that both children end up with ≥ MIN_PANE_CELLS. When `total` is too small
/// to honor both minimums, returns `(0.5, 0.5)` — callers should avoid
/// splitting such tiny areas in the first place.
fn ratio_bounds(total: u16) -> (f32, f32) {
    if total < 2 * MIN_PANE_CELLS {
        return (0.5, 0.5);
    }
    let min = MIN_PANE_CELLS as f32 / total as f32;
    (min, 1.0 - min)
}

impl PaneNode {
    pub fn pane_ids(&self) -> Vec<usize> {
        match self {
            PaneNode::Leaf(p) => vec![p.id],
            PaneNode::Split { first, second, .. } => {
                let mut ids = first.pane_ids();
                ids.extend(second.pane_ids());
                ids
            }
        }
    }

    pub fn find_pane(&self, id: usize) -> Option<&Pane> {
        match self {
            PaneNode::Leaf(p) if p.id == id => Some(p),
            PaneNode::Leaf(_) => None,
            PaneNode::Split { first, second, .. } => {
                first.find_pane(id).or_else(|| second.find_pane(id))
            }
        }
    }

    pub fn find_pane_mut(&mut self, id: usize) -> Option<&mut Pane> {
        match self {
            PaneNode::Leaf(p) if p.id == id => Some(p),
            PaneNode::Leaf(_) => None,
            PaneNode::Split { first, second, .. } => {
                first.find_pane_mut(id).or_else(|| second.find_pane_mut(id))
            }
        }
    }

    pub fn first_pane(&self) -> &Pane {
        match self {
            PaneNode::Leaf(p) => p,
            PaneNode::Split { first, .. } => first.first_pane(),
        }
    }

    pub fn pane_count(&self) -> usize {
        match self {
            PaneNode::Leaf(_) => 1,
            PaneNode::Split { first, second, .. } => first.pane_count() + second.pane_count(),
        }
    }

    pub fn agent_count(&self) -> usize {
        match self {
            PaneNode::Leaf(p) => usize::from(p.backend.is_some()),
            PaneNode::Split { first, second, .. } => first.agent_count() + second.agent_count(),
        }
    }

    /// True if any pane in this subtree has an unread notification.
    pub fn has_notification(&self) -> bool {
        match self {
            PaneNode::Leaf(p) => p.has_notification,
            PaneNode::Split { first, second, .. } => {
                first.has_notification() || second.has_notification()
            }
        }
    }

    /// Collect all agent names in the tree.
    pub fn agent_names(&self) -> Vec<String> {
        match self {
            PaneNode::Leaf(p) => vec![p.agent_name.clone()],
            PaneNode::Split { first, second, .. } => {
                let mut names = first.agent_names();
                names.extend(second.agent_names());
                names
            }
        }
    }

    /// Check if any pane in the tree has the given agent name (no allocation).
    pub fn has_agent(&self, name: &str) -> bool {
        match self {
            PaneNode::Leaf(p) => p.agent_name == name,
            PaneNode::Split { first, second, .. } => {
                first.has_agent(name) || second.has_agent(name)
            }
        }
    }

    /// Find the pane ID for a given agent name.
    pub fn find_pane_id_by_agent(&self, name: &str) -> Option<usize> {
        match self {
            PaneNode::Leaf(p) if p.agent_name == name => Some(p.id),
            PaneNode::Leaf(_) => None,
            PaneNode::Split { first, second, .. } => first
                .find_pane_id_by_agent(name)
                .or_else(|| second.find_pane_id_by_agent(name)),
        }
    }
}

// --- Ownership-based tree transforms ---

fn split_in_tree(
    node: PaneNode,
    target_id: usize,
    dir: SplitDir,
    new_pane: Pane,
) -> (PaneNode, Option<Pane>) {
    match node {
        PaneNode::Leaf(p) if p.id == target_id => (
            PaneNode::Split {
                dir,
                ratio: DEFAULT_RATIO,
                first: Box::new(PaneNode::Leaf(p)),
                second: Box::new(PaneNode::Leaf(Box::new(new_pane))),
            },
            None,
        ),
        PaneNode::Leaf(p) => (PaneNode::Leaf(p), Some(new_pane)),
        PaneNode::Split {
            dir: d,
            ratio,
            first,
            second,
        } => {
            let (new_first, remaining) = split_in_tree(*first, target_id, dir, new_pane);
            if let Some(pane) = remaining {
                let (new_second, remaining) = split_in_tree(*second, target_id, dir, pane);
                (
                    PaneNode::Split {
                        dir: d,
                        ratio,
                        first: Box::new(new_first),
                        second: Box::new(new_second),
                    },
                    remaining,
                )
            } else {
                (
                    PaneNode::Split {
                        dir: d,
                        ratio,
                        first: Box::new(new_first),
                        second,
                    },
                    None,
                )
            }
        }
    }
}

fn remove_from_tree(node: PaneNode, target_id: usize) -> (PaneNode, Option<Pane>) {
    match node {
        PaneNode::Leaf(p) => (PaneNode::Leaf(p), None),
        PaneNode::Split {
            dir,
            ratio,
            first,
            second,
        } => {
            if let PaneNode::Leaf(ref p) = *first {
                if p.id == target_id {
                    let PaneNode::Leaf(removed) = *first else {
                        unreachable!()
                    };
                    return (*second, Some(*removed));
                }
            }
            if let PaneNode::Leaf(ref p) = *second {
                if p.id == target_id {
                    let PaneNode::Leaf(removed) = *second else {
                        unreachable!()
                    };
                    return (*first, Some(*removed));
                }
            }
            let (new_first, removed) = remove_from_tree(*first, target_id);
            if removed.is_some() {
                return (
                    PaneNode::Split {
                        dir,
                        ratio,
                        first: Box::new(new_first),
                        second,
                    },
                    removed,
                );
            }
            let (new_second, removed) = remove_from_tree(*second, target_id);
            (
                PaneNode::Split {
                    dir,
                    ratio,
                    first: Box::new(new_first),
                    second: Box::new(new_second),
                },
                removed,
            )
        }
    }
}

// --- Layout presets ---

/// Predefined pane arrangement patterns (tmux-compatible).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LayoutPreset {
    /// All panes side by side (vertical splits).
    EvenHorizontal,
    /// All panes stacked top to bottom (horizontal splits).
    EvenVertical,
    /// First pane large on left, rest stacked on right.
    MainVertical,
    /// First pane large on top, rest side by side on bottom.
    MainHorizontal,
    /// Balanced grid layout.
    Tiled,
}

impl LayoutPreset {
    /// Cycle to the next preset.
    pub fn next(self) -> Self {
        match self {
            Self::EvenHorizontal => Self::EvenVertical,
            Self::EvenVertical => Self::MainVertical,
            Self::MainVertical => Self::MainHorizontal,
            Self::MainHorizontal => Self::Tiled,
            Self::Tiled => Self::EvenHorizontal,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::EvenHorizontal => "even-horizontal",
            Self::EvenVertical => "even-vertical",
            Self::MainVertical => "main-vertical",
            Self::MainHorizontal => "main-horizontal",
            Self::Tiled => "tiled",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "even-horizontal" | "even-h" => Some(Self::EvenHorizontal),
            "even-vertical" | "even-v" => Some(Self::EvenVertical),
            "main-vertical" | "main-v" => Some(Self::MainVertical),
            "main-horizontal" | "main-h" => Some(Self::MainHorizontal),
            "tiled" | "tile" => Some(Self::Tiled),
            _ => None,
        }
    }

    pub fn all_names() -> &'static str {
        "even-horizontal, even-vertical, main-vertical, main-horizontal, tiled"
    }
}

/// Collect all panes from a tree in left-to-right order (consuming the tree).
fn flatten_tree_into(node: PaneNode, acc: &mut Vec<Pane>) {
    match node {
        PaneNode::Leaf(p) => acc.push(*p),
        PaneNode::Split { first, second, .. } => {
            flatten_tree_into(*first, acc);
            flatten_tree_into(*second, acc);
        }
    }
}

/// Build a binary tree splitting panes evenly. When `alternate` is true,
/// child splits use the opposite direction (tiled grid effect).
fn build_tree(panes: Vec<Pane>, dir: SplitDir, alternate: bool) -> PaneNode {
    debug_assert!(!panes.is_empty());
    if panes.len() == 1 {
        return PaneNode::Leaf(Box::new(panes.into_iter().next().expect("checked len")));
    }
    let mid = panes.len() / 2;
    let mut left = panes;
    let right = left.split_off(mid);
    let child_dir = if alternate { dir.opposite() } else { dir };
    PaneNode::Split {
        dir,
        ratio: DEFAULT_RATIO,
        first: Box::new(build_tree(left, child_dir, alternate)),
        second: Box::new(build_tree(right, child_dir, alternate)),
    }
}

/// Rebuild the pane tree according to a layout preset.
fn build_preset(panes: Vec<Pane>, preset: LayoutPreset) -> PaneNode {
    debug_assert!(!panes.is_empty());
    if panes.len() == 1 {
        return PaneNode::Leaf(Box::new(panes.into_iter().next().expect("checked len")));
    }
    match preset {
        LayoutPreset::EvenHorizontal => build_tree(panes, SplitDir::Vertical, false),
        LayoutPreset::EvenVertical => build_tree(panes, SplitDir::Horizontal, false),
        LayoutPreset::MainVertical => {
            let mut main = panes;
            let rest = main.split_off(1);
            PaneNode::Split {
                dir: SplitDir::Vertical,
                ratio: DEFAULT_RATIO,
                first: Box::new(PaneNode::Leaf(Box::new(
                    main.into_iter().next().expect("split_off(1)"),
                ))),
                second: Box::new(build_tree(rest, SplitDir::Horizontal, false)),
            }
        }
        LayoutPreset::MainHorizontal => {
            let mut main = panes;
            let rest = main.split_off(1);
            PaneNode::Split {
                dir: SplitDir::Horizontal,
                ratio: DEFAULT_RATIO,
                first: Box::new(PaneNode::Leaf(Box::new(
                    main.into_iter().next().expect("split_off(1)"),
                ))),
                second: Box::new(build_tree(rest, SplitDir::Vertical, false)),
            }
        }
        LayoutPreset::Tiled => build_tree(panes, SplitDir::Horizontal, true),
    }
}

// --- Split border hit-test and resize ---

/// Convert a stored ratio to a cell size. Self-corrects when `total` changed
/// since the ratio was set (e.g. terminal resize) by re-clamping to the bounds
/// valid for the current `total`. For `total < 2`, returns `total / 2`; the
/// caller shouldn't be splitting such an area, but we degrade gracefully
/// instead of panicking.
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
fn split_child_areas(
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

/// Walk the pane tree with area info to find a split border at (col, row).
/// A border is the 1-cell boundary between the two children of a split.
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
                    col == border_col && row >= ay && row < ay + ah
                }
            };
            if on_border {
                return Some(SplitBorderHit {
                    split_area: area,
                    dir: *dir,
                });
            }
            if let Some(hit) = find_split_border(first, first_area, col, row) {
                return Some(hit);
            }
            find_split_border(second, second_area, col, row)
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

// --- Pane swap ---

/// Swap two panes in the tree by ID. Returns true if both were found and swapped.
pub fn swap_panes(root: &mut PaneNode, id_a: usize, id_b: usize) -> bool {
    if id_a == id_b {
        return false;
    }
    match root {
        PaneNode::Leaf(_) => false,
        PaneNode::Split { first, second, .. } => {
            if let Some((pa, pb)) = find_two_panes(first, second, id_a, id_b) {
                std::mem::swap(pa, pb);
                true
            } else {
                false
            }
        }
    }
}

/// Find mutable references to two panes across sibling subtrees for swapping.
fn find_two_panes<'a>(
    first: &'a mut PaneNode,
    second: &'a mut PaneNode,
    id_a: usize,
    id_b: usize,
) -> Option<(&'a mut Pane, &'a mut Pane)> {
    let a_in_first = first.find_pane(id_a).is_some();
    let b_in_first = first.find_pane(id_b).is_some();

    match (a_in_first, b_in_first) {
        (true, false) => {
            let pa = first.find_pane_mut(id_a)?;
            let pb = second.find_pane_mut(id_b)?;
            Some((pa, pb))
        }
        (false, true) => {
            let pa = second.find_pane_mut(id_a)?;
            let pb = first.find_pane_mut(id_b)?;
            Some((pa, pb))
        }
        (true, true) => match first {
            PaneNode::Split {
                first: c1,
                second: c2,
                ..
            } => find_two_panes(c1, c2, id_a, id_b),
            _ => None,
        },
        (false, false) => match second {
            PaneNode::Split {
                first: c1,
                second: c2,
                ..
            } => find_two_panes(c1, c2, id_a, id_b),
            _ => None,
        },
    }
}

// --- Spatial navigation helpers ---

fn center(rect: (u16, u16, u16, u16)) -> (i32, i32) {
    let (x, y, w, h) = rect;
    (x as i32 + w as i32 / 2, y as i32 + h as i32 / 2)
}

fn overlaps_y(a: (u16, u16, u16, u16), b: (u16, u16, u16, u16)) -> bool {
    let a_top = a.1 as i32;
    let a_bot = a.1 as i32 + a.3 as i32;
    let b_top = b.1 as i32;
    let b_bot = b.1 as i32 + b.3 as i32;
    a_top < b_bot && b_top < a_bot
}

fn overlaps_x(a: (u16, u16, u16, u16), b: (u16, u16, u16, u16)) -> bool {
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

/// A tab containing a tree of panes.
pub struct Tab {
    pub name: String,
    root: Option<PaneNode>,
    pub focus_id: usize,
    pub zoomed: bool,
    pub pane_rects: std::collections::HashMap<usize, (u16, u16, u16, u16)>,
    /// Pane currently being selected with mouse (cached to avoid lookup on drag).
    pub selecting_pane: Option<usize>,
    /// Last applied layout preset (for cycling with next_layout).
    pub last_layout: Option<LayoutPreset>,
    /// Pane currently being dragged by title bar (drag-to-swap).
    pub dragging_pane: Option<usize>,
    /// Drop target pane during title bar drag.
    pub drag_target: Option<usize>,
}

impl Tab {
    pub fn new(name: String, pane: Pane) -> Self {
        let id = pane.id;
        Self {
            name,
            root: Some(PaneNode::Leaf(Box::new(pane))),
            focus_id: id,
            zoomed: false,
            pane_rects: std::collections::HashMap::new(),
            selecting_pane: None,
            last_layout: None,
            dragging_pane: None,
            drag_target: None,
        }
    }

    /// Construct a tab from an existing pane tree (used by session restore).
    pub fn with_root(name: String, root: PaneNode) -> Self {
        let first_id = root.first_pane().id;
        Self {
            name,
            root: Some(root),
            focus_id: first_id,
            zoomed: false,
            pane_rects: std::collections::HashMap::new(),
            selecting_pane: None,
            last_layout: None,
            dragging_pane: None,
            drag_target: None,
        }
    }

    pub fn root(&self) -> &PaneNode {
        self.root.as_ref().expect("root is always Some")
    }

    pub fn root_mut(&mut self) -> &mut PaneNode {
        self.root.as_mut().expect("root is always Some")
    }

    pub fn focused_pane(&self) -> Option<&Pane> {
        self.root().find_pane(self.focus_id)
    }

    pub fn cycle_focus(&mut self) {
        let ids = self.root().pane_ids();
        if let Some(pos) = ids.iter().position(|&id| id == self.focus_id) {
            self.focus_id = ids[(pos + 1) % ids.len()];
        }
    }

    pub fn focus_direction(&mut self, dir: Direction) {
        if self.pane_rects.len() < 2 {
            let delta = match dir {
                Direction::Up | Direction::Left => -1,
                Direction::Down | Direction::Right => 1,
            };
            let ids = self.root().pane_ids();
            if let Some(pos) = ids.iter().position(|&id| id == self.focus_id) {
                self.focus_id = ids[(pos as i32 + delta).rem_euclid(ids.len() as i32) as usize];
            }
            return;
        }

        let cur = match self.pane_rects.get(&self.focus_id) {
            Some(r) => *r,
            None => return,
        };
        let (cx, cy) = center(cur);

        let mut candidates: Vec<(usize, i32, bool)> = Vec::new();
        for (&id, &rect) in &self.pane_rects {
            if id == self.focus_id {
                continue;
            }
            let (rx, ry) = center(rect);
            let in_direction = match dir {
                Direction::Up => ry < cy,
                Direction::Down => ry > cy,
                Direction::Left => rx < cx,
                Direction::Right => rx > cx,
            };
            if !in_direction {
                continue;
            }
            let has_overlap = match dir {
                Direction::Left | Direction::Right => overlaps_y(cur, rect),
                Direction::Up | Direction::Down => overlaps_x(cur, rect),
            };
            let dist = match dir {
                Direction::Up | Direction::Down => (ry - cy).abs(),
                Direction::Left | Direction::Right => (rx - cx).abs(),
            };
            candidates.push((id, dist, has_overlap));
        }

        let best = candidates
            .iter()
            .filter(|(_, _, overlaps)| *overlaps)
            .min_by_key(|(_, dist, _)| *dist)
            .or_else(|| candidates.iter().min_by_key(|(_, dist, _)| *dist));

        if let Some(&(id, _, _)) = best {
            self.focus_id = id;
        } else {
            // Wrap around
            let mut wrap: Vec<(usize, i32, bool)> = Vec::new();
            for (&id, &rect) in &self.pane_rects {
                if id == self.focus_id {
                    continue;
                }
                let (rx, ry) = center(rect);
                let has_overlap = match dir {
                    Direction::Left | Direction::Right => overlaps_y(cur, rect),
                    Direction::Up | Direction::Down => overlaps_x(cur, rect),
                };
                let dist = match dir {
                    Direction::Up | Direction::Down => (ry - cy).abs(),
                    Direction::Left | Direction::Right => (rx - cx).abs(),
                };
                wrap.push((id, dist, has_overlap));
            }
            let farthest = wrap
                .iter()
                .filter(|(_, _, o)| *o)
                .max_by_key(|(_, d, _)| *d)
                .or_else(|| wrap.iter().max_by_key(|(_, d, _)| *d));
            if let Some(&(id, _, _)) = farthest {
                self.focus_id = id;
            }
        }
    }

    /// Rearrange all panes in this tab according to a layout preset.
    pub fn apply_layout(&mut self, preset: LayoutPreset) {
        let count = self.root().pane_count();
        if count < 2 {
            self.last_layout = Some(preset);
            return;
        }
        let root = self.root.take().expect("root is always Some");
        let mut panes = Vec::with_capacity(count);
        flatten_tree_into(root, &mut panes);
        self.root = Some(build_preset(panes, preset));
        self.last_layout = Some(preset);
        self.pane_rects.clear();
    }

    /// Cycle to the next layout preset.
    pub fn next_layout(&mut self) {
        let next = self
            .last_layout
            .map_or(LayoutPreset::EvenHorizontal, |p| p.next());
        self.apply_layout(next);
    }

    pub fn split_focused(&mut self, dir: SplitDir, new_pane: Pane) -> bool {
        let root = self.root.take().expect("root is always Some");
        let (new_root, remaining) = split_in_tree(root, self.focus_id, dir, new_pane);
        self.root = Some(new_root);
        remaining.is_none()
    }

    /// Pane ID whose rect contains (col, row), if any.
    pub fn pane_at(&self, col: u16, row: u16) -> Option<usize> {
        self.pane_rects
            .iter()
            .find(|(_, &(px, py, pw, ph))| col >= px && col < px + pw && row >= py && row < py + ph)
            .map(|(&id, _)| id)
    }

    /// Pane ID whose title-text region contains (col, row), if any.
    /// Title occupies columns [px+1, px+1+label_len+2) — matches the ` {label} `
    /// rendering in render_pane. Agent state suffix (` [state] `) is excluded so
    /// that clicks on it fall through to split-border resize.
    pub fn title_bar_at(&self, col: u16, row: u16) -> Option<usize> {
        for (&id, &(px, py, pw, _ph)) in &self.pane_rects {
            if row != py {
                continue;
            }
            let Some(pane) = self.root().find_pane(id) else {
                continue;
            };
            // Rendered title is ` {label} ` — measure with terminal cell
            // width (not char count) so CJK / emoji labels hit correctly.
            let title_width = UnicodeWidthStr::width(pane.label()) as u16 + 2;
            let start = px + 1;
            let end = (start + title_width).min(px + pw);
            if col >= start && col < end {
                return Some(id);
            }
        }
        None
    }

    /// Reset both drag fields after a title-bar drag completes or aborts.
    pub fn clear_drag(&mut self) {
        self.dragging_pane = None;
        self.drag_target = None;
    }

    /// Clear all in-progress UI state (selection tracking + drag tracking).
    /// Called when the user leaves this tab so a half-finished mouse
    /// interaction doesn't resume if they return to the tab later.
    pub fn clear_transient_input(&mut self) {
        self.selecting_pane = None;
        self.dragging_pane = None;
        self.drag_target = None;
    }

    /// Close the focused pane. Returns the removed pane's agent_name.
    pub fn close_focused(&mut self) -> Option<String> {
        self.close_pane_by_id(self.focus_id)
    }

    /// Close a pane by ID. Returns the removed pane's agent_name, or None if
    /// this is the last pane (tab should be removed instead).
    pub fn close_pane_by_id(&mut self, pane_id: usize) -> Option<String> {
        if self.root().pane_count() <= 1 {
            return None;
        }
        let ids = self.root().pane_ids();
        let next_id = ids
            .iter()
            .find(|&&id| id != pane_id)
            .copied()
            .unwrap_or(pane_id);

        let root = self.root.take().expect("root is always Some");
        let (new_root, removed) = remove_from_tree(root, pane_id);
        self.root = Some(new_root);
        if self.focus_id == pane_id {
            self.focus_id = next_id;
        }
        removed.map(|p| p.agent_name)
    }
}

/// Top-level layout.
pub struct Layout {
    pub tabs: Vec<Tab>,
    pub active: usize,
    next_pane_id: usize,
}

impl Layout {
    pub fn new() -> Self {
        Self {
            tabs: Vec::new(),
            active: 0,
            next_pane_id: 0,
        }
    }

    pub fn next_pane_id(&mut self) -> usize {
        let id = self.next_pane_id;
        self.next_pane_id += 1;
        id
    }

    pub fn add_tab(&mut self, tab: Tab) {
        self.switch_active(self.tabs.len());
        self.tabs.push(tab);
    }

    pub fn active_tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }

    pub fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active)
    }

    /// Change the active tab index, clearing the outgoing tab's in-progress
    /// mouse state (selection / drag tracking) so it doesn't resume if the
    /// user returns. Centralizing here keeps the invariant in one place.
    fn switch_active(&mut self, new_idx: usize) {
        if let Some(old) = self.tabs.get_mut(self.active) {
            old.clear_transient_input();
        }
        self.active = new_idx;
    }

    pub fn next_tab(&mut self) {
        if !self.tabs.is_empty() {
            self.switch_active((self.active + 1) % self.tabs.len());
        }
    }

    pub fn prev_tab(&mut self) {
        if !self.tabs.is_empty() {
            self.switch_active((self.active + self.tabs.len() - 1) % self.tabs.len());
        }
    }

    pub fn goto_tab(&mut self, idx: usize) {
        if idx < self.tabs.len() {
            self.switch_active(idx);
        }
    }

    pub fn close_tab(&mut self, idx: usize) -> Option<Tab> {
        if idx >= self.tabs.len() {
            return None;
        }
        let tab = self.tabs.remove(idx);
        if self.active >= self.tabs.len() && !self.tabs.is_empty() {
            self.switch_active(self.tabs.len() - 1);
        }
        Some(tab)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                rx: crossbeam::channel::bounded(1).1,
                id: 1,
                backend: None,
                working_dir: None,
                display_name: None,
                scroll_offset: 0,
                has_notification: false,
                fleet_instance_name: None,
                selection: None,
            }))),
            second: Box::new(PaneNode::Leaf(Box::new(Pane {
                agent_name: "b".to_string(),
                vterm: VTerm::new(10, 10),
                rx: crossbeam::channel::bounded(1).1,
                id: 2,
                backend: None,
                working_dir: None,
                display_name: None,
                scroll_offset: 0,
                has_notification: false,
                fleet_instance_name: None,
                selection: None,
            }))),
        };
        let area = (0u16, 0u16, 20u16, 10u16);
        let (first, second) = split_child_areas(area, SplitDir::Vertical, 0.5);
        let shared_col = first.0 + first.2 - 1;
        assert_eq!(shared_col, second.0);
        assert!(find_split_border(&root, area, shared_col, 5).is_some());
        // After overlap, the old border column (first.x + first.w) is now
        // inside `second`; no longer a border cell.
        assert!(find_split_border(&root, area, first.0 + first.2, 5).is_none());
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
                rx: crossbeam::channel::bounded(1).1,
                id: 1,
                backend: None,
                working_dir: None,
                display_name: None,
                scroll_offset: 0,
                has_notification: false,
                fleet_instance_name: None,
                selection: None,
            }))),
            second: Box::new(PaneNode::Leaf(Box::new(Pane {
                agent_name: "b".to_string(),
                vterm: VTerm::new(10, 10),
                rx: crossbeam::channel::bounded(1).1,
                id: 2,
                backend: None,
                working_dir: None,
                display_name: None,
                scroll_offset: 0,
                has_notification: false,
                fleet_instance_name: None,
                selection: None,
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
}
