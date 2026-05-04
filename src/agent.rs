//! Agent state and PTY management.
//!
//! Sync design: std::thread for PTY I/O, crossbeam broadcast for output distribution.
//! Single Mutex on AgentCore ensures atomic subscribe+dump.

use crate::backend::Backend;
use crate::health::HealthTracker;
use crate::state::StateTracker;
use crate::vterm::VTerm;
use parking_lot::Mutex;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

pub type PtyWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// Core state for one agent — protected by a single Mutex for atomic operations.
pub struct AgentCore {
    pub vterm: VTerm,
    pub subscribers: Vec<crossbeam_channel::Sender<Vec<u8>>>,
    pub state: StateTracker,
    pub health: HealthTracker,
}

/// Handle to interact with an agent.
#[allow(dead_code)]
pub struct AgentHandle {
    pub id: crate::types::InstanceId,
    pub name: String,
    pub backend_command: String,
    pub pty_writer: PtyWriter,
    pub pty_master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    pub core: Arc<Mutex<AgentCore>>,
    pub child: Arc<Mutex<Box<dyn portable_pty::Child + Send>>>,
    pub submit_key: String,
    pub inject_prefix: String,
    pub typed_inject: bool,
}

pub type AgentRegistry = Arc<Mutex<HashMap<String, AgentHandle>>>;

/// Handle for an externally connected agent (not PTY-managed by daemon).
pub struct ExternalAgentHandle {
    pub backend_command: String,
    pub pid: u32,
}

pub type ExternalRegistry = Arc<Mutex<HashMap<String, ExternalAgentHandle>>>;

/// Lock the external registry, recovering from poison.
pub fn lock_external(
    reg: &ExternalRegistry,
) -> parking_lot::MutexGuard<'_, HashMap<String, ExternalAgentHandle>> {
    reg.lock()
}

/// Environment variable names that fleet.yaml-supplied `env:` maps are NOT
/// allowed to override when spawning an agent. These either (a) carry
/// credentials that only the host user should control, (b) govern dynamic
/// linking and would let a hostile fleet.yaml load attacker-supplied code
/// into the spawned process, or (c) are agend's own runtime plumbing.
///
/// Matching is case-insensitive for cross-platform safety: Windows env is
/// case-insensitive, so `anthropic_api_key` and `ANTHROPIC_API_KEY` map to the
/// same variable there, and a pure case-sensitive deny-list would miss it.
const SENSITIVE_ENV_KEYS: &[&str] = &[
    // API credentials for backends we drive
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "OPENAI_API_KEY",
    "GOOGLE_API_KEY",
    "GEMINI_API_KEY",
    // Cloud credentials commonly present in dev environments
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    // Git forge tokens
    "GITHUB_TOKEN",
    "GITLAB_TOKEN",
    "NPM_TOKEN",
    // Dynamic-linker injection vectors (Linux / macOS)
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "LD_AUDIT",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
    // agend's own runtime wiring — overriding these lets a template redirect
    // the spawned agent to a different home / break MCP config discovery
    "AGEND_HOME",
    "AGEND_INSTANCE_NAME",
    "AGEND_ALLOWED_WORK_ROOTS",
    "AGEND_MCP_TOOLS_ALLOW",
    "AGEND_MCP_TOOLS_DENY",
];

/// Returns true if the env-var name is on the spawn-time deny-list.
pub fn is_sensitive_env_key(key: &str) -> bool {
    SENSITIVE_ENV_KEYS
        .iter()
        .any(|denied| denied.eq_ignore_ascii_case(key))
}

/// Validate and sanitize an instance name. Only allows [a-zA-Z0-9_-].
pub fn validate_name(name: &str) -> Result<&str, String> {
    if name.is_empty() {
        return Err("instance name cannot be empty".into());
    }
    if name.len() > 64 {
        return Err("instance name too long (max 64 chars)".into());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "instance name '{}' contains invalid characters (only a-z, 0-9, -, _ allowed)",
            name
        ));
    }
    Ok(name)
}

/// Error from [`resolve_instance`].
#[derive(Debug)]
pub enum ResolveError {
    NotFound(String),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(t) => write!(f, "instance '{t}' not found"),
        }
    }
}

/// Resolve a name-or-id string to `(InstanceId, display_name)` via fleet.yaml.
///
/// Resolution order per PLAN §3.3:
/// 1. Exact full-UUID match
/// 2. Exact 8-char short-id prefix match
/// 3. Exact instance name match
///
/// fleet.yaml `instances` is a `HashMap<String, _>` so name uniqueness is
/// structurally guaranteed — no Ambiguous error path needed.
pub fn resolve_instance(
    home: &std::path::Path,
    name_or_id: &str,
) -> Result<(crate::types::InstanceId, String), ResolveError> {
    let fleet = crate::fleet::FleetConfig::load(&home.join("fleet.yaml"))
        .ok()
        .unwrap_or_default();

    // 1. Full UUID match
    if let Some(id) = crate::types::InstanceId::parse(name_or_id) {
        for (name, inst) in &fleet.instances {
            if inst.id.as_deref().and_then(crate::types::InstanceId::parse) == Some(id.clone()) {
                return Ok((id, name.clone()));
            }
        }
        return Err(ResolveError::NotFound(name_or_id.to_string()));
    }

    // 2. Short-id prefix match (8 hex chars)
    if name_or_id.len() == 8 && name_or_id.chars().all(|c| c.is_ascii_hexdigit()) {
        for (name, inst) in &fleet.instances {
            if let Some(ref id_str) = inst.id {
                if let Some(id) = crate::types::InstanceId::parse(id_str) {
                    if id.short() == name_or_id {
                        return Ok((id, name.clone()));
                    }
                }
            }
        }
        // Fall through to name match
    }

    // 3. Exact name match — HashMap guarantees at most one entry per name.
    if let Some(inst) = fleet.instances.get(name_or_id) {
        let id = inst
            .id
            .as_deref()
            .and_then(crate::types::InstanceId::parse)
            .unwrap_or_default();
        return Ok((id, name_or_id.to_string()));
    }

    Err(ResolveError::NotFound(name_or_id.to_string()))
}

/// Lock the agent registry, recovering from poison.
pub fn lock_registry(
    reg: &AgentRegistry,
) -> parking_lot::MutexGuard<'_, std::collections::HashMap<String, AgentHandle>> {
    reg.lock()
}

