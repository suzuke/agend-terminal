//! Tab and pane layout management — tree-based nested splits.

pub mod pane;
pub mod preset;
pub mod split;
pub mod tab;
pub mod tree;

// Re-export all public types so callers keep using `crate::layout::X`.
pub use pane::{Pane, PaneSource, Selection};
pub use preset::LayoutPreset;
pub(crate) use split::split_chunks;
pub use split::{adjust_split_ratio, find_split_border, resize_focused, Direction, SplitBorderHit};
pub use tab::{DragTabTarget, Tab};
pub use tree::{swap_panes, PaneNode, SplitDir};

use ratatui::layout::Rect;

/// Where a moved pane should land in its new tab.
pub enum MovePlacement {
    /// Split the destination tab's focused pane in the given direction.
    /// Used by team-update auto-grouping and the keyboard move-pane command.
    SplitFocused { to_tab: usize, dir: SplitDir },
    /// Create a brand-new tab whose sole pane is the moved pane. Used when
    /// dragging a pane onto the tab bar's empty trailing area.
    NewTab { name: String },
}

/// Top-level layout.
pub struct Layout {
    pub tabs: Vec<Tab>,
    pub active: usize,
    next_pane_id: usize,
    /// Tab being dragged for reorder (index). Set by mouse handler.
    pub tab_reorder_source: Option<usize>,
    /// Drop target tab index during tab reorder drag.
    pub tab_reorder_target: Option<usize>,
}

pub const TAB_BAR_HEIGHT: u16 = 1;

/// True if the given screen row is within the tab bar area.
pub fn is_tab_bar_row(row: u16) -> bool {
    row < TAB_BAR_HEIGHT
}

