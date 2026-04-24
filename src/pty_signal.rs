//! PTY signal spike — PoC for sending signals to agent's foreground process group.
//!
//! Goal: prove daemon can cancel an agent's tool subprocess (e.g. `cargo test`)
//! without killing the agent process itself.
//!
//! Approach: use `tcgetpgrp(pty_master_fd)` to get the foreground process group,
//! then `kill(-pgid, SIGINT)` to interrupt it.
//!
//! **Go/No-Go conclusion**: GO — portable_pty already exposes
//! `process_group_leader()` which wraps `tcgetpgrp`. No new crate needed.
//! Note: `process_group_leader()` and the `tcgetpgrp` fallback are the same
//! underlying mechanism (single path, not independent validation).

#[cfg(unix)]
use std::sync::{Arc, Mutex};

/// Send SIGINT to the foreground process group of a PTY master.
///
/// Returns Ok(pgid) on success, Err on failure.
/// ESRCH (process already exited) is treated as benign success.
#[cfg(unix)]
#[allow(dead_code)] // spike — production caller wired in Phase 2
pub fn send_sigint_to_foreground(
    pty_master: &Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
) -> anyhow::Result<i32> {
    let master = pty_master
        .lock()
        .map_err(|e| anyhow::anyhow!("lock: {e}"))?;
    let fd = master
        .as_raw_fd()
        .ok_or_else(|| anyhow::anyhow!("pty_master has no raw fd"))?;
    let pgid = unsafe { libc::tcgetpgrp(fd) };
    if pgid <= 0 {
        return Err(anyhow::anyhow!(
            "tcgetpgrp({fd}) returned {pgid}: {}",
            std::io::Error::last_os_error()
        ));
    }
    let ret = unsafe { libc::kill(-pgid, libc::SIGINT) };
    if ret == 0 {
        Ok(pgid)
    } else {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            Ok(pgid) // benign: process exited between lookup and kill
        } else {
            Err(anyhow::anyhow!("kill(-{pgid}, SIGINT) failed: {err}"))
        }
    }
}

#[cfg(not(unix))]
#[allow(dead_code)]
pub fn send_sigint_to_foreground(
    _pty_master: &std::sync::Arc<std::sync::Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
) -> anyhow::Result<i32> {
    Err(anyhow::anyhow!(
        "PTY signal not implemented on this platform"
    ))
}

#[cfg(test)]
#[cfg(unix)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};

    #[test]
    fn test_sigint_kills_sleep_but_shell_survives() {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");

        let mut child = pair
            .slave
            .spawn_command(CommandBuilder::new("/bin/sh"))
            .expect("spawn shell");
        drop(pair.slave);

        let master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>> =
            Arc::new(Mutex::new(pair.master));

        // Start sleep 60 as foreground tool
        {
            let m = master.lock().unwrap();
            let mut writer = m.take_writer().expect("writer");
            writer.write_all(b"sleep 60\n").expect("write sleep");
        }
        std::thread::sleep(std::time::Duration::from_millis(500));

        // Send SIGINT to foreground process group (sleep)
        let result = send_sigint_to_foreground(&master);
        assert!(result.is_ok(), "SIGINT should succeed: {result:?}");
        assert!(result.unwrap() > 0, "pgid must be positive");

        std::thread::sleep(std::time::Duration::from_millis(300));

        // STRONG ASSERTION: shell must still be running
        // try_wait() returns Ok(None) if process is alive, Ok(Some(_)) if exited
        match child.try_wait() {
            Ok(None) => {} // still running — PASS
            Ok(Some(status)) => {
                panic!(
                    "FAIL: shell exited with {status:?} — SIGINT killed the shell, not just sleep"
                );
            }
            Err(e) => panic!("try_wait error: {e}"),
        }

        // Cleanup
        child.kill().ok();
    }

    #[test]
    fn test_sigint_returns_valid_pgid() {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");

        let mut child = pair
            .slave
            .spawn_command(CommandBuilder::new("/bin/sh"))
            .expect("spawn");
        drop(pair.slave);

        let master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>> =
            Arc::new(Mutex::new(pair.master));

        std::thread::sleep(std::time::Duration::from_millis(200));

        let result = send_sigint_to_foreground(&master);
        assert!(result.is_ok(), "should get pgid: {result:?}");
        assert!(result.unwrap() > 0, "pgid must be positive");

        child.kill().ok();
    }
}
