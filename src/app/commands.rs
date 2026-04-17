//! Command-palette command execution (`:spawn`, `:kill`, `:restart`, `:layout`,
//! `:send`, `:broadcast`, `:status`).
//!
//! Input is a whitespace-split command line. Returns `true` iff the command
//! mutated the layout in a way that requires a resize pass.

use crate::agent;
use crate::agent::AgentRegistry;
use crate::layout::{Layout, SplitDir, Tab};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Bundle of mutable references the command handler needs to affect the
/// running TUI state. Constructed by the caller for each invocation.
pub(super) struct CommandCtx<'a> {
    pub layout: &'a mut Layout,
    pub registry: &'a AgentRegistry,
    pub home: &'a Path,
    pub wakeup_tx: &'a crossbeam::channel::Sender<usize>,
    pub name_counter: &'a mut HashMap<String, usize>,
    pub telegram_state: &'a Option<Arc<Mutex<crate::telegram::TelegramState>>>,
}

/// Execute a command palette command. Returns true if layout changed (needs resize).
pub(super) fn execute(cmd: &str, ctx: &mut CommandCtx<'_>) -> bool {
    let parts: Vec<&str> = cmd.trim().splitn(3, ' ').collect();
    if parts.is_empty() {
        return false;
    }
    match parts[0] {
        "spawn" | "vsplit" | "hsplit" => {
            let base_name = parts.get(1).unwrap_or(&"agent");
            let backend_name = parts.get(2).unwrap_or(&"claude");
            let fleet_path = ctx.home.join("fleet.yaml");
            let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
            let pc = cols.saturating_sub(2);
            let pr = rows.saturating_sub(4);

            // unique_fleet_name guarantees inst_name is not yet in fleet.yaml
            let inst_name = super::pane_factory::unique_fleet_name(ctx.home, base_name);
            if let Err(e) = crate::fleet::add_instance_to_yaml(
                ctx.home,
                &inst_name,
                &crate::fleet::InstanceYamlEntry {
                    backend: Some(backend_name.to_string()),
                    working_directory: None,
                    role: None,
                },
            ) {
                tracing::warn!(name = %inst_name, error = %e, "failed to write fleet.yaml");
            }
            let fleet = crate::fleet::FleetConfig::load(&fleet_path).ok();
            let pane_result = if let Some(resolved) =
                fleet.as_ref().and_then(|f| f.resolve_instance(&inst_name))
            {
                super::pane_factory::create_pane_from_resolved(
                    &inst_name,
                    &resolved,
                    ctx.layout,
                    ctx.registry,
                    ctx.home,
                    pc,
                    pr,
                    ctx.wakeup_tx,
                    ctx.name_counter,
                )
            } else {
                let (command, args, submit_key) =
                    super::pane_factory::resolve_backend(backend_name, false);
                super::pane_factory::create_pane(
                    ctx.layout,
                    ctx.registry,
                    ctx.home,
                    &inst_name,
                    &command,
                    &args,
                    None,
                    &HashMap::new(),
                    &submit_key,
                    pc,
                    pr,
                    ctx.wakeup_tx,
                    ctx.name_counter,
                )
            };
            match pane_result {
                Ok(pane) => {
                    super::telegram_hooks::maybe_create_telegram_topic(
                        ctx.telegram_state,
                        ctx.registry,
                        ctx.home,
                        &pane,
                    );
                    match parts[0] {
                        "vsplit" => {
                            if let Some(tab) = ctx.layout.active_tab_mut() {
                                tab.split_focused(SplitDir::Vertical, pane);
                            }
                        }
                        "hsplit" => {
                            if let Some(tab) = ctx.layout.active_tab_mut() {
                                tab.split_focused(SplitDir::Horizontal, pane);
                            }
                        }
                        _ => {
                            let tab_name = pane.agent_name.clone();
                            ctx.layout.add_tab(Tab::new(tab_name, pane));
                        }
                    }
                    return true;
                }
                Err(e) => {
                    tracing::error!(name = %inst_name, backend = *backend_name, error = %e, "spawn failed")
                }
            }
        }
        "kill" => {
            if let Some(name) = parts.get(1) {
                if let Some(fleet_name) = lookup_fleet_name(ctx.layout, name) {
                    super::telegram_hooks::maybe_delete_telegram_topic(
                        ctx.telegram_state,
                        ctx.home,
                        &fleet_name,
                    );
                    let _ = crate::fleet::remove_instance_from_yaml(ctx.home, &fleet_name);
                }
                super::kill_agent(ctx.registry, name);
                super::tui_events::remove_agent_pane(name, ctx.layout);
                return true;
            }
        }
        "restart" => {
            let target_name = parts.get(1).map(|s| s.to_string()).or_else(|| {
                ctx.layout
                    .active_tab()
                    .and_then(|t| t.focused_pane())
                    .map(|p| p.agent_name.clone())
            });
            if let Some(name) = target_name {
                // Single pass: find pane info, fleet name, and location
                #[allow(clippy::type_complexity)]
                let mut pane_info: Option<(
                    String,
                    Option<PathBuf>,
                    Option<String>,
                    Option<String>,
                )> = None;
                let mut pane_loc: Option<(usize, usize)> = None;
                'outer: for (ti, tab) in ctx.layout.tabs.iter().enumerate() {
                    for id in tab.root().pane_ids() {
                        if let Some(p) = tab.root().find_pane(id) {
                            if p.agent_name == name {
                                let cmd = match &p.backend {
                                    Some(b) => b.preset().command.to_string(),
                                    None => {
                                        tracing::warn!(agent = name, "cannot restart shell pane");
                                        break 'outer;
                                    }
                                };
                                pane_info = Some((
                                    cmd,
                                    p.working_dir.clone(),
                                    p.display_name.clone(),
                                    p.fleet_instance_name.clone(),
                                ));
                                pane_loc = Some((ti, id));
                                break 'outer;
                            }
                        }
                    }
                }

                if let Some((backend_cmd, work_dir, display_name, fleet_name)) = pane_info {
                    super::kill_agent(ctx.registry, &name);
                    let _ =
                        std::fs::remove_file(ctx.home.join("sessions").join(format!("{name}.sid")));

                    let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
                    let pc = cols.saturating_sub(2);
                    let pr = rows.saturating_sub(4);
                    ctx.name_counter.remove(&name);

                    let pane_result = if let Some(ref fname) = fleet_name {
                        // Fleet agent — resolve from fleet.yaml (full config)
                        let fleet_path = ctx.home.join("fleet.yaml");
                        let fleet = crate::fleet::FleetConfig::load(&fleet_path).ok();
                        if let Some(resolved) =
                            fleet.as_ref().and_then(|f| f.resolve_instance(fname))
                        {
                            super::pane_factory::create_pane_from_resolved(
                                fname,
                                &resolved,
                                ctx.layout,
                                ctx.registry,
                                ctx.home,
                                pc,
                                pr,
                                ctx.wakeup_tx,
                                ctx.name_counter,
                            )
                        } else {
                            let (command, args, submit_key) =
                                super::pane_factory::resolve_backend(&backend_cmd, true);
                            super::pane_factory::create_pane(
                                ctx.layout,
                                ctx.registry,
                                ctx.home,
                                &name,
                                &command,
                                &args,
                                work_dir.as_deref(),
                                &HashMap::new(),
                                &submit_key,
                                pc,
                                pr,
                                ctx.wakeup_tx,
                                ctx.name_counter,
                            )
                        }
                    } else {
                        // Non-fleet pane — use backend preset directly
                        let (command, args, submit_key) =
                            super::pane_factory::resolve_backend(&backend_cmd, true);
                        super::pane_factory::create_pane(
                            ctx.layout,
                            ctx.registry,
                            ctx.home,
                            &name,
                            &command,
                            &args,
                            work_dir.as_deref(),
                            &HashMap::new(),
                            &submit_key,
                            pc,
                            pr,
                            ctx.wakeup_tx,
                            ctx.name_counter,
                        )
                    };
                    if let Ok(mut new_pane) = pane_result {
                        // Swap only vterm + rx into the existing pane slot
                        if let Some((ti, pid)) = pane_loc {
                            if let Some(pane) = ctx.layout.tabs[ti].root_mut().find_pane_mut(pid) {
                                std::mem::swap(&mut pane.vterm, &mut new_pane.vterm);
                                std::mem::swap(&mut pane.rx, &mut new_pane.rx);
                                pane.agent_name = new_pane.agent_name;
                                pane.display_name = display_name;
                                pane.scroll_offset = 0;
                                pane.has_notification = false;
                                return true;
                            }
                        }
                        // Fallback: add as new tab
                        let tab_name = new_pane.agent_name.clone();
                        ctx.layout.add_tab(Tab::new(tab_name, new_pane));
                        return true;
                    }
                }
            }
        }
        "layout" => {
            let Some(tab) = ctx.layout.active_tab_mut() else {
                return false;
            };
            let Some(name) = parts.get(1) else {
                tab.next_layout();
                return true;
            };
            let Some(preset) = crate::layout::LayoutPreset::from_name(name) else {
                tracing::warn!(
                    name = *name,
                    valid = crate::layout::LayoutPreset::all_names(),
                    "unknown layout preset"
                );
                return false;
            };
            tab.apply_layout(preset);
            return true;
        }
        "send" => {
            if parts.len() >= 3
                && !agent::send_to_registry(ctx.registry, "user", parts[1], parts[2])
            {
                tracing::warn!(target = parts[1], "send: agent not found in registry");
            }
        }
        "broadcast" => {
            if let Some(msg) = parts.get(1) {
                agent::broadcast_registry(ctx.registry, "user", msg, None);
            }
        }
        "status" => {
            let reg = agent::lock_registry(ctx.registry);
            for (name, handle) in reg.iter() {
                if let Ok(core) = handle.core.lock() {
                    tracing::info!(agent = name, state = ?core.state.get_state(), "status");
                }
            }
        }
        _ => {
            tracing::warn!(cmd = cmd, "unknown command");
        }
    }
    false
}

/// Look up the fleet_instance_name for an agent by scanning the layout.
fn lookup_fleet_name(layout: &Layout, agent_name: &str) -> Option<String> {
    for tab in &layout.tabs {
        for id in tab.root().pane_ids() {
            if let Some(pane) = tab.root().find_pane(id) {
                if pane.agent_name == agent_name {
                    return pane.fleet_instance_name.clone();
                }
            }
        }
    }
    None
}
