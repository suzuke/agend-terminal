//! Bisect tool for the Windows 11 Insider Dev 26200 ConPTY regression.
//!
//! See `docs/HANDOVER-windows-conpty-nested.md`. `pty_smoke v1` (baseline, no
//! glue) gets 20 bytes of cmd.exe banner on 26200. `agend-terminal daemon`
//! gets 0 bytes. This binary incrementally adds the structural pieces of
//! `src/agent.rs::spawn_agent` until the output drops to 0 — the step that
//! triggers the drop is the culprit.
//!
//! Usage (Windows only):
//!   cargo build --release --example pty_smoke
//!   AGEND_SMOKE_MODE=v1 target/release/examples/pty_smoke
//!   AGEND_SMOKE_MODE=v2 target/release/examples/pty_smoke
//!   ...
//!   AGEND_SMOKE_MODE=v8 target/release/examples/pty_smoke
//!
//! AGEND_SMOKE_OUT=path.log writes the verdict to a file (for DETACHED_PROCESS
//! testing where stdout is lost). Default output goes to stdout + stderr.
//!
//! Variants (additive; each includes all prior setup):
//!   v1: baseline — openpty, spawn, drop slave, clone reader, read on main
//!   v2: v1 + take_writer() between spawn and clone_reader
//!   v3: v2 + move master into Arc<Mutex<Box<dyn MasterPty + Send>>>
//!   v4: v3 + spawn the read loop into a dedicated thread
//!   v5: v4 + set env vars agend-terminal sets (TERM, COLORTERM, FORCE_COLOR,
//!       AGEND_INSTANCE_NAME, LANG)
//!   v6: v5 + prepend current_exe().parent() to PATH (mimics spawn_agent)
//!   v7: v6 + hold a fs2 exclusive lock on a .lock file during spawn (mimics
//!       daemon lock)
//!   v8: v7 + bind a TcpListener on 127.0.0.1:0 before spawn (mimics API /
//!       TUI socket server setup — even though real daemon binds these AFTER
//!       spawn, this tests whether extra handles in-process affect reads)

#![cfg(windows)]

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::env;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const READ_BUDGET_SECS: u64 = 10;

fn main() -> anyhow::Result<()> {
    let mode = env::var("AGEND_SMOKE_MODE").unwrap_or_else(|_| "v1".into());
    let out_path = env::var("AGEND_SMOKE_OUT").ok().map(PathBuf::from);

    let start = Instant::now();
    let timeout = Duration::from_secs(READ_BUDGET_SECS + 3);
    std::thread::spawn(move || {
        std::thread::sleep(timeout);
        eprintln!("[smoke] hard timeout — forcing exit");
        std::process::exit(2);
    });

    // --- v7 setup: hold a fs2 exclusive lock on a tempfile during spawn
    let _lock_guard = if variant_ge(&mode, "v7") {
        let p = std::env::temp_dir().join("pty_smoke_v7.lock");
        let f = std::fs::File::create(&p)?;
        fs2::FileExt::try_lock_exclusive(&f).ok();
        Some(f)
    } else {
        None
    };

    // --- v8 setup: bind an extra TcpListener on loopback
    let _tcp_guard = if variant_ge(&mode, "v8") {
        Some(std::net::TcpListener::bind("127.0.0.1:0")?)
    } else {
        None
    };

    // --- PTY setup (match minimal: rows=30)
    let pty = native_pty_system();
    let pair = pty.openpty(PtySize {
        rows: 30,
        cols: 120,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    // Build command. v5+ adds env vars, v6+ adds PATH manipulation.
    let mut cmd = CommandBuilder::new("cmd.exe");
    if variant_ge(&mode, "v5") {
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("FORCE_COLOR", "1");
        cmd.env("AGEND_INSTANCE_NAME", "smoke");
        if std::env::var("LANG").is_err() {
            cmd.env("LANG", "en_US.UTF-8");
        }
    }
    if variant_ge(&mode, "v6") {
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
    }

    let child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);

    // v2+: take_writer
    let _writer_guard: Option<Box<dyn Write + Send>> = if variant_ge(&mode, "v2") {
        Some(pair.master.take_writer()?)
    } else {
        None
    };

    let pty_reader = pair.master.try_clone_reader()?;

    // v3+: move master into Arc<Mutex<Box<dyn MasterPty + Send>>>.
    // IMPORTANT: v1/v2 keep `pair.master` on the stack (NOT dropped) because
    // dropping it calls `ClosePseudoConsole`, tearing the session down and
    // giving the reader immediate EOF. spawn_agent holds master alive via the
    // Arc all along — match that invariant in every variant.
    let (master_stack, _master_arc) = if variant_ge(&mode, "v3") {
        (None, Some(Arc::new(Mutex::new(pair.master))))
    } else {
        (Some(pair.master), None)
    };
    let _keep_master_alive = master_stack;
    let _keep_arc_alive: Option<Arc<Mutex<Box<dyn MasterPty + Send>>>> = _master_arc;

    let (total_bytes, snippet, err) = if variant_ge(&mode, "v4") {
        read_in_thread(pty_reader, start)
    } else {
        read_on_main(pty_reader, start)
    };

    let _ = child_kill(child);

    let verdict = if total_bytes == 0 {
        "SILENT"
    } else if total_bytes < 40 {
        "PARTIAL"
    } else {
        "OK"
    };
    let summary = format!(
        "mode={mode} total_bytes={total_bytes} verdict={verdict} err={err:?} snippet={:?}",
        String::from_utf8_lossy(&snippet).replace(['\r', '\n'], " ")
    );
    println!("{summary}");
    if let Some(p) = out_path {
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(p) {
            let _ = writeln!(f, "{summary}");
        }
    }
    Ok(())
}

fn variant_ge(mode: &str, needle: &str) -> bool {
    let parse = |s: &str| -> u32 { s.trim_start_matches('v').parse().unwrap_or(0) };
    parse(mode) >= parse(needle)
}

fn read_on_main(mut reader: Box<dyn Read + Send>, start: Instant) -> (usize, Vec<u8>, Option<String>) {
    let mut total = 0usize;
    let mut snippet: Vec<u8> = Vec::new();
    let mut buf = [0u8; 2048];
    let budget = Duration::from_secs(READ_BUDGET_SECS);
    loop {
        if start.elapsed() >= budget {
            break;
        }
        match reader.read(&mut buf) {
            Ok(0) => return (total, snippet, Some("EOF".into())),
            Ok(n) => {
                total += n;
                if snippet.len() < 120 {
                    snippet.extend_from_slice(&buf[..n.min(120 - snippet.len())]);
                }
                if total >= 512 {
                    break;
                }
            }
            Err(e) => return (total, snippet, Some(e.to_string())),
        }
    }
    (total, snippet, None)
}

fn read_in_thread(reader: Box<dyn Read + Send>, start: Instant) -> (usize, Vec<u8>, Option<String>) {
    let (tx, rx) = std::sync::mpsc::channel::<(usize, Vec<u8>, Option<String>)>();
    std::thread::spawn(move || {
        let r = read_on_main(reader, start);
        let _ = tx.send(r);
    });
    let budget = Duration::from_secs(READ_BUDGET_SECS + 1);
    rx.recv_timeout(budget)
        .unwrap_or((0, Vec::new(), Some("thread read timeout".into())))
}

fn child_kill(mut child: Box<dyn portable_pty::Child + Send + Sync>) -> anyhow::Result<()> {
    let _ = child.kill();
    Ok(())
}
