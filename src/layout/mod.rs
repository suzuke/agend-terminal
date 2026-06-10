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
use std::collections::HashMap;

/// Where a moved pane should land in its new tab.
pub enum MovePlacement {
    /// Split the destination tab's focused pane in the given direction.
    /// Used by team-update auto-grouping and the keyboard move-pane command.
    SplitFocused { to_tab: usize, dir: SplitDir },
    /// Create a brand-new tab whose sole pane is the moved pane. Used when
    /// dragging a pane onto the tab bar's empty trailing area.
    NewTab { name: String },
}

/// #1939: full placement of an agent's pane at removal time. Extends the
/// #1431 tab-name-only memory so a `SameTab` respawn (replace_instance /
/// restart_instance) restores the pane's position, not just its tab.
pub struct RemovedPanePlacement {
    pub tab_name: String,
    /// Index the tab occupied — re-inserts a recreated tab in place when the
    /// whole tab was closed (single-pane case) instead of appending.
    pub tab_idx: usize,
    /// Within-tab geometry. `None` when the pane was the tab's only pane.
    pub split: Option<RemovedSplit>,
}

/// #1939: the parent split of a removed pane.
pub struct RemovedSplit {
    pub dir: SplitDir,
    pub ratio: f32,
    /// Whether the removed pane was the `first` child of its parent split.
    pub was_first: bool,
    /// Agents of the sibling subtree at removal time (restore anchors).
    pub sibling_agents: Vec<String>,
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
    /// #1431/#1939: placement an agent's pane occupied at removal time, keyed
    /// by agent name. Recorded on pane removal and consumed by a subsequent
    /// `LayoutHint::SameTab` spawn (replace_instance / restart_instance) so
    /// the new pane returns to its original position. Bounded by distinct
    /// agent names.
    removed_pane_memory: HashMap<String, RemovedPanePlacement>,
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
            removed_pane_memory: HashMap::new(),
        }
    }

    /// #1431/#1939: record where `agent`'s pane currently sits (tab + parent
    /// split geometry) so a later `SameTab` spawn can restore it. Call BEFORE
    /// removing the pane; no-op if the agent has no pane.
    pub fn remember_removed_pane(&mut self, agent: &str) {
        if let Some((tab_idx, pane_id)) = self.find_agent_pane(agent) {
            let tab = &self.tabs[tab_idx];
            self.removed_pane_memory.insert(
                agent.to_string(),
                RemovedPanePlacement {
                    tab_name: tab.name.clone(),
                    tab_idx,
                    split: tree::parent_split_of(tab.root(), pane_id),
                },
            );
        }
    }

    /// #1431: take (and forget) the remembered placement for `agent`.
    pub fn take_removed_pane(&mut self, agent: &str) -> Option<RemovedPanePlacement> {
        self.removed_pane_memory.remove(agent)
    }

    /// #1939: place a `SameTab` respawn back at its remembered position.
    /// Fallback chain: remembered split next to the surviving siblings (exact
    /// slot) → focused split in the same tab (pre-#1939 behavior) → recreate
    /// the tab at its remembered index.
    pub fn restore_removed_pane(&mut self, placement: &RemovedPanePlacement, pane: Pane) {
        match self.tabs.iter().position(|t| t.name == placement.tab_name) {
            Some(idx) => {
                let pane = match &placement.split {
                    Some(split) => match self.tabs[idx].restore_split(split, pane) {
                        None => return,
                        Some(p) => p, // siblings gone → best-effort fallback
                    },
                    None => pane,
                };
                self.tabs[idx].split_focused(SplitDir::Horizontal, pane);
            }
            None => {
                self.insert_tab(
                    placement.tab_idx,
                    Tab::new(placement.tab_name.clone(), pane),
                );
            }
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

    /// #1939: `add_tab` at a position — insert `tab` at `idx` (clamped to the
    /// current tab count) and focus it.
    pub fn insert_tab(&mut self, idx: usize, tab: Tab) {
        let idx = idx.min(self.tabs.len());
        self.switch_active(idx);
        self.tabs.insert(idx, tab);
    }

    /// Append a tab without changing the active index. Used by the Attached
    /// app's remote-agent sync so a fleet.yaml hot-reload doesn't yank focus
    /// from whatever the user is currently working on.
    pub fn push_tab_preserve_focus(&mut self, tab: Tab) {
        self.tabs.push(tab);
    }

    /// #1591: index of the single-pane tab whose sole pane belongs to
    /// `agent_name`, if any. The Attached remote-agent sync is add-only (a gone
    /// agent's tab is RETAINED with stale output), so when a same-named agent
    /// re-appears — recovery respawn, or operator create-after-delete churn —
    /// this lets the sync REUSE the retained tab (reconnect in place) instead of
    /// appending a duplicate. Scoped to single-pane tabs: a tab the operator has
    /// split with other agents must not be clobbered, so it falls back to a
    /// fresh append in that (rare) case.
    pub fn single_pane_tab_index_for_agent(&self, agent_name: &str) -> Option<usize> {
        self.tabs.iter().position(|t| {
            t.root().pane_count() == 1 && t.root().first_pane().agent_name.as_str() == agent_name
        })
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
            agent_name: name.into(),
            instance_id: crate::types::InstanceId::default(),
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

    /// #1591: locate a retained single-pane tab by agent name; absent → None;
    /// a multi-pane (operator-split) tab is NOT matched (must not be clobbered).
    #[test]
    fn single_pane_tab_index_for_agent_1591() {
        let mut layout = Layout::new();
        layout.push_tab_preserve_focus(Tab::new("t-a".to_string(), leaf(1, "a")));
        layout.push_tab_preserve_focus(Tab::new("t-b".to_string(), leaf(2, "b")));
        assert_eq!(layout.single_pane_tab_index_for_agent("b"), Some(1));
        assert_eq!(layout.single_pane_tab_index_for_agent("a"), Some(0));
        assert_eq!(layout.single_pane_tab_index_for_agent("absent"), None);
        // A tab the operator split with a second agent is multi-pane → not a
        // reuse target (whole-tab replace would clobber the other pane).
        layout.tabs[1].split_focused(SplitDir::Vertical, leaf(3, "c"));
        assert_eq!(
            layout.single_pane_tab_index_for_agent("b"),
            None,
            "#1591: split (multi-pane) tab must not be a reuse target"
        );
    }

    /// #1591: re-appearance of a same-named agent REUSES its retained tab in
    /// place — no duplicate tab, and the operator's active tab is NOT stolen.
    #[test]
    fn reuse_retained_tab_no_duplicate_or_focus_steal_1591() {
        let mut layout = Layout::new();
        // Operator working on tab 0; a (now-stale, gone) agy-verify tab retained
        // at index 1 (add-only sync never removed it).
        layout.push_tab_preserve_focus(Tab::new("work".to_string(), leaf(1, "operator")));
        layout.push_tab_preserve_focus(Tab::new("agy-verify".to_string(), leaf(2, "agy-verify")));
        assert_eq!(layout.active, 0, "operator is on tab 0");
        let before = layout.tabs.len();

        // agy-verify is re-created → the sync's reuse path replaces the retained
        // tab in place with a fresh pane (id 99) rather than appending.
        let idx = layout
            .single_pane_tab_index_for_agent("agy-verify")
            .expect("retained agy-verify tab must be found");
        layout.tabs[idx] = Tab::new("agy-verify".to_string(), leaf(99, "agy-verify"));

        assert_eq!(
            layout.tabs.len(),
            before,
            "#1591: no duplicate tab appended"
        );
        assert_eq!(layout.active, 0, "#1591: operator focus not stolen");
        assert_eq!(
            layout.tabs[idx].root().first_pane().id,
            99,
            "#1591: retained tab now carries the fresh (reconnected) pane"
        );
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
    /// #1939: a restart of one pane in a 2-pane tab restores the exact slot —
    /// same side, same split direction, same ratio (not split_focused noise).
    #[test]
    fn restore_2pane_exact_side_dir_ratio_1939() {
        let mut layout = Layout::new();
        let root = PaneNode::Split {
            dir: SplitDir::Horizontal,
            ratio: 0.3,
            first: Box::new(PaneNode::Leaf(Box::new(leaf(1, "dev")))),
            second: Box::new(PaneNode::Leaf(Box::new(leaf(2, "dev2")))),
        };
        layout.add_tab(Tab::with_root("team".into(), root));

        layout.remember_removed_pane("dev");
        layout.tabs[0].close_pane_by_id(1);

        let placement = layout.take_removed_pane("dev").unwrap();
        let split = placement.split.as_ref().expect("parent split recorded");
        assert_eq!(split.dir, SplitDir::Horizontal);
        assert!((split.ratio - 0.3).abs() < f32::EPSILON);
        assert!(split.was_first, "dev was the first child");
        assert_eq!(split.sibling_agents, vec!["dev2".to_string()]);

        layout.restore_removed_pane(&placement, leaf(9, "dev"));
        match layout.tabs[0].root() {
            PaneNode::Split {
                dir,
                ratio,
                first,
                second,
            } => {
                assert_eq!(*dir, SplitDir::Horizontal);
                assert!((ratio - 0.3).abs() < f32::EPSILON, "ratio restored");
                assert!(
                    matches!(&**first, PaneNode::Leaf(p) if p.agent_name.as_str() == "dev" && p.id == 9),
                    "respawned dev back on the first side"
                );
                assert!(matches!(&**second, PaneNode::Leaf(p) if p.agent_name.as_str() == "dev2"));
            }
            PaneNode::Leaf(_) => panic!("expected a split root after restore"),
        }
    }

    /// #1939: when the removed pane's sibling was a SUBTREE (nested splits),
    /// the restore wraps that whole subtree — the original tree shape comes
    /// back, not a split against a single leaf inside it.
    #[test]
    fn restore_nested_sibling_subtree_shape_1939() {
        let mut layout = Layout::new();
        let sibling = PaneNode::Split {
            dir: SplitDir::Vertical,
            ratio: 0.6,
            first: Box::new(PaneNode::Leaf(Box::new(leaf(2, "dev2")))),
            second: Box::new(PaneNode::Leaf(Box::new(leaf(3, "reviewer")))),
        };
        let root = PaneNode::Split {
            dir: SplitDir::Horizontal,
            ratio: 0.5,
            first: Box::new(PaneNode::Leaf(Box::new(leaf(1, "dev")))),
            second: Box::new(sibling),
        };
        layout.add_tab(Tab::with_root("team".into(), root));

        layout.remember_removed_pane("dev");
        layout.tabs[0].close_pane_by_id(1);

        let placement = layout.take_removed_pane("dev").unwrap();
        layout.restore_removed_pane(&placement, leaf(9, "dev"));

        match layout.tabs[0].root() {
            PaneNode::Split {
                dir, first, second, ..
            } => {
                assert_eq!(*dir, SplitDir::Horizontal);
                assert!(
                    matches!(&**first, PaneNode::Leaf(p) if p.agent_name.as_str() == "dev"),
                    "dev back as first child of the root split"
                );
                match &**second {
                    PaneNode::Split {
                        dir, first, second, ..
                    } => {
                        assert_eq!(*dir, SplitDir::Vertical, "sibling subtree intact");
                        assert!(
                            matches!(&**first, PaneNode::Leaf(p) if p.agent_name.as_str() == "dev2")
                        );
                        assert!(
                            matches!(&**second, PaneNode::Leaf(p) if p.agent_name.as_str() == "reviewer")
                        );
                    }
                    PaneNode::Leaf(_) => panic!("sibling subtree must stay a split"),
                }
            }
            PaneNode::Leaf(_) => panic!("expected a split root after restore"),
        }
    }

    /// #1939: a single-pane tab (closed on delete) is recreated at its
    /// original tab index on respawn, not appended to the end.
    #[test]
    fn restore_recreates_closed_tab_at_original_index_1939() {
        let mut layout = Layout::new();
        layout.add_tab(Tab::new("t0".into(), leaf(1, "a")));
        layout.add_tab(Tab::new("mid".into(), leaf(2, "dev")));
        layout.add_tab(Tab::new("t2".into(), leaf(3, "c")));

        layout.remember_removed_pane("dev");
        layout.close_tab(1);
        assert_eq!(layout.tabs.len(), 2);

        let placement = layout.take_removed_pane("dev").unwrap();
        assert_eq!(placement.tab_idx, 1);
        assert!(placement.split.is_none(), "sole pane has no parent split");

        layout.restore_removed_pane(&placement, leaf(9, "dev"));
        assert_eq!(layout.tabs.len(), 3);
        assert_eq!(
            layout
                .tabs
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            vec!["t0", "mid", "t2"],
            "tab recreated in place, original order restored"
        );
        assert!(layout.tabs[1].root().has_agent("dev"));
        assert_eq!(layout.active, 1, "recreated tab focused (add_tab parity)");
    }

    /// #1939: all remembered sibling agents gone by respawn time → fall back
    /// to the pre-#1939 behavior (split the tab's focused pane); no panic.
    #[test]
    fn restore_falls_back_when_siblings_gone_1939() {
        let mut layout = Layout::new();
        layout.add_tab(Tab::new("team".into(), leaf(5, "newcomer")));
        let placement = RemovedPanePlacement {
            tab_name: "team".into(),
            tab_idx: 0,
            split: Some(RemovedSplit {
                dir: SplitDir::Vertical,
                ratio: 0.5,
                was_first: false,
                sibling_agents: vec!["dev2".into()],
            }),
        };
        layout.restore_removed_pane(&placement, leaf(9, "dev"));
        assert_eq!(layout.tabs.len(), 1, "no new tab");
        assert_eq!(layout.tabs[0].root().pane_count(), 2);
        assert!(layout.tabs[0].root().has_agent("dev"));
        assert!(layout.tabs[0].root().has_agent("newcomer"));
    }

    /// #1939: remembered tab gone AND its index now out of range → the
    /// recreated tab is clamped to the end; no panic.
    #[test]
    fn restore_clamps_out_of_range_tab_idx_1939() {
        let mut layout = Layout::new();
        layout.add_tab(Tab::new("t0".into(), leaf(1, "a")));
        let placement = RemovedPanePlacement {
            tab_name: "gone".into(),
            tab_idx: 7,
            split: None,
        };
        layout.restore_removed_pane(&placement, leaf(9, "dev"));
        assert_eq!(layout.tabs.len(), 2);
        assert_eq!(layout.tabs[1].name, "gone", "clamped to append");
        assert!(layout.tabs[1].root().has_agent("dev"));
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
