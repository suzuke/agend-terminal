//! New-tab / split menu construction and spawn dispatch.
//!
//! Kept out of `app::mod` so quick-spawn additions do not grow the already
//! grandfathered TUI event-loop module.

use super::{pane_factory, tui_spawn, MenuItem, MenuItemKind};
use crate::agent::{self, AgentRegistry};
use crate::backend::Backend;
use crate::layout::{Layout, Pane};
use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;

const FUGU_BACKEND_MENU_LABEL: &str = "[backend] Codex(Sakana)";

/// Build menu items for new-tab selection.
/// Fleet instances already running in the registry are excluded.
pub(super) fn build_menu_items(fleet_path: &Path, registry: &AgentRegistry) -> Vec<MenuItem> {
    let mut items = Vec::new();

    // Collect already-running agent names
    let running: Vec<String> = {
        let reg = agent::lock_registry(registry);
        reg.values().map(|h| h.name.to_string()).collect()
    };

    if let Ok(fleet) = crate::fleet::FleetConfig::load(fleet_path) {
        let mut names = fleet.instance_names();
        names.sort();
        for name in names {
            // Skip if exact name or deduped variant (name-1, name-2...) is running
            let already_open = running
                .iter()
                .any(|r| r == &name || r.starts_with(&format!("{name}-")));
            if already_open {
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
            // #2764 R8: the admission guard is OWNED here so it spans the
            // whole pane-create transaction below; a refusal (mid-delete /
            // independent create) ABORTS — never continue creating a pane for
            // an instance whose registration was refused.
            let _create_admission = match tui_spawn::add_instance_with_topic(
                home,
                &inst_name,
                &crate::fleet::InstanceYamlEntry {
                    backend: Some(backend.name().to_string()),
                    ..Default::default()
                },
            ) {
                Ok((_, adm)) => adm,
                Err(e) => {
                    tracing::warn!(error = %e, "TUI spawn aborted — instance not created");
                    return Err(anyhow::anyhow!("TUI spawn aborted: {e}"));
                }
            };
            // Resolve from fleet to get defaults merged
            let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)).ok();
            let pane_result = if let Some(resolved) =
                fleet.as_ref().and_then(|f| f.resolve_instance(&inst_name))
            {
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
            };
            // #2764 R9: hold-through-success-or-rollback — a pane failure
            // undoes the fleet/topic mutations while the admission guard is
            // still held, so a failed TUI create leaves ZERO residue.
            if pane_result.is_err() {
                tui_spawn::rollback_created_instance(home, &inst_name);
            }
            pane_result
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
            // #2764 R9: Fugu owns the admission like the Backend arm — a
            // refusal ABORTS (no pane), and a pane failure rolls the
            // fleet/topic mutations back while the guard is still held.
            let _create_admission = match tui_spawn::add_instance_with_topic(
                home,
                &inst_name,
                &crate::fleet::InstanceYamlEntry {
                    backend: Some("codex".to_string()),
                    args: Some(vec!["-p".to_string(), "fugu".to_string()]),
                    env: (!env.is_empty()).then_some(env),
                    ..Default::default()
                },
            ) {
                Ok((_, adm)) => adm,
                Err(e) => {
                    tracing::warn!(error = %e, "fugu spawn aborted — instance not created");
                    return Err(anyhow::anyhow!("fugu spawn aborted: {e}"));
                }
            };
            let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)).ok();
            let pane_result = if let Some(resolved) =
                fleet.as_ref().and_then(|f| f.resolve_instance(&inst_name))
            {
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
                Err(anyhow::anyhow!(
                    "failed to resolve fugu instance after creation"
                ))
            };
            if pane_result.is_err() {
                tui_spawn::rollback_created_instance(home, &inst_name);
            }
            pane_result
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

    #[test]
    fn fugu_menu_label_is_backend_style() {
        assert_eq!(FUGU_BACKEND_MENU_LABEL, "[backend] Codex(Sakana)");
    }
}
