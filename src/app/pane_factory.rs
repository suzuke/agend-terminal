//! Pane construction primitives — wrap agent::spawn_agent + local VTerm + output forwarder.
//!
//! `create_pane` is the core: spawns an agent, subscribes to its output stream, creates
//! a local VTerm, and runs a forwarder thread that pushes output into a crossbeam channel
//! while waking the TUI event loop. `create_pane_from_resolved` adds fleet-aware
//! instruction generation on top. `attach_pane` skips spawn and only subscribes —
//! used when the API server creates the agent out-of-band. `spawn_pane_tab` is the
//! create_pane + add_tab convenience. `resolve_backend` maps a backend name to
//! (command, args, submit_key). `unique_fleet_name` dedups a base name against
//! fleet.yaml.

use crate::agent::{self, AgentRegistry};
use crate::backend::Backend;
use crate::layout::{Layout, Pane, Tab};
use crate::vterm::VTerm;

use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;

/// Spawn an agent/shell via spawn_agent and add as a new tab.
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_pane_tab(
    layout: &mut Layout,
    registry: &AgentRegistry,
    home: &Path,
    base_name: &str,
    command: &str,
    args: &[String],
    working_dir: Option<&Path>,
    env: &HashMap<String, String>,
    submit_key: &str,
    cols: u16,
    rows: u16,
    wakeup_tx: &crossbeam::channel::Sender<usize>,
    name_counter: &mut HashMap<String, usize>,
) -> Result<()> {
    let pane = create_pane(
        layout,
        registry,
        home,
        base_name,
        command,
        args,
        working_dir,
        env,
        submit_key,
        cols,
        rows,
        wakeup_tx,
        name_counter,
    )?;
    let tab_name = pane.agent_name.clone();
    layout.add_tab(Tab::new(tab_name, pane));
    Ok(())
}

/// Create a pane backed by spawn_agent.
#[allow(clippy::too_many_arguments)]
pub(super) fn create_pane(
    layout: &mut Layout,
    registry: &AgentRegistry,
    home: &Path,
    base_name: &str,
    command: &str,
    args: &[String],
    working_dir: Option<&Path>,
    env: &HashMap<String, String>,
    submit_key: &str,
    cols: u16,
    rows: u16,
    wakeup_tx: &crossbeam::channel::Sender<usize>,
    name_counter: &mut HashMap<String, usize>,
) -> Result<Pane> {
    // Auto-dedup name
    let count = name_counter.entry(base_name.to_string()).or_insert(0);
    let name = if *count == 0 {
        base_name.to_string()
    } else {
        format!("{base_name}-{count}")
    };
    *count += 1;

    // Resolve working directory
    let work_dir = working_dir
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| home.join("workspace").join(&name));

    // Generate MCP config for agent backends
    if Backend::from_command(command).is_some() {
        crate::instructions::generate(&work_dir, command);
    }

    // Backend-specific flags (Claude's --append-system-prompt-file / --mcp-config /
    // --settings) are now injected centrally by agent::spawn_agent, so callers pass
    // raw args and spawn_agent enriches them from files under work_dir.
    agent::spawn_agent(
        &agent::SpawnConfig {
            name: &name,
            backend_command: command,
            args,
            cols,
            rows,
            env: Some(env),
            working_dir: Some(&work_dir),
            submit_key,
            home: Some(home),
            crash_tx: None,
            shutdown: None,
        },
        registry,
    )?;

    // Subscribe to the agent's output
    let (rx, dump) = {
        let reg = agent::lock_registry(registry);
        let handle = reg
            .get(&name)
            .ok_or_else(|| anyhow::anyhow!("agent not found after spawn"))?;
        agent::subscribe_with_dump(handle)
    };

    // Create local VTerm and feed the screen dump
    let mut vterm = VTerm::new(cols, rows);
    vterm.process(&dump);

    // Forward subscriber output to wakeup channel
    let pane_id = layout.next_pane_id();
    let tx = wakeup_tx.clone();
    let pane_rx = {
        let (fwd_tx, fwd_rx) = crossbeam::channel::unbounded::<Vec<u8>>();
        std::thread::Builder::new()
            .name(format!("{name}_fwd"))
            .spawn(move || {
                while let Ok(data) = rx.recv() {
                    if fwd_tx.send(data).is_err() {
                        break;
                    }
                    let _ = tx.send(pane_id);
                }
            })
            .ok();
        fwd_rx
    };

    let backend = Backend::from_command(command);

    Ok(Pane {
        agent_name: name,
        vterm,
        rx: pane_rx,
        id: pane_id,
        backend,
        working_dir: Some(work_dir),
        display_name: None,
        scroll_offset: 0,
        has_notification: false,
        fleet_instance_name: None,
        selection: None,
        source: crate::layout::PaneSource::Local,
    })
}

