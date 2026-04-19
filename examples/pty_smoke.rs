//! Minimal portable-pty 0.9.0 smoke test — spawn cmd.exe / /bin/bash, read up
//! to 2048 bytes of PTY output, print byte count + first 200 chars, exit.
//!
//! Used as the reference minimum-viable PTY client when debugging the 26200
//! ConPTY regression (see docs/HANDOVER-windows-conpty-nested.md). If this
//! works but `agend-terminal start` hangs, the fault is in our daemon glue,
//! not portable-pty.
//!
//! Build + run:
//!   cargo build --release --example pty_smoke
//!   target/release/examples/pty_smoke

#![cfg(windows)]

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::Read;
use std::time::{Duration, Instant};

fn main() -> anyhow::Result<()> {
    let pty = native_pty_system();
    let pair = pty.openpty(PtySize {
        rows: 30,
        cols: 120,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let cmd = CommandBuilder::new("cmd.exe");
    let mut child = pair.slave.spawn_command(cmd)?;
    // Drop slave so only child holds write end (mirrors normal PTY usage)
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader()?;

    let start = Instant::now();
    let mut total = 0usize;
    let mut snippet: Vec<u8> = Vec::new();
    let mut buf = [0u8; 2048];

    eprintln!("[smoke] spawned child, reading with 15s budget...");

    while start.elapsed() < Duration::from_secs(15) {
        match reader.read(&mut buf) {
            Ok(0) => {
                eprintln!("[smoke] EOF after {total} bytes");
                break;
            }
            Ok(n) => {
                total += n;
                if snippet.len() < 200 {
                    snippet.extend_from_slice(&buf[..n.min(200 - snippet.len())]);
                }
                eprintln!(
                    "[smoke] +{:.2}s read {n} bytes (cumulative {total})",
                    start.elapsed().as_secs_f32()
                );
                if total >= 2048 {
                    break;
                }
            }
            Err(e) => {
                eprintln!("[smoke] read err after {total}B: {e}");
                break;
            }
        }
    }

    let _ = child.kill();
    let _ = child.wait();

    println!("TOTAL_BYTES={total}");
    println!(
        "FIRST_SNIPPET={}",
        String::from_utf8_lossy(&snippet).replace(['\r', '\n'], " ")
    );

    if total == 0 {
        println!("VERDICT=SILENT (portable-pty broken in isolation)");
        std::process::exit(1);
    } else {
        println!("VERDICT=OUTPUT_RECEIVED (portable-pty works; bug is in agend-terminal glue)");
    }
    Ok(())
}
