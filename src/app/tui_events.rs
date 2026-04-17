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

/// Remove ALL panes for the given agent from every tab. Cleans up empty tabs.
pub(super) fn remove_agent_pane(name: &str, layout: &mut Layout) {
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
        if layout.tabs[tab_idx].root().pane_count() <= 1 {
            layout.close_tab(tab_idx);
        } else {
            layout.tabs[tab_idx].close_pane_by_id(pane_id);
        }
    }
}
