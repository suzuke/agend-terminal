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

/// #freeze-4 A1: chunk size the initial screen dump is split into when fed through
/// the pane's rx (instead of one unbounded `vterm.process(&dump)`). Matches the
/// PTY read size so the render loop's boot/steady drain treats dump bytes exactly
/// like live output — keeps the boot-phase per-frame TIME cap tight (a single
/// chunk can't blow it) and removes the last unbounded process on the restart path.
const DUMP_CHUNK_BYTES: usize = 8 * 1024;

/// #freeze-4 probe (env-gated, `AGEND_FREEZE_INSTRUMENT`): log the initial screen
/// dump size per pane at attach, so an operator restart-repro can size the
/// restart-flood (the `vterm.process(&dump)` cost was previously uninstrumented —
/// `#freeze-backlog` only sees the rx stream). Off by default → zero overhead.
fn freeze_dump_probe_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("AGEND_FREEZE_INSTRUMENT").is_ok_and(|v| !v.is_empty() && v != "0")
    })
}

/// Spawn an agent/shell via spawn_agent and add as a new tab.
#[allow(clippy::too_many_arguments)]
/// Whether a spawned pane is a managed fleet agent or an unmanaged local shell.
/// Drives the #1441 identity policy in `agent::spawn_agent`: a managed agent must
/// resolve its authoritative UUID from fleet.yaml (a missing entry is refused),
/// while a local/scratch shell — never a fleet member, no inbox identity — has no
/// second identity track to drift against and so mints a throwaway id (the same
/// path as the standalone `capture` CLI). Carried explicitly so the sensitive
/// guard is never widened to wrongly admit a real managed agent.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum SpawnIdentity {
    Managed,
    UnmanagedLocalShell,
}

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
    identity: SpawnIdentity,
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
        identity,
    )?;
    let tab_name = pane.agent_name.clone();
    layout.add_tab(Tab::new(tab_name.to_string(), pane));
    Ok(())
}

/// Create a pane backed by spawn_agent.
///
/// #render-first phase (a): this is now the back-to-back composition of two
/// halves — [`build_pane_placeholder`] (cheap, synchronous shell: name + cwd +
/// pane-id + empty VTerm + the forwarder receiver) and [`attach_agent_to_pane`]
/// (the expensive agent spawn + subscribe + VTerm seed + forwarder thread). The
/// composition is **byte-identical** to the prior single-function form: the
/// same side effects run in the same order and the returned `Pane` is unchanged.
/// Splitting here is what lets a later phase render the placeholder shell before
/// the per-agent spawn runs (in the background); phase (a) keeps the call site
/// fully synchronous.
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
    identity: SpawnIdentity,
) -> Result<Pane> {
    let (mut pane, fwd_tx) = build_pane_placeholder(
        layout,
        home,
        base_name,
        command,
        working_dir,
        cols,
        rows,
        name_counter,
    );
    attach_agent_to_pane(
        &mut pane, fwd_tx, registry, home, command, args, spawn_mode, env, submit_key, cols, rows,
        wakeup_tx, identity,
    )?;
    Ok(pane)
}

