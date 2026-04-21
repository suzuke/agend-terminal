//! Session persistence — save/load TUI pane layout and reconcile against fleet.yaml.
//!
//! fleet.yaml is the source of truth for which agents exist; `session.json` is a
//! layout-only hint describing how panes were arranged. On restore we reconcile:
//! agents in fleet but missing from session become new tabs; agents in session
//! but missing from fleet are dropped (their splits collapse to their sibling).

use crate::agent::AgentRegistry;
use crate::fleet;
use crate::layout::{Layout, PaneNode, SplitDir, Tab};

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Saved session layout for persistence across restarts.
#[derive(Serialize, Deserialize)]
struct Session {
    tabs: Vec<SessionTab>,
    active_tab: usize,
}

#[derive(Serialize, Deserialize)]
struct SessionTab {
    name: String,
    root: SessionNode,
}

#[derive(Serialize, Deserialize)]
enum SessionNode {
    Leaf(SessionPane),
    Split {
        dir: SplitDir,
        #[serde(default = "default_ratio")]
        ratio: f32,
        first: Box<SessionNode>,
        second: Box<SessionNode>,
    },
}

fn default_ratio() -> f32 {
    0.5
}

/// Layout-only pane info. Agent config comes from fleet.yaml on restore.
#[derive(Serialize, Deserialize)]
struct SessionPane {
    /// Fleet instance name (key in fleet.yaml). None for shell panes.
    fleet_instance_name: Option<String>,
    /// User-defined display name override.
    display_name: Option<String>,
}

/// Sync fleet.yaml to match current pane state on detach.
/// Removes fleet entries not present in any pane; adds panes with backend but missing from fleet.
pub(super) fn sync_fleet_yaml(home: &Path, layout: &Layout) {
    let fleet_path = home.join("fleet.yaml");
    let fleet = fleet::FleetConfig::load(&fleet_path).ok();

    // Collect all fleet_instance_names currently in panes
    let mut active_fleet_names: HashSet<String> = HashSet::new();
    for tab in &layout.tabs {
        for id in tab.root().pane_ids() {
            if let Some(pane) = tab.root().find_pane(id) {
                if let Some(ref name) = pane.fleet_instance_name {
                    active_fleet_names.insert(name.clone());
                }
            }
        }
    }

    // Batch-remove fleet entries not in any pane (single atomic write)
    if let Some(ref f) = fleet {
        let to_remove: Vec<String> = f
            .instance_names()
            .into_iter()
            .filter(|name| !active_fleet_names.contains(name))
            .collect();
        if !to_remove.is_empty() {
            let _ = fleet::remove_instances_from_yaml(home, &to_remove);
        }
    }
}

/// Save current session layout to disk. Only stores layout geometry, not agent config.
pub(super) fn save_session(home: &Path, layout: &Layout) {
    let tabs: Vec<SessionTab> = layout
        .tabs
        .iter()
        .map(|tab| SessionTab {
            name: tab.name.clone(),
            root: save_node(tab.root()),
        })
        .collect();

    let session = Session {
        active_tab: layout.active,
        tabs,
    };

    let path = home.join("session.json");
    if let Ok(json) = serde_json::to_string_pretty(&session) {
        let _ = std::fs::write(&path, json);
        tracing::info!(path = %path.display(), tabs = session.tabs.len(), "session saved");
    }
}

fn save_node(node: &PaneNode) -> SessionNode {
    match node {
        PaneNode::Leaf(pane) => SessionNode::Leaf(SessionPane {
            fleet_instance_name: pane.fleet_instance_name.clone(),
            display_name: pane.display_name.clone(),
        }),
        PaneNode::Split {
            dir,
            ratio,
            first,
            second,
        } => SessionNode::Split {
            dir: *dir,
            ratio: *ratio,
            first: Box::new(save_node(first)),
            second: Box::new(save_node(second)),
        },
    }
}

