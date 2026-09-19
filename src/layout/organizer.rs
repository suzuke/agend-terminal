//! #3631: transactional Team Organizer.
//!
//! Plans how managed (fleet-owned) panes scattered across tabs should be
//! regrouped into one tab per team (lead first) and offers a preview / apply /
//! undo cycle. Three invariants drive the design:
//!
//! 1. **Identity, never index.** Pane ids and [`Tab::id`] are the only keys; a
//!    plan is computed once and panes are located by id at apply time, so a tab
//!    index shifting mid-operation cannot retarget a move.
//! 2. **Local panes are inert.** A pane without a `fleet_instance_name` (local
//!    shell / scratch) is never part of a plan, so it is never moved and a mixed
//!    tab keeps it in place.
//! 3. **Single transaction.** [`apply`] captures a structural snapshot first and
//!    restores it if anything fails; nothing is half-applied. [`undo`] replays
//!    that pane-id snapshot — it is deliberately NOT a name-keyed memory.
//!
//! Ordering is delegated to [`crate::team_order::plan_team_order`] (#3630), so
//! teams and members land in the canonical order everywhere.

use super::pane::Pane;
use super::preset::{build_preset, LayoutPreset};
use super::tab::{DragTabTarget, Tab};
use super::tree::{PaneNode, SplitDir};
use super::Layout;
use crate::fleet::FleetConfig;
use std::collections::{BTreeMap, HashMap, HashSet};

/// Which teams the Organizer should arrange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrganizerScope {
    /// The team owning the active tab (or focused pane).
    CurrentTeam,
    /// A single named team.
    Team(String),
    /// Every non-corrupt team, plus the ungrouped area.
    AllTeams,
}

