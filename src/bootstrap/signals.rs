//! Unified signal handler installation.
//!
//! Two entry points:
//!
//! - [`install`] — for the background daemon. Uses the `ctrlc` crate with the
//!   `termination` feature, bundling SIGINT + SIGTERM + SIGHUP on Unix and
//!   CTRL_C_EVENT / CTRL_BREAK_EVENT on Windows.
//! - [`install_term_only`] — for the app's Owned path. Handles SIGTERM on
//!   Unix and CTRL_CLOSE/SHUTDOWN/LOGOFF on Windows **only**, leaving
//!   SIGINT / CTRL_C_EVENT to the default handler so ratatui's raw mode can
//!   still deliver Ctrl+C as 0x03 to the focused pane's PTY.
//!
//! Both handlers:
//! 1. Write `AGEND_CTRLC_SENTINEL` if set (debugging aid on Windows where
//!    the daemon's console may be detached).
//! 2. Set the `shutdown` flag so every cooperating thread sees it.
//! 3. Wake the main loop via `shutdown_tx` (daemon) / polling (app).

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

/// Shared flag set by the SIGTERM-only handler. Polled by [`term_requested`].
/// A static is required because Unix signal handlers are `extern "C" fn` and
/// cannot capture closures.
static TERM_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Returns `true` iff the SIGTERM-only handler has fired since install.
/// The app's main loop polls this each iteration and breaks on `true`.
pub fn term_requested() -> bool {
    TERM_REQUESTED.load(Ordering::Relaxed)
}

/// Install a handler for SIGTERM-class signals **only** — the app variant.
///
/// Unix: `sigaction(SIGTERM)` with a minimal handler that flips
/// [`TERM_REQUESTED`]. SIGINT and SIGHUP are left alone so ratatui can keep
/// delivering Ctrl+C to the focused pane's PTY and shell-exit (SIGHUP) keeps
/// the default "kill the process group" semantics.
///
/// Windows: `SetConsoleCtrlHandler` filtering to `CTRL_CLOSE_EVENT`,
/// `CTRL_LOGOFF_EVENT`, `CTRL_SHUTDOWN_EVENT`. Returns `FALSE` for
/// `CTRL_C_EVENT` / `CTRL_BREAK_EVENT` so ratatui's crossterm reader keeps
/// seeing them as KeyEvents.
pub fn install_term_only() {
    #[cfg(unix)]
    unsafe {
        install_unix_sigterm();
    }
    #[cfg(windows)]
    unsafe {
        install_windows_close();
    }
}

#[cfg(unix)]
unsafe fn install_unix_sigterm() {
    extern "C" fn handler(_signum: libc::c_int) {
        // Signal-handler-safe: only atomic ops, no allocation, no tracing.
        TERM_REQUESTED.store(true, Ordering::Relaxed);
    }
    let mut action: libc::sigaction = std::mem::zeroed();
    action.sa_sigaction = handler as *const () as libc::sighandler_t;
    libc::sigemptyset(&mut action.sa_mask);
    // SA_RESTART keeps blocking syscalls (e.g. crossterm's event::read)
    // transparent to callers — they retry instead of returning EINTR.
    action.sa_flags = libc::SA_RESTART;
    if libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut()) != 0 {
        tracing::warn!(
            error = %std::io::Error::last_os_error(),
            "sigaction(SIGTERM) failed, app will not shut down on `stop`"
        );
    }
}

#[cfg(windows)]
unsafe fn install_windows_close() {
    use windows_sys::Win32::Foundation::{BOOL, FALSE, TRUE};
    use windows_sys::Win32::System::Console::{
        SetConsoleCtrlHandler, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
    };

    unsafe extern "system" fn handler(ctrl_type: u32) -> BOOL {
        match ctrl_type {
            CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => {
                TERM_REQUESTED.store(true, Ordering::Relaxed);
                TRUE
            }
            // Leave CTRL_C_EVENT / CTRL_BREAK_EVENT to default / crossterm.
            _ => FALSE,
        }
    }

    if SetConsoleCtrlHandler(Some(handler), 1) == 0 {
        tracing::warn!(
            error = %std::io::Error::last_os_error(),
            "SetConsoleCtrlHandler failed, app will not shut down on close"
        );
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// Raising SIGTERM against our own process must flip `term_requested`.
    /// Run single-threaded because `TERM_REQUESTED` is process-global.
    #[test]
    #[ignore = "mutates process-global SIGTERM disposition; run explicitly"]
    fn install_term_only_catches_sigterm() {
        assert!(!term_requested(), "flag must start clear");
        install_term_only();
        unsafe {
            assert_eq!(libc::raise(libc::SIGTERM), 0, "raise SIGTERM");
        }
        // Give the signal handler a moment on slow CI.
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(term_requested(), "handler must have flipped the flag");
    }
}
