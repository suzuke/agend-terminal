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

use crate::agent::AgentRegistry;
use crate::fleet;
use crate::layout::{Layout, Pane, PaneNode, SplitDir, Tab};

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;

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
type PaneBuilder<'a> = dyn FnMut(&SessionPane, &mut Layout) -> Option<Pane> + 'a;

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
    let fleet_path = crate::fleet::fleet_yaml_path(home);
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

    // H2: guard — if no panes have fleet_instance_name, the layout wasn't
    // populated from fleet.yaml (crash recovery / fresh start). Skip sync
    // to avoid deleting all fleet entries.
    if active_fleet_names.is_empty() {
        return;
    }

    // Batch-remove fleet entries not in any pane (single atomic write)
    if let Some(ref f) = fleet {
        let to_remove: Vec<String> = f
            .instance_names()
            .into_iter()
            .filter(|name| !active_fleet_names.contains(name))
            .collect();
        if !to_remove.is_empty() {
            tracing::info!(removed = ?to_remove, "sync_fleet_yaml: removing fleet entries not in any pane");
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

/// Restore with reconciliation (Owned mode): fleet.yaml is source of truth for agents,
/// session.json is a layout hint. Returns true if anything was spawned.
#[allow(clippy::too_many_arguments)]
pub(super) fn restore_with_reconciliation(
    home: &Path,
    fleet_path: &Path,
    layout: &mut Layout,
    registry: &AgentRegistry,
    wakeup_tx: &crossbeam_channel::Sender<usize>,
    name_counter: &mut HashMap<String, usize>,
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

    // Owned pane builder: agent → spawn local PTY (Resume mode); shell →
    // spawn local shell (Fresh mode). Single closure to avoid dual-mutable-
    // borrow on `name_counter` + `registry`.
    let mut pane_builder = |sp: &SessionPane, layout: &mut Layout| -> Option<Pane> {
        match sp.fleet_instance_name.as_deref() {
            Some(name) => {
                let resolved = fleet.as_ref().and_then(|f| f.resolve_instance(name))?;
                super::pane_factory::create_pane_from_resolved(
                    name,
                    &resolved,
                    layout,
                    registry,
                    home,
                    cols,
                    rows,
                    wakeup_tx,
                    name_counter,
                    crate::backend::SpawnMode::Resume,
                )
                .ok()
            }
            None => {
                let shell =
                    std::env::var("SHELL").unwrap_or_else(|_| crate::default_shell().to_string());
                super::pane_factory::create_pane(
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
                .ok()
            }
        }
    };

    let applied = apply_session_layout(home, &agent_source, &mut pane_builder, layout);
    if applied {
        return true;
    }

    // No session.json or empty → rule 1: auto-start fleet (Owned-only).
    if !agent_source.is_empty() {
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

    // Rule 4: nothing → caller adds shell tab.
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
            Ok(pane) => Some(pane),
            Err(e) => {
                tracing::warn!(agent = %name, error = %e, "remote pane attach failed");
                None
            }
        }
    };

    let applied = apply_session_layout(home, &agent_source, &mut pane_builder, layout);

    if applied {
        return true;
    }

    // Rule 1 (Attached fallback): no session.json — build tabs alphabetically
    // from daemon registry. Matches the pre-#895 Attached restore behavior
    // for fresh attaches with no prior layout.
    if !agent_source.is_empty() {
        let mut names: Vec<String> = agent_source.iter().cloned().collect();
        names.sort();
        for name in &names {
            let synthetic_sp = SessionPane {
                fleet_instance_name: Some(name.clone()),
                display_name: None,
            };
            if let Some(pane) = pane_builder(&synthetic_sp, layout) {
                let tab_name = pane.agent_name.to_string();
                layout.add_tab(Tab::new(tab_name, pane));
            }
        }
        if !layout.tabs.is_empty() {
            return true;
        }
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

    for tab in &session.tabs {
        if let Some(root_node) =
            restore_node_reconciled(&tab.root, agent_source, pane_builder, layout, &mut placed)
        {
            layout.add_tab(Tab::with_root(tab.name.clone(), root_node));
        }
    }

    // Rule 3: agents in source but not placed → append as new tabs,
    // grouped by team where teams are defined in fleet.yaml.
    let mut unplaced: Vec<String> = agent_source.difference(&placed).cloned().collect();
    unplaced.sort();

    let teams = crate::teams::list_all(home);
    let mut team_members: HashMap<String, Vec<String>> = HashMap::new();
    let mut standalone = Vec::new();
    for name in &unplaced {
        if let Some(team) = teams.iter().find(|t| t.members.contains(name)) {
            team_members
                .entry(team.name.clone())
                .or_default()
                .push(name.clone());
        } else {
            standalone.push(name.clone());
        }
    }

    for (team_name, members) in &team_members {
        let team = teams.iter().find(|t| t.name == *team_name);
        let orchestrator = team.and_then(|t| t.orchestrator.as_deref());
        let mut sorted = members.clone();
        sorted.sort_by(|a, b| {
            let a_is_orch = orchestrator == Some(a.as_str());
            let b_is_orch = orchestrator == Some(b.as_str());
            b_is_orch.cmp(&a_is_orch).then(a.cmp(b))
        });

        let mut tab_created = false;
        for name in &sorted {
            let synthetic_sp = SessionPane {
                fleet_instance_name: Some(name.clone()),
                display_name: None,
            };
            if let Some(pane) = pane_builder(&synthetic_sp, layout) {
                if !tab_created {
                    layout.add_tab(Tab::new(team_name.clone(), pane));
                    tab_created = true;
                } else if let Some(tab) = layout.active_tab_mut() {
                    tab.split_focused(SplitDir::Horizontal, pane);
                }
            }
        }
    }

    for name in &standalone {
        let synthetic_sp = SessionPane {
            fleet_instance_name: Some(name.clone()),
            display_name: None,
        };
        if let Some(pane) = pane_builder(&synthetic_sp, layout) {
            let tab_name = pane.agent_name.to_string();
            layout.add_tab(Tab::new(tab_name, pane));
        }
    }

    if session.active_tab < layout.tabs.len() {
        layout.active = session.active_tab;
    }

    if !layout.tabs.is_empty() {
        let _ = std::fs::remove_file(&session_path);
        tracing::info!(
            tabs = layout.tabs.len(),
            "session restored with reconciliation"
        );
        return true;
    }

    false
}

/// Walk a SessionNode tree, building panes via the caller's closures. Returns
/// the corresponding PaneNode tree, or None if every leaf was dropped (Rule 2
/// collapses through to None).
///
/// **C1.4 silent drop**: a Leaf whose `fleet_instance_name` is `Some(name)`
/// where `name` is NOT in `agent_source` returns None silently (no warn log).
/// This is the operator's variant-3 scenario — stale session names that no
/// longer match the daemon registry.
fn restore_node_reconciled(
    node: &SessionNode,
    agent_source: &HashSet<String>,
    pane_builder: &mut PaneBuilder<'_>,
    layout: &mut Layout,
    placed: &mut HashSet<String>,
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
                placed.insert(name.to_string());
            }
            // For agent leaves (Some) and shell leaves (None), the closure
            // dispatches internally and returns None when unsupported.
            let mut pane = pane_builder(sp, layout)?;
            pane.display_name = sp.display_name.clone();
            Some(PaneNode::Leaf(Box::new(pane)))
        }
        SessionNode::Split {
            dir,
            ratio,
            first,
            second,
        } => {
            let f = restore_node_reconciled(first, agent_source, pane_builder, layout, placed);
            let s = restore_node_reconciled(second, agent_source, pane_builder, layout, placed);
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
            vterm: VTerm::new(10, 10),
            rx: crossbeam_channel::bounded(1).1,
            id,
            backend: None,
            working_dir: None,
            display_name: None,
            scroll_offset: 0,
            has_notification: false,
            fleet_instance_name: fleet_name.map(String::from),
            last_input_at: None,
            pending_notification_count: 0,
            selection: None,
            source: PaneSource::Local,
        }
    }

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("agend-session-test-{}-{}", tag, std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        dir
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
                .map(|(name, root)| SessionTab { name, root })
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

        // Find the only `session::save_session(&home, &layout);` call site.
        let lines: Vec<&str> = source.lines().collect();
        let save_idx = lines
            .iter()
            .position(|l| l.contains("session::save_session(&home, &layout)"))
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
                display_name: None,
            })),
            second: Box::new(SessionNode::Split {
                dir: SplitDir::Vertical,
                ratio: 0.5,
                first: Box::new(SessionNode::Leaf(SessionPane {
                    fleet_instance_name: Some("dev-1".to_string()),
                    display_name: None,
                })),
                second: Box::new(SessionNode::Leaf(SessionPane {
                    fleet_instance_name: Some("dev-2".to_string()),
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
                        display_name: None,
                    }),
                ),
                (
                    "B-tab".to_string(),
                    SessionNode::Leaf(SessionPane {
                        fleet_instance_name: Some("B".to_string()),
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
                        display_name: None,
                    }),
                ),
                (
                    "B-tab".to_string(),
                    SessionNode::Leaf(SessionPane {
                        fleet_instance_name: Some("B".to_string()),
                        display_name: None,
                    }),
                ),
                (
                    "C-stale-tab".to_string(),
                    SessionNode::Leaf(SessionPane {
                        fleet_instance_name: Some("C-stale".to_string()),
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
}