impl OrganizerScope {
    pub fn label(&self) -> String {
        match self {
            Self::CurrentTeam => "current team".to_string(),
            Self::Team(name) => format!("team {name}"),
            Self::AllTeams => "all teams".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrganizerError {
    /// No team in scope (e.g. active tab is local-only).
    EmptyScope,
    /// A planned pane id no longer exists — the plan is stale.
    StalePlan,
    /// Undo refused: the layout's pane set changed since the snapshot.
    PaneSetChanged,
    /// Undo requested with no transaction snapshot recorded.
    NoSnapshot,
}

impl OrganizerError {
    pub fn message(&self) -> String {
        match self {
            Self::EmptyScope => "no team in scope to arrange".to_string(),
            Self::StalePlan => "layout changed since preview — reopen :arrange".to_string(),
            Self::PaneSetChanged => "cannot undo: panes changed since arrange".to_string(),
            Self::NoSnapshot => "nothing to undo".to_string(),
        }
    }
}

/// A destination tab in a plan.
#[derive(Debug, Clone)]
pub struct OrganizerGroup {
    /// Tab label.
    pub name: String,
    /// Pane ids in final order (lead first for a team).
    pub pane_ids: Vec<usize>,
    /// Display labels, parallel to `pane_ids`, for the preview.
    pub members: Vec<String>,
    /// Existing tab identity to reuse, if this group already owns a tab.
    pub reuse_tab_id: Option<u64>,
    /// Preset used only when the group's tab must be (re)built.
    pub preset: LayoutPreset,
}

/// A pure preview. Building one never mutates the layout.
#[derive(Debug, Clone)]
pub struct OrganizerPlan {
    pub groups: Vec<OrganizerGroup>,
    /// Number of panes that would change tab.
    pub moves: usize,
}

/// The scopes offered by the interactive Organizer, in cycle order: the current
/// team first, then each non-empty team in canonical order, then all teams.
pub fn available_scopes(layout: &Layout, fleet: &FleetConfig) -> Vec<OrganizerScope> {
    let managed = managed_panes_by_name(layout);
    let live: Vec<&str> = managed.keys().map(String::as_str).collect();
    let order = crate::team_order::plan_team_order(fleet, live);
    let mut scopes = vec![OrganizerScope::CurrentTeam];
    for group in &order.groups {
        if !group.corrupt && !group.members.is_empty() {
            scopes.push(OrganizerScope::Team(group.name.clone()));
        }
    }
    scopes.push(OrganizerScope::AllTeams);
    scopes
}

/// Build a plan without touching the layout.
pub fn plan(layout: &Layout, fleet: &FleetConfig, scope: &OrganizerScope) -> OrganizerPlan {
    let managed = managed_panes_by_name(layout);
    let live: Vec<&str> = managed.keys().map(String::as_str).collect();
    let order = crate::team_order::plan_team_order(fleet, live);

    let in_scope: Vec<&str> = match scope {
        OrganizerScope::AllTeams => order
            .groups
            .iter()
            .filter(|group| !group.corrupt)
            .map(|group| group.name.as_str())
            .collect(),
        OrganizerScope::Team(name) => order
            .groups
            .iter()
            .filter(|group| !group.corrupt && group.name == *name)
            .map(|group| group.name.as_str())
            .collect(),
        OrganizerScope::CurrentTeam => current_team(layout, &order).into_iter().collect::<Vec<_>>(),
    };

    let mut groups = Vec::new();
    for group in &order.groups {
        if group.corrupt || !in_scope.contains(&group.name.as_str()) {
            continue;
        }
        let mut pane_ids = Vec::new();
        let mut members = Vec::new();
        for member in &group.members {
            let Some((pane_id, label)) = managed.get(member) else {
                continue;
            };
            pane_ids.push(*pane_id);
            members.push(label.clone());
        }
        if pane_ids.is_empty() {
            continue;
        }
        groups.push(OrganizerGroup {
            name: group.name.clone(),
            reuse_tab_id: existing_tab_for(layout, &pane_ids),
            pane_ids,
            members,
            preset: LayoutPreset::MainVertical,
        });
    }

    if matches!(scope, OrganizerScope::AllTeams) {
        let mut pane_ids = Vec::new();
        let mut members = Vec::new();
        for name in &order.ungrouped {
            let Some((pane_id, label)) = managed.get(name) else {
                continue;
            };
            pane_ids.push(*pane_id);
            members.push(label.clone());
        }
        if !pane_ids.is_empty() {
            groups.push(OrganizerGroup {
                name: "ungrouped".to_string(),
                reuse_tab_id: existing_tab_for(layout, &pane_ids),
                pane_ids,
                members,
                preset: LayoutPreset::Tiled,
            });
        }
    }

    let pane_tab = pane_locations(layout);
    let moves = groups
        .iter()
        .flat_map(|group| group.pane_ids.iter().map(move |id| (group, *id)))
        .filter(|(group, pane_id)| {
            group
                .reuse_tab_id
                .map(|tab_id| pane_tab.get(pane_id).copied() != Some(tab_id))
                .unwrap_or(true)
        })
        .count();

    OrganizerPlan { groups, moves }
}

/// Apply a plan as a single transaction. Returns a snapshot for [`undo`] via the
/// layout's `organizer_undo` slot. On any failure the original layout is intact.
pub fn apply(layout: &mut Layout, plan: &OrganizerPlan) -> Result<(), OrganizerError> {
    if plan.groups.is_empty() {
        return Err(OrganizerError::EmptyScope);
    }
    let present: HashSet<usize> = layout.all_pane_ids().into_iter().collect();
    let mut planned = HashSet::new();
    for group in &plan.groups {
        for pane_id in &group.pane_ids {
            if !present.contains(pane_id) {
                return Err(OrganizerError::StalePlan);
            }
            if !planned.insert(*pane_id) {
                return Err(OrganizerError::StalePlan);
            }
        }
    }

    let snapshot = capture(layout);
    // #3631: capture each pane's transient tab-level UI state (zoom / in-progress
    // drag / selection anchor) by STABLE PANE ID before anything is moved, so a
    // rehomed pane or a rebuilt tab keeps it. `Pane::selection` (the text
    // highlight) rides along with the moved Pane object itself.
    let ui = capture_ui_state(layout);
    let orig_active = layout.active;
    let orig_active_id = layout.tabs.get(layout.active).map(|tab| tab.id);
    let orig_focus_id = layout.tabs.get(layout.active).map(|tab| tab.focus_id);

    // A group whose reused tab already holds exactly its pane set is left
    // verbatim; every other group's panes are extracted and rebuilt.
    let mut moved: HashSet<usize> = HashSet::new();
    for group in &plan.groups {
        let current_ids: Vec<usize> = group
            .reuse_tab_id
            .and_then(|tab_id| layout.tabs.iter().find(|tab| tab.id == tab_id))
            .map(|tab| tab.root().pane_ids())
            .unwrap_or_default();
        let already_correct =
            group.reuse_tab_id.is_some() && same_id_set(&current_ids, &group.pane_ids);
        if already_correct {
            continue;
        }
        for pane_id in &group.pane_ids {
            moved.insert(*pane_id);
        }
    }

    let mut pool: HashMap<usize, Pane> = HashMap::new();
    let mut new_tabs: Vec<Tab> = Vec::new();
    let mut anchors: HashMap<usize, usize> = HashMap::new();
    let mut built: Vec<(usize, usize, Tab)> = Vec::new();

    // #3631: the guard owns the pre-apply tabs, so a panic mid-rebuild restores
    // the not-yet-processed tabs instead of leaving `layout.tabs` empty. Only
    // `commit` publishes the fully built replacement (single assignment).
    let mut guard = PendingTabsGuard::new(layout);
    while let Some(mut tab) = guard.pop_front() {
        let tab_ids = tab.root().pane_ids();
        let tab_moved = tab_ids.iter().any(|id| moved.contains(id));
        if !tab_moved {
            new_tabs.push(tab);
            continue;
        }
        for (group_idx, group) in plan.groups.iter().enumerate() {
            if group.pane_ids.iter().any(|id| tab_ids.contains(id)) {
                anchors.entry(group_idx).or_insert(new_tabs.len());
            }
        }
        let Some(root) = tab.root.take() else {
            continue; // invariant: every tab keeps a root
        };
        let (remaining, removed) = extract_panes(root, &moved);
        for pane in removed {
            pool.insert(pane.id, pane);
        }
        if let Some(root) = remaining {
            tab.root = Some(root);
            // The pane this tab was focused on left (which, when zoomed, is the
            // zoomed pane): repoint focus and drop the now-meaningless zoom.
            if tab.root().find_pane(tab.focus_id).is_none() {
                tab.focus_id = tab.root().first_pane().id;
                tab.zoomed = false;
            }
            if tab.dragging_pane.is_some_and(|id| moved.contains(&id)) {
                tab.clear_drag();
            }
            if tab.selecting_pane.is_some_and(|id| moved.contains(&id)) {
                tab.selecting_pane = None;
            }
            new_tabs.push(tab);
        }
    }

    for (group_idx, group) in plan.groups.iter().enumerate() {
        let panes: Vec<Pane> = group
            .pane_ids
            .iter()
            .filter_map(|id| pool.remove(id))
            .collect();
        if panes.is_empty() {
            continue;
        }
        let root = build_group_tree(panes, group.preset);
        let mut tab = Tab::with_root(group.name.clone(), root);
        tab.last_layout = Some(group.preset);
        if let Some(reuse) = group.reuse_tab_id {
            if !new_tabs.iter().any(|existing| existing.id == reuse) {
                tab.id = reuse;
            }
        }
        if let Some(focus_id) = orig_focus_id {
            if tab.root().find_pane(focus_id).is_some() {
                tab.focus_id = focus_id;
            }
        }
        let anchor = anchors
            .get(&group_idx)
            .copied()
            .unwrap_or(new_tabs.len())
            .min(new_tabs.len());
        built.push((anchor, group_idx, tab));
    }

    built.sort_by_key(|(anchor, group_idx, _)| (*anchor, *group_idx));
    let mut inserted_at_anchor: HashMap<usize, usize> = HashMap::new();
    for (anchor, _group_idx, tab) in built {
        let offset = inserted_at_anchor.entry(anchor).or_insert(0);
        let pos = (anchor + *offset).min(new_tabs.len());
        new_tabs.insert(pos, tab);
        *offset += 1;
    }

    // Re-apply the captured transient UI state now that final tab order (and so
    // drag-target-tab indices) is known.
    let index_of_tab: HashMap<u64, usize> = new_tabs
        .iter()
        .enumerate()
        .map(|(idx, tab)| (tab.id, idx))
        .collect();
    for tab in new_tabs.iter_mut() {
        apply_ui_state(tab, &ui, &index_of_tab);
    }

    let active = orig_active_id
        .and_then(|id| new_tabs.iter().position(|tab| tab.id == id))
        .or_else(|| {
            orig_focus_id.and_then(|id| {
                new_tabs
                    .iter()
                    .position(|tab| tab.root().find_pane(id).is_some())
            })
        })
        .unwrap_or_else(|| orig_active.min(new_tabs.len().saturating_sub(1)));

    guard.commit(new_tabs);
    layout.active = active;
    layout.organizer_undo = Some(snapshot);
    Ok(())
}

/// RAII keeper of the pre-apply tabs. [`commit`](PendingTabsGuard::commit) hands
/// ownership of the rebuilt tabs to the layout; if the apply scope unwinds first,
/// `Drop` puts the not-yet-processed tabs back so `layout.tabs` is never empty.
struct PendingTabsGuard<'a> {
    layout: &'a mut Layout,
    pending: Vec<Tab>,
    armed: bool,
}

impl<'a> PendingTabsGuard<'a> {
    fn new(layout: &'a mut Layout) -> Self {
        let pending = std::mem::take(&mut layout.tabs);
        Self {
            layout,
            pending,
            armed: true,
        }
    }

    fn pop_front(&mut self) -> Option<Tab> {
        if self.pending.is_empty() {
            None
        } else {
            Some(self.pending.remove(0))
        }
    }

    fn commit(mut self, tabs: Vec<Tab>) {
        self.armed = false;
        self.layout.tabs = tabs;
    }
}

impl Drop for PendingTabsGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.layout.tabs = std::mem::take(&mut self.pending);
        }
    }
}

