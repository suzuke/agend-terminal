//! Layout presets — predefined pane arrangement patterns.

use super::pane::Pane;
use super::tree::{PaneNode, SplitDir, DEFAULT_RATIO};
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
pub(super) fn flatten_tree_into(node: PaneNode, acc: &mut Vec<Pane>) {
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
pub(super) fn build_tree(panes: Vec<Pane>, dir: SplitDir, alternate: bool) -> PaneNode {
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
pub(super) fn build_preset(panes: Vec<Pane>, preset: LayoutPreset) -> PaneNode {
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
