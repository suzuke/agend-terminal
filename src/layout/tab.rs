//! Tab — container for a pane tree with focus tracking.

use super::pane::Pane;
use super::preset::{build_preset, flatten_tree_into, LayoutPreset};
use super::split::{center, overlaps_x, overlaps_y, Direction};
use super::tree::{remove_from_tree, split_in_tree, PaneNode, SplitDir};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DragTabTarget {
    /// Pointer is over the tab at this index. On drop, move the pane into that tab.
    ExistingTab(usize),
    /// Pointer is past the last tab / over the `[+]` button area. On drop,
    /// create a new tab named after the pane's agent.
    NewTab,
}

/// A tab containing a tree of panes.
pub struct Tab {
    pub name: String,
    pub(super) root: Option<PaneNode>,
    pub focus_id: usize,
    pub zoomed: bool,
    pub pane_rects: std::collections::HashMap<usize, (u16, u16, u16, u16)>,
    /// Pane currently being selected with mouse (cached to avoid lookup on drag).
    pub selecting_pane: Option<usize>,
    /// Last applied layout preset (for cycling with next_layout).
    pub last_layout: Option<LayoutPreset>,
    /// Pane currently being dragged by title bar (drag-to-swap).
    pub dragging_pane: Option<usize>,
    /// Drop target pane during title bar drag (intra-tab swap).
    pub drag_target: Option<usize>,
    /// Cross-tab drop target during title bar drag. Set when the pointer is
    /// over the tab bar while a pane is being dragged; mutually exclusive with
    /// `drag_target` (each mouse move picks one based on pointer position).
    pub drag_target_tab: Option<DragTabTarget>,
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
            drag_target_tab: None,
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
            drag_target_tab: None,
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

    pub fn focused_pane_mut(&mut self) -> Option<&mut Pane> {
        let focus_id = self.focus_id;
        self.root_mut().find_pane_mut(focus_id)
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
        self.split_at_pane(self.focus_id, dir, new_pane)
    }

    /// Split the pane with `target_id` in `dir`, attaching `new_pane` as the
    /// second child. Returns `true` if the target was found and split; `false`
    /// if the target was absent (the tree is left unchanged and `new_pane` is
    /// dropped — callers who need recovery should check `has_agent` first).
    pub fn split_at_pane(&mut self, target_id: usize, dir: SplitDir, new_pane: Pane) -> bool {
        let root = self.root.take().expect("root is always Some");
        let (new_root, remaining) = split_in_tree(root, target_id, dir, new_pane);
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
        use unicode_width::UnicodeWidthStr;
        for (&id, &(px, py, _pw, _ph)) in &self.pane_rects {
            if row != py {
                continue;
            }
            let pane = match self.root().find_pane(id) {
                Some(p) => p,
                None => continue,
            };
            // Hit area covers only the rendered ` {label} ` region starting
            // at px+1 (first col is the border glyph). Clicks outside the
            // label text fall through to border resize handling.
            let label_w = UnicodeWidthStr::width(pane.label()) as u16;
            let hit_start = px + 1;
            let hit_end = hit_start + label_w + 2; // leading space + label + trailing space
            if col >= hit_start && col < hit_end {
                return Some(id);
            }
        }
        None
    }

    /// Reset all drag fields after a title-bar drag completes or aborts.
    pub fn clear_drag(&mut self) {
        self.dragging_pane = None;
        self.drag_target = None;
        self.drag_target_tab = None;
    }

    /// Clear all in-progress UI state (selection tracking + drag tracking).
    /// Called when the user leaves this tab so a half-finished mouse
    /// interaction doesn't resume if they return to the tab later.
    pub fn clear_transient_input(&mut self) {
        self.selecting_pane = None;
        self.dragging_pane = None;
        self.drag_target = None;
        self.drag_target_tab = None;
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

    /// Detach a pane from this tab's tree without destroying its VTerm or PTY
    /// subscription, returning the full `Pane` so the caller can reinsert it
    /// into another tab. Returns `None` when `pane_id` is not in this tab, or
    /// when it is the sole pane (the tab would be left empty — callers moving
    /// the last pane must consume the whole tab via `Layout::move_pane_across_tabs`
    /// which handles source-tab removal).
    pub fn detach_pane(&mut self, pane_id: usize) -> Option<Pane> {
        if self.root().pane_count() <= 1 {
            return None;
        }
        self.root().find_pane(pane_id)?;
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
        // Clear transient UI state referencing the departing pane so a
        // half-finished drag/select doesn't resume against a pane that
        // no longer lives here. `drag_target_tab` is cleared alongside
        // `dragging_pane` because a cross-tab drop intent without a source
        // pane is meaningless.
        if self.dragging_pane == Some(pane_id) {
            self.dragging_pane = None;
            self.drag_target_tab = None;
        }
        if self.drag_target == Some(pane_id) {
            self.drag_target = None;
        }
        if self.selecting_pane == Some(pane_id) {
            self.selecting_pane = None;
        }
        removed
    }
}