/// ANSI escape sequence stripper for dialog detection.
/// Public ANSI strip for capture command.
pub fn strip_ansi_pub(s: &str) -> String {
    strip_ansi(s)
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    while let Some(&ch) = chars.peek() {
                        chars.next();
                        if ch.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                Some(']') => {
                    // G1: OSC sequence — terminated by BEL (\x07) or ST (\x1b\\)
                    chars.next();
                    while let Some(&ch) = chars.peek() {
                        if ch == '\x07' {
                            chars.next();
                            break;
                        }
                        if ch == '\x1b' {
                            chars.next();
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                        chars.next();
                    }
                }
                Some('(' | ')') => {
                    chars.next();
                    chars.next();
                }
                _ => {
                    chars.next();
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Exit event sent from the PTY reaper to the daemon main loop.
#[derive(Debug, Clone)]
pub enum AgentExitEvent {
    /// Agent crashed or exited unexpectedly — daemon should respawn.
    Crash(String),
    /// Agent exited cleanly (exit code 0, e.g. `/exit` or `/quit`) — no respawn.
    CleanExit(String),
}

/// Channel for exit events from reaper to daemon.
pub type CrashChannel = crossbeam_channel::Sender<AgentExitEvent>;

/// Configuration for spawning an agent.
///
/// `args` are **user-only**: the caller passes whatever they'd add on top of
/// the backend's baseline (e.g. `--model foo`), and `spawn_agent` prepends the
/// preset args dictated by `spawn_mode`. Callers should not compose
/// `--trust-all-tools`, `--dangerously-skip-permissions`, etc. themselves —
/// otherwise those flags get double-applied.
pub struct SpawnConfig<'a> {
    pub name: &'a str,
    pub backend_command: &'a str,
    pub args: &'a [String],
    pub spawn_mode: crate::backend::SpawnMode,
    pub cols: u16,
    pub rows: u16,
    pub env: Option<&'a HashMap<String, String>>,
    pub working_dir: Option<&'a std::path::Path>,
    pub submit_key: &'a str,
    pub home: Option<&'a std::path::Path>,
    pub crash_tx: Option<CrashChannel>,
    pub shutdown: Option<Arc<std::sync::atomic::AtomicBool>>,
}

/// Build a `CommandBuilder` with resolved args, env, and working directory.
///
/// Extracted from `spawn_agent` so the command-construction logic (arg
/// enrichment, env filtering, PATH prepend, cwd validation) is isolated
/// from the PTY plumbing that follows.
fn build_command(config: &SpawnConfig) -> anyhow::Result<(CommandBuilder, Option<Backend>)> {
    let SpawnConfig {
        name,
        backend_command,
        args,
        spawn_mode,
        working_dir,
        env,
        home,
        ..
    } = config;

    let detected_backend = Backend::from_command(backend_command);

    // argv = preset (per spawn_mode) + caller args + backend spawn_flags.
    // Centralized here so callers don't double-apply preset args.
    let enriched_args: Vec<String> = {
        let preset = detected_backend
            .as_ref()
            .map(|b| b.preset_spawn_args(*spawn_mode))
            .unwrap_or_default();
        let flags = detected_backend
            .as_ref()
            .zip(*working_dir)
            .map(|(b, wd)| b.spawn_flags(wd))
            .unwrap_or_default();
        preset
            .into_iter()
            .chain(args.iter().cloned())
            .chain(flags)
            .collect()
    };

    // Resolve bare command names to absolute paths via `which` before handing
    // them to `CommandBuilder`. On Windows, npm global installs drop both a
    // Unix-style shell-script (no extension) and a `.cmd` wrapper in the same
    // directory; `CreateProcessW`'s PATHEXT search walks the exact name first
    // and picks the extensionless Unix script, which blows up with
    // ERROR_BAD_EXE_FORMAT (193). Pre-resolving gives us the `.cmd` path
    // unambiguously. On Unix this is a no-op — `execvp` already does the
    // equivalent PATH walk — but keeping the same code path on both platforms
    // avoids a `#[cfg(windows)]` split here.
    let resolved_command =
        which::which(backend_command).unwrap_or_else(|_| std::path::PathBuf::from(backend_command));
    let mut cmd = CommandBuilder::new(&resolved_command);
    cmd.args(&enriched_args);
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("FORCE_COLOR", "1");
    cmd.env("AGEND_INSTANCE_NAME", name);

    if std::env::var("LANG").is_err() {
        cmd.env("LANG", "en_US.UTF-8");
    }

    // User env from fleet.yaml. Drop entries on the sensitive-env deny-list
    // so a hostile template cannot override ANTHROPIC_API_KEY, LD_PRELOAD,
    // AGEND_HOME, etc. with attacker-controlled values inherited by the
    // spawned agent process.
    if let Some(env_map) = *env {
        for (k, v) in env_map {
            if is_sensitive_env_key(k) {
                tracing::warn!(
                    instance = %name,
                    key = %k,
                    "dropping fleet.yaml env override for sensitive key"
                );
                continue;
            }
            cmd.env(k, v);
        }
    }

    // Add agend-terminal binary to PATH (use platform PATH separator)
    if let Ok(exe) = std::env::current_exe() {
        if let Some(bin_dir) = exe.parent() {
            let mut paths: Vec<PathBuf> = vec![bin_dir.to_path_buf()];
            if let Some(existing) = std::env::var_os("PATH") {
                paths.extend(std::env::split_paths(&existing));
            }
            if let Ok(joined) = std::env::join_paths(paths) {
                cmd.env("PATH", joined);
            }
        }
    }

    if let Some(dir) = working_dir {
        // Defense-in-depth: the API spawn handler already calls
        // validate_working_directory at admission, but a symlink could have
        // been swapped in between admission and spawn. Revalidate here both
        // before and after create_dir_all so the final cwd we hand to the PTY
        // provably resolves inside AGEND_HOME / AGEND_ALLOWED_WORK_ROOTS.
        // If no home is available (ad-hoc test spawn), skip the recheck.
        if let Some(home_path) = *home {
            if let Err(e) = crate::api::validate_working_directory(dir, home_path) {
                anyhow::bail!("working_directory validation failed at spawn: {e}");
            }
            std::fs::create_dir_all(dir).ok();
            // Second pass: now that the leaf exists, canonicalisation walks
            // through any symlink and the starts_with check inside the
            // validator catches escape-via-symlink.
            let resolved = crate::api::validate_working_directory(dir, home_path)
                .map_err(|e| anyhow::anyhow!("working_directory escapes via symlink: {e}"))?;
            cmd.cwd(&resolved);
        } else {
            tracing::warn!(
                instance = %name,
                dir = %dir.display(),
                "spawn without AGEND_HOME — working_directory symlink recheck skipped"
            );
            std::fs::create_dir_all(dir).ok();
            cmd.cwd(dir);
        }
    }

    Ok((cmd, detected_backend))
}

pub fn spawn_agent(config: &SpawnConfig, registry: &AgentRegistry) -> anyhow::Result<()> {
    let SpawnConfig {
        name,
        backend_command,
        args: _,
        spawn_mode: _,
        cols,
        rows,
        env: _,
        working_dir,
        submit_key,
        home,
        crash_tx,
        shutdown,
    } = config;

    let (cmd, detected_backend) = build_command(config)?;

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: *rows,
            cols: *cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| anyhow::anyhow!("Failed to open PTY: {e}"))?;

    // RAII guard arms the rollback for partial-failure between here and the
    // commit() at the end of this fn. Sprint 20 F1: previously a take_writer /
    // try_clone_reader / pty_read_loop spawn failure left an orphan PID (no
    // registry entry) or a phantom registry entry (no read thread).
    let mut rollback = crate::daemon::lifecycle::SpawnRollback::new(name, registry);

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| anyhow::anyhow!("Failed to spawn '{backend_command}': {e}"))?;
    drop(pair.slave);
    let child_arc: Arc<Mutex<Box<dyn portable_pty::Child + Send>>> = Arc::new(Mutex::new(child));
    rollback.mark_child_spawned(Arc::clone(&child_arc));

    let pty_writer: PtyWriter = Arc::new(Mutex::new(
        pair.master
            .take_writer()
            .map_err(|e| anyhow::anyhow!("take_writer: {e}"))?,
    ));
    let mut pty_reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| anyhow::anyhow!("clone_reader: {e}"))?;
    let pty_master: Arc<Mutex<Box<dyn MasterPty + Send>>> = Arc::new(Mutex::new(pair.master));

    let core = Arc::new(Mutex::new(AgentCore {
        vterm: VTerm::with_pty_writer(*cols, *rows, Arc::clone(&pty_writer)),
        subscribers: Vec::new(),
        state: StateTracker::new(detected_backend.as_ref()),
        health: HealthTracker::new(),
    }));

    // Register in registry
    {
        // Sprint 46 P2: resolve instance ID from fleet.yaml (backfilled by P1).
        let instance_id = config
            .home
            .and_then(|h| crate::fleet::FleetConfig::load(&h.join("fleet.yaml")).ok())
            .and_then(|c| {
                c.instances
                    .get(*name)
                    .and_then(|i| i.id.as_deref())
                    .and_then(crate::types::InstanceId::parse)
            })
            .unwrap_or_default();
        let mut reg = registry.lock();
        reg.insert(
            name.to_string(),
            AgentHandle {
                id: instance_id,
                name: name.to_string(),
                backend_command: backend_command.to_string(),
                pty_writer: Arc::clone(&pty_writer),
                pty_master: Arc::clone(&pty_master),
                core: Arc::clone(&core),
                child: Arc::clone(&child_arc),
                submit_key: submit_key.to_string(),
                inject_prefix: detected_backend
                    .as_ref()
                    .map(|b| b.preset().inject_prefix.to_string())
                    .unwrap_or_default(),
                typed_inject: detected_backend
                    .as_ref()
                    .map(|b| b.preset().typed_inject)
                    .unwrap_or(false),
            },
        );
    }
    rollback.mark_registered();

    // PTY read thread — feeds VTerm + broadcasts + auto-dismiss trust dialog + session reaper
    let core2 = Arc::clone(&core);
    let pw = Arc::clone(&pty_writer);
    let reg_for_reaper = Arc::clone(registry);
    let home_for_reaper = home.map(|p| p.to_path_buf());
    let crash_tx_for_reaper = crash_tx.clone();
    let dismiss: Vec<(String, Vec<u8>)> = detected_backend
        .as_ref()
        .map(|b| {
            b.preset()
                .dismiss_patterns
                .iter()
                .map(|(p, k)| (p.to_string(), k.to_vec()))
                .collect()
        })
        .unwrap_or_default();
    let shutdown_for_reaper = shutdown.clone();
    let n = name.to_string();
    let ctx = PtyReadContext {
        name: n.clone(),
        core: core2,
        pty_writer: pw,
        registry: reg_for_reaper,
        home: home_for_reaper,
        crash_tx: crash_tx_for_reaper,
        dismiss_patterns: dismiss,
        shutdown: shutdown_for_reaper,
    };
    // fire-and-forget: pty_read_loop terminates on PTY EOF, which fires when
    // the child process is killed during shutdown / delete. JoinHandle is
    // discarded because the loop's exit is signalled via the OS-side PTY
    // close, not via a stored handle.
    std::thread::Builder::new()
        .name(format!("{n}_pty_read"))
        .spawn(move || {
            pty_read_loop(&mut pty_reader, &ctx);
        })?;

    // Backends whose CLI does not auto-load the instructions file (e.g. Kiro)
    // need the file contents injected as the first user message on Ready.
    if let Some(b) = detected_backend.as_ref() {
        let preset = b.preset();
        if preset.inject_instructions_on_ready {
            if let Some(dir) = working_dir {
                // Read the instructions body here — while we hold the spawn
                // context and before the `Ready` poll window starts — so an
                // external process mutating the file between write and
                // bootstrap cannot inject a different prompt. Skip the
                // bootstrap entirely if the file is missing/empty.
                let path = dir.join(preset.instructions_path);
                match std::fs::read_to_string(&path) {
                    Ok(content) if !content.trim().is_empty() => {
                        spawn_instructions_bootstrap(
                            Arc::clone(registry),
                            name.to_string(),
                            content,
                            std::time::Duration::from_secs(preset.ready_timeout_secs + 15),
                            shutdown.clone(),
                        );
                    }
                    Ok(_) => {
                        tracing::warn!(
                            agent = %name,
                            path = %path.display(),
                            "instructions file empty, skipping bootstrap inject"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            agent = %name,
                            path = %path.display(),
                            error = %e,
                            "instructions file unreadable, skipping bootstrap inject"
                        );
                    }
                }
            }
        }
    }

    // Disarm the rollback guard — all ordered mutations succeeded.
    rollback.commit();

    tracing::info!(agent = name, backend = backend_command, args = %config.args.join(" "), "spawned");
    Ok(())
}

