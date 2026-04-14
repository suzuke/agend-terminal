//! Tab and pane layout management — tree-based nested splits.

use crate::backend::Backend;
use crate::state::{AgentState, StateTracker};
use crate::vterm::VTerm;
use portable_pty::PtySize;
use std::io::Write;
use std::sync::{Arc, Mutex};

pub type PtyWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// A single pane displaying one agent's terminal output.
pub struct Pane {
    pub agent_name: String,
    pub vterm: VTerm,
    pub rx: crossbeam::channel::Receiver<Vec<u8>>,
    pub id: usize,
    pub backend: Option<Backend>,
    pub state_tracker: StateTracker,
    pub pty_writer: PtyWriter,
    #[allow(dead_code)] // used by resize()
    pub pty_master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
    pub child: Arc<Mutex<Box<dyn portable_pty::Child + Send>>>,
}

impl Pane {
    pub fn state(&self) -> AgentState {
        self.state_tracker.get_state()
    }

    pub fn drain_output(&mut self) {
        while let Ok(data) = self.rx.try_recv() {
            self.vterm.process(&data);
            if self.backend.is_some() {
                let text = String::from_utf8_lossy(&data);
                let stripped = crate::agent::strip_ansi_pub(&text);
                self.state_tracker.feed(&stripped);
            }
        }
    }

    #[allow(dead_code)]
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.vterm.resize(cols, rows);
        if let Ok(master) = self.pty_master.lock() {
            let _ = master.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
    }

    pub fn write_to_pty(&self, data: &[u8]) {
        if let Ok(mut w) = self.pty_writer.lock() {
            let _ = w.write_all(data);
            let _ = w.flush();
        }
    }

    fn kill(&self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }
    }
}

/// Split direction.
#[derive(Clone, Copy, PartialEq)]
pub enum SplitDir {
    Horizontal,
    Vertical,
}

/// Tree node: either a leaf (pane) or a split containing two children.
pub enum PaneNode {
    Leaf(Box<Pane>),
    Split {
        dir: SplitDir,
        first: Box<PaneNode>,
        second: Box<PaneNode>,
    },
}

impl PaneNode {
    /// Collect all pane IDs in tree order.
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

    /// Find a pane by ID.
    pub fn find_pane(&self, id: usize) -> Option<&Pane> {
        match self {
            PaneNode::Leaf(p) if p.id == id => Some(p),
            PaneNode::Leaf(_) => None,
            PaneNode::Split { first, second, .. } => {
                first.find_pane(id).or_else(|| second.find_pane(id))
            }
        }
    }

    /// Find a pane by ID (mutable).
    pub fn find_pane_mut(&mut self, id: usize) -> Option<&mut Pane> {
        match self {
            PaneNode::Leaf(p) if p.id == id => Some(p),
            PaneNode::Leaf(_) => None,
            PaneNode::Split { first, second, .. } => {
                first.find_pane_mut(id).or_else(|| second.find_pane_mut(id))
            }
        }
    }

    /// Get the first pane (for tab state display).
    pub fn first_pane(&self) -> &Pane {
        match self {
            PaneNode::Leaf(p) => p,
            PaneNode::Split { first, .. } => first.first_pane(),
        }
    }

    /// Count total panes.
    pub fn pane_count(&self) -> usize {
        match self {
            PaneNode::Leaf(_) => 1,
            PaneNode::Split { first, second, .. } => first.pane_count() + second.pane_count(),
        }
    }

    /// Count panes with a backend (agents, not shells).
    pub fn agent_count(&self) -> usize {
        match self {
            PaneNode::Leaf(p) => usize::from(p.backend.is_some()),
            PaneNode::Split { first, second, .. } => first.agent_count() + second.agent_count(),
        }
    }
}

// --- Ownership-based tree transforms (avoid dummy values) ---