/// Cheap, synchronous half of [`create_pane`]: dedup the name, resolve the
/// working directory, allocate the pane id, and build a `Pane` with an EMPTY
/// VTerm plus the forwarder's receiver end. It does NOT spawn the agent or touch
/// the registry — [`attach_agent_to_pane`] does that and seeds the VTerm with
/// the first screen dump. Returns the placeholder pane together with the
/// `fwd_tx` sender the attach step wires the agent's output into.
///
/// The empty VTerm is deliberate (phase-(a) byte-identical: the prior code also
/// built a fresh `VTerm::new` and only filled it from the dump in the attach
/// step). The forwarder channel is created up front so the pane owns a valid
/// receiver immediately, independent of when the agent spawns.
#[allow(clippy::too_many_arguments)]
fn build_pane_placeholder(
    layout: &mut Layout,
    home: &Path,
    base_name: &str,
    command: &str,
    working_dir: Option<&Path>,
    cols: u16,
    rows: u16,
    name_counter: &mut HashMap<String, usize>,
) -> (Pane, crossbeam_channel::Sender<Vec<u8>>) {
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
        .unwrap_or_else(|| crate::paths::workspace_dir(home).join(&name));

    let pane_id = layout.next_pane_id();
    let (fwd_tx, fwd_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
    let backend = Backend::from_command(command);

    let pane = Pane {
        agent_name: name.into(),
        // Filled by `attach_agent_to_pane` once the agent's authoritative UUID
        // is resolved; `Default` is the never-routed placeholder until then.
        instance_id: crate::types::InstanceId::default(),
        vterm: VTerm::new(cols, rows),
        rx: fwd_rx,
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
        offthread: None,
    };
    (pane, fwd_tx)
}

/// Agent-backed half of [`create_pane`]: generate the MCP config + skills, spawn
/// the agent (the expensive fork/exec), resolve its authoritative UUID, subscribe
/// to its output, seed the pane's VTerm with the initial screen dump, and park
/// the forwarder thread that drives the placeholder's `fwd_tx`. Mutates `pane` in
/// place (`instance_id` + `vterm`); all other fields were set by the placeholder.
#[allow(clippy::too_many_arguments)]
fn attach_agent_to_pane(
    pane: &mut Pane,
    fwd_tx: crossbeam_channel::Sender<Vec<u8>>,
    registry: &AgentRegistry,
    home: &Path,
    command: &str,
    args: &[String],
    spawn_mode: crate::backend::SpawnMode,
    env: &HashMap<String, String>,
    submit_key: &str,
    cols: u16,
    rows: u16,
    wakeup_tx: &crossbeam_channel::Sender<usize>,
    identity: SpawnIdentity,
) -> Result<()> {
    let name = pane.agent_name.to_string();
    let work_dir = pane
        .working_dir
        .clone()
        .unwrap_or_else(|| crate::paths::workspace_dir(home).join(&name));
    // #render-first phase-(b): the back-to-back composition of the worker-safe
    // expensive half (`spawn_and_subscribe`) and the main-thread cheap half
    // (`apply_attachment`) is byte-identical to the prior inline body — same side
    // effects, same order. Splitting here is what lets the deferred path run
    // `spawn_and_subscribe` on a background worker (touches only the shareable
    // registry) while the render thread runs `apply_attachment` (pane mutation).
    let (instance_id, rx, dump) = spawn_and_subscribe(
        registry, home, &name, command, args, spawn_mode, env, submit_key, &work_dir, cols, rows,
        identity,
    )?;
    apply_attachment(pane, instance_id, rx, dump, fwd_tx, wakeup_tx);
    Ok(())
}

/// Expensive, **worker-safe** half of an attach (#render-first phase-(b)): the
/// part that touches only the shareable `registry: Arc<Mutex>` and the filesystem
/// — generate MCP config, install skills, spawn the agent (the fork/exec), resolve
/// its UUID, and subscribe to its output. Returns the authoritative UUID + the
/// subscriber receiver + the initial screen dump for the main thread to finish
/// wiring via [`apply_attachment`]. Mutates NO `Pane`/`Layout`, so it can run off
/// the render thread on a background worker.
#[allow(clippy::too_many_arguments)]
fn spawn_and_subscribe(
    registry: &AgentRegistry,
    home: &Path,
    name: &str,
    command: &str,
    args: &[String],
    spawn_mode: crate::backend::SpawnMode,
    env: &HashMap<String, String>,
    submit_key: &str,
    work_dir: &Path,
    cols: u16,
    rows: u16,
    identity: SpawnIdentity,
) -> Result<(
    crate::types::InstanceId,
    crossbeam_channel::Receiver<Vec<u8>>,
    Vec<u8>,
)> {
    // Generate MCP config for agent backends
    if Backend::from_command(command).is_some() {
        crate::instructions::generate(work_dir, command);
    }

    // #1083: install skills for TUI-spawned panes (app mode).
    // App mode sets resolve_agents=false so the daemon spawn loop is
    // empty; pane_factory is the sole spawn path. Mirrors the
    // install_for_agent call in spawn_and_register_agent (cold-boot)
    // and spawn_one (SPAWN RPC). Best-effort: failures log + continue.
    {
        let skills_filter: Option<Vec<String>> =
            crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
                .ok()
                .and_then(|c| c.instances.get(name).and_then(|i| i.skills.clone()));
        let custom_skills_source: Option<std::path::PathBuf> =
            crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
                .ok()
                .and_then(|c| c.instances.get(name).and_then(|i| i.skills_path.clone()))
                .map(|p| crate::fleet::resolve::expand_tilde_path(&p));
        let backend_skill = Backend::from_command(command).and_then(|b| b.skill_dir_name());
        match crate::skills::install_for_agent_backend_with_source(
            home,
            work_dir,
            skills_filter.as_deref(),
            backend_skill,
            custom_skills_source.as_deref(),
        ) {
            Ok(outcomes) => {
                let modes: Vec<(&str, crate::skills::InstallMode)> = outcomes
                    .iter()
                    .map(|o| (o.backend.as_str(), o.mode))
                    .collect();
                tracing::info!(agent = %name, ?modes, "pane skills auto-install complete");
            }
            Err(e) => {
                tracing::warn!(agent = %name, error = %e, "pane skills auto-install failed");
            }
        }
    }

    // Backend-specific flags (Claude's --append-system-prompt-file / --mcp-config /
    // --settings) are now injected centrally by agent::spawn_agent, so callers pass
    // raw args and spawn_agent enriches them from files under work_dir.
    let spawn_mode = spawn_mode.downgraded_for(command, Some(work_dir));
    // #1441 identity: a managed agent passes its real `home` so spawn_agent
    // resolves the authoritative fleet.yaml UUID; a local/scratch shell passes
    // `None` (it is not a fleet member) so spawn_agent mints a throwaway id —
    // exactly like the standalone `capture` CLI. `home` is still used below for
    // skills/cwd/work_dir; only the *identity* source is gated here.
    let identity_home = match identity {
        SpawnIdentity::Managed => Some(home),
        SpawnIdentity::UnmanagedLocalShell => None,
    };
    // #1441: spawn_agent returns the authoritative InstanceId it used as the
    // registry key — reuse it directly to route the pane's lookups. (Was a
    // second `resolve_uuid(home, name)` here, which fails for a local shell with
    // no fleet.yaml entry even after a successful spawn — the original bug.)
    let instance_id = agent::spawn_agent(
        &agent::SpawnConfig {
            name,
            backend_command: command,
            args,
            spawn_mode,
            cols,
            rows,
            env: Some(env),
            working_dir: Some(work_dir),
            submit_key,
            home: identity_home,
            crash_tx: None,
            // restart-freeze 真嫌#1 (t-…55279): hand every Owned-mode agent the
            // process-global app shutdown flag so `app_teardown` can flip it and
            // each PTY-close handler takes the fast `is_shutdown` early-return
            // during a parallel teardown (was `None` → slow per-thread exit poll
            // + crash/shell-fallback events on every restart kill).
            shutdown: Some(Arc::clone(super::app_shutdown_flag())),
        },
        registry,
    )?;

    // Subscribe to the agent's output
    let (rx, dump) = {
        let reg = agent::lock_registry(registry);
        let handle = reg
            .get(&instance_id)
            .ok_or_else(|| anyhow::anyhow!("agent not found after spawn"))?;
        agent::subscribe_with_dump(handle)
    };

    Ok((instance_id, rx, dump))
}

/// Cheap, **main-thread** half of an attach (#render-first phase-(b)): apply the
/// [`spawn_and_subscribe`] result to the placeholder pane and start the output
/// forwarder. MUST run on the render thread — it mutates the `Pane` (which lives
/// in the main-thread-owned `Layout`).
///
/// #freeze-4 A1: the initial screen dump is pushed into the pane's rx (chunked,
/// AHEAD of the forwarder) instead of an unbounded one-shot `vterm.process(&dump)`.
/// The forwarder is started only after the whole dump is enqueued, so FIFO
/// preserves the seed-before-stream order (dump bytes are drained before any
/// post-subscribe byte). This routes the dump through the SAME bounded drain path
/// as live output — the render loop's boot-phase (time-capped) / steady-state
/// (byte-capped) `drain_all_panes` bounds the per-frame cost, removing the last
/// unbounded process on the restart path. (At restart, every pane's dump enqueues
/// at once → the boot phase absorbs the flood instead of an interactive freeze.)
fn apply_attachment(
    pane: &mut Pane,
    instance_id: crate::types::InstanceId,
    rx: crossbeam_channel::Receiver<Vec<u8>>,
    dump: Vec<u8>,
    fwd_tx: crossbeam_channel::Sender<Vec<u8>>,
    wakeup_tx: &crossbeam_channel::Sender<usize>,
) {
    pane.instance_id = instance_id;
    let pane_id = pane.id;
    // #freeze-4 A1: enqueue the dump through rx (chunked, FIFO before the forwarder)
    // rather than processing it unbounded into the VTerm here.
    if freeze_dump_probe_enabled() && !dump.is_empty() {
        tracing::info!(
            tag = "#freeze-dump",
            pane_id = pane_id,
            dump_bytes = dump.len(),
            chunks = dump.len().div_ceil(DUMP_CHUNK_BYTES),
            "attach screen dump enqueued to rx"
        );
    }
    for chunk in dump.chunks(DUMP_CHUNK_BYTES) {
        if fwd_tx.send(chunk.to_vec()).is_err() {
            return; // pane already dropped → nothing to forward
        }
    }
    // Wake the loop so the just-enqueued dump is drained (the boot phase / drain
    // re-arm catches it; the forwarder also wakes on each subsequent live chunk).
    let _ = wakeup_tx.send(pane_id);

    // Forward subscriber output to wakeup channel via the placeholder's fwd_tx
    let name = pane.agent_name.to_string();
    let tx = wakeup_tx.clone();
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

    // Option X (off-thread parse, flag AGEND_OFFTHREAD_PARSE, default OFF): spawn a
    // per-pane parser thread that owns its OWN `VTerm` and consumes a CLONE of the
    // pane's `rx` (the screen dump enqueued above + every live chunk the forwarder
    // pushes there). The main-thread `drain_output` no-ops while `offthread` is
    // `Some`, so the parser is the sole consumer (no work-stealing split), and
    // `render_pane` paints the published snapshot instead of parsing on the main
    // thread. Flag OFF → no thread spawned, byte-identical to the path above.
    if crate::render::offthread::offthread_parse_enabled() {
        let parser_vterm = VTerm::new(pane.vterm.cols(), pane.vterm.rows());
        // spawn returns None if the OS thread can't be created → leave
        // `offthread = None` so the pane keeps the byte-identical main-thread drain
        // path rather than being stranded with a dead parser (#2404 r6 ③).
        pane.offthread = crate::render::offthread::spawn_offthread_parser(
            pane_id,
            name,
            pane.rx.clone(),
            parser_vterm,
            wakeup_tx.clone(),
        );
    }
}

// ── #render-first phase-(b): deferred (background) attach ──────────────────
//
// OWNED restore builds all placeholders synchronously (µs) then schedules the
// expensive per-agent spawn on a bounded background pool, so the TUI's first
// draw no longer waits on N sequential fork/exec + skills-install. Layout +
// name_counter stay main-thread-exclusive; workers only touch the shareable
// `registry: Arc<Mutex>` and hand results back over a channel.

/// The worker-side recipe for a deferred attach. Holds NO `Pane`/`Layout`
/// reference (those are the render thread's exclusive `&mut`), only owned data
/// safe to move to a background worker.
// One per restored agent/shell, moved once into a worker — the `Agent`
// (carries a `ResolvedInstance`) vs `Direct` size gap doesn't matter.
#[allow(clippy::large_enum_variant)]
pub(super) enum AttachSpec {
    /// A fleet agent — the worker re-runs the per-agent prep (peers/team/worktree/
    /// model/args + fleet-aware instructions) then spawns, mirroring the
    /// synchronous `create_pane_from_resolved` ordering off the render thread.
    Agent {
        fleet_name: String,
        deduped_name: String,
        resolved: crate::fleet::ResolvedInstance,
        spawn_mode: crate::backend::SpawnMode,
        cols: u16,
        rows: u16,
    },
    /// A shell (or any direct command) — final spawn params already resolved.
    Direct {
        name: String,
        command: String,
        args: Vec<String>,
        spawn_mode: crate::backend::SpawnMode,
        env: HashMap<String, String>,
        submit_key: String,
        work_dir: std::path::PathBuf,
        cols: u16,
        rows: u16,
    },
}

impl AttachSpec {
    fn name(&self) -> &str {
        match self {
            AttachSpec::Agent { deduped_name, .. } => deduped_name,
            AttachSpec::Direct { name, .. } => name,
        }
    }
}

/// A placeholder pane awaiting background attach: its id + the forwarder sender
/// the attach result wires up + the worker recipe.
pub(super) struct AttachJob {
    pub pane_id: usize,
    pub fwd_tx: crossbeam_channel::Sender<Vec<u8>>,
    pub spec: AttachSpec,
}

/// Result a worker hands back to the render thread over `attach_rx`.
// A handful of these are sent per restart (one per restored agent), so the
// `Ready`/`Failed` size gap is irrelevant — boxing would only add an alloc on
// the hot success path.
#[allow(clippy::large_enum_variant)]
pub(super) enum AttachOutcome {
    Ready {
        pane_id: usize,
        instance_id: crate::types::InstanceId,
        rx: crossbeam_channel::Receiver<Vec<u8>>,
        dump: Vec<u8>,
        /// The real spawn cwd (worktree-resolved for agents) → update the pane.
        work_dir: std::path::PathBuf,
        /// The agent's (deduped) name — used to kill the orphan if its pane was
        /// closed while the attach was in flight (F1).
        name: String,
    },
    Failed {
        pane_id: usize,
        name: String,
        err: String,
    },
}

impl AttachOutcome {
    /// The placeholder pane this outcome targets (used to look it up in the Layout).
    pub(super) fn pane_id(&self) -> usize {
        match self {
            AttachOutcome::Ready { pane_id, .. } | AttachOutcome::Failed { pane_id, .. } => {
                *pane_id
            }
        }
    }
}

/// Pre-feed the placeholder VTerm with a "Starting" banner so the operator sees a
/// labelled shell immediately, before the background attach completes. This is the
/// one behavioral change vs the empty-VTerm phase-(a) placeholder.
fn prefeed_starting(vterm: &mut VTerm, name: &str) {
    vterm.process(format!("\u{23f3} Starting {name}\u{2026}\r\n").as_bytes());
}

/// Main-thread: build the cheap placeholder pane for a fleet AGENT (name dedup +
/// pane id + "Starting" banner) plus the [`AttachJob`] a worker will run. The
/// expensive spawn (skills + fork/exec + subscribe + fleet instructions) is
/// deferred to [`run_attach`]; nothing here forks a process.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_deferred_agent_pane(
    fleet_name: &str,
    resolved: &crate::fleet::ResolvedInstance,
    layout: &mut Layout,
    home: &Path,
    cols: u16,
    rows: u16,
    name_counter: &mut HashMap<String, usize>,
    spawn_mode: crate::backend::SpawnMode,
) -> (Pane, AttachJob) {
    let (mut pane, fwd_tx) = build_pane_placeholder(
        layout,
        home,
        fleet_name,
        &resolved.backend_command,
        resolved.working_directory.as_deref(),
        cols,
        rows,
        name_counter,
    );
    let deduped_name = pane.agent_name.to_string();
    pane.fleet_instance_name = Some(fleet_name.to_string());
    prefeed_starting(&mut pane.vterm, &deduped_name);
    let job = AttachJob {
        pane_id: pane.id,
        fwd_tx,
        spec: AttachSpec::Agent {
            fleet_name: fleet_name.to_string(),
            deduped_name,
            resolved: resolved.clone(),
            spawn_mode,
            cols,
            rows,
        },
    };
    (pane, job)
}

