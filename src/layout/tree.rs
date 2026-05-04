//! Pane tree — PaneNode, SplitDir, tree transforms, swap.

use super::pane::Pane;
/// Split direction.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
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

pub(crate) const DEFAULT_RATIO: f32 = 0.5;

/// Minimum cells (columns or rows) required per pane child to remain usable.
/// All ratio clamps derive from this so the bounds scale with terminal size —
/// a 400-cell area allows near-full drag range, while a 10-cell area still
/// guarantees both sides are visible.
pub(crate) const MIN_PANE_CELLS: u16 = 3;

/// Valid ratio bounds for a split of `total` cells. Returns `(min, max)` such
/// that both children end up with ≥ MIN_PANE_CELLS. When `total` is too small
/// to honor both minimums, returns `(0.5, 0.5)` — callers should avoid
/// splitting such tiny areas in the first place.
pub(crate) fn ratio_bounds(total: u16) -> (f32, f32) {
    if total < 2 * MIN_PANE_CELLS {
        return (0.5, 0.5);
    }
    let min = MIN_PANE_CELLS as f32 / total as f32;
    (min, 1.0 - min)
}

impl PaneNode {
    /// Collect pane IDs into an existing buffer, avoiding intermediate allocations during recursion.
    pub fn collect_pane_ids(&self, buf: &mut Vec<usize>) {
        match self {
            PaneNode::Leaf(p) => buf.push(p.id),
            PaneNode::Split { first, second, .. } => {
                first.collect_pane_ids(buf);
                second.collect_pane_ids(buf);
            }
        }
    }

    pub fn pane_ids(&self) -> Vec<usize> {
        let mut ids = Vec::new();
        self.collect_pane_ids(&mut ids);
        ids
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

pub(super) fn split_in_tree(
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

pub(super) fn remove_from_tree(node: PaneNode, target_id: usize) -> (PaneNode, Option<Pane>) {
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::backend::Backend;
    use crate::layout::pane::PaneSource;
    use crate::vterm::VTerm;

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
    fn ratio_bounds_symmetric_when_room() {
        let (lo, hi) = ratio_bounds(100);
        assert!((lo + hi - 1.0).abs() < f32::EPSILON);
    }
    #[test]
    fn ratio_bounds_degenerate_when_tiny() {
        assert_eq!(ratio_bounds(5), (0.5, 0.5));
        assert_eq!(ratio_bounds(0), (0.5, 0.5));
    }
    #[test]
    fn ratio_bounds_min_cells_enforced() {
        let (lo, _) = ratio_bounds(30);
        let first = (lo * 30.0).round() as u16;
        assert_eq!(first, MIN_PANE_CELLS);
    }
    #[test]
    fn pane_count_and_agent_count_across_split() {
        use crate::layout::tab::Tab;
        let mut tab = Tab::new("mixed".to_string(), leaf_agent(1, "alice"));
        assert!(tab.split_focused(SplitDir::Vertical, leaf(2, "shell")));
        assert_eq!(tab.root().pane_count(), 2);
        assert_eq!(tab.root().agent_count(), 1);
    }
    #[test]
    fn swap_panes_across_nested_split() {
        use crate::layout::tab::Tab;
        let mut tab = Tab::new("t".to_string(), leaf(1, "a"));
        assert!(tab.split_focused(SplitDir::Vertical, leaf(2, "b")));
        tab.focus_id = 1;
        assert!(tab.split_focused(SplitDir::Horizontal, leaf(3, "c")));
        tab.focus_id = 2;
        assert!(tab.split_focused(SplitDir::Horizontal, leaf(4, "d")));
        let pre = tab.root().pane_ids();
        let first_id = pre[0];
        let last_id = *pre.last().unwrap();
        assert!(swap_panes(tab.root_mut(), first_id, last_id));
        let post = tab.root().pane_ids();
        assert_eq!(post.len(), pre.len());
        assert_eq!(post[0], last_id);
        assert_eq!(*post.last().unwrap(), first_id);
    }
}
