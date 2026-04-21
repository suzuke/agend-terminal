//! Pane construction primitives — wrap agent::spawn_agent + local VTerm + output forwarder.
//!
//! `create_pane` is the core: spawns an agent, subscribes to its output stream, creates
//! a local VTerm, and runs a forwarder thread that pushes output into a crossbeam channel
//! while waking the TUI event loop. `create_pane_from_resolved` adds fleet-aware
//! instruction generation on top. `attach_pane` skips spawn and only subscribes —
//! used when the API server creates the agent out-of-band. `spawn_pane_tab` is the
//! create_pane + add_tab convenience. `resolve_backend` maps a backend name to
//! (command, submit_key). `unique_fleet_name` dedups a base name against
//! fleet.yaml.

use crate::agent::{self, AgentRegistry};
use crate::backend::Backend;
use crate::bridge_client::BridgeClient;
use crate::framing::{self, TAG_DATA};
use crate::layout::{Layout, Pane, Tab};
use crate::vterm::VTerm;

use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Spawn an agent/shell via spawn_agent and add as a new tab.
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_pane_tab(
    layout: &mut Layout,
    registry: &AgentRegistry,
    home: &Path,
    base_name: &str,
    command: &str,
    args: &[String],
    spawn_mode: crate::backend::SpawnMode,
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
        spawn_mode,
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
    spawn_mode: crate::backend::SpawnMode,
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
            spawn_mode,
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
        // Fleet entries reattach to their prior CLI session — the working_dir
        // persists across daemon restarts, so ask the backend to resume.
        crate::backend::SpawnMode::Resume,
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

/// Build a pane backed by a remote daemon-hosted agent.
///
/// Connects a [`BridgeClient`], parks a reader thread that forwards every
/// `TAG_DATA` frame into the pane's output channel, and returns a pane whose
/// `source` is `PaneSource::Remote`. The daemon writes the current vterm
/// dump as the first `TAG_DATA` frame (see `daemon::tui_bridge`), so the
/// local VTerm starts empty and catches up as soon as the pane is drained —
/// no explicit dump processing needed here.
///
/// `backend` is derived from `fleet.yaml` so the `[from:...]` notification
/// heuristic in `Pane::drain_output` behaves the same as for Local panes.
/// A missing fleet entry leaves `backend = None`, disabling only that
/// heuristic — input/resize still work.
pub(super) fn create_remote_pane(
    name: &str,
    home: &Path,
    fleet_path: &Path,
    layout: &mut Layout,
    cols: u16,
    rows: u16,
    wakeup_tx: &crossbeam::channel::Sender<usize>,
) -> Result<Pane> {
    let mut client = BridgeClient::connect(home, name, cols, rows)?;
    let mut reader = client
        .take_reader()
        .ok_or_else(|| anyhow::anyhow!("bridge_client reader already taken"))?;

    let pane_id = layout.next_pane_id();
    let (fwd_tx, pane_rx) = crossbeam::channel::unbounded::<Vec<u8>>();
    let tx = wakeup_tx.clone();
    let thread_name = format!("{name}_remote_fwd");
    std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || loop {
            match framing::read_tagged_frame(&mut reader) {
                Ok((TAG_DATA, data)) => {
                    if fwd_tx.send(data).is_err() {
                        break;
                    }
                    let _ = tx.send(pane_id);
                }
                // Daemon never emits TAG_RESIZE toward clients today. Ignore
                // unknown tags rather than tearing down a healthy session.
                Ok(_) => {}
                Err(_) => break,
            }
        })
        .ok();

    let backend = crate::fleet::FleetConfig::load(fleet_path)
        .ok()
        .and_then(|f| f.resolve_instance(name))
        .and_then(|r| Backend::from_command(&r.backend_command));

    Ok(Pane {
        agent_name: name.to_string(),
        vterm: VTerm::new(cols, rows),
        rx: pane_rx,
        id: pane_id,
        backend,
        working_dir: None,
        display_name: None,
        scroll_offset: 0,
        has_notification: false,
        fleet_instance_name: Some(name.to_string()),
        selection: None,
        source: crate::layout::PaneSource::Remote(Arc::new(Mutex::new(client))),
    })
}

/// Map a backend name to its spawn command and submit key.
pub(super) fn resolve_backend(backend_name: &str) -> (String, String) {
    if let Some(b) = Backend::from_command(backend_name) {
        let p = b.preset();
        (p.command.to_string(), p.submit_key.to_string())
    } else {
        (backend_name.to_string(), "\r".to_string())
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
