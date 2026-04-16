//! Tab and pane layout management — tree-based nested splits.

use crate::backend::Backend;
use crate::vterm::VTerm;
use std::path::PathBuf;

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

fn remove_from_tree(node: PaneNode, target_id: usize) -> (PaneNode, Option<Pane>) {
    match node {
        PaneNode::Leaf(p) => (PaneNode::Leaf(p), None),
        PaneNode::Split { dir, first, second } => {
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
                    first: Box::new(new_first),
                    second: Box::new(new_second),
                },
                removed,
            )
        }
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

    pub fn split_focused(&mut self, dir: SplitDir, new_pane: Pane) -> bool {
        let root = self.root.take().expect("root is always Some");
        let (new_root, remaining) = split_in_tree(root, self.focus_id, dir, new_pane);
        self.root = Some(new_root);
        remaining.is_none()
    }

    /// Close the focused pane. Returns the removed pane's agent_name.
    pub fn close_focused(&mut self) -> Option<String> {
        if self.root().pane_count() <= 1 {
            return None;
        }
        let ids = self.root().pane_ids();
        let next_id = ids
            .iter()
            .find(|&&id| id != self.focus_id)
            .copied()
            .unwrap_or(self.focus_id);

        let root = self.root.take().expect("root is always Some");
        let (new_root, removed) = remove_from_tree(root, self.focus_id);
        self.root = Some(new_root);
        self.focus_id = next_id;
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