/// Split the leaf with `target_id` into a Split node. Returns remaining pane if not found.
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
                first: Box::new(PaneNode::Leaf(p)),
                second: Box::new(PaneNode::Leaf(Box::new(new_pane))),
            },
            None,
        ),
        PaneNode::Leaf(p) => (PaneNode::Leaf(p), Some(new_pane)),
        PaneNode::Split {
            dir: d,
            first,
            second,
        } => {
            let (new_first, remaining) = split_in_tree(*first, target_id, dir, new_pane);
            if let Some(pane) = remaining {
                let (new_second, remaining) = split_in_tree(*second, target_id, dir, pane);
                (
                    PaneNode::Split {
                        dir: d,
                        first: Box::new(new_first),
                        second: Box::new(new_second),
                    },
                    remaining,
                )
            } else {
                (
                    PaneNode::Split {
                        dir: d,
                        first: Box::new(new_first),
                        second,
                    },
                    None,
                )
            }
        }
    }
}

/// Remove a leaf with `target_id`. The sibling replaces the parent Split.
fn remove_from_tree(node: PaneNode, target_id: usize) -> (PaneNode, Option<Pane>) {
    match node {
        PaneNode::Leaf(p) => (PaneNode::Leaf(p), None), // can't remove root leaf
        PaneNode::Split {
            dir,
            first,
            second,
        } => {
            // Check if first child is the target
            if let PaneNode::Leaf(ref p) = *first {
                if p.id == target_id {
                    let PaneNode::Leaf(removed) = *first else {
                        unreachable!()
                    };
                    removed.kill();
                    return (*second, Some(*removed));
                }
            }
            // Check if second child is the target
            if let PaneNode::Leaf(ref p) = *second {
                if p.id == target_id {
                    let PaneNode::Leaf(removed) = *second else {
                        unreachable!()
                    };
                    removed.kill();
                    return (*first, Some(*removed));
                }
            }
            // Recurse into first
            let (new_first, removed) = remove_from_tree(*first, target_id);
            if removed.is_some() {
                return (
                    PaneNode::Split {
                        dir,
                        first: Box::new(new_first),
                        second,
                    },
                    removed,
                );
            }
            // Recurse into second
            let (new_second, removed) = remove_from_tree(*second, target_id);
            (
                PaneNode::Split {
                    dir,
                    first: Box::new(new_first),
                    second: Box::new(new_second),
                },
                removed,
            )
        }
    }
}

/// Kill all panes in a tree.
fn kill_all(node: &PaneNode) {
    match node {
        PaneNode::Leaf(p) => p.kill(),
        PaneNode::Split { first, second, .. } => {
            kill_all(first);
            kill_all(second);
        }
    }
}

/// Center point of a rect (x, y, w, h).
fn center(rect: (u16, u16, u16, u16)) -> (i32, i32) {
    let (x, y, w, h) = rect;
    (x as i32 + w as i32 / 2, y as i32 + h as i32 / 2)
}

/// Check if two rects overlap on the Y axis (for Left/Right navigation).
fn overlaps_y(a: (u16, u16, u16, u16), b: (u16, u16, u16, u16)) -> bool {
    let a_top = a.1 as i32;
    let a_bot = a.1 as i32 + a.3 as i32;
    let b_top = b.1 as i32;
    let b_bot = b.1 as i32 + b.3 as i32;
    a_top < b_bot && b_top < a_bot
}

/// Check if two rects overlap on the X axis (for Up/Down navigation).
fn overlaps_x(a: (u16, u16, u16, u16), b: (u16, u16, u16, u16)) -> bool {
    let a_left = a.0 as i32;
    let a_right = a.0 as i32 + a.2 as i32;
    let b_left = b.0 as i32;
    let b_right = b.0 as i32 + b.2 as i32;
    a_left < b_right && b_left < a_right
}