/// Poll until the agent reaches Ready, then inject the pre-read instructions
/// content as a first user message. Used by backends (Kiro) whose CLI does
/// not auto-load the steering file.
///
/// The `content` is captured at spawn time (see call site) rather than
/// re-read after Ready: this closes the mutation window where an external
/// process could swap the instructions file between write and inject.
fn spawn_instructions_bootstrap(
    registry: AgentRegistry,
    name: String,
    content: String,
    timeout: std::time::Duration,
    shutdown: Option<Arc<std::sync::atomic::AtomicBool>>,
) {
    let thread_name = format!("{name}_instr_boot");
    // fire-and-forget: instruction-bootstrap thread polls Ready then injects
    // the snapshotted instructions content. Observes shutdown flag inside the
    // poll loop (returns early on shutdown). JoinHandle dropped because the
    // thread is short-lived and one missed bootstrap on shutdown is cosmetic.
    let spawn_result = std::thread::Builder::new().name(thread_name).spawn(move || {
        let _census = crate::thread_census::register("instr_boot"); // M3: was "pty_reader"
        let deadline = std::time::Instant::now() + timeout;
        let poll_interval = std::time::Duration::from_millis(200);

        loop {
            if let Some(ref s) = shutdown {
                if s.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
            }
            if std::time::Instant::now() >= deadline {
                tracing::warn!(
                    agent = %name,
                    "instructions bootstrap timed out waiting for Ready"
                );
                return;
            }

            let ready = {
                let reg = &registry.lock();
                match reg.get(&name) {
                    Some(h) => {
                        let core = &h.core.lock();
                        core.state.get_state() == crate::state::AgentState::Ready
                    }
                    None => return, // agent gone
                }
            };
            if ready {
                break;
            }
            std::thread::sleep(poll_interval);
        }

        // Small settle delay so the prompt is fully painted before we type.
        // This is a UI-layer concern (avoiding a torn prompt paint) — the
        // content itself was already snapshotted at spawn, so the delay no
        // longer widens an external-mutation window.
        std::thread::sleep(std::time::Duration::from_millis(500));

        let reg = &registry.lock();
        if let Some(handle) = reg.get(&name) {
            if let Err(e) = inject_to_agent(handle, content.as_bytes()) {
                tracing::warn!(agent = %name, error = %e, "instructions bootstrap inject failed");
            } else {
                tracing::info!(
                    agent = %name,
                    bytes = content.len(),
                    "instructions bootstrap injected"
                );
            }
        }
    });
    if let Err(e) = spawn_result {
        tracing::warn!(error = %e, "failed to spawn instructions bootstrap thread");
    }
}

