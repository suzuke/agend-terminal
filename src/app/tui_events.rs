//! TUI-side handling of events from the API server.
//!
//! The API server (hit via MCP or HTTP) signals agent/team lifecycle changes on
//! a bounded crossbeam channel. The TUI event loop receives these and mutates
//! the layout accordingly — auto-creating or removing tabs/panes.

use crate::agent::{self, AgentRegistry};
use crate::layout::{Layout, MovePlacement, SplitDir, Tab};

/// Events sent from the API server to the TUI event loop when agents or teams
/// are created/deleted via MCP tools. The TUI reacts by auto-creating or
/// removing tabs/panes.
#[derive(Debug, Clone)]
pub(crate) enum TuiEvent {
    InstanceCreated {
        name: String,
        layout: LayoutHint,
        /// The caller instance (auto-filled by the MCP handler). Used as the
        /// split anchor when `target_pane` is `None` or not currently displayed.
        spawner: Option<String>,
        /// Explicit split anchor requested via `create_instance`'s
        /// `target_pane` argument. When set and the agent is currently
        /// displayed in any tab, the new pane is attached next to it,
        /// overriding `spawner`.
        target_pane: Option<String>,
    },
    InstanceDeleted {
        name: String,
    },
    TeamCreated {
        name: String,
        members: Vec<String>,
    },
    /// Emitted by `UPDATE_TEAM` when members are added to or removed from an
    /// existing team. `added` members migrate into the team tab (created on
    /// demand); `removed` members vanish from the team tab. Noop diffs (e.g.
    /// re-adding an existing member) are filtered out by the API server.
    TeamMembersChanged {
        name: String,
        added: Vec<String>,
        removed: Vec<String>,
    },
    /// Emitted by the `move_pane` MCP tool. Relocates the pane currently
    /// displaying `agent` into `target_tab`: if the tab exists, the pane
    /// splits its focused pane along `split_dir`; otherwise a new tab named
    /// `target_tab` is created with the moved pane as root (split_dir ignored).
    PaneMoved {
        agent: String,
        target_tab: String,
        split_dir: SplitDir,
    },
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) enum LayoutHint {
    #[default]
    Tab,
    SplitRight,
    SplitBelow,
}

pub(crate) type TuiEventSender = crossbeam::channel::Sender<TuiEvent>;

/// Adapter that converts [`crate::api::ApiEvent`] into [`TuiEvent`] and
/// forwards it over the crossbeam channel to the TUI event loop.
pub(crate) struct TuiNotifier {
    pub tx: TuiEventSender,
}

impl crate::api::ApiNotifier for TuiNotifier {
    fn notify(&self, event: crate::api::ApiEvent) {
        let tui_event = match event {
            crate::api::ApiEvent::InstanceCreated {
                name,
                layout,
                spawner,
                target_pane,
            } => {
                let hint = match layout {
                    crate::api::LayoutHint::Tab => LayoutHint::Tab,
                    crate::api::LayoutHint::SplitRight => LayoutHint::SplitRight,
                    crate::api::LayoutHint::SplitBelow => LayoutHint::SplitBelow,
                };
                TuiEvent::InstanceCreated {
                    name,
                    layout: hint,
                    spawner,
                    target_pane,
                }
            }
            crate::api::ApiEvent::InstanceDeleted { name } => TuiEvent::InstanceDeleted { name },
            crate::api::ApiEvent::TeamCreated { name, members } => {
                TuiEvent::TeamCreated { name, members }
            }
            crate::api::ApiEvent::TeamMembersChanged {
                name,
                added,
                removed,
            } => TuiEvent::TeamMembersChanged {
                name,
                added,
                removed,
            },
            crate::api::ApiEvent::PaneMoved {
                agent,
                target_tab,
                split_dir,
            } => {
                let dir = match split_dir {
                    crate::api::PaneMoveSplitDir::Horizontal => SplitDir::Horizontal,
                    crate::api::PaneMoveSplitDir::Vertical => SplitDir::Vertical,
                };
                TuiEvent::PaneMoved {
                    agent,
                    target_tab,
                    split_dir: dir,
                }
            }
        };
        if let Err(e) = self.tx.try_send(tui_event) {
            tracing::warn!(error = %e, "TUI event send failed");
        }
    }
}