/// Direction for spatial pane navigation.
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
    /// Always Some except during tree transforms.
    root: Option<PaneNode>,
    pub focus_id: usize,
    pub zoomed: bool,
    /// Cached pane positions from last render (pane_id → Rect).
    pub pane_rects: std::collections::HashMap<usize, (u16, u16, u16, u16)>, // x, y, w, h
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

    /// Cycle focus to the next pane.
    pub fn cycle_focus(&mut self) {
        let ids = self.root().pane_ids();
        if let Some(pos) = ids.iter().position(|&id| id == self.focus_id) {
            self.focus_id = ids[(pos + 1) % ids.len()];
        }
    }

    /// Move focus by delta (+1 forward, -1 backward) in tree order.
    #[allow(dead_code)]
    pub fn move_focus(&mut self, delta: i32) {
        let ids = self.root().pane_ids();
        if let Some(pos) = ids.iter().position(|&id| id == self.focus_id) {
            let new = (pos as i32 + delta).rem_euclid(ids.len() as i32) as usize;
            self.focus_id = ids[new];
        }
    }

    /// Move focus spatially based on pane positions from last render.
    /// Prioritizes panes that overlap on the perpendicular axis (e.g., pressing
    /// Right prefers panes at the same vertical position).
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

        // Collect candidates in the target direction
        let mut candidates: Vec<(usize, i32, bool)> = Vec::new(); // (id, distance, overlaps)

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
            // Check overlap on the perpendicular axis
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

        // Prefer overlapping candidates; among those, pick nearest
        let best = candidates
            .iter()
            .filter(|(_, _, overlaps)| *overlaps)
            .min_by_key(|(_, dist, _)| *dist)
            .or_else(|| candidates.iter().min_by_key(|(_, dist, _)| *dist));

        if let Some(&(id, _, _)) = best {
            self.focus_id = id;
        } else {
            // Wrap: find farthest pane in opposite direction with overlap
            let mut wrap_candidates: Vec<(usize, i32, bool)> = Vec::new();
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
                wrap_candidates.push((id, dist, has_overlap));
            }
            let farthest = wrap_candidates
                .iter()
                .filter(|(_, _, overlaps)| *overlaps)
                .max_by_key(|(_, dist, _)| *dist)
                .or_else(|| wrap_candidates.iter().max_by_key(|(_, dist, _)| *dist));
            if let Some(&(id, _, _)) = farthest {
                self.focus_id = id;
            }
        }
    }

    /// Split the focused pane. The new pane becomes the second child.
    pub fn split_focused(&mut self, dir: SplitDir, new_pane: Pane) -> bool {
        let root = self.root.take().expect("root is always Some");
        let (new_root, remaining) = split_in_tree(root, self.focus_id, dir, new_pane);
        self.root = Some(new_root);
        remaining.is_none()
    }

    /// Close the focused pane. Returns false if it's the only pane.
    pub fn close_focused(&mut self) -> bool {
        if self.root().pane_count() <= 1 {
            return false;
        }
        let ids = self.root().pane_ids();
        let next_id = ids
            .iter()
            .find(|&&id| id != self.focus_id)
            .copied()
            .unwrap_or(self.focus_id);

        let root = self.root.take().expect("root is always Some");
        let (new_root, _removed) = remove_from_tree(root, self.focus_id);
        self.root = Some(new_root);
        self.focus_id = next_id;
        true
    }

    /// Kill all child processes.
    pub fn kill_all(&self) {
        kill_all(self.root());
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
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
    }

    pub fn active_tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }

    pub fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active)
    }

    pub fn next_tab(&mut self) {
        if !self.tabs.is_empty() {
            self.active = (self.active + 1) % self.tabs.len();
        }
    }

    pub fn prev_tab(&mut self) {
        if !self.tabs.is_empty() {
            self.active = (self.active + self.tabs.len() - 1) % self.tabs.len();
        }
    }

    pub fn goto_tab(&mut self, idx: usize) {
        if idx < self.tabs.len() {
            self.active = idx;
        }
    }

    pub fn close_tab(&mut self, idx: usize) -> Option<Tab> {
        if idx >= self.tabs.len() {
            return None;
        }
        let tab = self.tabs.remove(idx);
        if self.active >= self.tabs.len() && !self.tabs.is_empty() {
            self.active = self.tabs.len() - 1;
        }
        Some(tab)
    }
}