/// Main-thread: build the cheap placeholder pane for a SHELL / direct command
/// plus its [`AttachJob`] (final spawn params already resolved).
#[allow(clippy::too_many_arguments)]
pub(super) fn build_deferred_direct_pane(
    layout: &mut Layout,
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
    name_counter: &mut HashMap<String, usize>,
) -> (Pane, AttachJob) {
    let (mut pane, fwd_tx) = build_pane_placeholder(
        layout,
        home,
        base_name,
        command,
        working_dir,
        cols,
        rows,
        name_counter,
    );
    let deduped_name = pane.agent_name.to_string();
    let work_dir = pane
        .working_dir
        .clone()
        .unwrap_or_else(|| crate::paths::workspace_dir(home).join(&deduped_name));
    prefeed_starting(&mut pane.vterm, &deduped_name);
    let job = AttachJob {
        pane_id: pane.id,
        fwd_tx,
        spec: AttachSpec::Direct {
            name: deduped_name,
            command: command.to_string(),
            args: args.to_vec(),
            spawn_mode,
            env: env.clone(),
            submit_key: submit_key.to_string(),
            work_dir,
            cols,
            rows,
        },
    };
    (pane, job)
}

/// #render-first phase-(b) F2 (r6): if app teardown began while a worker was
/// spawning, reap the just-registered child so no live orphan survives quit.
/// Remove + terminate happen UNDER the registry lock, serialized against the
/// teardown's one-shot drain. The teardown sets the shutdown flag BEFORE the
/// drain, so every interleaving is covered: a child registered before the drain
/// is reaped by the drain itself (this call then finds None), and a child
/// registered after the drain is observed here (the late finisher always sees the
/// flag) and reaped. No registered-but-unterminated child can outlive teardown.
/// Returns true if a child was reaped (i.e. we are shutting down).
fn reap_late_registration_if_shutdown(
    registry: &AgentRegistry,
    instance_id: crate::types::InstanceId,
    shutting_down: bool,
) -> bool {
    if !shutting_down {
        return false;
    }
    let drained: Vec<(String, crate::daemon::ChildHandle)> = {
        let mut reg = agent::lock_registry(registry);
        reg.remove(&instance_id)
            .map(|h| (h.name.to_string(), h.child))
            .into_iter()
            .collect()
    };
    crate::daemon::terminate_agents_parallel(drained);
    true
}