/// Context for PTY read loop reaper (reduces argument count).
struct PtyReadContext {
    name: String,
    core: Arc<Mutex<AgentCore>>,
    pty_writer: PtyWriter,
    registry: AgentRegistry,
    home: Option<std::path::PathBuf>,
    crash_tx: Option<CrashChannel>,
    dismiss_patterns: Vec<(String, Vec<u8>)>,
    shutdown: Option<Arc<std::sync::atomic::AtomicBool>>,
}

/// PTY read loop: feeds VTerm, broadcasts output, auto-dismisses dialogs, handles exit.
fn pty_read_loop(pty_reader: &mut dyn Read, ctx: &PtyReadContext) {
    let PtyReadContext {
        name,
        core,
        pty_writer,
        registry,
        home,
        crash_tx,
        dismiss_patterns,
        shutdown,
    } = ctx;
    let mut buf = [0u8; 8192];
    let mut dismiss_cooldown_until: Option<std::time::Instant> = None;
    let debug_reads = std::env::var("AGEND_DEBUG_PTY_READ").is_ok();
    let mut read_count: u64 = 0;
    let mut total_bytes: u64 = 0;

    loop {
        match pty_reader.read(&mut buf) {
            Ok(0) => {
                if debug_reads {
                    eprintln!(
                        "[pty_read {name}] EOF after {read_count} reads, {total_bytes} bytes"
                    );
                }
                handle_pty_close(name, registry, home, crash_tx, shutdown);
                break;
            }
            Ok(n_bytes) => {
                if debug_reads {
                    read_count += 1;
                    total_bytes += n_bytes as u64;
                    let snip: String = buf[..n_bytes.min(64)]
                        .iter()
                        .map(|b| {
                            if b.is_ascii_graphic() || *b == b' ' {
                                (*b as char).to_string()
                            } else {
                                format!("\\x{:02x}", b)
                            }
                        })
                        .collect();
                    eprintln!(
                        "[pty_read {name}] read#{read_count} {n_bytes}B total={total_bytes} first64={snip}"
                    );
                }
                let data = &buf[..n_bytes];

                // Feed VTerm + state detection + broadcast (under same lock = atomic),
                // then scan the rendered screen for dismiss patterns. Scanning
                // post-render means we match what the user actually sees —
                // Ink-style TUIs that draw char-by-char with cursor positioning
                // won't defeat us (VTerm resolves the geometry). Cooldown: 10s.
                let screen = {
                    let mut c = core.lock();
                    c.vterm.process(data);
                    let rows = c.vterm.rows() as usize;
                    let screen = c.vterm.tail_lines(rows);
                    c.state.feed(&screen);
                    c.subscribers.retain(|tx| tx.send(data.to_vec()).is_ok());
                    screen
                };

                let in_cooldown = dismiss_cooldown_until
                    .map(|t| std::time::Instant::now() < t)
                    .unwrap_or(false);
                if !in_cooldown && try_dismiss_dialog(name, &screen, pty_writer, dismiss_patterns) {
                    dismiss_cooldown_until =
                        Some(std::time::Instant::now() + std::time::Duration::from_secs(10));
                }
            }
            Err(e) => {
                if debug_reads {
                    eprintln!(
                        "[pty_read {name}] ERR after {read_count} reads, {total_bytes} bytes: {e}"
                    );
                }
                break;
            }
        }
    }
}

/// Sprint 21 F-NEW1: kill the process tree rooted at the agent's child PID,
/// if still registered. Looks up the PID through the registry → child mutex
/// chain, then delegates to [`crate::process::kill_process_tree`] (PR-U #158).
///
/// No-op if the agent is not in the registry (already cleaned up) or if the
/// child has no live PID (already reaped). Idempotent on dead PIDs — safe to
/// call multiple times during the same crash-detection sequence.
fn sweep_child_tree(name: &str, registry: &AgentRegistry) {
    let pid: Option<u32> = {
        let reg = registry.lock();
        reg.get(name).and_then(|h| h.child.lock().process_id())
    };
    if let Some(pid) = pid {
        crate::process::kill_process_tree(pid);
    }
}

