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
    pub command: String,
    pub pty_writer: PtyWriter,
    pub pty_master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    pub core: Arc<Mutex<AgentCore>>,
    pub child: Arc<Mutex<Box<dyn portable_pty::Child + Send>>>,
    pub submit_key: String,
    pub inject_prefix: String,
    pub typed_inject: bool,
}

pub type AgentRegistry = Arc<Mutex<HashMap<String, AgentHandle>>>;

/// Lock the agent registry, recovering from poison.
pub fn lock_registry(
    reg: &AgentRegistry,
) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, AgentHandle>> {
    reg.lock().unwrap_or_else(|e| e.into_inner())
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
pub struct SpawnConfig<'a> {
    pub name: &'a str,
    pub command: &'a str,
    pub args: &'a [String],
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
        command,
        args,
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

    let mut cmd = CommandBuilder::new(command);
    cmd.args(*args);
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("FORCE_COLOR", "1");
    cmd.env("AGEND_INSTANCE_NAME", name);

    if std::env::var("LANG").is_err() {
        cmd.env("LANG", "en_US.UTF-8");
    }

    // User env
    if let Some(env_map) = *env {
        for (k, v) in env_map {
            cmd.env(k, v);
        }
    }

    // Add agend-terminal binary to PATH
    if let Ok(exe) = std::env::current_exe() {
        if let Some(bin_dir) = exe.parent() {
            let current_path = std::env::var("PATH").unwrap_or_default();
            cmd.env("PATH", format!("{}:{current_path}", bin_dir.display()));
        }
    }

    if let Some(dir) = working_dir {
        std::fs::create_dir_all(dir).ok();
        cmd.cwd(dir);
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| anyhow::anyhow!("Failed to spawn '{command}': {e}"))?;
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

    let detected_backend = Backend::from_command(command);
    let core = Arc::new(Mutex::new(AgentCore {
        vterm: VTerm::new(*cols, *rows),
        subscribers: Vec::new(),
        state: StateTracker::new(detected_backend.as_ref()),
        health: HealthTracker::new(),
    }));

    // Register in registry
    {
        let mut reg = registry.lock().unwrap_or_else(|e| e.into_inner());
        reg.insert(
            name.to_string(),
            AgentHandle {
                name: name.to_string(),
                command: command.to_string(),
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

    tracing::info!(agent = name, command, args = %args.join(" "), "spawned");
    Ok(())
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
    let mut dialog_dismissed = false;

    loop {
        match pty_reader.read(&mut buf) {
            Ok(0) => {
                handle_pty_close(name, registry, home, crash_tx, shutdown);
                break;
            }
            Ok(n_bytes) => {
                let data = &buf[..n_bytes];

                // Auto-dismiss trust/update dialogs
                if !dialog_dismissed {
                    dialog_dismissed = try_dismiss_dialog(
                        name,
                        data,
                        &mut detect_buf,
                        pty_writer,
                        dismiss_patterns,
                    );
                }

                // Feed VTerm + state detection + broadcast (under same lock = atomic)
                {
                    let mut c = core.lock().unwrap_or_else(|e| e.into_inner());
                    c.vterm.process(data);
                    let stripped = strip_ansi(&String::from_utf8_lossy(data));
                    c.state.feed(&stripped);
                    c.subscribers.retain(|tx| tx.send(data.to_vec()).is_ok());
                }
            }
            Err(_) => break,
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
            let sock = crate::daemon::agent_socket_path(home, name);
            let _ = std::fs::remove_file(&sock);
        }
        return;
    }

    tracing::info!(agent = name, "PTY closed, waiting for process exit");

    // Wait up to 2s for process to fully exit
    let mut exit_code: Option<i32> = None;
    for _ in 0..20 {
        let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
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
        Some(0) => false,
        Some(c) => {
            tracing::warn!(agent = name, exit_code = c, "crash");
            true
        }
        None => {
            tracing::warn!(agent = name, "process didn't exit in 2s, treating as crash");
            true
        }
    };

    if is_crash {
        // Set Restarting state (don't remove from registry)
        if let Ok(reg) = registry.lock() {
            if let Some(handle) = reg.get(name) {
                if let Ok(mut core) = handle.core.lock() {
                    core.state.set_restarting();
                }
            }
        }
        if let Some(ref tx) = crash_tx {
            let _ = tx.send(name.to_string());
        }
    } else {
        tracing::info!(agent = name, "graceful exit (code 0), no respawn");
        if let Ok(mut reg) = registry.lock() {
            reg.remove(name);
        }
        if let Some(ref home) = home {
            let sock = crate::daemon::agent_socket_path(home, name);
            let _ = std::fs::remove_file(&sock);
        }
    }
}

/// Try to auto-dismiss dialogs using backend-configurable patterns. Returns true if dismissed.
fn try_dismiss_dialog(
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
            let _ = pty_writer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .write_all(key_seq);
            detect_buf.clear();
            return true;
        }
    }

    false
}

/// Write data to an agent's PTY (atomic write — for attach path).
pub fn write_to_agent(agent: &AgentHandle, data: &[u8]) -> crate::error::Result<()> {
    let mut w = agent.pty_writer.lock().unwrap_or_else(|e| e.into_inner());
    w.write_all(data)
        .map_err(crate::error::AgendError::PtyWrite)?;
    w.flush().map_err(crate::error::AgendError::PtyWrite)?;
    Ok(())
}

/// Write data to an agent's PTY byte-by-byte with small delays.
#[allow(dead_code)]
pub fn write_to_agent_typed(agent: &AgentHandle, data: &[u8]) -> crate::error::Result<()> {
    let mut w = agent.pty_writer.lock().unwrap_or_else(|e| e.into_inner());
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
    let mut w = agent.pty_writer.lock().unwrap_or_else(|e| e.into_inner());

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

/// Get atomic subscribe + screen dump (under core lock — no output gap).
/// Creates a new per-subscriber channel. Each subscriber gets ALL output (broadcast).
pub fn subscribe_with_dump(
    agent: &AgentHandle,
) -> (crossbeam::channel::Receiver<Vec<u8>>, Vec<u8>) {
    let mut core = agent.core.lock().unwrap_or_else(|e| e.into_inner());
    let dump = core.vterm.dump_screen();
    let (tx, rx) = crossbeam::channel::unbounded();
    core.subscribers.push(tx);
    (rx, dump)
}
