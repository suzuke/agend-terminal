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
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

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
    wakeup_tx: &crossbeam_channel::Sender<usize>,
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
    wakeup_tx: &crossbeam_channel::Sender<usize>,
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
    let spawn_mode = spawn_mode.downgraded_for(command, Some(&work_dir));
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
        let (fwd_tx, fwd_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        // fire-and-forget: forwarder exits when fwd_tx.send() fails (pane
        // dropped → fwd_rx dropped → send returns Err) or rx.recv() fails
        // (agent removed → broadcast sender dropped). H1: lifecycle is
        // correct — pane drop triggers forwarder exit via channel close.
        std::thread::Builder::new()
            .name(format!("{name}_fwd"))
            .spawn(move || {
                while let Ok(data) = rx.recv() {
                    if fwd_tx.send(data).is_err() {
                        break; // H1: pane closed, fwd_rx dropped
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
        last_input_at: None,
        pending_notification_count: 0,
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
    wakeup_tx: &crossbeam_channel::Sender<usize>,
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
        let (fwd_tx, fwd_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        // fire-and-forget: same lifecycle as create_pane forwarder (H1).
        std::thread::Builder::new()
            .name(format!("{n}_fwd"))
            .spawn(move || {
                while let Ok(data) = rx.recv() {
                    if fwd_tx.send(data).is_err() {
                        break; // H1: pane closed, fwd_rx dropped
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
        last_input_at: None,
        pending_notification_count: 0,
        selection: None,
        source: crate::layout::PaneSource::Local,
    })
}

/// Create a pane from a fleet ResolvedInstance (full config: env, args, model, etc.).
///
/// `spawn_mode` reflects caller intent — system rehydrate (daemon restart, session
/// restore, crash respawn) passes `Resume` to reattach the CLI's prior conversation
/// in that cwd; user-initiated new creation (backend picker, `:spawn`) passes
/// `Fresh` so the new instance does not inherit a leftover session. Callers that
/// explicitly reattach an existing fleet instance (fleet-instance picker, `:restart`)
/// also pass `Resume`.
#[allow(clippy::too_many_arguments)]
pub(super) fn create_pane_from_resolved(
    fleet_name: &str,
    resolved: &crate::fleet::ResolvedInstance,
    layout: &mut Layout,
    registry: &AgentRegistry,
    home: &Path,
    cols: u16,
    rows: u16,
    wakeup_tx: &crossbeam_channel::Sender<usize>,
    name_counter: &mut HashMap<String, usize>,
    spawn_mode: crate::backend::SpawnMode,
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
    // Team context drives the two-section peer rendering in agend.md
    // (team members vs other fleet agents). Owned here, borrowed into
    // AgentContext.
    let team_record = crate::teams::find_team_for(home, fleet_name);
    let team_ctx = team_record
        .as_ref()
        .map(|t| crate::instructions::TeamContext {
            name: t.name.as_str(),
            orchestrator: t.orchestrator.as_deref(),
            members: t.members.as_slice(),
        });
    let ctx = crate::instructions::AgentContext {
        name: fleet_name,
        role: resolved.role.as_deref(),
        fleet_peers: &peers,
        team: team_ctx.as_ref(),
    };

    let mut pane = create_pane(
        layout,
        registry,
        home,
        fleet_name,
        &resolved.backend_command,
        &resolved.args,
        spawn_mode,
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
    wakeup_tx: &crossbeam_channel::Sender<usize>,
) -> Result<Pane> {
    let mut client = BridgeClient::connect(home, name, cols, rows)?;
    let mut reader = client
        .take_reader()
        .ok_or_else(|| anyhow::anyhow!("bridge_client reader already taken"))?;

    let pane_id = layout.next_pane_id();
    let (fwd_tx, pane_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
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
        last_input_at: None,
        pending_notification_count: 0,
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

/// Mint a unique fleet instance name like `base-a3f2c1`.
///
/// Suffix is 6 hex chars derived from the current subsecond nanos XORed with a
/// process-local counter, so two spawns in the same nanosecond still differ.
/// Collision probability against fleet.yaml ∪ `workspace/` ∪ `inbox/` is
/// checked and retried up to 100 times before falling back to `-N`.
///
/// Always adding a suffix (vs. returning bare `base` when free) is deliberate:
/// each spawn gets a fresh workspace directory, so closing "codex" and
/// opening another never silently reuses `workspace/codex/` with its leftover
/// `.codex/`, `AGENTS.md`, and git state.
pub(super) fn unique_fleet_name(home: &Path, base: &str) -> String {
    unique_fleet_name_with(home, base, std::iter::from_fn(|| Some(short_id())))
}

/// Testable core of [`unique_fleet_name`]: takes the suffix iterator as input
/// so tests can inject a deterministic sequence and actually exercise the
/// collision-skip path (a random `short_id()` lands in a pre-seeded collision
/// bucket with probability ~10⁻⁷, so the tests would otherwise be vacuous).
fn unique_fleet_name_with(home: &Path, base: &str, mut ids: impl Iterator<Item = u32>) -> String {
    let fleet = crate::fleet::FleetConfig::load(&home.join("fleet.yaml")).ok();
    let taken = |name: &str| -> bool {
        if fleet
            .as_ref()
            .is_some_and(|f| f.instances.contains_key(name))
        {
            return true;
        }
        if home.join("workspace").join(name).exists() {
            return true;
        }
        if crate::inbox::inbox_path_resolved(home, name).exists() {
            return true;
        }
        false
    };
    for _ in 0..100 {
        let id = ids.next().unwrap_or(0);
        let candidate = format!("{base}-{id:06x}");
        if !taken(&candidate) {
            return candidate;
        }
    }
    // Extremely unlikely fallback when 100 suffixes all collide
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|c| !taken(c))
        .expect("infinite iterator")
}

/// 24-bit id derived from the current subsecond nanos XORed with a process-
/// local counter. Same-nanosecond callers still differ via the counter.
fn short_id() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    (nanos ^ seq) & 0xFF_FFFF
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("agend_unique_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("create tmp home");
        p
    }

    #[test]
    fn unique_name_always_suffixed() {
        let home = tmp_home("always");
        let name = unique_fleet_name(&home, "codex");
        assert!(name.starts_with("codex-"), "name was {name}");
        let suffix = &name["codex-".len()..];
        assert_eq!(suffix.len(), 6, "expected 6-hex suffix, got {name}");
        assert!(
            suffix.chars().all(|c| c.is_ascii_hexdigit()),
            "non-hex suffix in {name}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn unique_name_successive_calls_differ() {
        let home = tmp_home("diff");
        let a = unique_fleet_name(&home, "codex");
        // Realize `a` as a workspace so the next call must not collide with it.
        std::fs::create_dir_all(home.join("workspace").join(&a)).expect("create a");
        let b = unique_fleet_name(&home, "codex");
        assert_ne!(a, b);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn unique_name_skips_workspace_collision() {
        let home = tmp_home("ws");
        std::fs::create_dir_all(home.join("workspace/codex-000001")).expect("seed 1");
        std::fs::create_dir_all(home.join("workspace/codex-000002")).expect("seed 2");
        // Feed id sequence that hits both collisions then succeeds on 3
        let name = unique_fleet_name_with(&home, "codex", [0x1u32, 0x2, 0x3].into_iter());
        assert_eq!(name, "codex-000003");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn unique_name_skips_inbox_collision() {
        let home = tmp_home("ib");
        std::fs::create_dir_all(home.join("inbox")).expect("create inbox");
        std::fs::write(home.join("inbox/codex-000001.jsonl"), "").expect("seed 1");
        std::fs::write(home.join("inbox/codex-000002.jsonl"), "").expect("seed 2");
        let name = unique_fleet_name_with(&home, "codex", [0x1u32, 0x2, 0x3].into_iter());
        assert_eq!(name, "codex-000003");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn unique_name_falls_back_after_100_collisions() {
        let home = tmp_home("fallback");
        // Seed the exact 100 suffixes the iterator will produce
        for n in 1..=100u32 {
            std::fs::create_dir_all(home.join("workspace").join(format!("codex-{n:06x}")))
                .expect("seed");
        }
        let name =
            unique_fleet_name_with(&home, "codex", (1u32..=100).collect::<Vec<_>>().into_iter());
        // Fallback path uses `-N` counter starting at 2
        assert_eq!(name, "codex-2");
        std::fs::remove_dir_all(&home).ok();
    }

    // --- Sprint 41 T-4: resolve_backend config resolution ---

    #[test]
    fn resolve_backend_known_preset_returns_command() {
        let (cmd, submit) = resolve_backend("claude");
        assert!(
            !cmd.is_empty(),
            "known backend must resolve to non-empty command"
        );
        assert_eq!(submit, "\r", "claude submit key must be \\r");
    }

    #[test]
    fn resolve_backend_unknown_returns_passthrough() {
        let (cmd, submit) = resolve_backend("my-custom-cli");
        assert_eq!(cmd, "my-custom-cli", "unknown backend must pass through");
        assert_eq!(
            submit, "\r",
            "unknown backend default submit key must be \\r"
        );
    }
}
