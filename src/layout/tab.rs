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

    /// Restore the durable view state captured by session persistence. Exact
    /// process identity wins; the name fallback is reserved for legacy
    /// sessions that did not persist an identity. A missing identity always
    /// falls back to the first pane instead of guessing a same-name successor.
    pub fn restore_view_state(
        &mut self,
        focus_ref: Option<crate::types::InstanceRef>,
        focus_name: Option<&str>,
        zoomed: bool,
    ) {
        let focus_id = focus_ref
            .and_then(|expected| {
                self.root().pane_ids().into_iter().find(|&id| {
                    self.root()
                        .find_pane(id)
                        .is_some_and(|pane| pane.instance_ref == Some(expected))
                })
            })
            .or_else(|| {
                if focus_ref.is_none() {
                    focus_name.and_then(|name| {
                        self.root().pane_ids().into_iter().find(|&id| {
                            self.root().find_pane(id).is_some_and(|pane| {
                                pane.agent_name.as_str() == name
                                    || pane.fleet_instance_name.as_deref() == Some(name)
                            })
                        })
                    })
                } else {
                    None
                }
            })
            .unwrap_or_else(|| self.root().first_pane().id);
        self.focus_id = focus_id;
        self.zoomed = zoomed;
    }

    pub fn root(&self) -> &PaneNode {
        self.root.as_ref().expect("root is always Some")
    }

    pub fn root_mut(&mut self) -> &mut PaneNode {
        self.root.as_mut().expect("root is always Some")
    }

    /// The tab-bar label for this tab: `" <name>[ !] "` — leading/trailing spaces
    /// plus a `" !"` unread-notification badge shown only on INACTIVE tabs. SHARED by
    /// `render_tab_bar` (the rendered span) and `tab_bar_hit_test` (its width) so the
    /// two can never drift on the label — a render-only widener the hit-test didn't
    /// count was the #777 misaligned-click bug.
    #[allow(dead_code)]
    pub fn tab_bar_label(&self, is_active: bool) -> String {
        self.tab_bar_label_with_team(is_active, None)
    }

    pub fn tab_bar_label_with_team(
        &self,
        is_active: bool,
        team_view: Option<&crate::team_view::TeamView>,
    ) -> String {
        let notif_badge = if self.root().has_notification() && !is_active {
            " !"
        } else {
            ""
        };
        let pane_ids = self.root().pane_ids();
        let members: Vec<&str> = pane_ids
            .iter()
            .filter_map(|id| {
                self.root()
                    .find_pane(*id)
                    .and_then(|pane| pane.fleet_instance_name.as_deref())
            })
            .collect();
        let all_have_identity = members.len() == pane_ids.len();
        let name = team_view
            .filter(|_| all_have_identity)
            .and_then(|view| view.authoritative_tab_name(members.iter().copied()))
            .unwrap_or(self.name.as_str());
        let lead_badge = team_view
            .filter(|_| all_have_identity)
            .and_then(|view| {
                let lead = view.authoritative_lead_for_members(members.iter().copied())?;
                // #3672: lead-absent (no pane in this tab belongs to the
                // lead) draws nothing. The old fallthrough called
                // `view.badge(lead, None)`, which by definition returns
                // `Uncertain` and drew ` [LEAD?]` — but a tab that simply
                // isn't the lead's tab carries no uncertainty about the
                // lead's identity. `badge()` itself (including its
                // anti-spoofing `None` arm) is untouched.
                let pane = pane_ids.iter().find_map(|id| {
                    self.root()
                        .find_pane(*id)
                        .filter(|pane| pane.fleet_instance_name.as_deref() == Some(lead))
                })?;
                let badge = view.badge(lead, pane.instance_ref.as_ref());
                match badge {
                    crate::team_view::LeadBadge::Lead => Some(" [LEAD]"),
                    crate::team_view::LeadBadge::Uncertain => Some(" [LEAD?]"),
                    crate::team_view::LeadBadge::None => None,
                }
            })
            .unwrap_or("");
        format!(" {name}{lead_badge}{notif_badge} ")
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

    /// #917: Flip the split direction of the parent split containing the focused pane.
    pub fn flip_focused_split(&mut self) -> bool {
        if let Some(root) = self.root.as_mut() {
            super::tree::flip_split_containing(root, self.focus_id)
        } else {
            false
        }
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

    /// #1939: restore `pane` to its remembered split position — wrap the
    /// minimal subtree containing the still-present sibling agents in a split
    /// of the remembered direction/ratio, with the pane on its remembered
    /// side. Returns the pane back when none of the sibling agents are
    /// displayed in this tab (caller picks a fallback placement).
    #[allow(dead_code)]
    pub fn restore_split(&mut self, split: &super::RemovedSplit, pane: Pane) -> Option<Pane> {
        let anchors: std::collections::HashSet<usize> = split
            .sibling_agents
            .iter()
            .filter_map(|a| self.root().find_pane_id_by_agent(a))
            .collect();
        if anchors.is_empty() {
            return Some(pane);
        }
        let root = self.root.take().expect("root is always Some");
        let (new_root, leftover) =
            super::tree::wrap_subtree_with_split(root, &anchors, split, pane);
        self.root = Some(new_root);
        leftover
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
    #[allow(dead_code)]
    pub fn title_bar_at(&self, col: u16, row: u16) -> Option<usize> {
        self.title_bar_at_with_team(col, row, None)
    }

    pub fn title_bar_at_with_team(
        &self,
        col: u16,
        row: u16,
        team_view: Option<&crate::team_view::TeamView>,
    ) -> Option<usize> {
        use unicode_width::UnicodeWidthStr;
        for (&id, &(px, py, pw, _ph)) in &self.pane_rects {
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
            let available = pw.saturating_sub(2);
            let base_w = UnicodeWidthStr::width(pane.label()) as u16 + 1;
            let badge_w = pane
                .fleet_instance_name
                .as_deref()
                .zip(team_view)
                .and_then(
                    |(name, view)| match view.badge(name, pane.instance_ref.as_ref()) {
                        crate::team_view::LeadBadge::Lead => {
                            Some(UnicodeWidthStr::width(" [LEAD]"))
                        }
                        crate::team_view::LeadBadge::Uncertain => {
                            Some(UnicodeWidthStr::width(" [LEAD?]"))
                        }
                        crate::team_view::LeadBadge::None => None,
                    },
                )
                .map(|width| width as u16);
            let hit_width = match badge_w {
                Some(badge_w) if available <= badge_w => available,
                Some(badge_w) => base_w.min(available - badge_w) + badge_w,
                None => base_w + 1,
            };
            let hit_start = px + 1;
            let hit_end = hit_start + hit_width;
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
        removed.map(|p| p.agent_name.to_string())
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::layout::pane::PaneSource;
    use crate::layout::tree::SplitDir;
    use crate::vterm::VTerm;
    use unicode_width::UnicodeWidthStr;

    fn leaf(id: usize, name: &str) -> Pane {
        Pane {
            agent_name: name.into(),
            instance_id: crate::types::InstanceId::default(),
            instance_ref: None,
            vterm: VTerm::new(10, 10),
            rx: crossbeam_channel::bounded(1).1,
            id,
            backend: None,
            working_dir: None,
            display_name: None,
            restart_error: None,
            scroll_offset: 0,
            has_notification: false,
            fleet_instance_name: None,
            last_input_at: None,
            pending_notification_count: 0,
            pending_decision_count: 0,
            selection: None,
            source: PaneSource::Local,
            offthread: None,
            _fwd_cancel: None,
        }
    }
    fn tab_with_pane(name: &str, id: usize, rect: (u16, u16, u16, u16)) -> Tab {
        let mut tab = Tab::new("test".into(), leaf(id, name));
        tab.pane_rects.insert(id, rect);
        tab
    }

    #[test]
    fn unicode_width_for_title_matches_terminal_cells() {
        assert_eq!(UnicodeWidthStr::width("alice") as u16, 5);
        assert_eq!(UnicodeWidthStr::width("代理") as u16, 4);
        assert_eq!(UnicodeWidthStr::width("a代") as u16, 3);
    }
    #[test]
    fn title_bar_at_hits_within_name_text() {
        let tab = tab_with_pane("alice", 1, (0, 0, 40, 10));
        assert_eq!(tab.title_bar_at(1, 0), Some(1));
        assert_eq!(tab.title_bar_at(6, 0), Some(1));
        assert_eq!(tab.title_bar_at(7, 0), Some(1));
    }
    #[test]
    fn title_bar_at_misses_outside_name_text() {
        let tab = tab_with_pane("alice", 1, (0, 0, 40, 10));
        assert_eq!(tab.title_bar_at(0, 0), None);
        assert_eq!(tab.title_bar_at(8, 0), None);
        assert_eq!(tab.title_bar_at(30, 0), None);
    }
    #[test]
    fn title_bar_at_name_fills_pane_width() {
        let tab = tab_with_pane("longname", 1, (0, 0, 11, 10));
        for col in 1..11 {
            assert_eq!(tab.title_bar_at(col, 0), Some(1), "col {col}");
        }
    }

    #[test]
    fn title_bar_hit_test_includes_authoritative_badge() {
        let lead_ref = crate::types::InstanceRef::new(crate::types::InstanceId::new(), 1);
        let mut pane = leaf(1, "lead");
        pane.fleet_instance_name = Some("lead".into());
        pane.instance_ref = Some(lead_ref);
        let mut tab = tab_with_pane("lead", 1, (0, 0, 20, 10));
        tab.root = Some(PaneNode::Leaf(Box::new(pane)));
        let config: crate::fleet::FleetConfig = serde_yaml_ng::from_str(
            "teams:\n  ops:\n    members: [lead]\n    orchestrator: lead\n",
        )
        .unwrap();
        let mut roster = std::collections::HashMap::new();
        roster.insert("lead".to_string(), lead_ref);
        let view = crate::team_view::TeamView::from_fleet(config, Some(roster));

        assert_eq!(
            tab.title_bar_at_with_team(11, 0, Some(&view)),
            Some(1),
            "clicking the rendered [LEAD] suffix must select the pane title"
        );
    }

    /// #3629 (superseded by #3672): a team tab without its lead used to draw
    /// ` [LEAD?]` to expose uncertainty. #3672 reclassifies lead-absent as
    /// do-not-draw — a tab that simply isn't the lead's tab (e.g. a moved-out
    /// member tab) shows the team name with no lead badge.
    #[test]
    fn member_only_team_tab_shows_uncertain_badge_when_lead_is_offline_3629() {
        let member_ref = crate::types::InstanceRef::new(crate::types::InstanceId::new(), 2);
        let mut pane = leaf(1, "member");
        pane.fleet_instance_name = Some("member".into());
        pane.instance_ref = Some(member_ref);
        let mut tab = tab_with_pane("member", 1, (0, 0, 20, 10));
        tab.root = Some(PaneNode::Leaf(Box::new(pane)));
        let config: crate::fleet::FleetConfig = serde_yaml_ng::from_str(
            "teams:\n  ops:\n    members: [lead, member]\n    orchestrator: lead\n",
        )
        .unwrap();
        let mut roster = std::collections::HashMap::new();
        roster.insert("member".to_string(), member_ref);
        let view = crate::team_view::TeamView::from_fleet(config, Some(roster));

        assert_eq!(
            tab.tab_bar_label_with_team(true, Some(&view)),
            " ops ",
            "#3672: a team tab without its lead draws no lead badge"
        );
    }
    /// #3672 RED: a tab with no pane belonging to the lead must draw NO
    /// lead badge — the lead-absent case (e.g. a moved-out member tab) is
    /// not uncertainty about the lead's identity, it is simply not the
    /// lead's tab. The old call-site `None => view.badge(lead, None)` arm
    /// returned `Uncertain` by definition and drew ` [LEAD?]`.
    #[test]
    fn lead_absent_tab_draws_no_lead_badge_3672() {
        let member_ref = crate::types::InstanceRef::new(crate::types::InstanceId::new(), 2);
        let mut pane = leaf(1, "member");
        pane.fleet_instance_name = Some("member".into());
        pane.instance_ref = Some(member_ref);
        let mut tab = tab_with_pane("member", 1, (0, 0, 20, 10));
        tab.root = Some(PaneNode::Leaf(Box::new(pane)));
        let config: crate::fleet::FleetConfig = serde_yaml_ng::from_str(
            "teams:\n  ops:\n    members: [lead, member]\n    orchestrator: lead\n",
        )
        .unwrap();
        let mut roster = std::collections::HashMap::new();
        roster.insert("member".to_string(), member_ref);
        // Roster ALSO knows the lead (fresh identity) — the tab still has
        // no pane belonging to it, so no badge may be drawn.
        let lead_ref = crate::types::InstanceRef::new(crate::types::InstanceId::new(), 1);
        roster.insert("lead".to_string(), lead_ref);
        let view = crate::team_view::TeamView::from_fleet(config, Some(roster));

        assert_eq!(
            tab.tab_bar_label_with_team(true, Some(&view)),
            " ops ",
            "#3672: a tab without the lead draws no lead badge"
        );
    }
    #[test]
    fn split_at_pane_targets_non_focused_pane() {
        let mut tab = Tab::new("t".to_string(), leaf(1, "a"));
        assert!(tab.split_focused(SplitDir::Vertical, leaf(2, "b")));
        assert_eq!(tab.focus_id, 1);
        assert!(tab.split_at_pane(2, SplitDir::Horizontal, leaf(3, "c")));
        assert_eq!(tab.root().pane_count(), 3);
        assert!(tab.root().has_agent("c"));
    }
    #[test]
    fn split_at_pane_returns_false_when_target_missing() {
        let mut tab = Tab::new("t".to_string(), leaf(1, "a"));
        assert!(!tab.split_at_pane(999, SplitDir::Vertical, leaf(2, "b")));
        assert_eq!(tab.root().pane_count(), 1);
    }
    #[test]
    fn close_focused_updates_focus_to_sibling() {
        let mut tab = Tab::new("t".to_string(), leaf(1, "a"));
        assert!(tab.split_focused(SplitDir::Vertical, leaf(2, "b")));
        assert_eq!(tab.focus_id, 1);
        let removed = tab.close_focused();
        assert_eq!(removed.as_deref(), Some("a"));
        assert_eq!(tab.focus_id, 2);
    }
    #[test]
    fn close_pane_by_id_returns_none_when_last() {
        let mut tab = Tab::new("t".to_string(), leaf(1, "only"));
        assert!(tab.close_pane_by_id(1).is_none());
        assert_eq!(tab.root().pane_count(), 1);
    }
    #[test]
    fn cycle_focus_wraps_around_three_panes() {
        let mut tab = Tab::new("t".to_string(), leaf(1, "a"));
        assert!(tab.split_focused(SplitDir::Vertical, leaf(2, "b")));
        assert!(tab.split_focused(SplitDir::Vertical, leaf(3, "c")));
        let start = tab.focus_id;
        tab.cycle_focus();
        tab.cycle_focus();
        tab.cycle_focus();
        assert_eq!(tab.focus_id, start);
    }
    #[test]
    fn apply_layout_even_horizontal_preserves_pane_count() {
        let mut tab = Tab::new("t".to_string(), leaf(1, "a"));
        assert!(tab.split_focused(SplitDir::Vertical, leaf(2, "b")));
        assert!(tab.split_focused(SplitDir::Horizontal, leaf(3, "c")));
        tab.pane_rects.insert(1, (0, 0, 10, 10));
        tab.apply_layout(LayoutPreset::EvenHorizontal);
        assert_eq!(tab.root().pane_count(), 3);
        assert_eq!(tab.last_layout, Some(LayoutPreset::EvenHorizontal));
        assert!(tab.pane_rects.is_empty());
    }
    #[test]
    fn next_layout_cycles_from_none_to_even_horizontal() {
        let mut tab = Tab::new("t".to_string(), leaf(1, "a"));
        assert!(tab.split_focused(SplitDir::Vertical, leaf(2, "b")));
        assert!(tab.last_layout.is_none());
        tab.next_layout();
        assert_eq!(tab.last_layout, Some(LayoutPreset::EvenHorizontal));
    }
    #[test]
    fn detach_pane_refuses_sole_pane() {
        let mut tab = Tab::new("t".to_string(), leaf(1, "a"));
        assert!(tab.detach_pane(1).is_none());
        assert_eq!(tab.root().pane_count(), 1);
    }
    #[test]
    fn detach_pane_missing_id_returns_none() {
        let mut tab = Tab::new("t".to_string(), leaf(1, "a"));
        assert!(tab.split_focused(SplitDir::Vertical, leaf(2, "b")));
        assert!(tab.detach_pane(999).is_none());
        assert_eq!(tab.root().pane_count(), 2);
    }
    #[test]
    fn detach_pane_returns_pane_and_moves_focus() {
        let mut tab = Tab::new("t".to_string(), leaf(1, "a"));
        assert!(tab.split_focused(SplitDir::Vertical, leaf(2, "b")));
        tab.focus_id = 1;
        let d = tab.detach_pane(1).unwrap();
        assert_eq!(d.agent_name.as_str(), "a");
        assert_eq!(tab.focus_id, 2);
    }
    #[test]
    fn detach_pane_clears_transient_state() {
        let mut tab = Tab::new("t".to_string(), leaf(1, "a"));
        assert!(tab.split_focused(SplitDir::Vertical, leaf(2, "b")));
        tab.dragging_pane = Some(1);
        tab.drag_target = Some(1);
        tab.selecting_pane = Some(1);
        let _ = tab.detach_pane(1).unwrap();
        assert!(tab.dragging_pane.is_none());
        assert!(tab.drag_target.is_none());
        assert!(tab.selecting_pane.is_none());
    }
}