/// Handle PTY close: determine if crash, graceful exit, or daemon shutdown.
fn handle_pty_close(
    name: &str,
    registry: &AgentRegistry,
    home: &Option<std::path::PathBuf>,
    crash_tx: &Option<CrashChannel>,
    shutdown: &Option<Arc<std::sync::atomic::AtomicBool>>,
) {
    // Check if daemon is shutting down — if so, this is not a crash
    let is_shutdown = shutdown
        .as_ref()
        .map(|s| s.load(std::sync::atomic::Ordering::Relaxed))
        .unwrap_or(false);

    if is_shutdown {
        tracing::info!(agent = name, "stopped (daemon shutdown)");
        {
            let mut reg = registry.lock();
            reg.remove(name);
        }
        if let Some(ref home) = home {
            crate::ipc::remove_port(&crate::daemon::run_dir(home), name);
        }
        return;
    }

    tracing::info!(agent = name, "PTY closed, waiting for process exit");

    // Wait up to 2s for process to fully exit
    let mut exit_code: Option<i32> = None;
    for _ in 0..20 {
        let reg = registry.lock();
        // Agent removed from registry → shutdown or explicit delete. Not a crash.
        if reg.get(name).is_none() {
            tracing::debug!(agent = name, "not in registry, skipping crash handling");
            return;
        }
        if let Some(handle) = reg.get(name) {
            {
                let mut c = handle.child.lock();
                if let Ok(Some(status)) = c.try_wait() {
                    exit_code = Some(status.exit_code() as i32);
                    break;
                }
            }
        }
        drop(reg);
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    let is_crash = match exit_code {
        Some(0) | Some(130) => false, // Graceful exit: 0 = normal, 130 = SIGINT (user Ctrl+C / /quit)
        Some(137) | Some(143) => {
            // SIGKILL (137) / SIGTERM (143) — killed by daemon or user
            tracing::info!(
                agent = name,
                exit_code = exit_code.unwrap_or(0),
                "killed by signal, not crash"
            );
            false
        }
        Some(c) => {
            tracing::warn!(agent = name, exit_code = c, "crash");
            true
        }
        None => {
            tracing::warn!(agent = name, "process didn't exit in 2s, treating as crash");
            true
        }
    };

    // Distinguish user-initiated clean exit from daemon-initiated kill.
    // SIGKILL/SIGTERM are daemon kills — not user /exit or /quit.
    let is_user_clean_exit = matches!(exit_code, Some(0) | Some(130));

    // Sprint 21 F-NEW1: sweep the child process tree before respawn fires so
    // any leaked grandchildren (kiro-cli's bun + acp etc.) don't survive across
    // the respawn boundary and collide with the new process tree's port/file
    // locks. PR-U #158 added kill_process_tree to handle_kill / handle_delete /
    // run_core but missed this PTY-EOF crash-detection path. Covers both branches:
    // try_wait succeeded (graceful or crash exit may still leave grandchildren
    // orphaned to PID 1) and the 2s timeout (leader still alive).
    sweep_child_tree(name, registry);

    if is_user_clean_exit {
        // User-initiated clean exit (code 0 or 130): /exit, /quit, Ctrl+C.
        // Do NOT respawn the agent — spawn a shell replacement instead
        // (tmux-style: pane stays alive with a shell prompt).
        tracing::info!(
            agent = name,
            ?exit_code,
            "clean exit, spawning shell fallback"
        );
        if let Some(ref home) = home {
            crate::event_log::log(home, "clean_exit", name, "agent exited cleanly");
        }

        // Grab terminal size from the existing VTerm before removing.
        let (cols, rows) = {
            let reg = registry.lock();
            reg.get(name)
                .map(|h| {
                    let c = h.core.lock();
                    (c.vterm.cols(), c.vterm.rows())
                })
                .unwrap_or_else(|| crossterm::terminal::size().unwrap_or((120, 40)))
        };

        // Read working_dir from metadata (agent handle doesn't store it).
        let work_dir: Option<std::path::PathBuf> = home.as_ref().and_then(|h| {
            let meta_path = h.join("metadata").join(format!("{name}.json"));
            std::fs::read_to_string(meta_path)
                .ok()
                .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
                .and_then(|v| {
                    v["working_directory"]
                        .as_str()
                        .map(std::path::PathBuf::from)
                })
        });

        // Remove old agent from registry so spawn_agent can reuse the name.
        {
            let mut reg = registry.lock();
            reg.remove(name);
        }
        if let Some(ref home) = home {
            crate::ipc::remove_port(&crate::daemon::run_dir(home), name);
        }

        // Spawn $SHELL as replacement. Best-effort: if it fails, notify
        // daemon for cleanup instead.
        let shell = crate::default_shell();
        let spawn_result = spawn_agent(
            &SpawnConfig {
                name,
                backend_command: shell,
                args: &[],
                spawn_mode: crate::backend::SpawnMode::Fresh,
                cols,
                rows,
                env: None,
                working_dir: work_dir.as_deref(),
                submit_key: "\r",
                home: home.as_deref(),
                crash_tx: crash_tx.clone(),
                shutdown: shutdown.clone(),
            },
            registry,
        );
        match spawn_result {
            Ok(()) => {
                tracing::info!(agent = name, shell, "shell fallback spawned");
                // Start TUI socket for the shell agent so the pane can connect.
                if let Some(ref home) = home {
                    let rdir = crate::daemon::run_dir(home);
                    let reg = Arc::clone(registry);
                    let n = name.to_string();
                    // fire-and-forget: shell TUI server exits when agent removed.
                    let _ = std::thread::Builder::new()
                        .name(format!("{n}_tui"))
                        .spawn(move || crate::daemon::serve_agent_tui(&n, &rdir, &reg));
                }
            }
            Err(e) => {
                tracing::warn!(agent = name, error = %e, "shell fallback failed");
                // Fall through: notify daemon for cleanup.
                if let Some(ref tx) = crash_tx {
                    let _ = tx.try_send(AgentExitEvent::CleanExit(name.to_string()));
                }
            }
        }
        return;
    }

    if !is_crash {
        // SIGKILL/SIGTERM — daemon-initiated kill, not user action.
        // Already handled by shutdown check above; if we reach here it's
        // an explicit `kill` command. Remove from registry, no respawn.
        {
            let mut reg = registry.lock();
            reg.remove(name);
        }
        if let Some(ref home) = home {
            crate::ipc::remove_port(&crate::daemon::run_dir(home), name);
        }
        return;
    }

    // Crash: set Restarting state and notify daemon for respawn.
    tracing::info!(agent = name, "setting restarting state");
    {
        let reg = registry.lock();
        if let Some(handle) = reg.get(name) {
            let mut core = handle.core.lock();
            core.state.set_restarting();
        }
    }
    if let Some(ref tx) = crash_tx {
        if let Err(e) = tx.try_send(AgentExitEvent::Crash(name.to_string())) {
            tracing::warn!(agent = %name, error = %e, "crash channel full — respawn event dropped");
        }
    }
}

/// Try to auto-dismiss dialogs using backend-configurable patterns. Returns true if dismissed.
/// `screen` is the VTerm-rendered view the user sees — not raw PTY bytes —
/// so Ink-style TUIs that paint char-by-char with cursor positioning still match.
pub fn try_dismiss_dialog(
    name: &str,
    screen: &str,
    pty_writer: &PtyWriter,
    dismiss_patterns: &[(String, Vec<u8>)],
) -> bool {
    if dismiss_patterns.is_empty() {
        return false;
    }

    for (pattern, key_seq) in dismiss_patterns {
        if screen.contains(pattern.as_str()) {
            tracing::info!(agent = name, pattern, "auto-dismissing dialog");
            // Delayed write: TUI escape-sequence parsers need time to distinguish
            // \x1b (ESC key) from \x1b[ (CSI start).  Writing immediately causes
            // Ink-based TUIs (kiro-cli) to interpret \x1b as "ESC to cancel".
            // H2: bounded dismiss — skip if one already in-flight for this agent.
            // Prevents thread accumulation from rapid dialog re-detection.
            static DISMISS_IN_FLIGHT: std::sync::LazyLock<
                parking_lot::Mutex<std::collections::HashSet<String>>,
            > = std::sync::LazyLock::new(|| {
                parking_lot::Mutex::new(std::collections::HashSet::new())
            });
            {
                let mut inflight = DISMISS_IN_FLIGHT.lock();
                if inflight.contains(name) {
                    return true; // dismiss already pending
                }
                inflight.insert(name.to_string());
            }
            let writer = Arc::clone(pty_writer);
            let keys = key_seq.clone();
            let agent = name.to_string();
            // fire-and-forget: dialog-dismiss keystroke writer is short-lived
            // (sleep 300ms then write). H2: removes from in-flight set on exit.
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(300));
                // Send keys in chunks split on \r/\n boundaries with delay between,
                // so TUI frameworks process navigation before confirmation.
                let mut w = writer.lock();
                let mut start = 0;
                for (i, &b) in keys.iter().enumerate() {
                    if b == b'\r' || b == b'\n' {
                        // Send everything up to (not including) this Enter
                        if start < i {
                            let _ = w.write_all(&keys[start..i]);
                            let _ = w.flush();
                            drop(w);
                            std::thread::sleep(std::time::Duration::from_millis(200));
                            w = writer.lock();
                        }
                        // Send the Enter
                        let _ = w.write_all(&keys[i..=i]);
                        let _ = w.flush();
                        start = i + 1;
                    }
                }
                if start < keys.len() {
                    let _ = w.write_all(&keys[start..]);
                    let _ = w.flush();
                }
                tracing::debug!(agent = %agent, "dismiss keystrokes sent");
                // H2: remove from in-flight set
                DISMISS_IN_FLIGHT.lock().remove(&agent);
            });
            return true;
        }
    }

    false
}