/// Transient per-pane tab-level UI state captured before a reorder, keyed by the
/// pane that owns it so it can be re-applied after tabs are rebuilt.
#[derive(Default)]
struct UiStateCapture {
    /// Focused pane of a zoomed tab (zoom belongs to the pane it was showing).
    zoomed_pane: Option<usize>,
    /// Pane the mouse was mid-selecting in some tab.
    selecting_pane: Option<usize>,
    /// In-progress title-bar drag (one globally).
    dragging: Option<DraggingCapture>,
}

struct DraggingCapture {
    pane_id: usize,
    target_pane: Option<usize>,
    /// Drop-target tab captured by STABLE id (or "new tab"), so it survives the
    /// reorder instead of going stale on an index.
    target_tab: Option<TabTargetCapture>,
}

enum TabTargetCapture {
    Existing(u64),
    NewTab,
}

fn capture_ui_state(layout: &Layout) -> UiStateCapture {
    let mut capture = UiStateCapture::default();
    for tab in &layout.tabs {
        if capture.zoomed_pane.is_none() && tab.zoomed {
            capture.zoomed_pane = Some(tab.focus_id);
        }
        if capture.selecting_pane.is_none() {
            capture.selecting_pane = tab.selecting_pane;
        }
        if capture.dragging.is_none() {
            if let Some(pane_id) = tab.dragging_pane {
                let target_tab = tab.drag_target_tab.map(|target| match target {
                    DragTabTarget::ExistingTab(idx) => layout
                        .tabs
                        .get(idx)
                        .map(|t| TabTargetCapture::Existing(t.id))
                        .unwrap_or(TabTargetCapture::NewTab),
                    DragTabTarget::NewTab => TabTargetCapture::NewTab,
                });
                capture.dragging = Some(DraggingCapture {
                    pane_id,
                    target_pane: tab.drag_target,
                    target_tab,
                });
            }
        }
    }
    capture
}

fn apply_ui_state(tab: &mut Tab, ui: &UiStateCapture, index_of_tab: &HashMap<u64, usize>) {
    if let Some(zoomed) = ui.zoomed_pane {
        if tab.root().find_pane(zoomed).is_some() {
            tab.zoomed = true;
            tab.focus_id = zoomed;
        }
    }
    if let Some(selecting) = ui.selecting_pane {
        if tab.root().find_pane(selecting).is_some() {
            tab.selecting_pane = Some(selecting);
        }
    }
    if let Some(dragging) = &ui.dragging {
        if tab.root().find_pane(dragging.pane_id).is_some() {
            tab.dragging_pane = Some(dragging.pane_id);
            tab.drag_target = dragging
                .target_pane
                .filter(|target| tab.root().find_pane(*target).is_some());
            tab.drag_target_tab = dragging
                .target_tab
                .as_ref()
                .and_then(|target| match target {
                    TabTargetCapture::Existing(id) => index_of_tab
                        .get(id)
                        .copied()
                        .map(DragTabTarget::ExistingTab),
                    TabTargetCapture::NewTab => Some(DragTabTarget::NewTab),
                });
        }
    }
}

