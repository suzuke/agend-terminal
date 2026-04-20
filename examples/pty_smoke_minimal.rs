//! Exact replica of the original pty_smoke that produced 20 bytes earlier.
//! No variant logic, no cleverness, nothing extra.
//!
//! If AGEND_SMOKE_OUT is set, writes the final byte count to that file too
//! (so DETACHED_PROCESS runs can be inspected even when stdout is gone).

#[cfg(not(windows))]
fn main() {
    eprintln!("pty_smoke_minimal is Windows-only");
}

#[cfg(windows)]
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
#[cfg(windows)]
use std::io::Read;
#[cfg(windows)]
use std::time::{Duration, Instant};

#[cfg(windows)]
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
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader()?;

    let start = Instant::now();
    let mut total = 0usize;
    let mut buf = [0u8; 2048];

    // Hard timeout via background thread (read is blocking)
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(12));
        eprintln!("[minimal] hard timeout");
        std::process::exit(2);
    });

    eprintln!("[minimal] spawned child, reading...");
    while start.elapsed() < Duration::from_secs(10) {
        match reader.read(&mut buf) {
            Ok(0) => {
                eprintln!("[minimal] EOF after {total} bytes");
                break;
            }
            Ok(n) => {
                total += n;
                eprintln!(
                    "[minimal] +{:.2}s read {n} bytes (cumulative {total})",
                    start.elapsed().as_secs_f32()
                );
                if total >= 2048 {
                    break;
                }
            }
            Err(e) => {
                eprintln!("[minimal] err after {total}B: {e}");
                break;
            }
        }
    }

    let _ = child.kill();
    let _ = child.wait();
    println!("MINIMAL total_bytes={total}");

    if let Ok(p) = std::env::var("AGEND_SMOKE_OUT") {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
        {
            use std::io::Write;
            let _ = writeln!(
                f,
                "MINIMAL total_bytes={total} launched_at={:?}",
                std::time::SystemTime::now()
            );
        }
    }
    Ok(())
}