/// Restore with reconciliation: fleet.yaml is source of truth for agents,
/// session.json is a layout hint. Returns true if anything was spawned.
#[allow(clippy::too_many_arguments)]
pub(super) fn restore_with_reconciliation(
    home: &Path,
    fleet_path: &Path,
    layout: &mut Layout,
    registry: &AgentRegistry,
    wakeup_tx: &crossbeam::channel::Sender<usize>,
    name_counter: &mut HashMap<String, usize>,
    cols: u16,
    rows: u16,
) -> bool {
    let fleet = fleet::FleetConfig::load(fleet_path).ok();
    let fleet_names: HashSet<String> = fleet
        .as_ref()
        .map(|f| f.instance_names().into_iter().collect())
        .unwrap_or_default();

    // Try loading session.json as layout hint
    let session_path = home.join("session.json");
    let session: Option<Session> = std::fs::read_to_string(&session_path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok());

    if let Some(session) = session {
        let _ = std::fs::remove_file(&session_path);
        if !session.tabs.is_empty() {
            let mut placed: HashSet<String> = HashSet::new();

            for tab in &session.tabs {
                if let Some(root_node) = restore_node_reconciled(
                    &tab.root,
                    fleet.as_ref(),
                    home,
                    layout,
                    registry,
                    wakeup_tx,
                    name_counter,
                    cols,
                    rows,
                    &mut placed,
                ) {
                    layout.add_tab(Tab::with_root(tab.name.clone(), root_node));
                }
            }

            // Rule 3: fleet agents not in session → append as new tabs
            let mut unplaced: Vec<String> = fleet_names.difference(&placed).cloned().collect();
            unplaced.sort();
            for name in &unplaced {
                if let Some(resolved) = fleet.as_ref().and_then(|f| f.resolve_instance(name)) {
                    if let Ok(pane) = super::pane_factory::create_pane_from_resolved(
                        name,
                        &resolved,
                        layout,
                        registry,
                        home,
                        cols,
                        rows,
                        wakeup_tx,
                        name_counter,
                    ) {
                        let tab_name = pane.agent_name.clone();
                        layout.add_tab(Tab::new(tab_name, pane));
                    }
                }
            }

            if session.active_tab < layout.tabs.len() {
                layout.active = session.active_tab;
            }

            if !layout.tabs.is_empty() {
                tracing::info!(
                    tabs = layout.tabs.len(),
                    "session restored with reconciliation"
                );
                return true;
            }
        }
    }

    // No session.json or empty → rule 1: auto-start fleet
    if !fleet_names.is_empty() {
        return super::api_server::auto_start_fleet(
            fleet_path,
            layout,
            registry,
            home,
            cols,
            rows,
            wakeup_tx,
            name_counter,
        );
    }

    // Rule 4: nothing → caller adds shell tab
    false
}

#[allow(clippy::too_many_arguments)]
fn restore_node_reconciled(
    node: &SessionNode,
    fleet: Option<&fleet::FleetConfig>,
    home: &Path,
    layout: &mut Layout,
    registry: &AgentRegistry,
    wakeup_tx: &crossbeam::channel::Sender<usize>,
    name_counter: &mut HashMap<String, usize>,
    cols: u16,
    rows: u16,
    placed: &mut HashSet<String>,
) -> Option<PaneNode> {
    match node {
        SessionNode::Leaf(sp) => {
            match &sp.fleet_instance_name {
                Some(fleet_name) => {
                    let resolved = fleet?.resolve_instance(fleet_name)?;
                    placed.insert(fleet_name.clone());
                    let mut pane = super::pane_factory::create_pane_from_resolved(
                        fleet_name,
                        &resolved,
                        layout,
                        registry,
                        home,
                        cols,
                        rows,
                        wakeup_tx,
                        name_counter,
                    )
                    .ok()?;
                    pane.display_name = sp.display_name.clone();
                    Some(PaneNode::Leaf(Box::new(pane)))
                }
                None => {
                    // Shell pane — recreate fresh
                    let shell = std::env::var("SHELL")
                        .unwrap_or_else(|_| crate::default_shell().to_string());
                    let mut pane = super::pane_factory::create_pane(
                        layout,
                        registry,
                        home,
                        "shell",
                        &shell,
                        &[],
                        crate::backend::SpawnMode::Fresh,
                        None,
                        &HashMap::new(),
                        "\r",
                        cols,
                        rows,
                        wakeup_tx,
                        name_counter,
                    )
                    .ok()?;
                    pane.display_name = sp.display_name.clone();
                    Some(PaneNode::Leaf(Box::new(pane)))
                }
            }
        }
        SessionNode::Split {
            dir,
            ratio,
            first,
            second,
        } => {
            let f = restore_node_reconciled(
                first,
                fleet,
                home,
                layout,
                registry,
                wakeup_tx,
                name_counter,
                cols,
                rows,
                placed,
            );
            let s = restore_node_reconciled(
                second,
                fleet,
                home,
                layout,
                registry,
                wakeup_tx,
                name_counter,
                cols,
                rows,
                placed,
            );
            match (f, s) {
                (Some(f), Some(s)) => Some(PaneNode::Split {
                    dir: *dir,
                    ratio: *ratio,
                    first: Box::new(f),
                    second: Box::new(s),
                }),
                // Rule 2: one side missing → collapse, sibling takes full space
                (Some(node), None) | (None, Some(node)) => Some(node),
                (None, None) => None,
            }
        }
    }
}