/// Undo the last [`apply`] by replaying its pane-id snapshot. Fail-closed: if
/// the live pane set differs from the snapshot, nothing is touched.
pub fn undo(layout: &mut Layout) -> Result<(), OrganizerError> {
    let Some(snapshot) = layout.organizer_undo.take() else {
        return Err(OrganizerError::NoSnapshot);
    };
    restore(layout, &snapshot)
}

/// Capture the layout's structure by pane id (NOT by agent name).
pub fn capture(layout: &Layout) -> LayoutSnapshot {
    LayoutSnapshot {
        active: layout.active,
        tabs: layout
            .tabs
            .iter()
            .map(|tab| TabSnapshot {
                id: tab.id,
                name: tab.name.clone(),
                focus_id: tab.focus_id,
                zoomed: tab.zoomed,
                last_layout: tab.last_layout,
                selecting_pane: tab.selecting_pane,
                dragging_pane: tab.dragging_pane,
                drag_target: tab.drag_target,
                drag_target_tab: tab.drag_target_tab,
                root: capture_node(tab.root()),
            })
            .collect(),
    }
}

fn restore(layout: &mut Layout, snapshot: &LayoutSnapshot) -> Result<(), OrganizerError> {
    let current: HashSet<usize> = layout.all_pane_ids().into_iter().collect();
    let target: HashSet<usize> = snapshot.pane_ids().into_iter().collect();
    if current != target {
        return Err(OrganizerError::PaneSetChanged);
    }
    // #3631: move every Pane into a pool (VTerm/selection intact) while the
    // guard keeps `layout.tabs` non-empty until the rebuilt tabs are published.
    let mut guard = PendingTabsGuard::new(layout);
    let mut pool: HashMap<usize, Pane> = HashMap::new();
    while let Some(mut tab) = guard.pop_front() {
        let mut panes = Vec::new();
        if let Some(root) = tab.root.take() {
            super::preset::flatten_tree_into(root, &mut panes);
        }
        for pane in panes {
            pool.insert(pane.id, pane);
        }
    }
    let mut tabs = Vec::with_capacity(snapshot.tabs.len());
    for tab_snapshot in &snapshot.tabs {
        let root = build_snapshot_node(&tab_snapshot.root, &mut pool)
            .expect("validated snapshot pane ids are all present");
        let mut tab = Tab::with_root(tab_snapshot.name.clone(), root);
        tab.id = tab_snapshot.id;
        tab.focus_id = tab_snapshot.focus_id;
        tab.zoomed = tab_snapshot.zoomed;
        tab.last_layout = tab_snapshot.last_layout;
        tab.selecting_pane = tab_snapshot.selecting_pane;
        tab.dragging_pane = tab_snapshot.dragging_pane;
        tab.drag_target = tab_snapshot.drag_target;
        tab.drag_target_tab = tab_snapshot.drag_target_tab;
        tabs.push(tab);
    }
    guard.commit(tabs);
    layout.active = snapshot.active.min(layout.tabs.len().saturating_sub(1));
    Ok(())
}

/// Structural, pane-id-keyed snapshot used for rollback and undo.
#[derive(Debug, Clone)]
pub struct LayoutSnapshot {
    active: usize,
    tabs: Vec<TabSnapshot>,
}

impl LayoutSnapshot {
    fn pane_ids(&self) -> Vec<usize> {
        let mut ids = Vec::new();
        for tab in &self.tabs {
            tab.root.collect_pane_ids(&mut ids);
        }
        ids
    }
}

#[derive(Debug, Clone)]
struct TabSnapshot {
    id: u64,
    name: String,
    focus_id: usize,
    zoomed: bool,
    last_layout: Option<LayoutPreset>,
    selecting_pane: Option<usize>,
    dragging_pane: Option<usize>,
    drag_target: Option<usize>,
    drag_target_tab: Option<DragTabTarget>,
    root: NodeSnapshot,
}

#[derive(Debug, Clone)]
enum NodeSnapshot {
    Leaf(usize),
    Split {
        dir: SplitDir,
        ratio: f32,
        first: Box<NodeSnapshot>,
        second: Box<NodeSnapshot>,
    },
}

impl NodeSnapshot {
    fn collect_pane_ids(&self, out: &mut Vec<usize>) {
        match self {
            NodeSnapshot::Leaf(id) => out.push(*id),
            NodeSnapshot::Split { first, second, .. } => {
                first.collect_pane_ids(out);
                second.collect_pane_ids(out);
            }
        }
    }
}

fn capture_node(node: &PaneNode) -> NodeSnapshot {
    match node {
        PaneNode::Leaf(pane) => NodeSnapshot::Leaf(pane.id),
        PaneNode::Split {
            dir,
            ratio,
            first,
            second,
        } => NodeSnapshot::Split {
            dir: *dir,
            ratio: *ratio,
            first: Box::new(capture_node(first)),
            second: Box::new(capture_node(second)),
        },
    }
}

fn build_snapshot_node(node: &NodeSnapshot, pool: &mut HashMap<usize, Pane>) -> Option<PaneNode> {
    match node {
        NodeSnapshot::Leaf(id) => pool.remove(id).map(|pane| PaneNode::Leaf(Box::new(pane))),
        NodeSnapshot::Split {
            dir,
            ratio,
            first,
            second,
        } => {
            let f = build_snapshot_node(first, pool)?;
            let s = build_snapshot_node(second, pool)?;
            Some(PaneNode::Split {
                dir: *dir,
                ratio: *ratio,
                first: Box::new(f),
                second: Box::new(s),
            })
        }
    }
}

