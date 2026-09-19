//! Session persistence — save/load TUI pane layout and reconcile against agent registry.
//!
//! `session.json` is a layout-only hint describing how panes were arranged. The
//! AGENT REGISTRY is the source of truth for which agents exist; the registry's
//! source depends on bootstrap mode:
//!
//! - **Owned**: agent registry = `fleet.yaml` instance names. Panes spawn local PTYs.
//! - **Attached** (#895 / #910): agent registry = daemon's in-memory registry
//!   via `runtime::list_agents_with_fallback` (falls back to `.port` glob if
//!   the API is briefly unresponsive). Panes attach to daemon-owned PTYs via
//!   `create_remote_pane`.
//!
//! On restore we reconcile session against the active registry:
//! - Agents in registry but missing from session → new tabs (Rule 3, team-grouped).
//! - Agents in session but missing from registry → silent drop; their splits
//!   collapse to their sibling (Rule 2). The drop is silent because in Attached
//!   mode the daemon's registry naturally drifts as agents are added/removed
//!   between attaches; warning every transient mismatch would be noise.

use crate::fleet;
use crate::layout::{Layout, Pane, PaneNode, SplitDir, Tab};

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Closure type: build a Pane for a SessionPane leaf.
///
/// Branches internally on `SessionPane::fleet_instance_name`:
/// - `Some(name)`: build agent pane (mode-specific — Owned spawns local PTY,
///   Attached attaches via bridge client).
/// - `None`: build shell pane (Owned spawns local shell; Attached returns
///   `None` — no in-app shell support).
///
/// Single-closure design avoids dual-mutable-borrow on shared state
/// (`name_counter` and `registry` in Owned mode).
pub(super) type PaneBuilder<'a> = dyn FnMut(&SessionPane, &mut Layout) -> Option<Pane> + 'a;

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
    /// Focus identity is optional for backwards compatibility with legacy
    /// session files. When present, InstanceRef is authoritative.
    #[serde(default)]
    focus: Option<SessionFocus>,
    /// Zoom is a view preference, not pane data; old sessions default false.
    #[serde(default)]
    zoomed: bool,
}

