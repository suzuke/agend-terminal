//! Unified signal handler installation.
//!
//! The daemon handles all termination signals. The TUI handles only
//! SIGTERM-class signals so Ctrl+C still reaches the focused pane.

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

static TERM_REQUESTED: AtomicBool = AtomicBool::new(false);

pub fn term_requested() -> bool {
    TERM_REQUESTED.load(Ordering::Relaxed)
}

pub fn install_term_only() {
    #[cfg(unix)]
    unsafe {
        extern "C" fn handler(_signum: libc::c_int) {
            TERM_REQUESTED.store(true, Ordering::Relaxed);
        }
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handler as *const () as libc::sighandler_t;
        libc::sigemptyset(&mut action.sa_mask);
        action.sa_flags = libc::SA_RESTART;
        if libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut()) != 0 {
            tracing::warn!(
                error = %std::io::Error::last_os_error(),
                "sigaction(SIGTERM) failed, app will not shut down cleanly"
            );
        }
    }

    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Console::{
            SetConsoleCtrlHandler, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
        };
        unsafe extern "system" fn handler(ctrl_type: u32) -> i32 {
            match ctrl_type {
                CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => {
                    TERM_REQUESTED.store(true, Ordering::Relaxed);
                    1
                }
                _ => 0,
            }
        }
        if SetConsoleCtrlHandler(Some(handler), 1) == 0 {
            tracing::warn!(
                error = %std::io::Error::last_os_error(),
                "SetConsoleCtrlHandler failed, app will not shut down cleanly"
            );
        }
    }
}