fn build_group_tree(panes: Vec<Pane>, preset: LayoutPreset) -> PaneNode {
    debug_assert!(!panes.is_empty());
    if panes.len() == 1 {
        PaneNode::Leaf(Box::new(panes.into_iter().next().expect("checked len")))
    } else {
        build_preset(panes, preset)
    }
}

/// Remove every pane whose id is in `ids`, collapsing leftover splits. Returns
/// the remaining tree (None when everything was removed) and the extracted panes.
fn extract_panes(node: PaneNode, ids: &HashSet<usize>) -> (Option<PaneNode>, Vec<Pane>) {
    match node {
        PaneNode::Leaf(pane) => {
            if ids.contains(&pane.id) {
                (None, vec![*pane])
            } else {
                (Some(PaneNode::Leaf(pane)), Vec::new())
            }
        }
        PaneNode::Split {
            dir,
            ratio,
            first,
            second,
        } => {
            let (first_remaining, mut removed) = extract_panes(*first, ids);
            let (second_remaining, second_removed) = extract_panes(*second, ids);
            removed.extend(second_removed);
            match (first_remaining, second_remaining) {
                (Some(first), Some(second)) => (
                    Some(PaneNode::Split {
                        dir,
                        ratio,
                        first: Box::new(first),
                        second: Box::new(second),
                    }),
                    removed,
                ),
                (Some(node), None) | (None, Some(node)) => (Some(node), removed),
                (None, None) => (None, removed),
            }
        }
    }
}

/// Map each managed instance name to its pane id + display label. Resolution is
/// deterministic: prefer a pane carrying an exact `InstanceRef`, then the lowest
/// pane id.
fn managed_panes_by_name(layout: &Layout) -> BTreeMap<String, (usize, String)> {
    let mut candidates: HashMap<String, Vec<(usize, String, bool)>> = HashMap::new();
    for tab in &layout.tabs {
        for pane_id in tab.root().pane_ids() {
            let Some(pane) = tab.root().find_pane(pane_id) else {
                continue;
            };
            let Some(name) = pane.fleet_instance_name.as_deref() else {
                continue;
            };
            candidates.entry(name.to_string()).or_default().push((
                pane_id,
                pane.label().to_string(),
                pane.instance_ref.is_some(),
            ));
        }
    }
    let mut resolved = BTreeMap::new();
    for (name, mut options) in candidates {
        options.sort_by_key(|(pane_id, _, has_ref)| (!*has_ref, *pane_id));
        if let Some((pane_id, label, _)) = options.into_iter().next() {
            resolved.insert(name, (pane_id, label));
        }
    }
    resolved
}

fn pane_locations(layout: &Layout) -> HashMap<usize, u64> {
    let mut map = HashMap::new();
    for tab in &layout.tabs {
        for pane_id in tab.root().pane_ids() {
            map.insert(pane_id, tab.id);
        }
    }
    map
}

fn existing_tab_for(layout: &Layout, pane_ids: &[usize]) -> Option<u64> {
    if pane_ids.is_empty() {
        return None;
    }
    layout
        .tabs
        .iter()
        .find(|tab| same_id_set(&tab.root().pane_ids(), pane_ids))
        .map(|tab| tab.id)
}

fn same_id_set(a: &[usize], b: &[usize]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let set: HashSet<usize> = a.iter().copied().collect();
    set.len() == a.len() && b.iter().all(|id| set.contains(id))
}