/// Handle a TuiEvent from the API server (auto-create/remove tabs/panes).
pub(super) fn handle_tui_event(
    event: TuiEvent,
    layout: &mut Layout,
    registry: &AgentRegistry,
    wakeup_tx: &crossbeam::channel::Sender<usize>,
) {
    tracing::info!(event = ?event, tabs = layout.tabs.len(), "handle_tui_event");
    match event {
        TuiEvent::InstanceCreated {
            name,
            layout: hint,
            spawner,
            target_pane,
        } => {
            handle_instance_created(
                &name,
                hint,
                spawner.as_deref(),
                target_pane.as_deref(),
                layout,
                registry,
                wakeup_tx,
            );
        }
        TuiEvent::InstanceDeleted { name } => {
            handle_instance_deleted(&name, layout);
        }
        TuiEvent::TeamCreated { name, members } => {
            handle_team_created(&name, &members, layout, registry, wakeup_tx);
        }
        TuiEvent::TeamMembersChanged {
            name,
            added,
            removed,
        } => {
            handle_team_members_changed(&name, &added, &removed, layout, registry, wakeup_tx);
        }
        TuiEvent::PaneMoved {
            agent,
            target_tab,
            split_dir,
        } => {
            handle_pane_moved(&agent, &target_tab, split_dir, layout);
        }
    }
}

/// Apply a `PaneMoved` event — find the pane displaying `agent` and relocate
/// it into `target_tab`. If the target tab exists the moved pane splits its
/// focused pane along `split_dir`; otherwise a new tab named `target_tab` is
/// created and the moved pane becomes its root.
///
/// No-op if the agent is not displayed anywhere, the source is the only pane
/// in the only tab (detach would leave layout empty), or the target tab is the
/// same as the source tab and contains only this pane.
fn handle_pane_moved(agent: &str, target_tab: &str, split_dir: SplitDir, layout: &mut Layout) {
    tracing::debug!(
        agent,
        target_tab,
        dir = ?split_dir,
        "handle_pane_moved"
    );
    let (from_idx, pane_id) = match layout.find_agent_pane(agent) {
        Some(s) => s,
        None => {
            tracing::warn!(agent, "handle_pane_moved: agent not displayed, ignored");
            return;
        }
    };

    let placement = match layout.tabs.iter().position(|t| t.name == target_tab) {
        Some(to_idx) if to_idx == from_idx => return,
        Some(to_idx) => MovePlacement::SplitFocused {
            to_tab: to_idx,
            dir: split_dir,
        },
        None => MovePlacement::NewTab {
            name: target_tab.to_string(),
        },
    };
    if layout
        .move_pane_across_tabs(from_idx, pane_id, placement)
        .is_none()
    {
        tracing::warn!(agent, target_tab, "handle_pane_moved: move refused");
    }
}

/// Where a newly-created pane should land.
#[derive(Debug)]
enum SplitAnchor {
    /// Split the pane with this `pane_id` inside the tab at `tab_idx`.
    Pane { tab_idx: usize, pane_id: usize },
    /// Split whatever pane is focused in this tab.
    Focused { tab_idx: usize },
}

