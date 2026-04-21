//! Agent state and PTY management.
//!
//! Sync design: std::thread for PTY I/O, crossbeam broadcast for output distribution.
//! Single Mutex on AgentCore ensures atomic subscribe+dump.

use crate::backend::Backend;
use crate::health::HealthTracker;
use crate::state::StateTracker;
use crate::vterm::VTerm;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub type PtyWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// Core state for one agent — protected by a single Mutex for atomic operations.
pub struct AgentCore {
    pub vterm: VTerm,
    pub subscribers: Vec<crossbeam::channel::Sender<Vec<u8>>>,
    pub state: StateTracker,
    pub health: HealthTracker,
}

/// Handle to interact with an agent.
#[allow(dead_code)]
pub struct AgentHandle {
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
) -> std::sync::MutexGuard<'_, HashMap<String, ExternalAgentHandle>> {
    crate::sync::lock_poisoned(reg, "agent_registry")
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

/// Lock the agent registry, recovering from poison.
pub fn lock_registry(
    reg: &AgentRegistry,
) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, AgentHandle>> {
    crate::sync::lock_poisoned(reg, "agent_registry")
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
                            if ch == 'C' || ch == 'D' {
                                out.push(' ');
                            }
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(&ch) = chars.peek() {
                        chars.next();
                        if ch == '\x07' || ch == '\\' {
                            break;
                        }
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

/// Spawn an agent with PTY and register in registry.
/// Channel for crash events from reaper to daemon.
pub type CrashChannel = crossbeam::channel::Sender<String>;

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

pub fn spawn_agent(config: &SpawnConfig, registry: &AgentRegistry) -> anyhow::Result<()> {
    let SpawnConfig {
        name,
        backend_command,
        args,
        spawn_mode,
        cols,
        rows,
        env,
        working_dir,
        submit_key,
        home,
        crash_tx,
        shutdown,
    } = config;
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: *rows,
            cols: *cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| anyhow::anyhow!("Failed to open PTY: {e}"))?;

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
            // No `home` means no allow-list to validate against. All
            // production spawn paths thread `home` through `SpawnConfig`; the
            // only call sites that legitimately pass `None` are ad-hoc test
            // spawns (tests/integration.rs). Emit a warn so that if a future
            // code path regresses and spawns without home, the lost
            // symlink-escape guard shows up in logs instead of silently
            // degrading. Tests suppress tracing output so this stays quiet
            // under `cargo test` while still being visible in a live daemon.
            tracing::warn!(
                instance = %name,
                dir = %dir.display(),
                "spawn without AGEND_HOME — working_directory symlink recheck skipped"
            );
            std::fs::create_dir_all(dir).ok();
            cmd.cwd(dir);
        }
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| anyhow::anyhow!("Failed to spawn '{backend_command}': {e}"))?;
    drop(pair.slave);

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
        let mut reg = crate::sync::lock_poisoned(registry, "agent_registry");
        reg.insert(
            name.to_string(),
            AgentHandle {
                name: name.to_string(),
                backend_command: backend_command.to_string(),
                pty_writer: Arc::clone(&pty_writer),
                pty_master: Arc::clone(&pty_master),
                core: Arc::clone(&core),
                child: Arc::new(Mutex::new(child)),
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

    tracing::info!(agent = name, backend = backend_command, args = %args.join(" "), "spawned");
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
    let spawn_result = std::thread::Builder::new().name(thread_name).spawn(move || {
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
                let reg = crate::sync::lock_poisoned(&registry, "agent_registry");
                match reg.get(&name) {
                    Some(h) => {
                        let core = crate::sync::lock_poisoned(&h.core, "agent_core");
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

        let reg = crate::sync::lock_poisoned(&registry, "agent_registry");
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
    let mut detect_buf = Vec::with_capacity(4096);
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

                // Auto-dismiss trust/update dialogs (cooldown: 10s after last dismiss)
                let in_cooldown = dismiss_cooldown_until
                    .map(|t| std::time::Instant::now() < t)
                    .unwrap_or(false);
                if !in_cooldown
                    && try_dismiss_dialog(name, data, &mut detect_buf, pty_writer, dismiss_patterns)
                {
                    dismiss_cooldown_until =
                        Some(std::time::Instant::now() + std::time::Duration::from_secs(10));
                }

                // Feed VTerm + state detection + broadcast (under same lock = atomic)
                {
                    let mut c = crate::sync::lock_poisoned(core, "agent_core");
                    c.vterm.process(data);
                    // Detection runs against the current vterm screen. The
                    // grid already has ANSI resolved, so state.feed() gets
                    // plain user-visible text. Hash-dedup inside feed()
                    // skips cycles where the screen didn't change.
                    let rows = c.vterm.rows() as usize;
                    let screen = c.vterm.tail_lines(rows);
                    c.state.feed(&screen);
                    c.subscribers.retain(|tx| tx.send(data.to_vec()).is_ok());
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
        if let Ok(mut reg) = registry.lock() {
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
        let reg = crate::sync::lock_poisoned(registry, "agent_registry");
        // Agent removed from registry → shutdown or explicit delete. Not a crash.
        if reg.get(name).is_none() {
            tracing::debug!(agent = name, "not in registry, skipping crash handling");
            return;
        }
        if let Some(handle) = reg.get(name) {
            if let Ok(mut c) = handle.child.lock() {
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
        Some(0) => false, // Graceful exit
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

    // In daemon mode, ALL unexpected exits trigger respawn — even exit 0.
    // An agent exiting on its own (not via `kill` or shutdown) is unexpected.
    // Only daemon shutdown and explicit `kill` skip respawn (handled above).
    if is_crash {
        tracing::info!(agent = name, "setting restarting state");
    } else {
        tracing::warn!(
            agent = name,
            "unexpected exit (code 0), treating as crash for respawn"
        );
    }
    // Set Restarting state (don't remove from registry)
    if let Ok(reg) = registry.lock() {
        if let Some(handle) = reg.get(name) {
            if let Ok(mut core) = handle.core.lock() {
                core.state.set_restarting();
            }
        }
    }
    if let Some(ref tx) = crash_tx {
        // Non-blocking send: the channel is bounded (see daemon::run), so a
        // stalled reaper must not wedge this PTY close handler. Dropping a
        // crash event means one agent skips auto-respawn — the next health
        // probe will still catch a persistent failure.
        if let Err(e) = tx.try_send(name.to_string()) {
            tracing::warn!(agent = %name, error = %e, "crash channel full — respawn event dropped");
        }
    }
}

/// Try to auto-dismiss dialogs using backend-configurable patterns. Returns true if dismissed.
pub fn try_dismiss_dialog(
    name: &str,
    data: &[u8],
    detect_buf: &mut Vec<u8>,
    pty_writer: &PtyWriter,
    dismiss_patterns: &[(String, Vec<u8>)],
) -> bool {
    if dismiss_patterns.is_empty() {
        return false;
    }
    detect_buf.extend_from_slice(data);
    if detect_buf.len() > 8192 {
        let d = detect_buf.len() - 8192;
        detect_buf.drain(..d);
    }
    let clean = strip_ansi(&String::from_utf8_lossy(detect_buf));

    for (pattern, key_seq) in dismiss_patterns {
        if clean.contains(pattern.as_str()) {
            tracing::info!(agent = name, pattern, "auto-dismissing dialog");
            // Delayed write: TUI escape-sequence parsers need time to distinguish
            // \x1b (ESC key) from \x1b[ (CSI start).  Writing immediately causes
            // Ink-based TUIs (kiro-cli) to interpret \x1b as "ESC to cancel".
            let writer = Arc::clone(pty_writer);
            let keys = key_seq.clone();
            let agent = name.to_string();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(300));
                // Send keys in chunks split on \r/\n boundaries with delay between,
                // so TUI frameworks process navigation before confirmation.
                let mut w = crate::sync::lock_poisoned(&writer, "pty_writer");
                let mut start = 0;
                for (i, &b) in keys.iter().enumerate() {
                    if b == b'\r' || b == b'\n' {
                        // Send everything up to (not including) this Enter
                        if start < i {
                            let _ = w.write_all(&keys[start..i]);
                            let _ = w.flush();
                            drop(w);
                            std::thread::sleep(std::time::Duration::from_millis(200));
                            w = crate::sync::lock_poisoned(&writer, "pty_writer");
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
            });
            detect_buf.clear();
            return true;
        }
    }

    false
}

/// Write data to an agent's PTY (atomic write — for attach path).
pub fn write_to_agent(agent: &AgentHandle, data: &[u8]) -> crate::error::Result<()> {
    let mut w = crate::sync::lock_poisoned(&agent.pty_writer, "pty_writer");
    w.write_all(data)
        .map_err(crate::error::AgendError::PtyWrite)?;
    w.flush().map_err(crate::error::AgendError::PtyWrite)?;
    Ok(())
}

/// Write data to an agent's PTY byte-by-byte with small delays.
#[allow(dead_code)]
pub fn write_to_agent_typed(agent: &AgentHandle, data: &[u8]) -> crate::error::Result<()> {
    let mut w = crate::sync::lock_poisoned(&agent.pty_writer, "pty_writer");
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
    let mut w = crate::sync::lock_poisoned(&agent.pty_writer, "pty_writer");

    // Write prefix + text
    if agent.typed_inject {
        for byte in prefix.iter().chain(text.iter()) {
            w.write_all(&[*byte])?;
            w.flush()?;
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    } else {
        if !prefix.is_empty() {
            w.write_all(prefix)?;
            w.flush()?;
        }
        w.write_all(text)?;
        w.flush()?;
    }

    // Delay before submit
    std::thread::sleep(std::time::Duration::from_millis(20));

    // Write submit key
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
    let reg = lock_registry(registry);
    let targets: Vec<String> = reg
        .iter()
        .filter(|(name, handle)| {
            (exclude != Some(name.as_str()))
                && crate::backend::Backend::from_command(&handle.backend_command).is_some()
        })
        .map(|(name, handle)| {
            let _ = inject_to_agent(handle, msg_bytes);
            name.clone()
        })
        .collect();
    targets
}

/// Get atomic subscribe + screen dump (under core lock — no output gap).
/// Creates a new per-subscriber channel. Each subscriber gets ALL output (broadcast).
pub fn subscribe_with_dump(
    agent: &AgentHandle,
) -> (crossbeam::channel::Receiver<Vec<u8>>, Vec<u8>) {
    let mut core = crate::sync::lock_poisoned(&agent.core, "agent_core");
    let dump = core.vterm.dump_screen();
    let (tx, rx) = crossbeam::channel::unbounded();
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
}