/// Commit a successful deferred spawn as a `Ready` outcome — UNLESS app teardown
/// began while we were spawning (the worker passed `run_attach`'s entry shutdown
/// check, then `spawn_agent` registered a child). In that case reap our own child
/// (`reap_late_registration_if_shutdown`) and report `Failed` instead of leaking a
/// live agent the one-shot teardown drain may have missed (the F2 race r6 found).
#[allow(clippy::too_many_arguments)]
fn finish_attach(
    registry: &AgentRegistry,
    pane_id: usize,
    instance_id: crate::types::InstanceId,
    rx: crossbeam_channel::Receiver<Vec<u8>>,
    dump: Vec<u8>,
    work_dir: std::path::PathBuf,
    name: String,
) -> AttachOutcome {
    let shutting_down = super::app_shutdown_flag().load(std::sync::atomic::Ordering::SeqCst);
    if reap_late_registration_if_shutdown(registry, instance_id, shutting_down) {
        return AttachOutcome::Failed {
            pane_id,
            name,
            err: "app shutting down during spawn".to_string(),
        };
    }
    AttachOutcome::Ready {
        pane_id,
        instance_id,
        rx,
        dump,
        work_dir,
        name,
    }
}

/// Worker entry: run ONE deferred [`AttachJob`]'s spec off the render thread,
/// returning an [`AttachOutcome`] for the main thread to apply. Honors the app
/// shutdown flag at TWO points: at entry (early-abort: never fork a NEW child once
/// teardown began) and, via [`finish_attach`], right after a successful spawn
/// (reap a child registered after the one-shot teardown drain — the F2 race).
pub(super) fn run_attach(
    spec: AttachSpec,
    pane_id: usize,
    registry: &AgentRegistry,
    home: &Path,
) -> AttachOutcome {
    if super::app_shutdown_flag().load(std::sync::atomic::Ordering::SeqCst) {
        return AttachOutcome::Failed {
            pane_id,
            name: spec.name().to_string(),
            err: "app shutting down".to_string(),
        };
    }
    match spec {
        AttachSpec::Agent {
            fleet_name,
            deduped_name,
            resolved,
            spawn_mode,
            cols,
            rows,
        } => {
            // Mirror create_pane_from_resolved's per-agent prep, off-thread.
            let fleet_path = crate::fleet::fleet_yaml_path(home);
            let peers: Vec<(String, Option<String>)> = crate::fleet::FleetConfig::load(&fleet_path)
                .map(|f| {
                    f.instances
                        .iter()
                        .map(|(n, c)| (n.clone(), c.role.clone()))
                        .collect()
                })
                .unwrap_or_default();
            let team_record = crate::teams::find_team_for(home, &fleet_name);
            let extra_instructions = crate::instructions::resolve_extra_for(
                &resolved,
                fleet_path.parent().unwrap_or(home),
            );
            // #render-first phase-(b) F3 (accepted residual, lead-tracked follow-up):
            // W=3 workers may now resolve auto-worktrees CONCURRENTLY, so the FIRST
            // concurrent same-repo `git worktree add` can race on `.git/index.lock`
            // and the loser falls back (losing isolation that once). Bounded: git
            // serializes the op, only the #2234 reconcile-flag-ON create() path forks
            // a worktree here (default OFF), and RESTART is idempotent — so not fixed
            // in this PR.
            let mut working_dir = resolved.working_directory.clone();
            if let Some(wt) = crate::worktree::resolve_auto_worktree(home, &fleet_name, &resolved) {
                working_dir = Some(wt);
            }
            let mut args = resolved.args.clone();
            if let Some(ref model) = resolved.model {
                let model_val = Backend::from_command(&resolved.backend_command)
                    .map(|b| b.format_model_arg(model))
                    .unwrap_or_else(|| model.clone());
                args.push("--model".to_string());
                args.push(model_val);
            }
            let command = resolved.backend_command.clone();
            let work_dir = working_dir
                .clone()
                .unwrap_or_else(|| crate::paths::workspace_dir(home).join(&deduped_name));
            match spawn_and_subscribe(
                registry,
                home,
                &deduped_name,
                &command,
                &args,
                spawn_mode,
                &resolved.env,
                &resolved.submit_key,
                &work_dir,
                cols,
                rows,
                SpawnIdentity::Managed,
            ) {
                Ok((instance_id, rx, dump)) => {
                    // Overwrite basic instructions with the fleet-aware version
                    // (same ordering as the synchronous create_pane_from_resolved).
                    let team_ctx = team_record
                        .as_ref()
                        .map(|t| crate::instructions::TeamContext {
                            name: t.name.as_str(),
                            orchestrator: t.orchestrator.as_deref(),
                            members: t.members.as_slice(),
                        });
                    let ctx = crate::instructions::AgentContext {
                        name: &fleet_name,
                        role: resolved.role.as_deref(),
                        fleet_peers: &peers,
                        team: team_ctx.as_ref(),
                        extra_instructions: extra_instructions.as_deref(),
                    };
                    crate::instructions::generate_with_context(&work_dir, &command, Some(&ctx));
                    finish_attach(
                        registry,
                        pane_id,
                        instance_id,
                        rx,
                        dump,
                        work_dir,
                        deduped_name,
                    )
                }
                Err(e) => AttachOutcome::Failed {
                    pane_id,
                    name: deduped_name,
                    err: e.to_string(),
                },
            }
        }
        AttachSpec::Direct {
            name,
            command,
            args,
            spawn_mode,
            env,
            submit_key,
            work_dir,
            cols,
            rows,
        } => match spawn_and_subscribe(
            registry,
            home,
            &name,
            &command,
            &args,
            spawn_mode,
            &env,
            &submit_key,
            &work_dir,
            cols,
            rows,
            // AttachSpec::Direct is the deferred SHELL / direct-command path
            // (see `build_deferred_direct_pane`) — unmanaged, no fleet.yaml entry.
            SpawnIdentity::UnmanagedLocalShell,
        ) {
            Ok((instance_id, rx, dump)) => {
                finish_attach(registry, pane_id, instance_id, rx, dump, work_dir, name)
            }
            Err(e) => AttachOutcome::Failed {
                pane_id,
                name,
                err: e.to_string(),
            },
        },
    }
}