/// Resolve the split anchor for a new instance.
///
/// Precedence:
///   1. `target_pane` — exact pane in whichever tab currently displays it.
///   2. `spawner` — focused pane of the caller's tab (legacy behavior).
///   3. `None` — fall back to a new tab.
///
/// Only consulted for `SplitRight` / `SplitBelow` hints; `Tab` hints always
/// return `None` here.
fn resolve_split_anchor(
    hint: LayoutHint,
    target_pane: Option<&str>,
    spawner: Option<&str>,
    layout: &Layout,
) -> Option<SplitAnchor> {
    if matches!(hint, LayoutHint::Tab) {
        return None;
    }
    if let Some(tp) = target_pane {
        for (tab_idx, tab) in layout.tabs.iter().enumerate() {
            if let Some(pane_id) = tab.root().find_pane_id_by_agent(tp) {
                return Some(SplitAnchor::Pane { tab_idx, pane_id });
            }
        }
        tracing::info!(
            target_pane = tp,
            "resolve_split_anchor: target_pane not displayed, falling back to spawner"
        );
    }
    spawner.and_then(|s| {
        layout
            .tabs
            .iter()
            .position(|tab| tab.root().has_agent(s))
            .map(|tab_idx| SplitAnchor::Focused { tab_idx })
    })
}

#[allow(clippy::too_many_arguments)]
fn handle_instance_created(
    name: &str,
    hint: LayoutHint,
    spawner: Option<&str>,
    target_pane: Option<&str>,
    layout: &mut Layout,
    registry: &AgentRegistry,
    wakeup_tx: &crossbeam::channel::Sender<usize>,
) {
    tracing::info!(
        agent = name,
        hint = ?hint,
        spawner = ?spawner,
        target_pane = ?target_pane,
        tabs_before = layout.tabs.len(),
        "handle_instance_created begin"
    );
    if layout.tabs.iter().any(|tab| tab.root().has_agent(name)) {
        tracing::info!(
            agent = name,
            "handle_instance_created: agent already in layout, deduped"
        );
        return;
    }

    let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));

    // Resolve placement BEFORE attaching a pane. Each attach_pane call
    // subscribes to the agent's output and spawns a forwarder thread;
    // discarding an attached pane leaves an orphan subscription that lingers
    // until the agent next emits data (indefinite on idle agents). Pre-checking
    // ensures we only attach once.
    let anchor = resolve_split_anchor(hint, target_pane, spawner, layout);

    let pane = match super::pane_factory::attach_pane(
        name,
        registry,
        cols,
        rows.saturating_sub(4),
        wakeup_tx,
        layout,
    ) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(agent = name, error = %e, "failed to attach pane for new instance");
            return;
        }
    };

    match anchor {
        Some(a) => {
            let dir = match hint {
                LayoutHint::SplitRight => SplitDir::Horizontal,
                LayoutHint::SplitBelow => SplitDir::Vertical,
                // Tab was filtered out by resolve_split_anchor.
                LayoutHint::Tab => unreachable!("Tab hint never produces a SplitAnchor"),
            };
            match a {
                SplitAnchor::Pane { tab_idx, pane_id } => {
                    // split_at_pane consumes `pane`. The pane_id was read from
                    // the same layout snapshot used to pick the tab, so it must
                    // still exist here.
                    layout.tabs[tab_idx].split_at_pane(pane_id, dir, pane);
                }
                SplitAnchor::Focused { tab_idx } => {
                    layout.tabs[tab_idx].split_focused(dir, pane);
                }
            }
        }
        None => {
            layout.add_tab(Tab::new(name.to_string(), pane));
        }
    }
}

fn handle_instance_deleted(name: &str, layout: &mut Layout) {
    remove_agent_pane(name, layout);
}

