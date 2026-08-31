//! New-tab / split menu construction and spawn dispatch.
//!
//! Kept out of `app::mod` so quick-spawn additions do not grow the already
//! grandfathered TUI event-loop module.

use super::{pane_factory, tui_spawn, MenuItem, MenuItemKind};
use crate::agent::{self, AgentRegistry};
use crate::backend::Backend;
use crate::layout::{Layout, Pane};
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::Path;

const FUGU_BACKEND_MENU_LABEL: &str = "[backend] Codex(Sakana)";

/// Build menu items for new-tab selection.
/// Fleet instances already running in the registry are excluded.
pub(super) fn build_menu_items(
    fleet_path: &Path,
    registry: &AgentRegistry,
    layout: &Layout,
) -> Vec<MenuItem> {
    let mut items = Vec::new();

    // Collect already-running agent names
    let running: Vec<String> = {
        let reg = agent::lock_registry(registry);
        reg.values().map(|h| h.name.to_string()).collect()
    };
    let attached = attached_fleet_names(layout);

    if let Ok(fleet) = crate::fleet::FleetConfig::load(fleet_path) {
        let mut names = fleet.instance_names();
        names.sort();
        for name in names {
            // Skip if exact name or deduped variant (name-1, name-2...) is running
            let already_open = running
                .iter()
                .any(|r| r == &name || r.starts_with(&format!("{name}-")));
            if already_open || attached.contains(&name) {
                continue;
            }
            let label = if let Some(resolved) = fleet.resolve_instance(&name) {
                format!("{name}  ({backend})", backend = resolved.backend_command)
            } else {
                name.clone()
            };
            items.push(MenuItem {
                label: format!("[fleet] {label}"),
                kind: MenuItemKind::FleetInstance(name),
            });
        }
    }

    for backend in Backend::all() {
        if backend.is_installed() {
            items.push(MenuItem {
                label: format!("[backend] {}", backend.name()),
                kind: MenuItemKind::Backend(backend.clone()),
            });
        }
    }

    // #2441: one-click Fugu via the codex harness. Present it as a backend
    // variant, not a separate top-level menu class, so it sits with codex.
    if crate::provider_detect::detect_default().status
        == crate::provider_detect::FuguStatus::Available
    {
        items.push(MenuItem {
            label: FUGU_BACKEND_MENU_LABEL.to_string(),
            kind: MenuItemKind::Fugu,
        });
    }

    items.push(MenuItem {
        label: "[shell] bash".to_string(),
        kind: MenuItemKind::Shell,
    });

    items
}

/// Collect exact fleet instance names attached to panes across every tab.
/// Local shells have no fleet name and therefore never suppress a menu entry.
pub(super) fn attached_fleet_names(layout: &Layout) -> HashSet<String> {
    layout
        .tabs
        .iter()
        .flat_map(|tab| {
            tab.root()
                .pane_ids()
                .into_iter()
                .filter_map(|id| tab.root().find_pane(id))
                .filter_map(|pane| pane.fleet_instance_name.clone())
        })
        .collect()
}