#[derive(Serialize, Deserialize)]
struct SessionFocus {
    #[serde(default)]
    instance_ref: Option<crate::types::InstanceRef>,
    #[serde(default)]
    fleet_instance_name: Option<String>,
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
pub(super) struct SessionPane {
    /// Fleet instance name (key in fleet.yaml). None for shell panes.
    pub(super) fleet_instance_name: Option<String>,
    /// Exact process identity when the pane was saved. Legacy sessions omit
    /// this field and remain name-only hints until the live registry confirms
    /// the current incarnation.
    #[serde(default)]
    pub(super) instance_ref: Option<crate::types::InstanceRef>,
    /// User-defined display name override.
    pub(super) display_name: Option<String>,
}

/// Save current session layout to disk. Only stores layout geometry, not agent config.
/// Serialize the current layout to the pretty-printed `session.json` form.
fn serialize_session(layout: &Layout) -> Option<String> {
    let session = Session {
        active_tab: layout.active,
        tabs: layout
            .tabs
            .iter()
            .map(|tab| SessionTab {
                name: tab.name.clone(),
                root: save_node(tab.root()),
                focus: save_focus(tab),
                zoomed: tab.zoomed,
            })
            .collect(),
    };
    serde_json::to_string_pretty(&session).ok()
}

const RETIRED_SESSION_FILE: &str = "session.retired.json";

static PENDING_RETIRED_REFS: std::sync::OnceLock<
    std::sync::Mutex<HashMap<PathBuf, HashSet<crate::types::InstanceRef>>>,
> = std::sync::OnceLock::new();

#[derive(Serialize, Deserialize, Default)]
struct RetiredSession {
    #[serde(default)]
    instance_refs: Vec<crate::types::InstanceRef>,
}

fn retired_session_path(home: &Path) -> std::path::PathBuf {
    home.join(RETIRED_SESSION_FILE)
}

fn load_retired_refs(home: &Path) -> HashSet<crate::types::InstanceRef> {
    std::fs::read_to_string(retired_session_path(home))
        .ok()
        .and_then(|content| serde_json::from_str::<RetiredSession>(&content).ok())
        .map(|pending| pending.instance_refs.into_iter().collect())
        .unwrap_or_default()
}

pub(super) fn record_retired_ref(home: &Path, instance_ref: crate::types::InstanceRef) -> bool {
    let path = retired_session_path(home);
    let pending_store = PENDING_RETIRED_REFS.get_or_init(Default::default);
    let mut pending_store = pending_store.lock().expect("pending retired session store");
    let saved = {
        let pending_refs = pending_store.entry(path.clone()).or_default();
        pending_refs.insert(instance_ref);
        persist_retired_refs(home, pending_refs).unwrap_or(false)
    };
    if saved {
        pending_store.remove(&path);
    }
    saved
}

fn clear_retired_refs(home: &Path) {
    let path = retired_session_path(home);
    let _ = std::fs::remove_file(&path);
    if let Some(store) = PENDING_RETIRED_REFS.get() {
        store
            .lock()
            .expect("pending retired session store")
            .remove(&path);
    }
}

fn persist_retired_refs(
    home: &Path,
    pending_refs: &HashSet<crate::types::InstanceRef>,
) -> Result<bool, serde_json::Error> {
    let mut refs = load_retired_refs(home);
    refs.extend(pending_refs.iter().copied());
    let mut instance_refs: Vec<_> = refs.into_iter().collect();
    instance_refs.sort_by_key(|reference| (reference.instance_id.full(), reference.generation));
    let pending = RetiredSession { instance_refs };
    let json = serde_json::to_string_pretty(&pending)?;
    Ok(crate::store::atomic_write(&retired_session_path(home), json.as_bytes()).is_ok())
}

fn retry_pending_retired_refs(home: &Path) -> bool {
    let path = retired_session_path(home);
    let Some(store) = PENDING_RETIRED_REFS.get() else {
        return true;
    };
    let mut store = store.lock().expect("pending retired session store");
    let Some(pending_refs) = store.get(&path) else {
        return true;
    };
    let saved = persist_retired_refs(home, pending_refs).unwrap_or(false);
    if saved {
        store.remove(&path);
    }
    saved
}

fn save_focus(tab: &Tab) -> Option<SessionFocus> {
    let pane = tab.root().find_pane(tab.focus_id)?;
    if pane.instance_ref.is_none() && pane.fleet_instance_name.is_none() {
        return None;
    }
    Some(SessionFocus {
        instance_ref: pane.instance_ref,
        fleet_instance_name: pane.fleet_instance_name.clone(),
    })
}

pub(super) fn save_session(home: &Path, layout: &Layout) -> bool {
    let Some(json) = serialize_session(layout) else {
        return false;
    };
    let path = home.join("session.json");
    let saved = crate::store::atomic_write(&path, json.as_bytes()).is_ok();
    if saved {
        clear_retired_refs(home);
        tracing::info!(path = %path.display(), "session saved");
    } else {
        let _ = retry_pending_retired_refs(home);
    }
    saved
}

/// #1479: write `session.json` only when the serialized layout differs from the
/// last write (`cache`). Called on a throttled main-loop tick so a hard crash
/// (kill -9 / power loss) still preserves the actual on-screen layout — graceful
/// exit's `save_session` only covers clean shutdowns. Change-gated against the
/// exact serialized form (no signature drift), so it never rewrites the file
/// when nothing changed. Returns true if a write happened.
pub(super) fn save_session_if_changed(
    home: &Path,
    layout: &Layout,
    cache: &mut Option<String>,
) -> bool {
    let Some(json) = serialize_session(layout) else {
        return false;
    };
    if cache.as_deref() == Some(json.as_str()) {
        return false;
    }
    let path = home.join("session.json");
    if crate::store::atomic_write(&path, json.as_bytes()).is_ok() {
        *cache = Some(json);
        clear_retired_refs(home);
        true
    } else {
        let _ = retry_pending_retired_refs(home);
        false
    }
}

fn save_node(node: &PaneNode) -> SessionNode {
    match node {
        PaneNode::Leaf(pane) => SessionNode::Leaf(SessionPane {
            fleet_instance_name: pane.fleet_instance_name.clone(),
            instance_ref: pane.instance_ref,
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

/// Restore with reconciliation (Owned mode): fleet.yaml is source of truth for agents,
/// session.json is a layout hint. Returns true if anything was spawned.
#[allow(clippy::too_many_arguments)]
pub(super) fn restore_with_reconciliation(
    home: &Path,
    fleet_path: &Path,
    layout: &mut Layout,
    name_counter: &mut HashMap<String, usize>,
    attach_jobs: &mut Vec<super::pane_factory::AttachJob>,
    cols: u16,
    rows: u16,
) -> bool {
    // Sprint 54 fleet-yaml unification: one-shot migrate legacy
    // teams.json runtime store into fleet.yaml `teams:` block. Runs
    // before fleet.yaml load below so the merged `teams:` section is
    // visible on first read. Idempotent — no-op once
    // teams.json.migrated marker exists.
    if let Err(e) = crate::fleet::migrate_teams_json_to_yaml(home) {
        tracing::warn!(error = %e, "teams.json migration failed at session startup");
    }
    let fleet = fleet::FleetConfig::load(fleet_path).ok();
    // Issue #474: defensive reconcile — prune deployment-store entries whose
    // member instances are no longer in fleet.yaml. Catches the case where
    // a previous session closed the last instance via TUI without going
    // through `deployment teardown`. Cheap (single store load + fleet
    // membership scan), runs once per daemon boot.
    let _ = crate::deployments::reconcile_orphans(home);
    let agent_source: HashSet<String> = fleet
        .as_ref()
        .map(|f| f.instance_names().into_iter().collect())
        .unwrap_or_default();

    // #render-first phase-(b): Owned pane builder now builds the CHEAP
    // placeholder (name dedup + pane id + "Starting" banner, µs) and collects an
    // `AttachJob` for the deferred background spawn — the per-agent fork/exec +
    // skills-install no longer runs synchronously here (that was the ~1s restore
    // freeze). `run_app` schedules the jobs on a bounded pool AFTER the render
    // loop is live. Single closure to keep `name_counter` + `attach_jobs` as the
    // only mutable captures (Layout is passed in).
    let mut pane_builder = |sp: &SessionPane, layout: &mut Layout| -> Option<Pane> {
        match sp.fleet_instance_name.as_deref() {
            Some(name) => {
                let resolved = fleet.as_ref().and_then(|f| f.resolve_instance(name))?;
                let (pane, job) = super::pane_factory::build_deferred_agent_pane(
                    name,
                    &resolved,
                    layout,
                    home,
                    cols,
                    rows,
                    name_counter,
                    crate::backend::SpawnMode::Resume,
                );
                attach_jobs.push(job);
                Some(pane)
            }
            None => {
                let shell = crate::shell_command();
                let (pane, job) = super::pane_factory::build_deferred_direct_pane(
                    layout,
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
                    name_counter,
                );
                attach_jobs.push(job);
                Some(pane)
            }
        }
    };

    let applied = apply_session_layout(home, &agent_source, &mut pane_builder, layout);
    if applied {
        return true;
    }

    // Owned mode no longer exists. The permanent thin client restores agents
    // only from the daemon-backed path below.
    false
}

/// Restore with reconciliation (Attached mode, #895 / #910): daemon's
/// in-memory registry via `runtime::list_agents_with_fallback` is source of
/// truth for agents; session.json is a layout hint. Returns true if any tab
/// was created.
///
/// Mirrors `restore_with_reconciliation` (Owned) but uses `create_remote_pane`
/// for pane construction. Shell panes from a session.json saved in Owned mode
/// get silently dropped (Attached mode doesn't currently support in-app shells).
pub(super) fn restore_with_reconciliation_attached(
    home: &Path,
    fleet_path: &Path,
    run_dir: &Path,
    layout: &mut Layout,
    wakeup_tx: &crossbeam_channel::Sender<usize>,
    cols: u16,
    rows: u16,
) -> bool {
    // #910 PR3 of 4: daemon-registry truth via runtime helper. Falls back
    // to the `.port` glob when API unreachable (preserves the pre-#895
    // Attached restore behavior). `run_dir` kept in scope for the
    // existing tab-builder closures below — only the agent-name source
    // is migrated here.
    let _ = run_dir; // glob path now hidden inside the helper; argument retained for ABI stability
    let agent_source: HashSet<String> = crate::runtime::list_agents_with_fallback(home)
        .into_iter()
        .collect();

    // Attached pane builder: agent → bridge client; shell → None (unsupported).
    let mut pane_builder = |sp: &SessionPane, layout: &mut Layout| -> Option<Pane> {
        let name = sp.fleet_instance_name.as_deref()?;
        match super::pane_factory::create_remote_pane(
            name, home, fleet_path, layout, cols, rows, wakeup_tx,
        ) {
            Ok(pane) if pane.instance_ref.is_some() => Some(pane),
            Ok(_) => {
                tracing::warn!(
                    agent = %name,
                    "remote pane attach returned no instance identity; refusing ambiguous restore"
                );
                None
            }
            Err(e) => {
                tracing::warn!(agent = %name, error = %e, "remote pane attach failed");
                None
            }
        }
    };

    let applied =
        apply_session_layout_with_identity(home, &agent_source, &mut pane_builder, layout, true);

    if applied {
        return true;
    }

    // Rule 1 (Attached fallback): no session.json — build canonical team tabs
    // from daemon registry. The daemon supplies presence/identity only; the
    // shared plan owns team and member ordering.
    if !agent_source.is_empty()
        && place_agents_team_grouped_with_identity(
            home,
            &agent_source.iter().cloned().collect::<Vec<_>>(),
            &mut pane_builder,
            layout,
            true,
        )
    {
        return true;
    }

    false
}

/// Core reconciliation: read session.json, walk tabs, build panes via the
/// caller-provided closures, append unplaced agents (Rule 3), drop missing
/// agents (Rule 2 via `restore_node_reconciled` returning None for leaves
/// whose name is not in `agent_source`).
///
/// Returns true if at least one tab was created.
fn apply_session_layout(
    home: &Path,
    agent_source: &HashSet<String>,
    pane_builder: &mut PaneBuilder<'_>,
    layout: &mut Layout,
) -> bool {
    apply_session_layout_with_identity(home, agent_source, pane_builder, layout, false)
}

/// #3627 test seam: let a sibling test module drive the REAL session reload
/// path (retired-ref drop included) without widening the production surface.
#[cfg(test)]
pub(super) fn apply_session_layout_for_test(
    home: &Path,
    agent_source: &HashSet<String>,
    pane_builder: &mut PaneBuilder<'_>,
    layout: &mut Layout,
) -> bool {
    apply_session_layout(home, agent_source, pane_builder, layout)
}

fn apply_session_layout_with_identity(
    home: &Path,
    agent_source: &HashSet<String>,
    pane_builder: &mut PaneBuilder<'_>,
    layout: &mut Layout,
    require_identity: bool,
) -> bool {
    let session_path = home.join("session.json");
    let session: Option<Session> = std::fs::read_to_string(&session_path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok());

    let Some(session) = session else {
        return false;
    };
    if session.tabs.is_empty() {
        return false;
    }

    let mut placed: HashSet<String> = HashSet::new();
    let retired_refs = load_retired_refs(home);
    let mut successor_panes = Vec::new();

    for tab in &session.tabs {
        if let Some(root_node) = restore_node_reconciled(
            &tab.root,
            agent_source,
            &retired_refs,
            pane_builder,
            layout,
            &mut placed,
            &mut successor_panes,
            require_identity,
        ) {
            let mut restored = Tab::with_root(tab.name.clone(), root_node);
            let (focus_ref, focus_name) = tab
                .focus
                .as_ref()
                .map(|focus| (focus.instance_ref, focus.fleet_instance_name.as_deref()))
                .unwrap_or((None, None));
            restored.restore_view_state(focus_ref, focus_name, tab.zoomed);
            layout.add_tab(restored);
        }
    }

    // A same-name successor was built from the live daemon, but its identity
    // did not match the saved leaf. Keep that current pane and append it as a
    // new view rather than invoking the remote attach path a second time.
    for pane in successor_panes {
        let name = pane.agent_name.to_string();
        if placed.insert(name.clone()) {
            layout.add_tab(Tab::new(name, pane));
        }
    }

    // Rule 3: agents in source but not placed → append as new tabs,
    // grouped by team where teams are defined in fleet.yaml.
    let mut unplaced: Vec<String> = agent_source.difference(&placed).cloned().collect();
    unplaced.sort();
    if require_identity {
        place_agents_team_grouped_with_identity(home, &unplaced, pane_builder, layout, true);
    } else {
        place_agents_team_grouped(home, &unplaced, pane_builder, layout);
    }

    if session.active_tab < layout.tabs.len() {
        layout.active = session.active_tab;
    }

    if !layout.tabs.is_empty() {
        let _ = std::fs::remove_file(&session_path);
        clear_retired_refs(home);
        tracing::info!(
            tabs = layout.tabs.len(),
            "session restored with reconciliation"
        );
        return true;
    }

    false
}

/// #1479: place a flat list of agents into tabs **grouped by team** (members of
/// the same fleet.yaml team share one tab, orchestrator-first; teamless agents
/// each get their own tab). Shared by the session-restore Rule-3 path and the
/// no-session-json `auto_start_fleet` fallback, so a hard restart (session.json
/// absent) groups identically to a live `create_instance`. The input order is
/// only daemon presence; `TeamConfig.members` is canonical for placement.
/// Returns true if any pane was placed.
pub(super) fn place_agents_team_grouped(
    home: &Path,
    agents: &[String],
    pane_builder: &mut PaneBuilder<'_>,
    layout: &mut Layout,
) -> bool {
    place_agents_team_grouped_with_identity(home, agents, pane_builder, layout, false)
}

fn place_agents_team_grouped_with_identity(
    home: &Path,
    agents: &[String],
    pane_builder: &mut PaneBuilder<'_>,
    layout: &mut Layout,
    require_identity: bool,
) -> bool {
    let fleet =
        crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)).unwrap_or_default();
    let plan = crate::team_order::plan_team_order(&fleet, agents.iter().map(String::as_str));

    let mut placed_any = false;
    for group in &plan.groups {
        let mut tab_created = false;
        for name in &group.members {
            let synthetic_sp = SessionPane {
                fleet_instance_name: Some(name.clone()),
                instance_ref: None,
                display_name: None,
            };
            if let Some(pane) = pane_builder(&synthetic_sp, layout)
                .filter(|pane| !require_identity || pane.instance_ref.is_some())
            {
                placed_any = true;
                if !tab_created {
                    layout.add_tab(Tab::new(group.name.clone(), pane));
                    tab_created = true;
                } else if let Some(tab) = layout.active_tab_mut() {
                    if let Some(target_id) = tab.root().pane_ids().last().copied() {
                        tab.split_at_pane(target_id, SplitDir::Horizontal, pane);
                    }
                }
            }
        }
    }

    for name in &plan.ungrouped {
        let synthetic_sp = SessionPane {
            fleet_instance_name: Some(name.clone()),
            instance_ref: None,
            display_name: None,
        };
        if let Some(pane) = pane_builder(&synthetic_sp, layout)
            .filter(|pane| !require_identity || pane.instance_ref.is_some())
        {
            placed_any = true;
            let tab_name = pane.agent_name.to_string();
            layout.add_tab(Tab::new(tab_name, pane));
        }
    }
    placed_any
}

/// Walk a SessionNode tree, building panes via the caller's closures. Returns
/// the corresponding PaneNode tree, or None if every leaf was dropped (Rule 2
/// collapses through to None).
///
/// **C1.4 silent drop**: a Leaf whose `fleet_instance_name` is `Some(name)`
/// where `name` is NOT in `agent_source` returns None silently (no warn log).
/// This is the operator's variant-3 scenario — stale session names that no
/// longer match the daemon registry.
#[allow(clippy::too_many_arguments)]
fn restore_node_reconciled(
    node: &SessionNode,
    agent_source: &HashSet<String>,
    retired_refs: &HashSet<crate::types::InstanceRef>,
    pane_builder: &mut PaneBuilder<'_>,
    layout: &mut Layout,
    placed: &mut HashSet<String>,
    successor_panes: &mut Vec<Pane>,
    require_identity: bool,
) -> Option<PaneNode> {
    match node {
        SessionNode::Leaf(sp) => {
            // C1.4 silent drop: if leaf names a fleet agent NOT in current
            // agent source (registry drift between attaches), return None
            // silently. Sibling's full-space takeover handled at Split level.
            if let Some(name) = sp.fleet_instance_name.as_deref() {
                if !agent_source.contains(name) {
                    return None;
                }
            }
            if sp
                .instance_ref
                .is_some_and(|instance_ref| retired_refs.contains(&instance_ref))
            {
                return None;
            }
            // For agent leaves (Some) and shell leaves (None), the closure
            // dispatches internally and returns None when unsupported.
            let mut pane = pane_builder(sp, layout)?;
            if require_identity && sp.fleet_instance_name.is_some() && pane.instance_ref.is_none() {
                return None;
            }
            // Identity 比對只用 instance_id（跨 boot 穩定）：generation 是
            // daemon-owned process incarnation，跨重啟必變；整 ref 比對會把
            // 新 boot 的同名同 id pane 整批 drop（tab 全滅）。generation 只用
            // 於同 boot stale 判定（retired_refs，見上），此處不參與相等。
            if let Some(saved_ref) = sp.instance_ref {
                let same_instance = pane
                    .instance_ref
                    .is_some_and(|live| live.instance_id == saved_ref.instance_id);
                if !same_instance {
                    pane.display_name = sp.display_name.clone();
                    successor_panes.push(pane);
                    return None;
                }
            }
            if let Some(name) = sp.fleet_instance_name.as_deref() {
                if !placed.insert(name.to_string()) {
                    return None;
                }
            }
            pane.display_name = sp.display_name.clone();
            Some(PaneNode::Leaf(Box::new(pane)))
        }
        SessionNode::Split {
            dir,
            ratio,
            first,
            second,
        } => {
            // Depth-safety invariant (no explicit max-depth guard needed):
            // `apply_session_layout` parses session.json with `serde_json::from_str`,
            // whose default `recursion_limit` (128) rejects a pathologically deep
            // document AT PARSE TIME (→ `None` → fleet.yaml rebuild fallback) before
            // it ever becomes a `SessionNode` tree — so this recursion can never
            // receive a degenerate depth. If a NON-serde tree-feed path is ever
            // added, or `disable_recursion_limit` is set anywhere, add an explicit
            // depth guard here.
            let f = restore_node_reconciled(
                first,
                agent_source,
                retired_refs,
                pane_builder,
                layout,
                placed,
                successor_panes,
                require_identity,
            );
            let s = restore_node_reconciled(
                second,
                agent_source,
                retired_refs,
                pane_builder,
                layout,
                placed,
                successor_panes,
                require_identity,
            );
            match (f, s) {
                (Some(f), Some(s)) => Some(PaneNode::Split {
                    dir: *dir,
                    ratio: *ratio,
                    first: Box::new(f),
                    second: Box::new(s),
                }),
                // Rule 2: one side missing → collapse, sibling takes full space.
                (Some(node), None) | (None, Some(node)) => Some(node),
                (None, None) => None,
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::layout::{Pane, PaneSource};
    use crate::vterm::VTerm;

    fn test_pane(id: usize, agent: &str, fleet_name: Option<&str>) -> Pane {
        Pane {
            agent_name: agent.into(),
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
            fleet_instance_name: fleet_name.map(String::from),
            last_input_at: None,
            pending_notification_count: 0,
            pending_decision_count: 0,
            selection: None,
            source: PaneSource::Local,
            offthread: None,
            _fwd_cancel: None,
        }
    }

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("agend-session-test-{}-{}", tag, std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    /// #1479: `place_agents_team_grouped` puts team members in one tab and
    /// teamless agents in their own — the grouping a hard restart (no
    /// session.json) must reproduce instead of one naive tab per agent.
    #[test]
    fn place_agents_team_grouped_groups_team_members() {
        let home = tmp_home("team-group");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  dev-1: {}\n  dev-2: {}\n  solo: {}\nteams:\n  dev:\n    orchestrator: dev-1\n    members: [dev-1, dev-2]\n",
        )
        .expect("write fleet.yaml");

        let mut layout = Layout::new();
        let mut next_id = 0;
        let mut pane_builder = |sp: &SessionPane, _l: &mut Layout| -> Option<Pane> {
            next_id += 1;
            sp.fleet_instance_name
                .as_deref()
                .map(|n| test_pane(next_id, n, Some(n)))
        };
        let agents = vec!["dev-1".to_string(), "dev-2".to_string(), "solo".to_string()];
        let placed = place_agents_team_grouped(&home, &agents, &mut pane_builder, &mut layout);

        assert!(placed, "agents must be placed");
        assert_eq!(layout.tabs.len(), 2, "team 'dev' (1 tab) + 'solo' (1 tab)");
        let dev_tab = layout
            .tabs
            .iter()
            .find(|t| t.name == "dev")
            .expect("a tab named after the team");
        assert_eq!(
            dev_tab.root().pane_ids().len(),
            2,
            "both dev team members share the 'dev' tab"
        );
        let solo_tab = layout
            .tabs
            .iter()
            .find(|t| t.name == "solo")
            .expect("teamless agent gets its own tab");
        assert_eq!(solo_tab.root().pane_ids().len(), 1);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn red_3630_rule3_uses_canonical_team_and_member_order() {
        let home = tmp_home("red-3630-rule3-order");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  z-lead: {}\n  z-2: {}\n  z-1: {}\n  a-lead: {}\n  a-2: {}\n  solo: {}\nteams:\n  zeta:\n    orchestrator: z-lead\n    members: [z-lead, z-2, z-1]\n  alpha:\n    orchestrator: a-lead\n    members: [a-lead, a-2]\n",
        )
        .expect("write fleet.yaml");

        let mut layout = Layout::new();
        let mut next_id = 0;
        let mut pane_builder = |sp: &SessionPane, _l: &mut Layout| -> Option<Pane> {
            next_id += 1;
            sp.fleet_instance_name
                .as_deref()
                .map(|n| test_pane(next_id, n, Some(n)))
        };
        let agents = vec![
            "z-1".to_string(),
            "solo".to_string(),
            "a-2".to_string(),
            "z-lead".to_string(),
            "a-lead".to_string(),
            "z-2".to_string(),
        ];

        assert!(place_agents_team_grouped(
            &home,
            &agents,
            &mut pane_builder,
            &mut layout
        ));
        assert_eq!(
            layout
                .tabs
                .iter()
                .map(|tab| tab.name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "zeta", "solo"],
            "Rule-3 tabs must use lexical canonical team order"
        );
        assert_eq!(
            layout.tabs[0].root().agent_names(),
            vec!["a-lead", "a-2"],
            "Rule-3 members must preserve TeamConfig.members order"
        );
        assert_eq!(
            layout.tabs[1].root().agent_names(),
            vec!["z-lead", "z-2", "z-1"],
            "Rule-3 members must preserve TeamConfig.members order after lead"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn attached_no_session_fallback_uses_canonical_team_and_member_order() {
        let home = tmp_home("attached-3630-order");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  z-lead: {}\n  z-2: {}\n  z-1: {}\n  solo: {}\nteams:\n  zeta:\n    orchestrator: z-lead\n    members: [z-lead, z-2, z-1]\n",
        )
        .expect("write fleet.yaml");

        let mut layout = Layout::new();
        let mut next_id = 0;
        let mut pane_builder = |sp: &SessionPane, _l: &mut Layout| -> Option<Pane> {
            next_id += 1;
            let mut pane = test_pane(next_id, sp.fleet_instance_name.as_deref()?, Some("zeta"));
            pane.instance_ref = Some(crate::types::InstanceRef::new(
                crate::types::InstanceId::new(),
                next_id as u64,
            ));
            Some(pane)
        };
        let agents = vec!["z-1".to_string(), "solo".to_string(), "z-lead".to_string()];

        assert!(place_agents_team_grouped_with_identity(
            &home,
            &agents,
            &mut pane_builder,
            &mut layout,
            true,
        ));
        assert_eq!(
            layout
                .tabs
                .iter()
                .map(|tab| tab.name.as_str())
                .collect::<Vec<_>>(),
            vec!["zeta", "solo"]
        );
        assert_eq!(
            layout.tabs[0].root().agent_names(),
            vec!["z-lead", "z-1"],
            "attached fallback must use configured order even when daemon presence is shuffled"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1479: throttled save writes only when the layout changed.
    #[test]
    fn save_session_if_changed_writes_only_on_change() {
        let home = tmp_home("save-if-changed");
        let mut layout = Layout::new();
        layout.add_tab(Tab::new(
            "t1".to_string(),
            test_pane(1, "dev", Some("dev-1")),
        ));
        let mut cache: Option<String> = None;

        assert!(
            save_session_if_changed(&home, &layout, &mut cache),
            "first call must write"
        );
        assert!(home.join("session.json").exists());
        assert!(
            !save_session_if_changed(&home, &layout, &mut cache),
            "unchanged layout must NOT rewrite"
        );
        layout.add_tab(Tab::new(
            "t2".to_string(),
            test_pane(2, "rev", Some("rev-1")),
        ));
        assert!(
            save_session_if_changed(&home, &layout, &mut cache),
            "changed layout must write"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn save_session_writes_valid_json() {
        let home = tmp_home("save-json");
        let mut layout = Layout::new();
        layout.add_tab(Tab::new(
            "tab1".to_string(),
            test_pane(1, "dev", Some("dev-abc")),
        ));
        save_session(&home, &layout);

        let path = home.join("session.json");
        assert!(path.exists(), "session.json must be written");
        let content = std::fs::read_to_string(&path).expect("read session.json");
        let parsed: serde_json::Value =
            serde_json::from_str(&content).expect("session.json must be valid JSON");
        assert!(parsed["tabs"].is_array());
        assert_eq!(parsed["tabs"].as_array().expect("tabs").len(), 1);
        assert_eq!(parsed["active_tab"], 0);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn save_node_preserves_fleet_instance_name() {
        let pane = test_pane(1, "dev", Some("dev-x1y2"));
        let node = PaneNode::Leaf(Box::new(pane));
        let saved = save_node(&node);
        match saved {
            SessionNode::Leaf(sp) => {
                assert_eq!(sp.fleet_instance_name, Some("dev-x1y2".to_string()));
            }
            _ => panic!("expected Leaf"),
        }
    }

    #[test]
    fn save_node_preserves_instance_ref() {
        let instance_ref = crate::types::InstanceRef::new(crate::types::InstanceId::new(), 37);
        let mut pane = test_pane(1, "dev", Some("dev-x1y2"));
        pane.instance_ref = Some(instance_ref);
        let saved = save_node(&PaneNode::Leaf(Box::new(pane)));
        match saved {
            SessionNode::Leaf(sp) => assert_eq!(sp.instance_ref, Some(instance_ref)),
            _ => panic!("expected Leaf"),
        }
    }

    #[test]
    fn save_node_preserves_split_structure() {
        let left = test_pane(1, "a", Some("a-1"));
        let right = test_pane(2, "b", Some("b-1"));
        let node = PaneNode::Split {
            dir: SplitDir::Vertical,
            ratio: 0.6,
            first: Box::new(PaneNode::Leaf(Box::new(left))),
            second: Box::new(PaneNode::Leaf(Box::new(right))),
        };
        let saved = save_node(&node);
        match saved {
            SessionNode::Split {
                dir,
                ratio,
                first,
                second,
            } => {
                assert_eq!(dir, SplitDir::Vertical);
                assert!((ratio - 0.6).abs() < 0.01);
                assert!(matches!(*first, SessionNode::Leaf(_)));
                assert!(matches!(*second, SessionNode::Leaf(_)));
            }
            _ => panic!("expected Split"),
        }
    }

    #[test]
    fn save_restore_roundtrip_json_shape() {
        // Save → read JSON → deserialise → verify structural equivalence
        let home = tmp_home("roundtrip");
        let mut layout = Layout::new();
        layout.add_tab(Tab::new(
            "main".to_string(),
            test_pane(1, "dev", Some("dev-rt")),
        ));
        layout.active = 0;
        save_session(&home, &layout);

        let content =
            std::fs::read_to_string(home.join("session.json")).expect("read session.json");
        let session: Session = serde_json::from_str(&content).expect("deserialise session");
        assert_eq!(session.tabs.len(), 1);
        assert_eq!(session.tabs[0].name, "main");
        assert_eq!(session.active_tab, 0);
        match &session.tabs[0].root {
            SessionNode::Leaf(sp) => {
                assert_eq!(sp.fleet_instance_name, Some("dev-rt".to_string()));
            }
            _ => panic!("expected Leaf root"),
        }
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn save_session_empty_layout_writes_empty_tabs() {
        let home = tmp_home("empty");
        let layout = Layout::new();
        save_session(&home, &layout);

        let content =
            std::fs::read_to_string(home.join("session.json")).expect("read session.json");
        let parsed: serde_json::Value = serde_json::from_str(&content).expect("valid JSON");
        assert_eq!(parsed["tabs"].as_array().expect("tabs").len(), 0);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn restore_does_not_reuse_saved_slot_for_same_name_successor() {
        let home = tmp_home("same-name-successor");
        let old_ref = crate::types::InstanceRef::new(crate::types::InstanceId::new(), 1);
        let new_ref = crate::types::InstanceRef::new(crate::types::InstanceId::new(), 2);
        write_session(
            &home,
            vec![(
                "stale-layout".to_string(),
                SessionNode::Leaf(SessionPane {
                    fleet_instance_name: Some("dev".to_string()),
                    instance_ref: Some(old_ref),
                    display_name: None,
                }),
            )],
        );

        let agent_source: HashSet<String> = ["dev".to_string()].into_iter().collect();
        let mut layout = Layout::new();
        let mut id_counter = 0usize;
        let mut pb = |sp: &SessionPane, _layout: &mut Layout| {
            id_counter += 1;
            let mut pane = test_pane(id_counter, sp.fleet_instance_name.as_deref()?, Some("dev"));
            pane.instance_ref = Some(new_ref);
            Some(pane)
        };

        assert!(apply_session_layout(
            &home,
            &agent_source,
            &mut pb,
            &mut layout
        ));
        assert_eq!(
            layout.tabs.len(),
            1,
            "successor must be placed exactly once"
        );
        assert_eq!(
            layout.tabs[0].name, "dev",
            "stale saved tab must not be reused"
        );
        let pane = layout.tabs[0].root().first_pane();
        assert_eq!(pane.instance_ref, Some(new_ref));
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn attached_restore_drops_identityless_legacy_agent_leaf() {
        let home = tmp_home("attached-legacy-no-identity");
        write_session(
            &home,
            vec![(
                "legacy".to_string(),
                SessionNode::Leaf(SessionPane {
                    fleet_instance_name: Some("dev".to_string()),
                    instance_ref: None,
                    display_name: None,
                }),
            )],
        );

        let agent_source: HashSet<String> = ["dev".to_string()].into_iter().collect();
        let mut layout = Layout::new();
        let mut pb = |sp: &SessionPane, _layout: &mut Layout| {
            Some(test_pane(
                1,
                sp.fleet_instance_name.as_deref()?,
                Some("dev"),
            ))
        };

        assert!(!apply_session_layout_with_identity(
            &home,
            &agent_source,
            &mut pb,
            &mut layout,
            true,
        ));
        assert!(layout.tabs.is_empty());
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn save_session_persists_focus_identity_and_zoom_state() {
        let home = tmp_home("focus-zoom");
        let mut first = test_pane(1, "first", Some("first"));
        first.instance_ref = Some(crate::types::InstanceRef::new(
            crate::types::InstanceId::new(),
            11,
        ));
        let mut second = test_pane(2, "second", Some("second"));
        second.instance_ref = Some(crate::types::InstanceRef::new(
            crate::types::InstanceId::new(),
            12,
        ));
        let mut tab = Tab::new("custom".to_string(), first);
        assert!(tab.split_focused(SplitDir::Vertical, second));
        tab.focus_id = 2;
        tab.zoomed = true;
        let mut layout = Layout::new();
        layout.add_tab(tab);

        save_session(&home, &layout);
        let value: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(home.join("session.json")).expect("session.json"),
        )
        .expect("valid session JSON");
        assert_eq!(value["tabs"][0]["zoomed"], true);
        assert!(value["tabs"][0]["focus"].is_object());
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn pending_retired_ref_survives_session_write_failure_and_replay() {
        let home = tmp_home("retired-retry");
        let old_ref = crate::types::InstanceRef::new(crate::types::InstanceId::new(), 7);
        let new_ref = crate::types::InstanceRef::new(crate::types::InstanceId::new(), 8);
        let mut old_pane = test_pane(1, "dev", Some("dev"));
        old_pane.instance_ref = Some(old_ref);
        let mut layout = Layout::new();
        layout.add_tab(Tab::new("dev".to_string(), old_pane));
        save_session(&home, &layout);
        assert!(layout.remove_fleet_instance_views_exact(old_ref));

        let session_path = home.join("session.json");
        crate::store::fail_next_atomic_write_for_test(&session_path);
        assert!(!save_session(&home, &layout));
        assert!(record_retired_ref(&home, old_ref));

        let agent_source: HashSet<String> = ["dev".to_string()].into_iter().collect();
        let mut replayed = Layout::new();
        let mut id_counter = 1usize;
        let mut pb = |sp: &SessionPane, _layout: &mut Layout| {
            id_counter += 1;
            let mut pane = test_pane(id_counter, sp.fleet_instance_name.as_deref()?, Some("dev"));
            pane.instance_ref = Some(new_ref);
            Some(pane)
        };
        assert!(apply_session_layout(
            &home,
            &agent_source,
            &mut pb,
            &mut replayed
        ));
        assert_eq!(replayed.tabs.len(), 1);
        assert_eq!(
            replayed.tabs[0].root().first_pane().instance_ref,
            Some(new_ref)
        );
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn restore_round_trips_focus_identity_and_zoom_state() {
        let home = tmp_home("focus-zoom-restore");
        let first_ref = crate::types::InstanceRef::new(crate::types::InstanceId::new(), 21);
        let second_ref = crate::types::InstanceRef::new(crate::types::InstanceId::new(), 22);
        let session = Session {
            active_tab: 0,
            tabs: vec![SessionTab {
                name: "custom".to_string(),
                root: SessionNode::Split {
                    dir: SplitDir::Vertical,
                    ratio: 0.6,
                    first: Box::new(SessionNode::Leaf(SessionPane {
                        fleet_instance_name: Some("first".to_string()),
                        instance_ref: Some(first_ref),
                        display_name: None,
                    })),
                    second: Box::new(SessionNode::Leaf(SessionPane {
                        fleet_instance_name: Some("second".to_string()),
                        instance_ref: Some(second_ref),
                        display_name: None,
                    })),
                },
                focus: Some(SessionFocus {
                    instance_ref: Some(second_ref),
                    fleet_instance_name: Some("second".to_string()),
                }),
                zoomed: true,
            }],
        };
        let json = serde_json::to_vec_pretty(&session).expect("serialize session fixture");
        crate::store::atomic_write(&home.join("session.json"), &json).expect("write fixture");

        let agent_source: HashSet<String> =
            ["first", "second"].into_iter().map(String::from).collect();
        let mut layout = Layout::new();
        let mut next_id = 0usize;
        let mut pb = |sp: &SessionPane, _layout: &mut Layout| {
            next_id += 1;
            let mut pane = test_pane(next_id, sp.fleet_instance_name.as_deref()?, None);
            pane.instance_ref = sp.instance_ref;
            Some(pane)
        };

        assert!(apply_session_layout(
            &home,
            &agent_source,
            &mut pb,
            &mut layout
        ));
        assert_eq!(layout.tabs.len(), 1);
        assert_eq!(layout.tabs[0].focus_id, 2);
        assert!(layout.tabs[0].zoomed);
        std::fs::remove_dir_all(home).ok();
    }

    // -----------------------------------------------------------------------
    // #895 Option B RED tests. Strict expected assertions per reviewer pushback;
    // pre-fix observed outcome documented per test in PR description.
    // -----------------------------------------------------------------------

    /// Helper: synthetic SessionPane → Pane builder for tests. Always succeeds
    /// when fleet_instance_name is Some (mocking either Owned local spawn OR
    /// Attached bridge attach). Returns None when fleet_instance_name is None
    /// (matches Attached-mode shell-unsupported policy).
    fn synthetic_pane_builder(
        next_id: &mut usize,
    ) -> impl FnMut(&SessionPane, &mut Layout) -> Option<Pane> + '_ {
        move |sp: &SessionPane, _layout: &mut Layout| {
            let name = sp.fleet_instance_name.as_deref()?;
            *next_id += 1;
            Some(test_pane(*next_id, name, Some(name)))
        }
    }

    /// Helper: write a session.json containing the given tabs.
    fn write_session(home: &Path, tabs: Vec<(String, SessionNode)>) {
        let session = Session {
            active_tab: 0,
            tabs: tabs
                .into_iter()
                .map(|(name, root)| SessionTab {
                    name,
                    root,
                    focus: None,
                    zoomed: false,
                })
                .collect(),
        };
        let path = home.join("session.json");
        std::fs::write(&path, serde_json::to_string_pretty(&session).unwrap()).unwrap();
    }

    /// RED-1 (load-bearing): the `if !attached_mode` gate in `app/mod.rs`
    /// MUST NOT wrap `session::save_session`. Structural source-grep test —
    /// directly proves the gate split.
    ///
    /// Pre-fix observed outcome (on `7a0096d`): `save_session` is gated → grep
    /// finds the call inside the `if !attached_mode` block → test FAILS.
    /// Post-fix observed outcome: `save_session` is ungated → call appears
    /// outside the block → test PASSES.
    #[test]
    fn red_1_save_session_is_ungated_in_attached_detach_path() {
        let source = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/app/mod.rs"),
        )
        .expect("read src/app/mod.rs");

        // Find the only `session::save_session(home, layout);` call site.
        // (#14 god-fn split: this moved out of `run_app` into the `app_teardown`
        // helper, which takes `home`/`layout` by reference — so the call lost the
        // `&` sigils. The #895 ungated invariant is unchanged: it must still be
        // the first statement, OUTSIDE any `if !attached_mode` block.)
        let lines: Vec<&str> = source.lines().collect();
        let save_idx = lines
            .iter()
            .position(|l| l.contains("session::save_session(home, layout)"))
            .expect("save_session call must exist in src/app/mod.rs");

        // Walk backwards: the most recent `if ... {` or `} else {` or
        // function-body `{` must NOT be the `if !attached_mode {` block.
        // Specifically: any `if !attached_mode {` between the function start
        // and the save call indicates a gated invocation.
        let mut depth = 0i32;
        for line in lines[..save_idx].iter().rev() {
            for ch in line.chars().rev() {
                match ch {
                    '}' => depth += 1,
                    '{' => {
                        if depth == 0 {
                            // This is the enclosing block opener.
                            assert!(
                                !line.contains("if !attached_mode"),
                                "RED-1 FAIL: `session::save_session` at line {} is wrapped by `if !attached_mode` — the gate must be split per #895 fix",
                                save_idx + 1
                            );
                            return;
                        }
                        depth -= 1;
                    }
                    _ => {}
                }
            }
        }
        panic!("could not locate enclosing block for save_session call");
    }

    /// RED-2 (regression lock): `apply_session_layout` with NO session.json
    /// returns false and does NOT mutate `layout`. Caller falls back to
    /// alphabetical default (Attached) or auto_start_fleet (Owned).
    ///
    /// Pre-fix observed outcome: PASS (existing no-session-file path
    /// already returns false). Post-fix: PASS (regression-lock; no
    /// behavior change).
    #[test]
    fn red_2_apply_session_layout_falls_back_when_session_missing() {
        let home = tmp_home("red-2-missing");
        let agent_source: HashSet<String> =
            ["A".to_string(), "B".to_string()].into_iter().collect();
        let mut layout = Layout::new();
        let mut id_counter = 0usize;
        let mut pb = synthetic_pane_builder(&mut id_counter);

        let started = apply_session_layout(&home, &agent_source, &mut pb, &mut layout);

        assert!(!started, "no session.json must return false");
        assert_eq!(layout.tabs.len(), 0, "layout must be untouched");
        std::fs::remove_dir_all(&home).ok();
    }

    /// RED-3a (load-bearing — user-visible value of B): session.json with a
    /// custom Split tree round-trips through `apply_session_layout` AND
    /// preserves the split topology (NOT alphabetical-collapsed).
    ///
    /// Pre-fix observed outcome (on `7a0096d`): the Attached branch at
    /// `app/mod.rs:238-268` never calls `apply_session_layout` (or even reads
    /// session.json), so custom splits would be DROPPED in any real
    /// Attached-mode restore. This unit test would fail because
    /// `apply_session_layout` is invoked but session.json was never written.
    /// Post-fix: PASS — `apply_session_layout` is wired into Attached and
    /// preserves the split tree.
    #[test]
    fn red_3a_apply_session_layout_round_trips_custom_split_topology() {
        let home = tmp_home("red-3a-split");
        // session.json: single tab "team-alpha" with horizontal split
        // (orch | vertical-split(dev-1, dev-2)).
        let root = SessionNode::Split {
            dir: SplitDir::Horizontal,
            ratio: 0.5,
            first: Box::new(SessionNode::Leaf(SessionPane {
                fleet_instance_name: Some("orch".to_string()),
                instance_ref: None,
                display_name: None,
            })),
            second: Box::new(SessionNode::Split {
                dir: SplitDir::Vertical,
                ratio: 0.5,
                first: Box::new(SessionNode::Leaf(SessionPane {
                    fleet_instance_name: Some("dev-1".to_string()),
                    instance_ref: None,
                    display_name: None,
                })),
                second: Box::new(SessionNode::Leaf(SessionPane {
                    fleet_instance_name: Some("dev-2".to_string()),
                    instance_ref: None,
                    display_name: None,
                })),
            }),
        };
        write_session(&home, vec![("team-alpha".to_string(), root)]);

        let agent_source: HashSet<String> = ["orch", "dev-1", "dev-2"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut layout = Layout::new();
        let mut id_counter = 0usize;
        let mut pb = synthetic_pane_builder(&mut id_counter);

        let started = apply_session_layout(&home, &agent_source, &mut pb, &mut layout);

        assert!(started, "session-with-tabs must return true");
        assert_eq!(layout.tabs.len(), 1, "exactly one tab");
        assert_eq!(layout.tabs[0].name, "team-alpha");
        // Topology check: root must be Split horizontal, second child Split vertical.
        match layout.tabs[0].root() {
            PaneNode::Split {
                dir: outer_dir,
                first: outer_first,
                second: outer_second,
                ..
            } => {
                assert_eq!(*outer_dir, SplitDir::Horizontal);
                assert!(matches!(**outer_first, PaneNode::Leaf(_)));
                match &**outer_second {
                    PaneNode::Split { dir: inner_dir, .. } => {
                        assert_eq!(*inner_dir, SplitDir::Vertical);
                    }
                    _ => panic!("expected inner Split, got Leaf — split topology collapsed"),
                }
            }
            PaneNode::Leaf(_) => panic!("expected Split root, got Leaf — topology lost"),
        }
        std::fs::remove_dir_all(&home).ok();
    }

    /// RED-3b (load-bearing): session.json has 2 tabs (A, B); agent source has
    /// 3 (A, B, C). Result: 3 tabs (A, B restored from session; C appended via
    /// Rule 3 reconciliation).
    ///
    /// Pre-fix observed outcome (on `7a0096d`): Attached branch builds 3 tabs
    /// alphabetically but session-derived layout for A and B is LOST. This
    /// unit test on `apply_session_layout` would currently fail because the
    /// Attached path doesn't call it. Post-fix: PASS — both session-derived
    /// tabs preserved + C appended.
    #[test]
    fn red_3b_apply_session_layout_appends_unplaced_agents_from_source() {
        let home = tmp_home("red-3b-grew");
        write_session(
            &home,
            vec![
                (
                    "A-tab".to_string(),
                    SessionNode::Leaf(SessionPane {
                        fleet_instance_name: Some("A".to_string()),
                        instance_ref: None,
                        display_name: None,
                    }),
                ),
                (
                    "B-tab".to_string(),
                    SessionNode::Leaf(SessionPane {
                        fleet_instance_name: Some("B".to_string()),
                        instance_ref: None,
                        display_name: None,
                    }),
                ),
            ],
        );

        let agent_source: HashSet<String> = ["A", "B", "C"].iter().map(|s| s.to_string()).collect();
        let mut layout = Layout::new();
        let mut id_counter = 0usize;
        let mut pb = synthetic_pane_builder(&mut id_counter);

        let started = apply_session_layout(&home, &agent_source, &mut pb, &mut layout);

        assert!(started);
        assert_eq!(layout.tabs.len(), 3, "expected 3 tabs: A, B, C");
        let tab_names: HashSet<String> = layout.tabs.iter().map(|t| t.name.clone()).collect();
        assert!(
            tab_names.contains("A-tab"),
            "session-derived A-tab preserved"
        );
        assert!(
            tab_names.contains("B-tab"),
            "session-derived B-tab preserved"
        );
        assert!(
            tab_names.contains("C"),
            "C appended via Rule 3 reconciliation (standalone tab named after agent)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// RED-3c (regression lock + operator scenario): session.json has 3 tabs
    /// (A, B, C-stale); agent source has different 3 (A, B, D-new). C-stale
    /// silently dropped; D-new appended via Rule 3.
    ///
    /// Pre-fix observed outcome (on `7a0096d`): Attached branch never reads
    /// session.json, so all 3 alphabetical tabs (A, B, D-new) would appear
    /// but the WARNING "stale references" never fires (no session.json
    /// read). This unit test on `apply_session_layout` would currently fail
    /// because the Attached path doesn't call it.
    /// Post-fix: PASS — exactly 3 tabs (A, B, D-new); C-stale silently dropped.
    #[test]
    fn red_3c_apply_session_layout_silently_drops_stale_agent_leaves() {
        let home = tmp_home("red-3c-stale");
        write_session(
            &home,
            vec![
                (
                    "A-tab".to_string(),
                    SessionNode::Leaf(SessionPane {
                        fleet_instance_name: Some("A".to_string()),
                        instance_ref: None,
                        display_name: None,
                    }),
                ),
                (
                    "B-tab".to_string(),
                    SessionNode::Leaf(SessionPane {
                        fleet_instance_name: Some("B".to_string()),
                        instance_ref: None,
                        display_name: None,
                    }),
                ),
                (
                    "C-stale-tab".to_string(),
                    SessionNode::Leaf(SessionPane {
                        fleet_instance_name: Some("C-stale".to_string()),
                        instance_ref: None,
                        display_name: None,
                    }),
                ),
            ],
        );

        let agent_source: HashSet<String> =
            ["A", "B", "D-new"].iter().map(|s| s.to_string()).collect();
        let mut layout = Layout::new();
        let mut id_counter = 0usize;
        let mut pb = synthetic_pane_builder(&mut id_counter);

        let started = apply_session_layout(&home, &agent_source, &mut pb, &mut layout);

        assert!(started);
        assert_eq!(
            layout.tabs.len(),
            3,
            "exactly 3 tabs (A, B, D-new); C-stale dropped"
        );
        let tab_names: HashSet<String> = layout.tabs.iter().map(|t| t.name.clone()).collect();
        assert!(tab_names.contains("A-tab"), "A preserved from session");
        assert!(tab_names.contains("B-tab"), "B preserved from session");
        assert!(
            !tab_names.contains("C-stale-tab"),
            "C-stale-tab must be silently dropped (agent not in source)"
        );
        assert!(
            tab_names.contains("D-new"),
            "D-new appended via Rule 3 (standalone tab named after agent)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn apply_session_layout_keeps_first_agent_leaf_and_drops_later_duplicate() {
        let home = tmp_home("duplicate-agent-leaf");
        let split = SessionNode::Split {
            dir: SplitDir::Vertical,
            ratio: 0.5,
            first: Box::new(SessionNode::Leaf(SessionPane {
                fleet_instance_name: Some("A".to_string()),
                instance_ref: None,
                display_name: None,
            })),
            second: Box::new(SessionNode::Leaf(SessionPane {
                fleet_instance_name: Some("B".to_string()),
                instance_ref: None,
                display_name: None,
            })),
        };
        let duplicate = SessionNode::Leaf(SessionPane {
            fleet_instance_name: Some("A".to_string()),
            instance_ref: None,
            display_name: None,
        });
        write_session(
            &home,
            vec![
                ("original-split".to_string(), split),
                ("A-duplicate".to_string(), duplicate),
            ],
        );

        let agent_source: HashSet<String> = ["A", "B"].iter().map(|s| s.to_string()).collect();
        let mut layout = Layout::new();
        let mut id_counter = 0usize;
        let mut pb = synthetic_pane_builder(&mut id_counter);

        assert!(apply_session_layout(
            &home,
            &agent_source,
            &mut pb,
            &mut layout
        ));
        assert_eq!(layout.tabs.len(), 1, "later duplicate tab must collapse");
        assert_eq!(layout.tabs[0].name, "original-split");
        assert_eq!(layout.tabs[0].root().pane_count(), 2);
        std::fs::remove_dir_all(&home).ok();
    }

    /// 世代比對致 tab 全滅 RED：saved instance_ref.generation 跨 daemon 重啟
    /// 必變，新 boot pane 身份恆不相等，整批 Leaf 被靜默 drop（tab 結構丟失）。
    /// 修法（decision d-20260915001547078786-0）：identity 比對只用
    /// instance_id（跨 boot 穩定），generation 僅同 boot stale 判定。
    /// 真實 producer `apply_session_layout` 驅動：saved generation=1，
    /// live pane generation=2（同 instance_id）。
    #[test]
    fn restore_keeps_tab_when_only_generation_changed() {
        let home = tmp_home("generation-keep");
        let id = crate::types::InstanceId::new();
        let saved_ref = crate::types::InstanceRef::new(id, 1);
        write_session(
            &home,
            vec![(
                "dev-tab".to_string(),
                SessionNode::Leaf(SessionPane {
                    fleet_instance_name: Some("dev".to_string()),
                    instance_ref: Some(saved_ref),
                    display_name: None,
                }),
            )],
        );
        let agent_source: HashSet<String> = ["dev".to_string()].into_iter().collect();
        let mut layout = Layout::new();
        let mut next_id = 0usize;
        let live_ref = crate::types::InstanceRef::new(id, 2);
        let mut pb = |sp: &SessionPane, _l: &mut Layout| -> Option<Pane> {
            let name = sp.fleet_instance_name.as_deref()?;
            next_id += 1;
            let mut pane = test_pane(next_id, name, Some(name));
            pane.instance_ref = Some(live_ref);
            Some(pane)
        };
        assert!(
            apply_session_layout(&home, &agent_source, &mut pb, &mut layout),
            "restore must succeed, got {} tabs",
            layout.tabs.len()
        );
        assert_eq!(
            layout.tabs.len(),
            1,
            "same instance_id with a new generation must keep its tab, not drop it"
        );
        assert_eq!(layout.tabs[0].name, "dev-tab");
        std::fs::remove_dir_all(&home).ok();
    }

    /// Pinning（非 RED，修正前後皆應通過）：instance_id 不同仍 drop——
    /// 寬鬆比對不能把不同實例誤認，successor 走新 tab 而非佔原位。
    #[test]
    fn restore_drops_leaf_when_instance_id_differs() {
        let home = tmp_home("id-differs-drop");
        let saved_ref = crate::types::InstanceRef::new(crate::types::InstanceId::new(), 1);
        write_session(
            &home,
            vec![(
                "dev-tab".to_string(),
                SessionNode::Leaf(SessionPane {
                    fleet_instance_name: Some("dev".to_string()),
                    instance_ref: Some(saved_ref),
                    display_name: None,
                }),
            )],
        );
        let agent_source: HashSet<String> = ["dev".to_string()].into_iter().collect();
        let mut layout = Layout::new();
        let mut next_id = 0usize;
        // 全新 instance_id（同名不同實體）→ Leaf drop，pane 走 successor 新 tab。
        let live_ref = crate::types::InstanceRef::new(crate::types::InstanceId::new(), 1);
        let mut pb = |sp: &SessionPane, _l: &mut Layout| -> Option<Pane> {
            let name = sp.fleet_instance_name.as_deref()?;
            next_id += 1;
            let mut pane = test_pane(next_id, name, Some(name));
            pane.instance_ref = Some(live_ref);
            Some(pane)
        };
        assert!(
            apply_session_layout(&home, &agent_source, &mut pb, &mut layout),
            "restore must succeed, got {} tabs",
            layout.tabs.len()
        );
        assert_eq!(layout.tabs.len(), 1);
        assert_eq!(
            layout.tabs[0].name, "dev",
            "different instance_id must NOT occupy the saved tab; successor takes a new name-only tab"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #3631 isolated-home smoke: arrange → retire → recreate → restart.
    ///
    /// Exercises the full Team Organizer lifecycle against a real `session.json`
    /// in a throwaway home: panes are scattered, arranged into one team tab,
    /// one member is retired and recreated under a new identity, the session is
    /// saved, and a restart re-consolidates the team deterministically while
    /// preserving the local shell.
    #[test]
    #[allow(clippy::expect_used)]
    fn smoke_arrange_retire_recreate_restart_3631() {
        use crate::layout::organizer::{self, OrganizerScope};
        use crate::types::{InstanceId, InstanceRef};

        let home = tmp_home("organizer-smoke-3631");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  dev-1: {}\n  dev-2: {}\nteams:\n  ops:\n    members: [dev-1, dev-2]\n    orchestrator: dev-1\n",
        )
        .expect("write fleet.yaml");

        let ref1 = InstanceRef::new(InstanceId::new(), 1);
        let ref2 = InstanceRef::new(InstanceId::new(), 2);
        let mut p1 = test_pane(1, "dev-1", Some("dev-1"));
        p1.instance_ref = Some(ref1);
        let mut p2 = test_pane(2, "dev-2", Some("dev-2"));
        p2.instance_ref = Some(ref2);

        // Scattered: each member alone in its own tab, plus a local shell.
        let mut layout = Layout::new();
        layout.add_tab(Tab::new("scatter-1".into(), p1));
        layout.add_tab(Tab::new("scatter-2".into(), p2));
        layout.add_tab(Tab::new("local".into(), test_pane(3, "shell", None)));
        layout.active = 1;

        let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home))
            .expect("fleet loads");
        let plan = organizer::plan(&layout, &fleet, &OrganizerScope::AllTeams);
        organizer::apply(&mut layout, &plan).expect("arrange applies");
        let ops = layout
            .tabs
            .iter()
            .find(|tab| tab.name == "ops")
            .expect("team tab");
        assert_eq!(ops.root().agent_names(), vec!["dev-1", "dev-2"]);
        assert!(save_session(&home, &layout), "commit saves session");

        // Retire dev-2, then recreate it under a fresh identity.
        assert!(layout.remove_fleet_instance_views_exact(ref2));
        assert!(record_retired_ref(&home, ref2));
        let new_ref2 = InstanceRef::new(InstanceId::new(), 20);
        let mut recreated = test_pane(20, "dev-2", Some("dev-2"));
        recreated.instance_ref = Some(new_ref2);
        layout.add_tab(Tab::new("recreated".into(), recreated));
        assert!(save_session(&home, &layout));

        // Restart: reconcile the saved arrangement against the live roster.
        let agent_source: HashSet<String> = ["dev-1".to_string(), "dev-2".to_string()]
            .into_iter()
            .collect();
        let mut restored = Layout::new();
        let mut next_id = 100usize;
        let mut pb = |sp: &SessionPane, _l: &mut Layout| -> Option<Pane> {
            next_id += 1;
            match sp.fleet_instance_name.as_deref() {
                Some("dev-1") => {
                    let mut pane = test_pane(next_id, "dev-1", Some("dev-1"));
                    pane.instance_ref = Some(InstanceRef::new(ref1.instance_id, 31));
                    Some(pane)
                }
                Some("dev-2") => {
                    let mut pane = test_pane(next_id, "dev-2", Some("dev-2"));
                    pane.instance_ref = Some(InstanceRef::new(new_ref2.instance_id, 32));
                    Some(pane)
                }
                Some(_) => None,
                None => Some(test_pane(next_id, "shell", None)),
            }
        };
        assert!(
            apply_session_layout(&home, &agent_source, &mut pb, &mut restored),
            "restart restores the saved layout"
        );
        let restored_panes: usize = restored
            .tabs
            .iter()
            .map(|tab| tab.root().pane_count())
            .sum();
        assert_eq!(
            restored_panes, 3,
            "restart restores dev-1, the recreated dev-2, and the local shell"
        );
        assert!(
            restored
                .tabs
                .iter()
                .any(|tab| tab.root().agent_names().contains(&"shell".to_string())),
            "the local shell survives a restart"
        );

        // Re-running the Organizer re-consolidates the recreated member.
        let plan = organizer::plan(&restored, &fleet, &OrganizerScope::AllTeams);
        organizer::apply(&mut restored, &plan).expect("re-arrange applies");
        let ops = restored
            .tabs
            .iter()
            .find(|tab| tab.name == "ops")
            .expect("team tab after restart");
        assert_eq!(ops.root().agent_names(), vec!["dev-1", "dev-2"]);

        std::fs::remove_dir_all(&home).ok();
    }
}