fn handle_team_created(
    team_name: &str,
    members: &[String],
    layout: &mut Layout,
    registry: &AgentRegistry,
    wakeup_tx: &crossbeam::channel::Sender<usize>,
) {
    tracing::info!(team = team_name, members = ?members, tabs_before = layout.tabs.len(), "handle_team_created begin");
    let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
    let pane_rows = rows.saturating_sub(4);

    // Filter members in two passes:
    //   1. must exist in registry (spawn_agent completed)
    //   2. must NOT already be displayed in any tab — defensive guard against
    //      re-entry. Mirrors `handle_instance_created`'s dedup check. With
    //      per-member dedup in CREATE_TEAM this should never skip anyone, but
    //      the check keeps behavior safe if the API path changes.
    let (running, missing): (Vec<&str>, Vec<&str>) = {
        let reg = agent::lock_registry(registry);
        let (r, m): (Vec<_>, Vec<_>) = members
            .iter()
            .map(|m| m.as_str())
            .partition(|m| reg.contains_key(*m));
        (r, m)
    };
    if !missing.is_empty() {
        tracing::warn!(team = team_name, missing = ?missing, "handle_team_created: members not in registry, skipped");
    }
    let running: Vec<&str> = running
        .into_iter()
        .filter(|m| {
            let already = layout.tabs.iter().any(|tab| tab.root().has_agent(m));
            if already {
                tracing::warn!(
                    team = team_name,
                    member = m,
                    "handle_team_created: member already in a tab, skipped"
                );
            }
            !already
        })
        .collect();
    tracing::info!(team = team_name, running = ?running, "handle_team_created: filter complete");

    if running.is_empty() {
        tracing::warn!(
            team = team_name,
            "handle_team_created: no running members, no tab created"
        );
        return;
    }

    let first_pane = match super::pane_factory::attach_pane(
        running[0], registry, cols, pane_rows, wakeup_tx, layout,
    ) {
        Ok(p) => {
            tracing::info!(
                team = team_name,
                first = running[0],
                "handle_team_created: first pane attached"
            );
            p
        }
        Err(e) => {
            tracing::warn!(team = team_name, first = running[0], error = %e, "handle_team_created: first attach_pane failed, no tab created");
            return;
        }
    };

    let mut tab = Tab::new(team_name.to_string(), first_pane);
    let mut attached = 1usize;

    for member in &running[1..] {
        match super::pane_factory::attach_pane(member, registry, cols, pane_rows, wakeup_tx, layout)
        {
            Ok(pane) => {
                tab.split_focused(SplitDir::Horizontal, pane);
                attached += 1;
            }
            Err(e) => {
                tracing::warn!(team = team_name, member = member, error = %e, "handle_team_created: split attach_pane failed");
            }
        }
    }

    let panes_in_tab = tab.root().pane_count();
    layout.add_tab(tab);
    tracing::info!(
        team = team_name,
        expected = running.len(),
        attached,
        panes_in_tab,
        tabs_after = layout.tabs.len(),
        "handle_team_created end"
    );
}

