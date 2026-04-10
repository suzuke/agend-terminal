//! Agent state and PTY management.
//!
//! Sync design: std::thread for PTY I/O, crossbeam broadcast for output distribution.
//! Single Mutex on AgentCore ensures atomic subscribe+dump.

use crate::backend::Backend;
use crate::state::StateTracker;
use crate::vterm::VTerm;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

#[allow(dead_code)]
pub type PtyWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// Core state for one agent — protected by a single Mutex for atomic operations.
pub struct AgentCore {
    pub vterm: VTerm,
    pub subscribers: Vec<crossbeam::channel::Sender<Vec<u8>>>,
    pub state: StateTracker,
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
}

pub type AgentRegistry = Arc<Mutex<HashMap<String, AgentHandle>>>;

/// ANSI escape sequence stripper for dialog detection.
/// Public ANSI strip for capture command.
pub fn strip_ansi_pub(s: &str) -> String { strip_ansi(s) }

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
pub fn spawn_agent(
    name: &str,
    command: &str,
    args: &[String],
    cols: u16,
    rows: u16,
    env: Option<&HashMap<String, String>>,
    working_dir: Option<&std::path::Path>,
    submit_key: &str,
    registry: &AgentRegistry,
    home: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| anyhow::anyhow!("Failed to open PTY: {e}"))?;

    let mut cmd = CommandBuilder::new(command);
    cmd.args(args);
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("FORCE_COLOR", "1");
    cmd.env("AGEND_INSTANCE_NAME", name);

    if std::env::var("LANG").is_err() {
        cmd.env("LANG", "en_US.UTF-8");
    }

    // User env
    if let Some(env_map) = env {
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
        vterm: VTerm::new(cols, rows),
        subscribers: Vec::new(),
        state: StateTracker::new(detected_backend.as_ref()),
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
            },
        );
    }

    // PTY read thread — feeds VTerm + broadcasts + auto-dismiss trust dialog + session reaper
    let core2 = Arc::clone(&core);
    let pw = Arc::clone(&pty_writer);
    let reg_for_reaper = Arc::clone(registry);
    let home_for_reaper = home.map(|p| p.to_path_buf());
    let n = name.to_string();
    std::thread::Builder::new()
        .name(format!("{n}_pty_read"))
        .spawn(move || {
            let mut buf = [0u8; 8192];
            let mut detect_buf = Vec::with_capacity(4096);
            let mut dialog_dismissed = false;
            loop {
                match pty_reader.read(&mut buf) {
                    Ok(0) => {
                        eprintln!("[{n}] PTY closed — reaping session");
                        // Session reaper: remove from registry + cleanup socket
                        if let Ok(mut reg) = reg_for_reaper.lock() {
                            reg.remove(&n);
                        }
                        if let Some(ref home) = home_for_reaper {
                            let sock = crate::daemon::agent_socket_path(home, &n);
                            let _ = std::fs::remove_file(&sock);
                        }
                        break;
                    }
                    Ok(n_bytes) => {
                        let data = &buf[..n_bytes];

                        // Auto-dismiss trust dialog
                        if !dialog_dismissed {
                            detect_buf.extend_from_slice(data);
                            if detect_buf.len() > 8192 {
                                let d = detect_buf.len() - 8192;
                                detect_buf.drain(..d);
                            }
                            let clean = strip_ansi(&String::from_utf8_lossy(&detect_buf));
                            if clean.contains("Yes, I trust")
                                || clean.contains("Yes, proceed")
                            {
                                eprintln!("[{n}] auto-dismissing trust dialog");
                                let _ = pw
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .write_all(b"\x1b[A\x1b[A\r");
                                dialog_dismissed = true;
                                detect_buf.clear();
                            }
                        }

                        // Feed VTerm + state detection + broadcast (under same lock = atomic)
                        {
                            let mut core =
                                core2.lock().unwrap_or_else(|e| e.into_inner());
                            core.vterm.process(data);
                            // State detection: strip ANSI, feed to tracker
                            let stripped = strip_ansi(&String::from_utf8_lossy(data));
                            core.state.feed(&stripped);
                            // Send to each subscriber, remove dead ones
                            core.subscribers
                                .retain(|tx| tx.send(data.to_vec()).is_ok());
                        }
                    }
                    Err(_) => break,
                }
            }
        })?;

    eprintln!("[{name}] spawned: {command}");
    Ok(())
}

/// Write data to an agent's PTY (atomic write).
#[allow(dead_code)]
pub fn write_to_agent(agent: &AgentHandle, data: &[u8]) -> anyhow::Result<()> {
    let mut w = agent.pty_writer.lock().unwrap_or_else(|e| e.into_inner());
    w.write_all(data)?;
    w.flush()?;
    Ok(())
}

/// Resize an agent's PTY + VTerm.
#[allow(dead_code)]
pub fn resize_agent(agent: &AgentHandle, cols: u16, rows: u16) -> anyhow::Result<()> {
    let master = agent.pty_master.lock().unwrap_or_else(|e| e.into_inner());
    master
        .resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| anyhow::anyhow!("resize: {e}"))?;
    let mut core = agent.core.lock().unwrap_or_else(|e| e.into_inner());
    core.vterm.resize(cols, rows);
    Ok(())
}

/// Get atomic subscribe + screen dump (under core lock — no output gap).
/// Creates a new per-subscriber channel. Each subscriber gets ALL output (broadcast).
pub fn subscribe_with_dump(agent: &AgentHandle) -> (crossbeam::channel::Receiver<Vec<u8>>, Vec<u8>) {
    let mut core = agent.core.lock().unwrap_or_else(|e| e.into_inner());
    let dump = core.vterm.dump_screen();
    let (tx, rx) = crossbeam::channel::unbounded();
    core.subscribers.push(tx);
    (rx, dump)
}
