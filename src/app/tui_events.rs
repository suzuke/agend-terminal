//! TUI-side handling of events from the API server.
//!
//! The API server (hit via MCP or HTTP) signals agent/team lifecycle changes on
//! a bounded crossbeam channel. The TUI event loop receives these and mutates
//! the layout accordingly — auto-creating or removing tabs/panes.

use crate::agent::{self, AgentRegistry};
use crate::layout::{Layout, SplitDir, Tab};

/// Events sent from the API server to the TUI event loop when agents or teams
/// are created/deleted via MCP tools. The TUI reacts by auto-creating or
/// removing tabs/panes.
#[derive(Debug, Clone)]
pub(crate) enum TuiEvent {
    InstanceCreated {
        name: String,
        layout: LayoutHint,
        spawner: Option<String>,
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
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) enum LayoutHint {
    #[default]
    Tab,
    SplitRight,
    SplitBelow,
}

impl LayoutHint {
    /// Parse a layout-hint string into the enum.
    /// Named `parse_hint` (not `from_str`) to avoid shadowing `std::str::FromStr::from_str`.
    pub(crate) fn parse_hint(s: &str) -> Self {
        match s {
            "split-right" => Self::SplitRight,
            "split-below" => Self::SplitBelow,
            _ => Self::Tab,
        }
    }
}

pub(crate) type TuiEventSender = crossbeam::channel::Sender<TuiEvent>;

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
        } => {
            handle_instance_created(&name, hint, spawner.as_deref(), layout, registry, wakeup_tx);
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
    }
}

fn handle_instance_created(
    name: &str,
    hint: LayoutHint,
    spawner: Option<&str>,
    layout: &mut Layout,
    registry: &AgentRegistry,
    wakeup_tx: &crossbeam::channel::Sender<usize>,
) {
    tracing::info!(agent = name, hint = ?hint, spawner = ?spawner, tabs_before = layout.tabs.len(), "handle_instance_created begin");
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
    let split_target_idx = match hint {
        LayoutHint::SplitRight | LayoutHint::SplitBelow => spawner.and_then(|spawner_name| {
            layout
                .tabs
                .iter()
                .position(|tab| tab.root().has_agent(spawner_name))
        }),
        LayoutHint::Tab => None,
    };

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

    match (hint, split_target_idx) {
        (LayoutHint::SplitRight | LayoutHint::SplitBelow, Some(idx)) => {
            let dir = match hint {
                LayoutHint::SplitRight => SplitDir::Horizontal,
                _ => SplitDir::Vertical,
            };
            // split_focused consumes the pane. If the rare case of no focused
            // pane in the target tab occurs, the pane is lost — acceptable
            // since we've already validated the tab has the spawner agent.
            layout.tabs[idx].split_focused(dir, pane);
        }
        _ => {
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

    // Strip each incoming member from any other tab first — a pane can only
    // live in one place, and split_focused would otherwise leave the old pane
    // subscribed. Skip the team tab itself to preserve an already-grouped pane.
    for member in &to_attach {
        let already_in_team = layout
            .tabs
            .iter()
            .any(|tab| tab.name == team_name && tab.root().has_agent(member));
        if already_in_team {
            continue;
        }
        // Remove from every other tab (there should be at most one, but loop
        // to stay safe if dedup ever slips).
        loop {
            let target = layout.tabs.iter().enumerate().find_map(|(tab_idx, tab)| {
                if tab.name == team_name {
                    None
                } else {
                    tab.root()
                        .find_pane_id_by_agent(member)
                        .map(|pane_id| (tab_idx, pane_id))
                }
            });
            let (tab_idx, pane_id) = match target {
                Some(t) => t,
                None => break,
            };
            if layout.tabs[tab_idx].root().pane_count() <= 1 {
                layout.close_tab(tab_idx);
            } else {
                layout.tabs[tab_idx].close_pane_by_id(pane_id);
            }
        }
    }

    // Attach each new member to the team tab. If the team tab doesn't exist
    // yet (e.g. add into a freshly-created empty team), build it from the
    // first member.
    let mut team_tab_idx = layout.tabs.iter().position(|tab| tab.name == team_name);
    let mut iter = to_attach.iter();
    if team_tab_idx.is_none() {
        // Find first member we can actually attach. `by_ref` keeps the
        // remaining members available for the split loop below.
        for member in iter.by_ref() {
            match super::pane_factory::attach_pane(
                member, registry, cols, pane_rows, wakeup_tx, layout,
            ) {
                Ok(pane) => {
                    let tab = Tab::new(team_name.to_string(), pane);
                    layout.add_tab(tab);
                    team_tab_idx = Some(layout.tabs.len() - 1);
                    break;
                }
                Err(e) => {
                    tracing::warn!(team = team_name, member = member, error = %e, "handle_team_members_changed: attach_pane failed");
                }
            }
        }
    }
    let tab_idx = match team_tab_idx {
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

    tracing::info!(
        team = team_name,
        tabs_after = layout.tabs.len(),
        "handle_team_members_changed end"
    );
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
}
