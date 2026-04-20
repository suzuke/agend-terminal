//! Unified signal handler installation.
//!
//! Ctrlc is configured with the `termination` feature in Cargo.toml, so the
//! single handler responds to SIGINT, SIGTERM, and SIGHUP on Unix and to
//! CTRL_C_EVENT / CTRL_BREAK_EVENT on Windows. Both `daemon::run_core` and
//! app's Owned path call [`install`] so their shutdown semantics match.
//!
//! The handler:
//! 1. Writes `AGEND_CTRLC_SENTINEL` (debugging aid on Windows where the
//!    daemon's console may be detached).
//! 2. Sets the `shutdown` flag so every cooperating thread sees it.
//! 3. Best-effort sends on `shutdown_tx` to wake any select! blocked on the
//!    main loop — avoids a 10s wait for the next tick.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Install the process-wide signal handler. Called once per process.
/// Subsequent calls return an error from `ctrlc::set_handler` (already set);
/// we log and continue rather than fail hard.
pub fn install(shutdown: Arc<AtomicBool>, shutdown_tx: crossbeam::channel::Sender<()>) {
    // Windows: re-enable CTRL+C delivery in case something (inherited parent
    // state, a dependency's init) has set the per-process "ignore CTRL+C"
    // flag. Without this, `SetConsoleCtrlHandler` routines are skipped
    // entirely for CTRL_C_EVENT and the daemon appears unresponsive to
    // Ctrl+C (while `agend-terminal stop` still works). CTRL_BREAK_EVENT is
    // unaffected by the flag — that's how the bug was isolated.
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
        SetConsoleCtrlHandler(None, 0);
    }

    if let Err(e) = ctrlc::set_handler(move || {
        if let Ok(path) = std::env::var("AGEND_CTRLC_SENTINEL") {
            let _ = std::fs::write(
                &path,
                format!("fired at {:?}\n", std::time::SystemTime::now()),
            );
        }
        tracing::info!("shutting down (signal received)");
        shutdown.store(true, Ordering::Relaxed);
        let _ = shutdown_tx.try_send(());
    }) {
        tracing::warn!(error = %e, "signal handler install failed, use `stop`");
    }
}