impl Layout {
    pub fn new() -> Self {
        Self {
            tabs: Vec::new(),
            active: 0,
            next_pane_id: 0,
            tab_reorder_source: None,
            tab_reorder_target: None,
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

    /// Append a tab without changing the active index. Used by the Attached
    /// app's remote-agent sync so a fleet.yaml hot-reload doesn't yank focus
    /// from whatever the user is currently working on.
    pub fn push_tab_preserve_focus(&mut self, tab: Tab) {
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

    /// Move a pane from one tab to another, preserving its VTerm, scrollback,
    /// and PTY subscription (unlike close + attach, which rebuilds state).
    pub fn move_pane_across_tabs(
        &mut self,
        from_tab: usize,
        pane_id: usize,
        placement: MovePlacement,
    ) -> Option<usize> {
        if from_tab >= self.tabs.len() {
            return None;
        }
        let split_target = match &placement {
            MovePlacement::SplitFocused { to_tab, .. } => {
                if *to_tab >= self.tabs.len() || *to_tab == from_tab {
                    return None;
                }
                Some(*to_tab)
            }
            MovePlacement::NewTab { .. } => None,
        };
        self.tabs[from_tab].root().find_pane(pane_id)?;

        let source_count = self.tabs[from_tab].root().pane_count();
        let (pane, adjusted_to_tab) = if source_count == 1 {
            let mut src = self.tabs.remove(from_tab);
            let root = src.root.take().expect("root is always Some");
            let pane = match root {
                PaneNode::Leaf(boxed) => *boxed,
                PaneNode::Split { .. } => {
                    unreachable!("pane_count == 1 must be a Leaf root")
                }
            };
            let adjusted_to = split_target.map(|t| if t > from_tab { t - 1 } else { t });
            if self.active == from_tab {
                self.active = adjusted_to
                    .unwrap_or(self.tabs.len())
                    .min(self.tabs.len().saturating_sub(1));
            } else if self.active > from_tab {
                self.active -= 1;
            }
            (pane, adjusted_to)
        } else {
            let pane = self.tabs[from_tab]
                .detach_pane(pane_id)
                .expect("pre-checked find_pane + pane_count > 1");
            (pane, split_target)
        };

        let moved_id = pane.id;
        match placement {
            MovePlacement::SplitFocused { dir, .. } => {
                let dest = adjusted_to_tab.expect("SplitFocused always yields a dest index");
                self.tabs[dest].split_focused(dir, pane);
                self.tabs[dest].focus_id = moved_id;
                Some(dest)
            }
            MovePlacement::NewTab { name } => {
                self.add_tab(Tab::new(name, pane));
                Some(self.tabs.len() - 1)
            }
        }
    }

    /// Find the `(tab_idx, pane_id)` hosting `agent`, if any.
    pub fn find_agent_pane(&self, agent: &str) -> Option<(usize, usize)> {
        self.tabs
            .iter()
            .enumerate()
            .find_map(|(i, t)| t.root().find_pane_id_by_agent(agent).map(|p| (i, p)))
    }
}

/// Resize all panes in the active tab to fit the given area.
pub fn resize_panes(pane_area: Rect, layout: &mut Layout, registry: &crate::agent::AgentRegistry) {
    let tab = match layout.tabs.get_mut(layout.active) {
        Some(t) => t,
        None => return,
    };
    let mut resizes: Vec<(usize, u16, u16)> = Vec::new();
    if tab.zoomed {
        let focus_id = tab.focus_id;
        if let Some(pane) = tab.root_mut().find_pane_mut(focus_id) {
            let w = pane_area.width.saturating_sub(2);
            let h = pane_area.height.saturating_sub(2);
            if w > 0 && h > 0 && (w != pane.vterm.cols() || h != pane.vterm.rows()) {
                pane.vterm.resize(w, h);
                resizes.push((pane.id, w, h));
            }
        }
    } else {
        let mut rects = std::mem::take(&mut tab.pane_rects);
        rects.clear();
        collect_resize_needs(pane_area, tab.root_mut(), &mut rects, &mut resizes);
        tab.pane_rects = rects;
    }
    for (id, cols, rows) in &resizes {
        if let Some(pane) = tab.root().find_pane(*id) {
            pane.resize_pty(registry, *cols, *rows);
        }
    }
}

fn collect_resize_needs(
    area: Rect,
    node: &mut PaneNode,
    rects: &mut std::collections::HashMap<usize, (u16, u16, u16, u16)>,
    resizes: &mut Vec<(usize, u16, u16)>,
) {
    match node {
        PaneNode::Leaf(pane) => {
            rects.insert(pane.id, (area.x, area.y, area.width, area.height));
            let w = area.width.saturating_sub(2);
            let h = area.height.saturating_sub(2);
            if w > 0 && h > 0 && (w != pane.vterm.cols() || h != pane.vterm.rows()) {
                pane.vterm.resize(w, h);
                resizes.push((pane.id, w, h));
            }
        }
        PaneNode::Split {
            dir,
            ratio,
            first,
            second,
        } => {
            let [c0, c1] = split::split_chunks(area, dir, *ratio);
            collect_resize_needs(c0, first, rects, resizes);
            collect_resize_needs(c1, second, rects, resizes);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
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

    #[test]
    fn layout_next_tab_wraps_at_boundary() {
        let mut layout = Layout::new();
        layout.add_tab(Tab::new("t1".to_string(), leaf(1, "a")));
        layout.add_tab(Tab::new("t2".to_string(), leaf(2, "b")));
        layout.add_tab(Tab::new("t3".to_string(), leaf(3, "c")));
        assert_eq!(layout.active, 2);
        layout.next_tab();
        assert_eq!(layout.active, 0);
    }
    #[test]
    fn move_pane_across_tabs_same_tab_rejected() {
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
            .unwrap();
        assert_eq!(dest, 1);
        assert_eq!(layout.tabs.len(), 2);
        assert_eq!(layout.tabs[0].root().pane_count(), 1);
        assert_eq!(layout.tabs[1].root().pane_count(), 2);
        assert!(layout.tabs[1].root().has_agent("b"));
        assert_eq!(layout.tabs[1].focus_id, 2);
    }
    #[test]
    fn move_pane_across_tabs_single_pane_source_removes_tab() {
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
            .unwrap();
        assert_eq!(dest, 0);
        assert_eq!(layout.tabs.len(), 1);
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
            .unwrap();
        assert_eq!(dest, 1);
        assert_eq!(layout.tabs.len(), 2);
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
}
