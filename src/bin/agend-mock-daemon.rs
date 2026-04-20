//! Mock daemon used exclusively by the `self_healing_supervisor` integration
//! tests. Behaviour is controlled entirely by env vars and files in
//! `$AGEND_HOME`, so the same compiled binary can stand in for both "v1" and
//! "v2" (tests fabricate a second copy with trailing padding bytes so the
//! sha256 differs while behaviour stays identical).
//!
//! This binary is **not** shipped: it lives under `src/bin/` purely so
//! `cargo test` picks it up in `target/debug/` alongside `agend-supervisor`.
//!
//! Contract with the tests:
//! - `--version` prints a one-line version string (taken from `MOCK_VERSION`
//!   or a default); used by `client::probe_new_binary_version`.
//! - `AGEND_SELF_TEST=1` → exits 0, or 1 if `MOCK_FAIL_SELF_TEST=1` also set.
//! - Otherwise runs as a daemon under the supervisor:
//!   - Sends `Ready` ping via `agend_terminal::supervisor::client::notify_ready`.
//!   - Writes a sentinel file `$AGEND_HOME/mock-ready-<pid>` so tests can
//!     observe which pid(s) actually booted.
//!   - Reads `$AGEND_HOME/mock-crashes-remaining`: if the file exists and
//!     contains a positive integer N, decrements it, sleeps briefly so the
//!     supervisor has time to register the Ready ping, then exits 1 to
//!     simulate a crash. Used by the rollback test.
//!   - On SIGTERM, exits 0 (via `ctrlc` — the crate already enables the
//!     `termination` feature for the main daemon, so this just reuses it).

#[cfg(unix)]
fn main() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version") {
        let v = std::env::var("MOCK_VERSION").unwrap_or_else(|_| "agend-mock-daemon 0.0.0".into());
        println!("{v}");
        return;
    }

    if std::env::var("AGEND_SELF_TEST").ok().as_deref() == Some("1") {
        if std::env::var("MOCK_FAIL_SELF_TEST").ok().as_deref() == Some("1") {
            eprintln!("mock: self-test failed (MOCK_FAIL_SELF_TEST=1)");
            std::process::exit(1);
        }
        eprintln!("mock: self-test ok");
        return;
    }

    let version = std::env::var("MOCK_VERSION").unwrap_or_else(|_| "mock-0.0".into());

    let stopping = Arc::new(AtomicBool::new(false));
    {
        let s = Arc::clone(&stopping);
        let _ = ctrlc::set_handler(move || {
            s.store(true, Ordering::SeqCst);
        });
    }

    // Ready ping — synchronous; returns once supervisor has acked.
    if let Err(e) = agend_terminal::supervisor::client::notify_ready(std::process::id(), &version) {
        eprintln!("mock: notify_ready failed: {e:#}");
    }

    // Sentinel so tests can observe ready-ness.
    let home = std::env::var("AGEND_HOME")
        .ok()
        .map(std::path::PathBuf::from);
    if let Some(ref h) = home {
        let sentinel = h.join(format!("mock-ready-{}", std::process::id()));
        let _ = std::fs::write(&sentinel, &version);
    }

    // Crash-counter file: tests arm this before the upgrade to simulate a
    // flaky new daemon. Decrement-then-exit ensures N crashes across N
    // respawns, after which subsequent boots are clean.
    if let Some(ref h) = home {
        let crash_file = h.join("mock-crashes-remaining");
        if let Ok(content) = std::fs::read_to_string(&crash_file) {
            if let Ok(n) = content.trim().parse::<u32>() {
                if n > 0 {
                    let _ = std::fs::write(&crash_file, (n - 1).to_string());
                    // Delay slightly so the supervisor observes Ready before
                    // the exit — this exercises the stability-window path,
                    // not the pre-ready timeout path.
                    std::thread::sleep(Duration::from_millis(300));
                    eprintln!("mock: crashing (remaining was {n})");
                    std::process::exit(1);
                }
            }
        }
    }

    while !stopping.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!("mock: graceful stop");
}

#[cfg(not(unix))]
fn main() {
    eprintln!("agend-mock-daemon is Unix-only");
    std::process::exit(1);
}
