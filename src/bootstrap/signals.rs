//! Unified signal handler installation.
//!
//! The background daemon uses the `ctrlc` crate with the `termination`
//! feature, bundling SIGINT + SIGTERM + SIGHUP on Unix and CTRL_C_EVENT /
//! CTRL_BREAK_EVENT on Windows.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Install the process-wide signal handler. Called once per process.
/// Subsequent calls return an error from `ctrlc::set_handler` (already set);
/// we log and continue rather than fail hard.
pub fn install(shutdown: Arc<AtomicBool>, shutdown_tx: crossbeam_channel::Sender<()>) {
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
        tracing::info!("shutting down (signal received)");
        // Sprint 57 Wave 3 PR-2 (#548 Q6): record reason taxonomy
        // BEFORE flipping the shutdown flag so the shutdown sequence
        // sees the right value when it reads.
        crate::daemon::record_shutdown_reason(crate::daemon::ShutdownReason::Signal);
        shutdown.store(true, Ordering::Relaxed);
        let _ = shutdown_tx.try_send(());
    }) {
        tracing::warn!(error = %e, "signal handler install failed, use `stop`");
    }
}