/// Apply an `update_team add/remove` diff to the layout. Moves `added`
/// members' panes into the team tab (creating it if absent) and drops
/// `removed` members' panes from the team tab (leaving other tabs untouched,
/// so a removed member isn't thrown off-screen entirely).
fn handle_team_members_changed(
    team_name: &str,
    added: &[String],
    removed: &[String],
    layout: &mut Layout,
    registry: &AgentRegistry,
    wakeup_tx: &crossbeam::channel::Sender<usize>,
) {
    tracing::info!(
        team = team_name,
        added = ?added,
        removed = ?removed,
        "handle_team_members_changed begin"
    );
    let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
    let pane_rows = rows.saturating_sub(4);

    // Remove: pull member out of the team tab only. If the member still has a
    // pane elsewhere (its own tab), leave that alone — the agent is still
    // running, just not grouped.
    for member in removed {
        if let Some(tab_idx) = layout
            .tabs
            .iter()
            .position(|tab| tab.name == team_name && tab.root().has_agent(member))
        {
            if let Some(pane_id) = layout.tabs[tab_idx].root().find_pane_id_by_agent(member) {
                if layout.tabs[tab_idx].root().pane_count() <= 1 {
                    layout.close_tab(tab_idx);
                } else {
                    layout.tabs[tab_idx].close_pane_by_id(pane_id);
                }
            }
        }
    }

    // Add: filter to members that exist in the registry (otherwise there's
    // nothing to attach). A member already inside the team tab is a no-op.
    let to_attach: Vec<&str> = {
        let reg = agent::lock_registry(registry);
        added
            .iter()
            .map(|m| m.as_str())
            .filter(|m| reg.contains_key(*m))
            .collect()
    };
    if to_attach.is_empty() {
        tracing::info!(
            team = team_name,
            "handle_team_members_changed: nothing to attach"
        );
        return;
    }

    // Add phase: move each incoming member into the team tab. Prefer MOVING
    // an existing pane (preserves VTerm scrollback and PTY subscription) over
    // rebuilding via attach_pane. The team tab may not exist yet — establish
    // it lazily from the first successful incoming member.
    let mut team_tab_idx = layout.tabs.iter().position(|tab| tab.name == team_name);
    let mut iter = to_attach.iter();
    if team_tab_idx.is_none() {
        for member in iter.by_ref() {
            if let Some((from_idx, pane_id)) = layout.find_agent_pane(member) {
                if let Some(new_idx) = layout.move_pane_across_tabs(
                    from_idx,
                    pane_id,
                    MovePlacement::NewTab {
                        name: team_name.to_string(),
                    },
                ) {
                    team_tab_idx = Some(new_idx);
                    break;
                }
            }
            // Member is registered but not displayed — synthesize a fresh pane.
            match super::pane_factory::attach_pane(
                member, registry, cols, pane_rows, wakeup_tx, layout,
            ) {
                Ok(pane) => {
                    layout.add_tab(Tab::new(team_name.to_string(), pane));
                    team_tab_idx = Some(layout.tabs.len() - 1);
                    break;
                }
                Err(e) => {
                    tracing::warn!(team = team_name, member = member, error = %e, "handle_team_members_changed: attach_pane failed");
                }
            }
        }
    }
    let mut tab_idx = match team_tab_idx {
        Some(i) => i,
        None => {
            tracing::warn!(
                team = team_name,
                "handle_team_members_changed: no team tab established"
            );
            return;
        }
    };

    for member in iter {
        if layout.tabs[tab_idx].root().has_agent(member) {
            continue;
        }
        // Source must live in a different tab; otherwise `has_agent` above
        // would have caught it.
        let source = layout.tabs.iter().enumerate().find_map(|(i, t)| {
            (i != tab_idx)
                .then(|| t.root().find_pane_id_by_agent(member).map(|p| (i, p)))
                .flatten()
        });
        if let Some((from_idx, pane_id)) = source {
            if let Some(new_idx) = layout.move_pane_across_tabs(
                from_idx,
                pane_id,
                MovePlacement::SplitFocused {
                    to_tab: tab_idx,
                    dir: SplitDir::Horizontal,
                },
            ) {
                tab_idx = new_idx;
            }
            continue;
        }
        match super::pane_factory::attach_pane(member, registry, cols, pane_rows, wakeup_tx, layout)
        {
            Ok(pane) => {
                layout.tabs[tab_idx].split_focused(SplitDir::Horizontal, pane);
            }
            Err(e) => {
                tracing::warn!(team = team_name, member = member, error = %e, "handle_team_members_changed: split attach_pane failed");
            }
        }
    }
}