/// Create a pane from a menu item selection (shared by NewTab and Split handlers).
#[allow(clippy::too_many_arguments)]
pub(super) fn pane_from_menu_item(
    item: MenuItem,
    fleet_path: &Path,
    layout: &mut Layout,
    registry: &AgentRegistry,
    home: &Path,
    cols: u16,
    rows: u16,
    wakeup_tx: &crossbeam_channel::Sender<usize>,
    name_counter: &mut HashMap<String, usize>,
) -> Result<Pane> {
    match item.kind {
        MenuItemKind::Shell => {
            let shell = crate::shell_command();
            pane_factory::create_pane(
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
                pane_factory::SpawnIdentity::UnmanagedLocalShell,
            )
        }
        MenuItemKind::Backend(backend) => {
            let preset = backend.preset();
            let inst_name = pane_factory::unique_fleet_name(home, preset.command);
            // #966: TUI Backend menu (ctrl+b c) previously called
            // `add_instance_to_yaml` directly, bypassing the topic-creation
            // side effect that `handle_spawn` does. Now routes through
            // `tui_spawn::add_instance_with_topic` so the channel topic is
            // created + topic_id persisted to topics.json at TUI-spawn time.
            if let Err(e) = tui_spawn::add_instance_with_topic(
                home,
                &inst_name,
                &crate::fleet::InstanceYamlEntry {
                    backend: Some(backend.name().to_string()),
                    ..Default::default()
                },
            ) {
                tracing::warn!(error = %e, "failed to write fleet.yaml");
            }
            // Resolve from fleet to get defaults merged
            let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)).ok();
            if let Some(resolved) = fleet.as_ref().and_then(|f| f.resolve_instance(&inst_name)) {
                pane_factory::create_pane_from_resolved(
                    &inst_name,
                    &resolved,
                    layout,
                    registry,
                    home,
                    cols,
                    rows,
                    wakeup_tx,
                    name_counter,
                    crate::backend::SpawnMode::Fresh,
                )
            } else {
                // Preset args are added by spawn_agent; no need to compose here.
                pane_factory::create_pane(
                    layout,
                    registry,
                    home,
                    &inst_name,
                    preset.command,
                    &[],
                    crate::backend::SpawnMode::Fresh,
                    None,
                    &HashMap::new(),
                    preset.submit_key,
                    cols,
                    rows,
                    wakeup_tx,
                    name_counter,
                    pane_factory::SpawnIdentity::Managed,
                )
            }
        }
        MenuItemKind::Fugu => {
            // Provision (idempotent) the Fugu Codex profile (`fugu.config.toml`)
            // in the shared codex home, then create the pane sharing that home and
            // selecting the profile via `codex -p fugu` (passed as per-instance
            // args). Sharing ~/.codex reuses its provider block + auth.json — no
            // isolated CODEX_HOME, no auth snapshot to drift. CODEX_HOME is set
            // ONLY when the profile lives outside the default ~/.codex.
            let detection = crate::provider_detect::detect_default();
            let codex_home = crate::provider_detect::ensure_fugu_profile(&detection)
                .map_err(|e| anyhow::anyhow!("failed to provision Fugu profile: {e}"))?;
            let inst_name = pane_factory::unique_fleet_name(home, "fugu");
            let mut env = HashMap::new();
            if crate::provider_detect::default_codex_home().as_ref() != Some(&codex_home) {
                env.insert("CODEX_HOME".to_string(), codex_home.display().to_string());
            }
            if let Err(e) = tui_spawn::add_instance_with_topic(
                home,
                &inst_name,
                &crate::fleet::InstanceYamlEntry {
                    backend: Some("codex".to_string()),
                    args: Some(vec!["-p".to_string(), "fugu".to_string()]),
                    env: (!env.is_empty()).then_some(env),
                    ..Default::default()
                },
            ) {
                tracing::warn!(error = %e, "failed to write fleet.yaml for fugu");
            }
            let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)).ok();
            if let Some(resolved) = fleet.as_ref().and_then(|f| f.resolve_instance(&inst_name)) {
                pane_factory::create_pane_from_resolved(
                    &inst_name,
                    &resolved,
                    layout,
                    registry,
                    home,
                    cols,
                    rows,
                    wakeup_tx,
                    name_counter,
                    crate::backend::SpawnMode::Fresh,
                )
            } else {
                anyhow::bail!("failed to resolve fugu instance after creation")
            }
        }
        MenuItemKind::FleetInstance(inst_name) => {
            let fleet = crate::fleet::FleetConfig::load(fleet_path)?;
            let resolved = fleet
                .resolve_instance(&inst_name)
                .ok_or_else(|| anyhow::anyhow!("fleet instance '{inst_name}' not found"))?;
            pane_factory::create_pane_from_resolved(
                &inst_name,
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Tab;

    #[test]
    fn fugu_menu_label_is_backend_style() {
        assert_eq!(FUGU_BACKEND_MENU_LABEL, "[backend] Codex(Sakana)");
    }

    #[test]
    fn attached_fleet_names_scan_every_tab_and_ignore_local_shells() {
        let mut layout = Layout::new();
        let local = Pane {
            agent_name: "shell".into(),
            instance_id: crate::types::InstanceId::default(),
            vterm: crate::vterm::VTerm::new(10, 10),
            rx: crossbeam_channel::bounded(1).1,
            id: 1,
            backend: None,
            working_dir: None,
            display_name: None,
            scroll_offset: 0,
            has_notification: false,
            fleet_instance_name: None,
            last_input_at: None,
            pending_notification_count: 0,
            pending_decision_count: 0,
            selection: None,
            source: crate::layout::PaneSource::Local,
            offthread: None,
            _fwd_cancel: None,
        };
        layout.add_tab(Tab::new("local".into(), local));
        let attached = Pane {
            agent_name: "label".into(),
            instance_id: crate::types::InstanceId::default(),
            vterm: crate::vterm::VTerm::new(10, 10),
            rx: crossbeam_channel::bounded(1).1,
            id: 2,
            backend: None,
            working_dir: None,
            display_name: None,
            scroll_offset: 0,
            has_notification: false,
            fleet_instance_name: Some("attached".into()),
            last_input_at: None,
            pending_notification_count: 0,
            pending_decision_count: 0,
            selection: None,
            source: crate::layout::PaneSource::Local,
            offthread: None,
            _fwd_cancel: None,
        };
        layout.add_tab(Tab::new("attached".into(), attached));

        let names = attached_fleet_names(&layout);
        assert!(names.contains("attached"));
        assert!(!names.contains("shell"));
    }

    #[test]
    fn build_menu_items_filters_attached_fleet_in_non_active_tab() {
        let home = std::env::temp_dir().join(format!("agend-menu-filter-{}", std::process::id()));
        std::fs::remove_dir_all(&home).ok();
        std::fs::create_dir_all(&home).expect("create menu fixture home");
        let fleet_path = home.join("fleet.yaml");
        std::fs::write(
            &fleet_path,
            "instances:\n  attached:\n    backend: claude\n  available:\n    backend: claude\n",
        )
        .expect("write fleet fixture");

        let mut layout = Layout::new();
        layout.add_tab(Tab::new("local".into(), menu_test_pane(1, None)));
        layout.add_tab(Tab::new(
            "attached".into(),
            menu_test_pane(2, Some("attached")),
        ));
        layout.active = 0;
        let registry: AgentRegistry = std::sync::Arc::new(parking_lot::Mutex::new(HashMap::new()));

        let items = build_menu_items(&fleet_path, &registry, &layout);
        assert!(!items.iter().any(|item| {
            matches!(&item.kind, MenuItemKind::FleetInstance(name) if name == "attached")
        }));
        assert!(items.iter().any(|item| {
            matches!(&item.kind, MenuItemKind::FleetInstance(name) if name == "available")
        }));

        std::fs::remove_dir_all(home).ok();
    }

    fn menu_test_pane(id: usize, fleet_instance_name: Option<&str>) -> Pane {
        Pane {
            agent_name: "menu-test".into(),
            instance_id: crate::types::InstanceId::default(),
            vterm: crate::vterm::VTerm::new(10, 10),
            rx: crossbeam_channel::bounded(1).1,
            id,
            backend: None,
            working_dir: None,
            display_name: None,
            scroll_offset: 0,
            has_notification: false,
            fleet_instance_name: fleet_instance_name.map(str::to_string),
            last_input_at: None,
            pending_notification_count: 0,
            pending_decision_count: 0,
            selection: None,
            source: crate::layout::PaneSource::Local,
            offthread: None,
            _fwd_cancel: None,
        }
    }
}