/// Write data to an agent's PTY (atomic write — for attach path).
pub fn write_to_agent(agent: &AgentHandle, data: &[u8]) -> crate::error::Result<()> {
    let mut w = agent.pty_writer.lock();
    w.write_all(data)
        .map_err(crate::error::AgendError::PtyWrite)?;
    w.flush().map_err(crate::error::AgendError::PtyWrite)?;
    Ok(())
}

/// Write data to an agent's PTY byte-by-byte with small delays.
#[allow(dead_code)]
pub fn write_to_agent_typed(agent: &AgentHandle, data: &[u8]) -> crate::error::Result<()> {
    let mut w = agent.pty_writer.lock();
    for byte in data {
        w.write_all(&[*byte])
            .map_err(crate::error::AgendError::PtyWrite)?;
        w.flush().map_err(crate::error::AgendError::PtyWrite)?;
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    Ok(())
}

/// Inject text + submit to agent PTY. Splits text from submit_key with a delay
/// so TUI frameworks process them as separate events.
/// - typed=false: write_all(prefix+text), delay, write_all(submit_key)
/// - typed=true: per-byte(prefix+text), delay, write_all(submit_key)
pub fn inject_to_agent(agent: &AgentHandle, text: &[u8]) -> crate::error::Result<()> {
    let prefix = agent.inject_prefix.as_bytes();
    let submit = agent.submit_key.as_bytes();

    if agent.typed_inject {
        // H1: collect all bytes first, then write in short lock bursts.
        // Previous pattern held pty_writer lock for 2ms × N bytes (~20s for 10KB).
        let all_bytes: Vec<u8> = prefix.iter().chain(text.iter()).copied().collect();
        for chunk in all_bytes.chunks(64) {
            let mut w = agent.pty_writer.lock();
            for &byte in chunk {
                w.write_all(&[byte])?;
                w.flush()?;
            }
            drop(w);
            std::thread::sleep(std::time::Duration::from_millis(2 * chunk.len() as u64));
        }
    } else {
        let mut w = agent.pty_writer.lock();
        if !prefix.is_empty() {
            w.write_all(prefix)?;
            w.flush()?;
        }
        w.write_all(text)?;
        w.flush()?;
        drop(w);
    }

    // Delay before submit
    std::thread::sleep(std::time::Duration::from_millis(20));

    // Write submit key
    let mut w = agent.pty_writer.lock();
    w.write_all(submit)?;
    w.flush()?;
    Ok(())
}

/// Send a message to a named agent via direct registry injection.
/// Returns true if the agent was found and injected.
pub fn send_to_registry(registry: &AgentRegistry, from: &str, target: &str, text: &str) -> bool {
    let reg = lock_registry(registry);
    if let Some(handle) = reg.get(target) {
        let msg = format!("[from:{from}] {text}");
        let _ = inject_to_agent(handle, msg.as_bytes());
        true
    } else {
        false
    }
}

/// Broadcast a message to all agents with recognized backends.
/// Skips `exclude` (typically the sender) if provided.
pub fn broadcast_registry(
    registry: &AgentRegistry,
    from: &str,
    text: &str,
    exclude: Option<&str>,
) -> Vec<String> {
    let msg = format!("[from:{from}] {text}");
    let msg_bytes = msg.as_bytes();
    // H1 fix: collect target names under lock, release, then re-acquire
    // per-target for inject. Avoids holding registry lock during inject().
    let target_names: Vec<String> = {
        let reg = lock_registry(registry);
        reg.iter()
            .filter(|(name, handle)| {
                (exclude != Some(name.as_str()))
                    && crate::backend::Backend::from_command(&handle.backend_command).is_some()
            })
            .map(|(name, _)| name.clone())
            .collect()
    }; // lock dropped here
    for name in &target_names {
        let reg = lock_registry(registry);
        if let Some(handle) = reg.get(name) {
            let _ = inject_to_agent(handle, msg_bytes);
        }
    }
    target_names
}

/// Get atomic subscribe + screen dump (under core lock — no output gap).
/// Creates a new per-subscriber channel. Each subscriber gets ALL output (broadcast).
pub fn subscribe_with_dump(agent: &AgentHandle) -> (crossbeam_channel::Receiver<Vec<u8>>, Vec<u8>) {
    let mut core = agent.core.lock();
    let dump = core.vterm.dump_screen();
    let (tx, rx) = crossbeam_channel::unbounded();
    core.subscribers.push(tx);
    (rx, dump)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_name_valid() {
        assert!(validate_name("hello").is_ok());
        assert!(validate_name("agent-1").is_ok());
        assert!(validate_name("my_agent").is_ok());
        assert!(validate_name("A123").is_ok());
    }

    #[test]
    fn validate_name_rejects_traversal() {
        assert!(validate_name("../etc").is_err());
        assert!(validate_name("foo/bar").is_err());
        assert!(validate_name("a b").is_err());
        assert!(validate_name("").is_err());
    }

    #[test]
    fn validate_name_rejects_long() {
        let long = "a".repeat(65);
        assert!(validate_name(&long).is_err());
        let ok = "a".repeat(64);
        assert!(validate_name(&ok).is_ok());
    }

    #[test]
    fn strip_ansi_basic() {
        assert_eq!(strip_ansi("hello"), "hello");
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(strip_ansi("\x1b[1;32mbold green\x1b[0m"), "bold green");
    }

    #[test]
    fn strip_ansi_cursor_move_no_space() {
        // CSI C (cursor forward) and D (cursor back) must not insert spaces
        assert_eq!(strip_ansi("\x1b[5Chello"), "hello");
        assert_eq!(strip_ansi("ab\x1b[2Dcd"), "abcd");
        // Other CSI codes also produce nothing
        assert_eq!(strip_ansi("\x1b[Hhome"), "home");
    }

    #[test]
    fn strip_ansi_osc() {
        assert_eq!(strip_ansi("\x1b]0;title\x07rest"), "rest");
    }

    #[test]
    fn sensitive_env_keys_covers_known_dangerous() {
        assert!(is_sensitive_env_key("ANTHROPIC_API_KEY"));
        assert!(is_sensitive_env_key("OPENAI_API_KEY"));
        assert!(is_sensitive_env_key("AWS_SECRET_ACCESS_KEY"));
        assert!(is_sensitive_env_key("LD_PRELOAD"));
        assert!(is_sensitive_env_key("DYLD_INSERT_LIBRARIES"));
        assert!(is_sensitive_env_key("AGEND_HOME"));
        assert!(is_sensitive_env_key("AGEND_MCP_TOOLS_DENY"));
    }

    #[test]
    fn sensitive_env_keys_is_case_insensitive() {
        // Windows env is case-insensitive; ensure lower-cased fleet.yaml keys
        // still hit the deny-list.
        assert!(is_sensitive_env_key("anthropic_api_key"));
        assert!(is_sensitive_env_key("Ld_Preload"));
    }

    #[test]
    fn sensitive_env_keys_allows_benign() {
        assert!(!is_sensitive_env_key("MY_APP_DEBUG"));
        assert!(!is_sensitive_env_key("LANG"));
        assert!(!is_sensitive_env_key("TERM"));
        assert!(!is_sensitive_env_key("PROMPT_OVERRIDE"));
    }

    fn test_writer() -> PtyWriter {
        Arc::new(Mutex::new(Box::new(Vec::<u8>::new())))
    }

    #[test]
    fn dismiss_fires_when_pattern_in_screen() {
        let patterns = vec![("Do you trust".to_string(), b"\n".to_vec())];
        let hit = try_dismiss_dialog(
            "t",
            "Do you trust the contents of this directory?",
            &test_writer(),
            &patterns,
        );
        assert!(hit);
    }

    #[test]
    fn dismiss_skips_when_pattern_absent() {
        let patterns = vec![("Do you trust".to_string(), b"\n".to_vec())];
        let hit = try_dismiss_dialog("t", "unrelated screen content", &test_writer(), &patterns);
        assert!(!hit);
    }

    #[test]
    fn dismiss_skips_when_no_patterns() {
        assert!(!try_dismiss_dialog("t", "anything", &test_writer(), &[]));
    }

    #[test]
    fn dismiss_matches_ink_style_cursor_painted_prompt() {
        // Regression for macOS: Ink-based TUIs (codex) paint text by
        // positioning the cursor before each segment. VTerm resolves this
        // into a clean screen; the old raw-byte strip_ansi path was fragile
        // on such streams. Drive VTerm with BSU + cursor positioning and
        // confirm the rendered screen still contains the pattern literally.
        let mut vt = crate::vterm::VTerm::new(80, 24);
        vt.process(b"\x1b[?2026h"); // begin synchronized update
        vt.process(b"\x1b[5;2HDo you trust"); // row 5 col 2
        vt.process(b"\x1b[5;15H the contents of this directory?");
        vt.process(b"\x1b[?2026l"); // end synchronized update
        let screen = vt.tail_lines(24);
        let patterns = vec![("Do you trust".to_string(), b"\n".to_vec())];
        assert!(try_dismiss_dialog("t", &screen, &test_writer(), &patterns));
    }

    /// Sprint 21 F-NEW1: verify that `sweep_child_tree` reaches grandchild
    /// processes spawned via the agent's PTY. Mirrors the kiro-cli pattern
    /// where the leader shell forks bun/mcp/acp grandchildren — the regression
    /// PR-U #158 fixed for explicit kill paths but missed for PTY-EOF crash
    /// detection.
    #[test]
    #[cfg(unix)]
    fn sweep_child_tree_kills_grandchild_via_process_group() {
        use parking_lot::Mutex;
        use portable_pty::{native_pty_system, CommandBuilder, PtySize};
        use std::collections::HashMap;

        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let pid_file =
            std::env::temp_dir().join(format!("agend-sweep-test-{}.pid", std::process::id()));
        let _ = std::fs::remove_file(&pid_file);
        // sh forks `sleep` into the background; sleep PID is recorded so the
        // test can verify it dies with the leader (group kill semantics).
        let cmd_str = format!("sleep 60 & echo $! > {} && wait", pid_file.display());
        let mut cmd = CommandBuilder::new("sh");
        cmd.args(["-c", &cmd_str]);
        cmd.cwd(std::env::temp_dir());
        let child = pair.slave.spawn_command(cmd).expect("spawn sh + sleep");
        drop(pair.slave);
        let shell_pid = child.process_id().expect("shell process_id");

        // Build a minimal AgentHandle so sweep_child_tree's registry-lookup
        // path is exercised end-to-end (not just kill_process_tree directly).
        let pty_writer: PtyWriter =
            Arc::new(Mutex::new(pair.master.take_writer().expect("take_writer")));
        let pty_master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>> =
            Arc::new(Mutex::new(pair.master));
        let core = Arc::new(Mutex::new(AgentCore {
            vterm: crate::vterm::VTerm::with_pty_writer(80, 24, Arc::clone(&pty_writer)),
            subscribers: Vec::new(),
            state: StateTracker::new(None),
            health: HealthTracker::new(),
        }));
        let handle = AgentHandle {
            id: crate::types::InstanceId::default(),
            name: "sweep-test".to_string(),
            backend_command: "sh".to_string(),
            pty_writer,
            pty_master,
            core,
            child: Arc::new(Mutex::new(child)),
            submit_key: "\r".to_string(),
            inject_prefix: String::new(),
            typed_inject: false,
        };
        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        registry.lock().insert("sweep-test".to_string(), handle);

        // Wait for sleep grandchild to start and write its PID
        for _ in 0..40 {
            if pid_file.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let sleep_pid: u32 = std::fs::read_to_string(&pid_file)
            .unwrap_or_default()
            .trim()
            .parse()
            .expect("parse sleep grandchild PID");
        assert!(
            crate::process::is_pid_alive(shell_pid),
            "shell leader must be alive before sweep"
        );
        assert!(
            crate::process::is_pid_alive(sleep_pid),
            "sleep grandchild must be alive before sweep"
        );

        // Invoke the new helper. Should kill the entire process group.
        sweep_child_tree("sweep-test", &registry);

        // Reap the shell child so kill(pid, 0) doesn't see it as a zombie.
        // Without wait(), the shell shows as "alive" even after SIGKILL
        // because we are its parent and never collected its exit status.
        {
            let reg = &registry.lock();
            if let Some(h) = reg.get("sweep-test") {
                {
                    let mut c = h.child.lock();
                    let _ = c.wait();
                }
            }
        }

        assert!(
            !crate::process::is_pid_alive(shell_pid),
            "shell leader must be dead (reaped) after sweep"
        );
        assert!(
            !crate::process::is_pid_alive(sleep_pid),
            "sleep grandchild must die with the group (kill_process_tree semantics)"
        );
        let _ = std::fs::remove_file(&pid_file);
    }

    #[test]
    fn sweep_child_tree_unregistered_name_is_no_op() {
        // Sprint 21 F-NEW1: registry lookup miss must not panic. The PTY-EOF
        // path may race against an explicit handle_delete that already cleaned
        // up the registry entry — sweep should simply find nothing to kill.
        use parking_lot::Mutex;
        use std::collections::HashMap;
        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        // Should not panic, should not error.
        sweep_child_tree("does-not-exist", &registry);
    }

    /// §3.5.10 concurrent-state fixture: multi-threaded producer/consumer
    /// through crossbeam_channel (the production import). Asserts message
    /// ordering invariant — bounded channel preserves FIFO order.
    /// Production-path-coupled: uses real crossbeam_channel::bounded.
    #[test]
    fn crossbeam_channel_concurrent_ordering() {
        let (tx, rx) = crossbeam_channel::bounded::<usize>(16);
        let n = 100;

        // Producer thread sends 0..n in order.
        let handle = std::thread::spawn(move || {
            for i in 0..n {
                tx.send(i).expect("send");
            }
            // tx drops here, closing the channel.
        });

        // Consumer drains concurrently (bounded channel blocks producer
        // after 16 items, so consumer must run in parallel).
        let mut received = Vec::with_capacity(n);
        loop {
            match rx.recv_timeout(std::time::Duration::from_secs(5)) {
                Ok(v) => received.push(v),
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    panic!("recv timed out after 5s — deadlock?");
                }
            }
        }

        handle.join().expect("producer");

        // FIFO ordering preserved.
        assert_eq!(received.len(), n);
        for (i, &v) in received.iter().enumerate() {
            assert_eq!(v, i, "message {i} out of order");
        }
    }

    // ── Sprint 46 P2: resolve_instance tests ────────────────────────────

    fn resolve_test_home(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static CTR: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "agend-resolve-{}-{}-{}",
            std::process::id(),
            name,
            CTR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn name_resolves_to_single_id() {
        let home = resolve_test_home("single");
        let id = crate::types::InstanceId::new();
        let yaml = format!(
            "defaults:\n  backend: claude\ninstances:\n  dev:\n    id: \"{}\"\n    role: Test\n",
            id.full()
        );
        std::fs::write(home.join("fleet.yaml"), yaml).unwrap();
        let (resolved_id, resolved_name) = resolve_instance(&home, "dev").unwrap();
        assert_eq!(resolved_id, id);
        assert_eq!(resolved_name, "dev");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn nonexistent_name_returns_not_found() {
        // fleet.yaml HashMap guarantees name uniqueness — no Ambiguous path.
        // Verify that a non-existent name returns NotFound.
        let home = resolve_test_home("notfound");
        let yaml = "defaults:\n  backend: claude\ninstances:\n  dev:\n    role: Test\n";
        std::fs::write(home.join("fleet.yaml"), yaml).unwrap();
        let result = resolve_instance(&home, "nonexistent");
        assert!(
            matches!(result, Err(ResolveError::NotFound(_))),
            "expected NotFound, got: {result:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