/// Remove ALL panes for the given agent from every tab. Cleans up empty tabs.
pub(super) fn remove_agent_pane(name: &str, layout: &mut Layout) {
    let mut removed = 0usize;
    loop {
        let target = layout.tabs.iter().enumerate().find_map(|(tab_idx, tab)| {
            tab.root()
                .find_pane_id_by_agent(name)
                .map(|pane_id| (tab_idx, pane_id))
        });
        let (tab_idx, pane_id) = match target {
            Some(t) => t,
            None => break,
        };
        let pane_count = layout.tabs[tab_idx].root().pane_count();
        if pane_count <= 1 {
            tracing::info!(
                agent = name,
                tab_idx,
                pane_id,
                "remove_agent_pane: closing tab (single pane)"
            );
            layout.close_tab(tab_idx);
        } else {
            tracing::info!(
                agent = name,
                tab_idx,
                pane_id,
                pane_count,
                "remove_agent_pane: closing pane (multi pane)"
            );
            layout.tabs[tab_idx].close_pane_by_id(pane_id);
        }
        removed += 1;
    }
    if removed == 0 {
        tracing::info!(
            agent = name,
            tabs = layout.tabs.len(),
            "remove_agent_pane: no matching pane found"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::layout::{Pane, PaneSource, Tab};
    use crate::vterm::VTerm;

    fn leaf(id: usize, agent: &str) -> Pane {
        Pane {
            agent_name: agent.to_string(),
            vterm: VTerm::new(10, 10),
            rx: crossbeam::channel::bounded(1).1,
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
    fn remove_agent_pane_closes_single_pane_tab() {
        // Reproduces the Telegram topic-close bug: a single-pane tab must be
        // removed (not just its pane) when the agent is deleted, otherwise the
        // tab persists pointing to a dead agent.
        let mut layout = Layout::new();
        layout.add_tab(Tab::new("opencode".into(), leaf(1, "opencode")));
        layout.add_tab(Tab::new("kiro".into(), leaf(2, "kiro")));
        assert_eq!(layout.tabs.len(), 2);

        remove_agent_pane("opencode", &mut layout);

        assert_eq!(layout.tabs.len(), 1, "single-pane tab should be removed");
        assert_eq!(layout.tabs[0].name, "kiro");
    }

    #[test]
    fn remove_agent_pane_noop_when_agent_missing() {
        let mut layout = Layout::new();
        layout.add_tab(Tab::new("kiro".into(), leaf(1, "kiro")));
        remove_agent_pane("ghost", &mut layout);
        assert_eq!(layout.tabs.len(), 1);
    }

    #[test]
    fn resolve_anchor_prefers_target_pane_over_spawner() {
        // target_pane lives in tab 1; spawner lives in tab 0. Precedence says
        // target_pane wins and we anchor on its exact pane_id.
        let mut layout = Layout::new();
        layout.add_tab(Tab::new("caller-tab".into(), leaf(1, "caller")));
        layout.add_tab(Tab::new("work-tab".into(), leaf(2, "worker")));

        let anchor = resolve_split_anchor(
            LayoutHint::SplitRight,
            Some("worker"),
            Some("caller"),
            &layout,
        )
        .expect("anchor");
        match anchor {
            SplitAnchor::Pane { tab_idx, pane_id } => {
                assert_eq!(tab_idx, 1);
                assert_eq!(pane_id, 2);
            }
            other => panic!("expected SplitAnchor::Pane, got {other:?}"),
        }
    }

    #[test]
    fn resolve_anchor_falls_back_to_spawner_when_target_missing() {
        // target_pane names an agent that isn't displayed anywhere —
        // resolution must fall back to the spawner's tab (focused pane).
        let mut layout = Layout::new();
        layout.add_tab(Tab::new("caller-tab".into(), leaf(1, "caller")));

        let anchor = resolve_split_anchor(
            LayoutHint::SplitBelow,
            Some("ghost"),
            Some("caller"),
            &layout,
        )
        .expect("anchor");
        match anchor {
            SplitAnchor::Focused { tab_idx } => assert_eq!(tab_idx, 0),
            other => panic!("expected SplitAnchor::Focused, got {other:?}"),
        }
    }

    #[test]
    fn resolve_anchor_returns_none_when_nothing_matches() {
        // Nothing to anchor on → caller must fall back to a new tab.
        let mut layout = Layout::new();
        layout.add_tab(Tab::new("other".into(), leaf(1, "other")));

        assert!(resolve_split_anchor(
            LayoutHint::SplitRight,
            Some("ghost"),
            Some("ghost"),
            &layout
        )
        .is_none());
    }

    #[test]
    fn resolve_anchor_tab_hint_always_none() {
        // LayoutHint::Tab means "new tab" — the resolver must ignore even
        // valid target_pane / spawner values.
        let mut layout = Layout::new();
        layout.add_tab(Tab::new("t".into(), leaf(1, "peer")));
        assert!(
            resolve_split_anchor(LayoutHint::Tab, Some("peer"), Some("peer"), &layout).is_none()
        );
    }
}