/// Attach a pane to an already-running agent (no spawn — subscribe only).
/// Used when the API server creates an agent via MCP and the TUI needs to show it.
pub(super) fn attach_pane(
    name: &str,
    registry: &AgentRegistry,
    cols: u16,
    rows: u16,
    wakeup_tx: &crossbeam::channel::Sender<usize>,
    layout: &mut Layout,
) -> Result<Pane> {
    let (rx, dump, backend_command) = {
        let reg = agent::lock_registry(registry);
        let handle = reg
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("agent '{name}' not found in registry"))?;
        let (rx, dump) = agent::subscribe_with_dump(handle);
        (rx, dump, handle.backend_command.clone())
    };

    let mut vterm = VTerm::new(cols, rows);
    vterm.process(&dump);

    let pane_id = layout.next_pane_id();
    let tx = wakeup_tx.clone();
    let pane_rx = {
        let n = name.to_string();
        let (fwd_tx, fwd_rx) = crossbeam::channel::unbounded::<Vec<u8>>();
        std::thread::Builder::new()
            .name(format!("{n}_fwd"))
            .spawn(move || {
                while let Ok(data) = rx.recv() {
                    if fwd_tx.send(data).is_err() {
                        break;
                    }
                    let _ = tx.send(pane_id);
                }
            })
            .ok();
        fwd_rx
    };

    let backend = Backend::from_command(&backend_command);

    Ok(Pane {
        agent_name: name.to_string(),
        vterm,
        rx: pane_rx,
        id: pane_id,
        backend,
        working_dir: None,
        display_name: None,
        scroll_offset: 0,
        has_notification: false,
        fleet_instance_name: Some(name.to_string()),
        selection: None,
        source: crate::layout::PaneSource::Local,
    })
}

/// Create a pane from a fleet ResolvedInstance (full config: env, args, model, etc.).
#[allow(clippy::too_many_arguments)]
pub(super) fn create_pane_from_resolved(
    fleet_name: &str,
    resolved: &crate::fleet::ResolvedInstance,
    layout: &mut Layout,
    registry: &AgentRegistry,
    home: &Path,
    cols: u16,
    rows: u16,
    wakeup_tx: &crossbeam::channel::Sender<usize>,
    name_counter: &mut HashMap<String, usize>,
) -> Result<Pane> {
    // Build fleet peer list for agent instructions
    let fleet_path = home.join("fleet.yaml");
    let peers: Vec<(String, Option<String>)> = crate::fleet::FleetConfig::load(&fleet_path)
        .map(|f| {
            f.instances
                .iter()
                .map(|(n, c)| (n.clone(), c.role.clone()))
                .collect()
        })
        .unwrap_or_default();
    let ctx = crate::instructions::AgentContext {
        name: fleet_name,
        role: resolved.role.as_deref(),
        fleet_peers: &peers,
    };

    let mut pane = create_pane(
        layout,
        registry,
        home,
        fleet_name,
        &resolved.backend_command,
        &resolved.args,
        resolved.working_directory.as_deref(),
        &resolved.env,
        &resolved.submit_key,
        cols,
        rows,
        wakeup_tx,
        name_counter,
    )?;

    // Overwrite basic instructions with fleet-aware version
    if let Some(ref wd) = pane.working_dir {
        crate::instructions::generate_with_context(wd, &resolved.backend_command, Some(&ctx));
    }
    pane.fleet_instance_name = Some(fleet_name.to_string());
    Ok(pane)
}

/// Resolve a backend command string into (command, args, submit_key).
/// If `fresh` is true, uses fresh_args (no resume) when available.
pub(super) fn resolve_backend(backend_name: &str, fresh: bool) -> (String, Vec<String>, String) {
    if let Some(b) = Backend::from_command(backend_name) {
        let p = b.preset();
        let args = if fresh {
            p.fresh_args.unwrap_or(p.args)
        } else {
            p.args
        };
        (
            p.command.to_string(),
            args.iter().map(|s| s.to_string()).collect(),
            p.submit_key.to_string(),
        )
    } else {
        (backend_name.to_string(), vec![], "\r".to_string())
    }
}

/// Dedup a base name against fleet.yaml. Returns `base` if free, else `base-2`, `base-3`…
pub(super) fn unique_fleet_name(home: &Path, base: &str) -> String {
    let Some(fleet) = crate::fleet::FleetConfig::load(&home.join("fleet.yaml")).ok() else {
        return base.to_string();
    };
    if !fleet.instances.contains_key(base) {
        return base.to_string();
    }
    // Infinite iterator over 2.. always finds a unique name
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|c| !fleet.instances.contains_key(c))
        .expect("infinite iterator")
}