/// Main-thread: apply a worker's [`AttachOutcome`] to its placeholder pane. On
/// `Ready` wires the agent in (instance id, dump, real cwd, forwarder); on
/// `Failed` flips the placeholder VTerm to a visible "failed to start" banner
/// (the pane stays a normal, closable Layout pane — no zombie). `fwd_tx` is the
/// placeholder's forwarder sender, retained by the caller keyed on `pane_id`.
pub(super) fn apply_attach_outcome(
    pane: &mut Pane,
    registry: &AgentRegistry,
    outcome: AttachOutcome,
    fwd_tx: crossbeam_channel::Sender<Vec<u8>>,
    wakeup_tx: &crossbeam_channel::Sender<usize>,
) {
    match outcome {
        AttachOutcome::Ready {
            instance_id,
            rx,
            dump,
            work_dir,
            ..
        } => {
            pane.working_dir = Some(work_dir);
            apply_attachment(pane, instance_id, rx, dump, fwd_tx, wakeup_tx);
            // #t-98760-8 (#2343 deferred-attach regression): snap the just-
            // registered PTY to the pane's CURRENT (already render-corrected) vterm
            // size. On a restored SPLIT layout the render loop corrected this
            // placeholder's VTERM to the split content rect BEFORE the deferred PTY
            // existed (`resize_pty` was a no-op on the unregistered placeholder), so
            // the render loop's vterm-vs-content gate now sees `vterm == content`
            // and never resizes the freshly-registered PTY — leaving the backend
            // forked at the single-pane spawn estimate (full-width wrap in a half
            // pane). Resize EXPLICITLY here (NOT via `needs_resize`, which the same
            // gate would no-op) now that `pane.instance_id` points at the live
            // handle. Deliberately scoped to this DEFERRED path: the synchronous
            // `attach_agent_to_pane` (which also calls `apply_attachment`) creates
            // the vterm at the PTY's spawn size, so it has no estimate/content gap.
            pane.resize_pty(registry, pane.vterm.cols(), pane.vterm.rows());
        }
        AttachOutcome::Failed { name, err, .. } => {
            // Dropping fwd_tx disconnects the placeholder's output channel
            // (no forwarder ever starts); the pane shows the failure and is
            // closable via the normal close path.
            drop(fwd_tx);
            tracing::error!(agent = %name, error = %err, "render-first: deferred attach failed");
            pane.vterm
                .process(format!("\u{26a0} failed to start {name}: {err}\r\n").as_bytes());
        }
    }
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
    // #1441: registry is UUID-keyed. Locate the live handle by its display
    // name and adopt its authoritative `id` as the pane's routing key — the
    // handle's id was itself resolved from fleet.yaml at spawn, so this is the
    // same single source, no `home` threading needed on the attach path.
    let (rx, dump, backend_command, instance_id) = {
        let reg = agent::lock_registry(registry);
        let (id, handle) = reg
            .iter()
            .find(|(_, h)| h.name.as_str() == name)
            .ok_or_else(|| anyhow::anyhow!("agent '{name}' not found in registry"))?;
        let (rx, dump) = agent::subscribe_with_dump(handle);
        (rx, dump, handle.backend_command.clone(), *id)
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
        agent_name: name.to_string().into(),
        instance_id,
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
        offthread: None,
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
    let fleet_path = crate::fleet::fleet_yaml_path(home);
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
    let extra_instructions =
        crate::instructions::resolve_extra_for(resolved, fleet_path.parent().unwrap_or(home));
    let ctx = crate::instructions::AgentContext {
        name: fleet_name,
        role: resolved.role.as_deref(),
        fleet_peers: &peers,
        team: team_ctx.as_ref(),
        extra_instructions: extra_instructions.as_deref(),
    };

    let mut working_dir = resolved.working_directory.clone();

    // #1858: auto-worktree decision is the single shared gate in
    // `crate::worktree::resolve_auto_worktree` — same fn the boot path
    // (`bootstrap::agent_resolve::resolve_one`) calls, so live-spawn and boot
    // can't drift. Opts in on `source_repo` / `git_branch` (#888) but only for
    // an explicit real-repo `working_directory`, never the daemon-managed
    // `workspace/<name>` default (which `ensure_project_root` git-inits).
    if let Some(wt_path) = crate::worktree::resolve_auto_worktree(home, fleet_name, resolved) {
        working_dir = Some(wt_path);
    }

    let mut args = resolved.args.clone();
    if let Some(ref model) = resolved.model {
        let model_val = Backend::from_command(&resolved.backend_command)
            .map(|b| b.format_model_arg(model))
            .unwrap_or_else(|| model.clone());
        args.push("--model".to_string());
        args.push(model_val);
    }

    let mut pane = create_pane(
        layout,
        registry,
        home,
        fleet_name,
        &resolved.backend_command,
        &args,
        spawn_mode,
        working_dir.as_deref(),
        &resolved.env,
        &resolved.submit_key,
        cols,
        rows,
        wakeup_tx,
        name_counter,
        SpawnIdentity::Managed,
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
        agent_name: name.to_string().into(),
        // #1441: remote panes route input/resize through their `BridgeClient`,
        // not the local registry, so this id is never used for routing. Resolve
        // it from fleet.yaml for consistency; default when absent (harmless —
        // unused on the remote path).
        instance_id: crate::fleet::resolve_uuid(home, name).unwrap_or_default(),
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
        offthread: None,
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
    let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)).ok();
    let taken = |name: &str| -> bool {
        if fleet
            .as_ref()
            .is_some_and(|f| f.instances.contains_key(name))
        {
            return true;
        }
        if crate::paths::workspace_dir(home).join(name).exists() {
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

    // ── #render-first phase-(b) ──────────────────────────────────────────────

    fn screen_of(pane: &Pane) -> String {
        String::from_utf8_lossy(&pane.vterm.dump_screen()).to_string()
    }

    /// #render-first phase-(b) — must-resolve #4 (new behavior) + deferral proof:
    /// the placeholder is built WITHOUT spawning (no registry needed) and shows a
    /// "Starting" banner; the spawn recipe rides along in the AttachJob.
    #[test]
    fn deferred_placeholder_shows_starting_and_defers_spawn() {
        let home = tmp_home("rf_starting");
        let mut layout = Layout::new();
        let mut name_counter = HashMap::new();
        let (pane, job) = build_deferred_direct_pane(
            &mut layout,
            &home,
            "shell",
            "/bin/sh",
            &[],
            crate::backend::SpawnMode::Fresh,
            None,
            &HashMap::new(),
            "\r",
            80,
            24,
            &mut name_counter,
        );
        let screen = screen_of(&pane);
        assert!(
            screen.contains("Starting") && screen.contains("shell"),
            "placeholder must show a 'Starting <name>' banner; got:\n{screen}"
        );
        assert_eq!(job.pane_id, pane.id, "job targets its placeholder");
        assert!(matches!(job.spec, AttachSpec::Direct { .. }));
        std::fs::remove_dir_all(&home).ok();
    }

    /// must-resolve: a worker `Ready` outcome enqueues the dump and starts the
    /// forwarder. #freeze-4 A1: the dump is no longer processed directly into the
    /// VTerm — it is pushed (chunked, FIFO) through the pane's output channel AHEAD
    /// of the forwarder, so the render loop's bounded drain handles it like live
    /// output. So the dump must arrive on the output channel FIRST, then
    /// post-subscribe stream bytes.
    #[test]
    fn apply_ready_outcome_enqueues_dump_then_forwards_freeze4() {
        let home = tmp_home("rf_ready");
        let mut layout = Layout::new();
        let mut name_counter = HashMap::new();
        let (mut pane, _job) = build_deferred_direct_pane(
            &mut layout,
            &home,
            "shell",
            "/bin/sh",
            &[],
            crate::backend::SpawnMode::Fresh,
            None,
            &HashMap::new(),
            "\r",
            80,
            24,
            &mut name_counter,
        );
        let pid = pane.id;
        let (sub_tx, sub_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let (fwd_tx, fwd_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let (wakeup_tx, _wakeup_rx) = crossbeam_channel::unbounded::<usize>();
        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        apply_attach_outcome(
            &mut pane,
            &registry,
            AttachOutcome::Ready {
                pane_id: pid,
                instance_id: crate::types::InstanceId::default(),
                rx: sub_rx,
                dump: b"DUMP-XYZ".to_vec(),
                work_dir: home.clone(),
                name: "shell".into(),
            },
            fwd_tx,
            &wakeup_tx,
        );
        // #freeze-4 A1: the dump is enqueued through the output channel (chunked,
        // FIFO) ahead of the forwarder — NOT seeded directly into the VTerm. It
        // must arrive FIRST (here "DUMP-XYZ" < one chunk → one message).
        let dumped = fwd_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("dump enqueued to the output channel");
        assert_eq!(
            dumped, b"DUMP-XYZ",
            "the dump must be enqueued to rx (ahead of the stream), not processed \
             unbounded into the vterm"
        );
        // Forwarder is live + FIFO: a subscriber byte arrives AFTER the dump.
        sub_tx
            .send(b"hello".to_vec())
            .expect("send on an open channel");
        let got = fwd_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("forwarded byte");
        assert_eq!(got, b"hello");
        std::fs::remove_dir_all(&home).ok();
    }

    /// #2404 r6 ② (decision C) — production-shaped: with an ACTIVE agent (the freeze
    /// scenario), closing the pane reaps the forwarder. Built via the real
    /// `build_deferred_direct_pane` + `apply_attach_outcome` path (not a synthetic
    /// clone): the forwarder targets the pane's OWN output channel (`job.fwd_tx` →
    /// `pane.rx`), so dropping the pane drops the last receiver and the agent's next
    /// byte makes the forwarder's `fwd_tx.send` fail → it exits. (The off-thread
    /// parser's deterministic reap is covered in `render::offthread`; the
    /// quiet-agent forwarder linger is a PRE-EXISTING managed behavior, not
    /// off-thread-introduced — follow-up t-20260622053855100612-41860-5.)
    #[test]
    fn active_agent_pane_close_reaps_forwarder_freeze_scenario() {
        let home = tmp_home("rf_fwd_reap");
        let mut layout = Layout::new();
        let mut name_counter = HashMap::new();
        let (mut pane, job) = build_deferred_direct_pane(
            &mut layout,
            &home,
            "shell",
            "/bin/sh",
            &[],
            crate::backend::SpawnMode::Fresh,
            None,
            &HashMap::new(),
            "\r",
            80,
            24,
            &mut name_counter,
        );
        let pid = pane.id;
        // Upstream agent broadcast — kept ALIVE for the whole test (active agent).
        let (sub_tx, sub_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let (wakeup_tx, wakeup_rx) = crossbeam_channel::unbounded::<usize>();
        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        // Real attach: the forwarder writes to the pane's OWN channel (job.fwd_tx →
        // pane.rx), so the pane is the sole receiver.
        apply_attach_outcome(
            &mut pane,
            &registry,
            AttachOutcome::Ready {
                pane_id: pid,
                instance_id: crate::types::InstanceId::default(),
                rx: sub_rx,
                dump: Vec::new(),
                work_dir: home.clone(),
                name: "shell".into(),
            },
            job.fwd_tx,
            &wakeup_tx,
        );
        // Drop the test's wakeup_tx so only the forwarder's clone keeps wakeup_rx
        // connected — its disconnect then observes the forwarder's exit.
        drop(wakeup_tx);
        // Close the pane while the agent is alive: drops pane.rx (the forwarder
        // channel's last receiver).
        drop(pane);
        // The active agent emits one more byte → the forwarder's fwd_tx.send fails
        // (no receivers) → it exits, dropping its wakeup_tx clone.
        sub_tx.send(b"x".to_vec()).expect("agent still alive");
        let mut reaped = false;
        for _ in 0..50 {
            if matches!(
                wakeup_rx.recv_timeout(std::time::Duration::from_millis(100)),
                Err(crossbeam_channel::RecvTimeoutError::Disconnected)
            ) {
                reaped = true;
                break;
            }
        }
        assert!(
            reaped,
            "active-agent pane close must reap the forwarder (the freeze scenario)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// must-resolve #3: a `Failed` outcome flips the placeholder to a visible
    /// "failed to start" banner and leaves it a never-routed (closable) pane.
    #[test]
    fn apply_failed_outcome_shows_banner_and_stays_closable() {
        let home = tmp_home("rf_failed");
        let mut layout = Layout::new();
        let mut name_counter = HashMap::new();
        let (mut pane, _job) = build_deferred_direct_pane(
            &mut layout,
            &home,
            "shell",
            "/bin/sh",
            &[],
            crate::backend::SpawnMode::Fresh,
            None,
            &HashMap::new(),
            "\r",
            80,
            24,
            &mut name_counter,
        );
        let pid = pane.id;
        // The placeholder's instance_id is the never-routed id assigned at build;
        // a Failed attach must NOT wire it to any agent (stays closable, no zombie).
        let orig_iid = pane.instance_id;
        let (fwd_tx, _fwd_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let (wakeup_tx, _w) = crossbeam_channel::unbounded::<usize>();
        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        apply_attach_outcome(
            &mut pane,
            &registry,
            AttachOutcome::Failed {
                pane_id: pid,
                name: "shell".into(),
                err: "boom".into(),
            },
            fwd_tx,
            &wakeup_tx,
        );
        let screen = screen_of(&pane);
        assert!(
            screen.contains("failed to start") && screen.contains("boom"),
            "failed attach must show a visible banner; got:\n{screen}"
        );
        assert_eq!(
            pane.instance_id, orig_iid,
            "Failed must leave the pane's routing id unchanged (never-routed placeholder)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #t-98760-8 (#2343 deferred-attach regression): on a restored SPLIT layout
    /// the render loop corrects the placeholder VTERM to the split content rect
    /// BEFORE the deferred PTY exists, so the later vterm-vs-content resize gate
    /// sees `vterm == content` and never resizes the freshly-registered PTY — the
    /// backend stays forked at the single-pane spawn estimate (full-width wrap in
    /// a half pane). `apply_attach_outcome` must explicitly snap the just-
    /// registered PTY to the (corrected) vterm size. RED before that explicit
    /// resize (PTY stuck at the 80×24 estimate); GREEN after.
    #[cfg(unix)] // `mk_test_handle` is `#[cfg(all(test, unix))]` (spawns a `true` PTY)
    #[test]
    fn restored_split_pane_pty_snapped_to_content_rect() {
        let home = tmp_home("rf_split_resize");
        let mut layout = Layout::new();
        let mut name_counter = HashMap::new();
        // Placeholder built at the single-pane spawn estimate.
        let (mut pane, _job) = build_deferred_direct_pane(
            &mut layout,
            &home,
            "shell",
            "/bin/sh",
            &[],
            crate::backend::SpawnMode::Fresh,
            None,
            &HashMap::new(),
            "\r",
            80,
            24,
            &mut name_counter,
        );
        // Simulate the render loop correcting the placeholder VTERM to the SPLIT
        // content rect (smaller than the estimate) BEFORE the PTY registers — the
        // exact #2343 ordering that defeats the vterm-vs-content gate.
        let (content_cols, content_rows) = (39u16, 20u16);
        pane.vterm.resize(content_cols, content_rows);

        // The real backend PTY was forked at the single-pane estimate (80×24, the
        // "full-screen" size `mk_test_handle` opens) and registered under instance_id.
        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let instance_id = crate::types::InstanceId::default();
        registry.lock().insert(
            instance_id,
            crate::agent::mk_test_handle("shell", instance_id),
        );

        let pane_id = pane.id;
        let (_sub_tx, sub_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let (fwd_tx, _fwd_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let (wakeup_tx, _w) = crossbeam_channel::unbounded::<usize>();
        apply_attach_outcome(
            &mut pane,
            &registry,
            AttachOutcome::Ready {
                pane_id,
                instance_id,
                rx: sub_rx,
                dump: Vec::new(),
                work_dir: home.clone(),
                name: "shell".into(),
            },
            fwd_tx,
            &wakeup_tx,
        );

        // The PTY must now be snapped to the split content rect — NOT left at the
        // 80×24 single-pane spawn estimate (the #2343 bug). Clone the master Arc
        // out of the registry guard so the lock is released before querying size.
        let master = {
            let reg = registry.lock();
            Arc::clone(&reg.get(&instance_id).expect("handle present").pty_master)
        };
        let size = master.lock().get_size().expect("get_size");
        assert_eq!(
            (size.cols, size.rows),
            (content_cols, content_rows),
            "restored split-pane PTY must be resized to the split content rect, not the \
             single-pane spawn estimate (got {}x{})",
            size.cols,
            size.rows
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// must-resolve #1 [BLOCKER]: with the main-thread keepalive `attach_tx`
    /// alive, the channel is NOT disconnected after every worker sender drops, so
    /// `recv` BLOCKS (Empty) rather than busy-spinning (Disconnected).
    #[test]
    fn attach_rx_keepalive_blocks_not_disconnects() {
        let (attach_tx, attach_rx) = crossbeam_channel::unbounded::<AttachOutcome>();
        let worker_senders: Vec<_> = (0..3).map(|_| attach_tx.clone()).collect();
        drop(worker_senders); // all workers exited
        assert!(
            matches!(
                attach_rx.try_recv(),
                Err(crossbeam_channel::TryRecvError::Empty)
            ),
            "keepalive must keep the channel connected (block, not spin)"
        );
        drop(attach_tx); // drop the keepalive too → now it disconnects
        assert!(
            matches!(
                attach_rx.try_recv(),
                Err(crossbeam_channel::TryRecvError::Disconnected)
            ),
            "without the keepalive the channel disconnects (the busy-spin we avoid)"
        );
    }

    /// The render-loop handback locates a placeholder by id across ALL tabs.
    #[test]
    fn layout_find_pane_mut_locates_placeholder_across_tabs() {
        let home = tmp_home("rf_find");
        let mut layout = Layout::new();
        let mut name_counter = HashMap::new();
        let (pane, _job) = build_deferred_direct_pane(
            &mut layout,
            &home,
            "shell",
            "/bin/sh",
            &[],
            crate::backend::SpawnMode::Fresh,
            None,
            &HashMap::new(),
            "\r",
            80,
            24,
            &mut name_counter,
        );
        let pid = pane.id;
        layout.add_tab(Tab::new("t".to_string(), pane));
        assert!(
            layout.find_pane_mut(pid).is_some(),
            "handback must find the pane"
        );
        assert!(layout.find_pane_mut(987654).is_none());
        std::fs::remove_dir_all(&home).ok();
    }

    /// #render-first phase-(b) F2 (r6) — the child-leak RACE regression. A worker
    /// that finishes its spawn (registers a child) AFTER teardown's one-shot drain
    /// must still have that child reaped, not orphaned. We model the post-drain
    /// state by inserting a real PTY-backed agent, then running the worker's reap
    /// path with `shutting_down=true` (the flag is set before the drain, so a late
    /// finisher always observes it) — the agent must be terminated + removed.
    #[cfg(unix)]
    #[test]
    fn reap_late_registration_kills_child_registered_after_drain() {
        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let id = crate::types::InstanceId::default();
        agent::lock_registry(&registry).insert(id, agent::mk_test_handle("rf-late", id));
        assert!(agent::lock_registry(&registry).contains_key(&id));
        // Detached worker finishing AFTER the drain → shutting_down=true → reap.
        assert!(reap_late_registration_if_shutdown(&registry, id, true));
        assert!(
            !agent::lock_registry(&registry).contains_key(&id),
            "a child registered after the drain must be terminated + removed, not orphaned"
        );
        // Control: not shutting down → never reaps (short-circuits before the registry).
        assert!(!reap_late_registration_if_shutdown(&registry, id, false));
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
        std::fs::create_dir_all(crate::paths::workspace_dir(&home).join(&a)).expect("create a");
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
            std::fs::create_dir_all(
                crate::paths::workspace_dir(&home).join(format!("codex-{n:06x}")),
            )
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
