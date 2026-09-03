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

    // #3499: `ctrlc`'s `termination` feature (Cargo.toml) bundles SIGINT,
    // SIGTERM AND SIGHUP into the ONE callback above — it cannot tell them
    // apart, so it registered SIGHUP as a shutdown trigger too. That is
    // wrong for a detached daemon: SIGHUP means "controlling terminal
    // closed" (or an explicit `kill -HUP`), neither of which is a reason
    // for a background daemon to exit. Installed AFTER `ctrlc::set_handler`
    // so this `sigaction` OVERRIDES its SIGHUP registration; SIGINT/SIGTERM
    // are untouched and keep going through `ctrlc`.
    //
    // A real handler function, NOT `SIG_IGN`: an ignored (SIG_IGN)
    // disposition is inherited across `exec` by every backend agent this
    // daemon spawns, which would silently change those children's SIGHUP
    // behavior too. An installed handler resets to SIG_DFL across `exec`,
    // so children are unaffected. `SA_RESTART` so a delivered SIGHUP
    // doesn't turn a blocking syscall in the daemon into an EINTR storm.
    //
    // The handler body does nothing at all (trivially async-signal-safe —
    // no `tracing` call, no allocation); the "operator can't see why it
    // died" visibility the issue asks for is covered by logging the policy
    // ONCE here, at install time, instead.
    #[cfg(unix)]
    unsafe {
        extern "C" fn ignore_sighup(_signum: libc::c_int) {}
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = ignore_sighup as *const () as libc::sighandler_t;
        libc::sigemptyset(&mut action.sa_mask);
        action.sa_flags = libc::SA_RESTART;
        if libc::sigaction(libc::SIGHUP, &action, std::ptr::null_mut()) != 0 {
            tracing::warn!(
                error = %std::io::Error::last_os_error(),
                "sigaction(SIGHUP) failed, daemon may not survive a lost controlling terminal"
            );
        } else {
            tracing::info!(
                "SIGHUP is ignored by this daemon (detached background process; \
                 SIGHUP is not a shutdown signal here) — see issue #3499"
            );
        }
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