fn current_team<'a>(
    layout: &Layout,
    order: &'a crate::team_order::TeamOrderPlan,
) -> Option<&'a str> {
    let member_team: HashMap<&str, &str> = order
        .groups
        .iter()
        .filter(|group| !group.corrupt)
        .flat_map(|group| {
            group
                .members
                .iter()
                .map(move |member| (member.as_str(), group.name.as_str()))
        })
        .collect();
    let names_for = |tab: &Tab| -> Vec<String> {
        tab.root()
            .pane_ids()
            .into_iter()
            .filter_map(|id| tab.root().find_pane(id))
            .filter_map(|pane| pane.fleet_instance_name.clone())
            .collect()
    };
    if let Some(tab) = layout.active_tab() {
        let names = names_for(tab);
        if let Some(first) = names.first() {
            if let Some(team) = member_team.get(first.as_str()) {
                if names
                    .iter()
                    .all(|name| member_team.get(name.as_str()) == Some(team))
                {
                    return Some(team);
                }
            }
        }
        if let Some(focus) = tab.root().find_pane(tab.focus_id) {
            if let Some(name) = focus.fleet_instance_name.as_deref() {
                if let Some(team) = member_team.get(name) {
                    return Some(team);
                }
            }
        }
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::layout::PaneSource;
    use crate::types::{InstanceId, InstanceRef};
    use crate::vterm::VTerm;

    fn pane(id: usize, agent: &str, fleet: Option<&str>) -> Pane {
        Pane {
            agent_name: agent.into(),
            instance_id: InstanceId::default(),
            instance_ref: fleet.map(|_| InstanceRef::new(InstanceId::new(), id as u64)),
            vterm: VTerm::new(10, 10),
            rx: crossbeam_channel::bounded(1).1,
            id,
            backend: None,
            working_dir: None,
            display_name: None,
            restart_error: None,
            scroll_offset: 0,
            has_notification: false,
            fleet_instance_name: fleet.map(str::to_string),
            last_input_at: None,
            pending_notification_count: 0,
            pending_decision_count: 0,
            selection: None,
            source: PaneSource::Local,
            offthread: None,
            _fwd_cancel: None,
        }
    }

    fn fleet(yaml: &str) -> FleetConfig {
        serde_yaml_ng::from_str(yaml).expect("valid fleet fixture")
    }

    const OPS: &str = "teams:\n  ops:\n    members: [lead, dev]\n    orchestrator: lead\n";

    /// Stable shape for equality assertions: (active, [(tab id, pane ids, focus, zoom)]).
    type Shape = (usize, Vec<(u64, Vec<usize>, usize, bool)>);

    fn shape(layout: &Layout) -> Shape {
        (
            layout.active,
            layout
                .tabs
                .iter()
                .map(|tab| (tab.id, tab.root().pane_ids(), tab.focus_id, tab.zoomed))
                .collect(),
        )
    }

    fn scattered() -> Layout {
        let mut layout = Layout::new();
        layout.add_tab(Tab::new("lead-tab".into(), pane(1, "lead", Some("lead"))));
        layout.add_tab(Tab::new("dev-tab".into(), pane(2, "dev", Some("dev"))));
        layout.active = 1;
        layout
    }

    #[test]
    fn scattered_team_panes_consolidate_into_one_team_tab_lead_first() {
        let fleet = fleet(OPS);
        let mut layout = scattered();
        layout.add_tab(Tab::new("shell".into(), pane(3, "shell", None)));
        layout.active = 1; // focus the dev tab; `add_tab` had moved focus to the shell

        let plan = plan(&layout, &fleet, &OrganizerScope::AllTeams);
        assert_eq!(plan.groups.len(), 1);
        assert_eq!(plan.groups[0].name, "ops");
        assert_eq!(
            plan.groups[0].pane_ids,
            vec![1, 2],
            "lead first, then member"
        );
        assert_eq!(plan.moves, 2);

        apply(&mut layout, &plan).unwrap();
        assert_eq!(layout.tabs.len(), 2, "team tab + untouched local shell tab");
        let ops = layout
            .tabs
            .iter()
            .find(|tab| tab.name == "ops")
            .expect("team tab built");
        assert_eq!(ops.root().agent_names(), vec!["lead", "dev"]);
        assert!(layout
            .tabs
            .iter()
            .any(|tab| tab.root().find_pane(3).is_some()));

        // Active tab + focus follow the previously focused pane (dev), never an index.
        assert!(
            layout.tabs[layout.active].root().find_pane(2).is_some(),
            "focus follows pane 2 to its new tab"
        );
        assert_eq!(layout.tabs[layout.active].focus_id, 2);
    }

    #[test]
    fn preview_never_mutates_layout() {
        let fleet = fleet(OPS);
        let mut layout = scattered();
        layout.add_tab(Tab::new("shell".into(), pane(3, "shell", None)));
        let before = shape(&layout);
        let _ = plan(&layout, &fleet, &OrganizerScope::AllTeams);
        assert_eq!(shape(&layout), before, "plan is a pure preview");
    }

    #[test]
    fn stale_plan_is_rejected_and_layout_is_untouched() {
        let fleet = fleet(OPS);
        let mut layout = scattered();
        let plan = plan(&layout, &fleet, &OrganizerScope::AllTeams);

        // A pane named by the plan disappears before apply.
        layout.close_tab(1);
        let before = shape(&layout);
        assert_eq!(apply(&mut layout, &plan), Err(OrganizerError::StalePlan));
        assert_eq!(
            shape(&layout),
            before,
            "failed apply rolls back to identical state"
        );
        assert!(
            !layout.tabs.is_empty(),
            "a rejected apply must never leave an empty layout"
        );
    }

    #[test]
    fn local_only_tab_keeps_custom_ratio_selection_and_focus() {
        let fleet = fleet(OPS);
        let mut layout = scattered();
        let root = PaneNode::Split {
            dir: SplitDir::Vertical,
            ratio: 0.3,
            first: Box::new(PaneNode::Leaf(Box::new(pane(4, "sh1", None)))),
            second: Box::new(PaneNode::Leaf(Box::new(pane(5, "sh2", None)))),
        };
        let mut local = Tab::with_root("local".into(), root);
        local.focus_id = 4;
        local.selecting_pane = Some(4);
        local.dragging_pane = Some(4);
        local.zoomed = true;
        let local_id = local.id;
        layout.add_tab(local);
        layout.active = 2;

        let plan = plan(&layout, &fleet, &OrganizerScope::AllTeams);
        apply(&mut layout, &plan).unwrap();

        let kept = layout
            .tabs
            .iter()
            .find(|tab| tab.id == local_id)
            .expect("local tab is untouched and keeps its identity");
        assert_eq!(kept.root().pane_ids(), vec![4, 5]);
        assert_eq!(kept.selecting_pane, Some(4));
        assert_eq!(kept.dragging_pane, Some(4), "drag state is preserved");
        assert!(kept.zoomed, "zoom state is preserved");
        assert_eq!(kept.focus_id, 4);
        match kept.root() {
            PaneNode::Split { ratio, .. } => assert!((ratio - 0.3).abs() < f32::EPSILON),
            PaneNode::Leaf(_) => panic!("local split geometry must be preserved"),
        }
    }

    #[test]
    fn mixed_tab_rehomes_managed_panes_but_keeps_local_shell() {
        let fleet = fleet(OPS);
        let mut layout = Layout::new();
        let mut mixed = Tab::new("mixed".into(), pane(1, "lead", Some("lead")));
        mixed.split_focused(SplitDir::Horizontal, pane(2, "shell", None));
        mixed.focus_id = 2;
        let mixed_id = mixed.id;
        layout.add_tab(mixed);
        layout.add_tab(Tab::new("dev-tab".into(), pane(3, "dev", Some("dev"))));

        let plan = plan(&layout, &fleet, &OrganizerScope::AllTeams);
        apply(&mut layout, &plan).unwrap();

        let shell_tab = layout
            .tabs
            .iter()
            .find(|tab| tab.id == mixed_id)
            .expect("the mixed tab survives for its local shell");
        assert_eq!(shell_tab.root().pane_ids(), vec![2]);
        assert_eq!(shell_tab.focus_id, 2);
        let ops = layout
            .tabs
            .iter()
            .find(|tab| tab.name == "ops")
            .expect("team tab built");
        assert_eq!(ops.root().pane_ids().len(), 2);
        assert!(ops.root().find_pane(1).is_some() && ops.root().find_pane(3).is_some());
    }

    #[test]
    fn already_arranged_team_tab_is_kept_verbatim() {
        let fleet = fleet(OPS);
        let root = PaneNode::Split {
            dir: SplitDir::Vertical,
            ratio: 0.25,
            first: Box::new(PaneNode::Leaf(Box::new(pane(1, "lead", Some("lead"))))),
            second: Box::new(PaneNode::Leaf(Box::new(pane(2, "dev", Some("dev"))))),
        };
        let team_tab = Tab::with_root("ops".into(), root);
        let team_id = team_tab.id;
        let mut layout = Layout::new();
        layout.add_tab(team_tab);
        layout.active = 0;

        let plan = plan(&layout, &fleet, &OrganizerScope::AllTeams);
        assert_eq!(plan.moves, 0, "already arranged");
        assert_eq!(plan.groups[0].reuse_tab_id, Some(team_id));
        apply(&mut layout, &plan).unwrap();

        assert_eq!(layout.tabs.len(), 1);
        assert_eq!(layout.tabs[0].id, team_id);
        match layout.tabs[0].root() {
            PaneNode::Split { ratio, .. } => assert!((ratio - 0.25).abs() < f32::EPSILON),
            PaneNode::Leaf(_) => panic!("custom ratio must survive a no-op arrange"),
        }
    }

    #[test]
    fn deterministic_canonical_member_order_follows_3630() {
        let fleet = fleet("teams:\n  ops:\n    members: [m3, lead, m2]\n    orchestrator: lead\n");
        let mut layout = Layout::new();
        layout.add_tab(Tab::new("t".into(), pane(1, "lead", Some("lead"))));
        layout.add_tab(Tab::new("t".into(), pane(2, "m2", Some("m2"))));
        layout.add_tab(Tab::new("t".into(), pane(3, "m3", Some("m3"))));

        let plan = plan(&layout, &fleet, &OrganizerScope::AllTeams);
        assert_eq!(
            plan.groups[0].pane_ids,
            vec![1, 3, 2],
            "orchestrator first, then canonical TeamConfig.members order"
        );
    }

    #[test]
    fn all_teams_includes_ungrouped_area() {
        let fleet = fleet("teams:\n  ops:\n    members: [lead, dev]\n    orchestrator: lead\n");
        let mut layout = scattered();
        layout.add_tab(Tab::new("solo".into(), pane(7, "solo", Some("solo"))));

        let plan = plan(&layout, &fleet, &OrganizerScope::AllTeams);
        let names: Vec<&str> = plan.groups.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, vec!["ops", "ungrouped"]);
        let ungrouped = plan.groups.iter().find(|g| g.name == "ungrouped").unwrap();
        assert_eq!(ungrouped.pane_ids, vec![7]);
    }

    #[test]
    fn current_team_scope_arranges_only_the_active_team() {
        let fleet = fleet(
            "teams:\n  ops:\n    members: [lead, dev]\n    orchestrator: lead\n  qa:\n    members: [qlead, qdev]\n    orchestrator: qlead\n",
        );
        let mut layout = Layout::new();
        layout.add_tab(Tab::new("ops".into(), pane(1, "lead", Some("lead"))));
        layout.add_tab(Tab::new("qa".into(), pane(3, "qlead", Some("qlead"))));
        layout.add_tab(Tab::new("qa2".into(), pane(4, "qdev", Some("qdev"))));
        layout.active = 0;

        let plan = plan(&layout, &fleet, &OrganizerScope::CurrentTeam);
        assert_eq!(plan.groups.len(), 1);
        assert_eq!(plan.groups[0].name, "ops");
    }

    #[test]
    fn undo_restores_the_transaction_snapshot() {
        let fleet = fleet(OPS);
        let mut layout = scattered();
        layout.add_tab(Tab::new("shell".into(), pane(3, "shell", None)));
        let before = shape(&layout);

        let plan = plan(&layout, &fleet, &OrganizerScope::AllTeams);
        apply(&mut layout, &plan).unwrap();
        assert_ne!(shape(&layout), before, "arrange changed the layout");

        undo(&mut layout).unwrap();
        assert_eq!(shape(&layout), before, "undo replays the pane-id snapshot");
    }

    #[test]
    fn undo_fails_closed_after_pane_set_changes() {
        let fleet = fleet(OPS);
        let mut layout = scattered();
        let plan = plan(&layout, &fleet, &OrganizerScope::AllTeams);
        apply(&mut layout, &plan).unwrap();

        layout.add_tab(Tab::new("extra".into(), pane(99, "extra", None)));
        let before = shape(&layout);
        assert_eq!(undo(&mut layout), Err(OrganizerError::PaneSetChanged));
        assert_eq!(
            shape(&layout),
            before,
            "refused undo leaves the layout alone"
        );
    }

    #[test]
    fn no_snapshot_undo_is_rejected() {
        let mut layout = Layout::new();
        assert_eq!(undo(&mut layout), Err(OrganizerError::NoSnapshot));
    }

    /// #3631 acceptance 4: a pane that is *rehomed* keeps its zoom, in-progress
    /// selection anchor, drag state, and text selection.
    #[test]
    fn moved_pane_keeps_zoom_selection_and_drag_state() {
        let fleet = fleet(OPS);
        let mut lead = pane(1, "lead", Some("lead"));
        lead.selection = Some(crate::layout::Selection {
            start: (0, 0),
            end: (2, 0),
        });
        let mut layout = Layout::new();
        layout.add_tab(Tab::new("scatter-lead".into(), lead));
        layout.add_tab(Tab::new("scatter-dev".into(), pane(2, "dev", Some("dev"))));
        // `add_tab` clears the outgoing tab's transient input, so seed the state
        // after the layout exists (as it would be at real apply time).
        layout.tabs[0].zoomed = true;
        layout.tabs[0].focus_id = 1;
        layout.tabs[0].selecting_pane = Some(1);
        layout.tabs[0].dragging_pane = Some(1);
        layout.active = 0;

        let plan = plan(&layout, &fleet, &OrganizerScope::AllTeams);
        apply(&mut layout, &plan).unwrap();

        let ops = layout
            .tabs
            .iter()
            .find(|tab| tab.name == "ops")
            .expect("team tab built");
        assert!(ops.root().find_pane(1).is_some() && ops.root().find_pane(2).is_some());
        assert!(ops.zoomed, "zoom survives a rehome");
        assert_eq!(ops.focus_id, 1, "the zoomed pane keeps focus");
        assert_eq!(ops.selecting_pane, Some(1), "selection anchor survives");
        assert_eq!(ops.dragging_pane, Some(1), "drag state survives");
        assert!(
            ops.root().find_pane(1).unwrap().selection.is_some(),
            "text selection rides with the moved Pane"
        );
    }

    /// #3631 acceptance 4 + undo completeness: undo is a perfect inverse for the
    /// transient tab-level state too.
    #[test]
    fn undo_restores_transient_tab_state() {
        let fleet = fleet(OPS);
        let mut layout = Layout::new();
        layout.add_tab(Tab::new(
            "scatter-lead".into(),
            pane(1, "lead", Some("lead")),
        ));
        let lead_tab_id = layout.tabs[0].id;
        layout.add_tab(Tab::new("scatter-dev".into(), pane(2, "dev", Some("dev"))));
        // Seed transient state after `add_tab` (which clears the outgoing tab).
        layout.tabs[0].zoomed = true;
        layout.tabs[0].selecting_pane = Some(1);
        layout.tabs[0].dragging_pane = Some(1);
        layout.tabs[0].drag_target = Some(1);
        layout.tabs[0].last_layout = Some(LayoutPreset::Tiled);
        layout.active = 0;

        let plan = plan(&layout, &fleet, &OrganizerScope::AllTeams);
        apply(&mut layout, &plan).unwrap();
        undo(&mut layout).unwrap();

        let restored = layout
            .tabs
            .iter()
            .find(|tab| tab.id == lead_tab_id)
            .expect("original tab restored by snapshot");
        assert!(restored.zoomed);
        assert_eq!(restored.selecting_pane, Some(1));
        assert_eq!(restored.dragging_pane, Some(1));
        assert_eq!(restored.drag_target, Some(1));
        assert_eq!(restored.last_layout, Some(LayoutPreset::Tiled));
        assert_eq!(layout.active, 0);
    }

    /// #3631 blocking-2: a panic inside the apply scope must not leave
    /// `layout.tabs` empty — the guard restores the unprocessed tabs on unwind.
    #[test]
    fn pending_tabs_guard_restores_on_unwind() {
        let mut layout = Layout::new();
        layout.add_tab(Tab::new("a".into(), pane(1, "a", None)));
        layout.add_tab(Tab::new("b".into(), pane(2, "b", None)));

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut guard = PendingTabsGuard::new(&mut layout);
            let _first = guard.pop_front();
            panic!("simulated mid-apply panic");
        }));

        assert!(result.is_err(), "the simulated panic must propagate");
        assert_eq!(
            layout.tabs.len(),
            1,
            "the guard restores the unprocessed tab instead of leaving 0 tabs"
        );
        assert!(layout.tabs[0].root().find_pane(2).is_some());
    }

    /// #3631 documented limitation: a team tab that has to be MERGED/REBUILT is
    /// laid out by its preset, so a source tab's custom split ratio cannot be
    /// carried across the merge (only untouched / already-correct tabs keep their
    /// geometry — see `already_arranged_team_tab_is_kept_verbatim`).
    #[test]
    fn merged_team_tab_geometry_follows_the_preset() {
        let fleet = fleet(OPS);
        let mut dev_tab = Tab::new("mixed".into(), pane(2, "dev", Some("dev")));
        dev_tab.split_focused(SplitDir::Horizontal, pane(9, "shell", None));
        if let PaneNode::Split { ratio, .. } = dev_tab.root_mut() {
            *ratio = 0.4;
        }
        let mut layout = Layout::new();
        layout.add_tab(dev_tab);
        layout.add_tab(Tab::new("lead".into(), pane(1, "lead", Some("lead"))));

        let plan = plan(&layout, &fleet, &OrganizerScope::AllTeams);
        apply(&mut layout, &plan).unwrap();

        let ops = layout
            .tabs
            .iter()
            .find(|tab| tab.name == "ops")
            .expect("team tab built");
        match ops.root() {
            PaneNode::Split { ratio, .. } => {
                assert!(
                    (ratio - 0.5).abs() < f32::EPSILON,
                    "merged tab uses the preset ratio, not the source's 0.4"
                );
            }
            PaneNode::Leaf(_) => panic!("two-pane team tab must be split"),
        }
    }
}
