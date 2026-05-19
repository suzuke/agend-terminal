//! Cross-platform process utilities.

#[cfg(not(any(unix, windows)))]
compile_error!("process module requires unix or windows");

/// Check if a process with the given PID is alive.
pub fn is_pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(windows)]
    {
        let handle = unsafe {
            windows_sys::Win32::System::Threading::OpenProcess(
                windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                pid,
            )
        };
        if handle.is_null() {
            return false;
        }
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(handle);
        }
        true
    }
}

/// Send SIGTERM to a process (Unix) or terminate it (Windows).
pub fn terminate(pid: u32) {
    if pid == 0 {
        tracing::warn!("terminate called with pid=0, skipping");
        return;
    }
    #[cfg(unix)]
    {
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }
    #[cfg(windows)]
    {
        let handle = unsafe {
            windows_sys::Win32::System::Threading::OpenProcess(
                windows_sys::Win32::System::Threading::PROCESS_TERMINATE,
                0,
                pid,
            )
        };
        if !handle.is_null() {
            unsafe {
                windows_sys::Win32::System::Threading::TerminateProcess(handle, 1);
                windows_sys::Win32::Foundation::CloseHandle(handle);
            }
        }
    }
}

/// Kill an entire process group. On Unix, sends SIGTERM to -pgid (all processes
/// in the group), then waits briefly and escalates to SIGKILL if still alive.
/// On Windows, falls back to TerminateProcess on the leader.
pub fn kill_process_tree(pid: u32) {
    if pid == 0 {
        tracing::warn!("kill_process_tree called with pid=0, skipping (would kill daemon)");
        return;
    }
    #[cfg(unix)]
    {
        // M2: query actual PGID instead of assuming PID==PGID
        let pgid = unsafe { libc::getpgid(pid as i32) };
        let kill_pgid = if pgid > 0 { -pgid } else { -(pid as i32) };
        // SIGTERM the entire process group
        unsafe {
            libc::kill(kill_pgid, libc::SIGTERM);
        }
        // Grace period, then unconditional SIGKILL (handles grandchildren
        // that ignore SIGTERM even if leader already exited).
        std::thread::sleep(std::time::Duration::from_millis(500));
        unsafe {
            libc::kill(kill_pgid, libc::SIGKILL);
        }
        // ESRCH (no such process) is fine — group already dead.
    }
    #[cfg(windows)]
    {
        terminate(pid);
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn test_kill_process_tree_kills_child_subprocess() {
        use std::os::unix::process::CommandExt;
        use std::process::Command;
        let pid_file =
            std::env::temp_dir().join(format!("agend-kill-test-{}.pid", std::process::id()));
        let cmd = format!("sleep 60 & echo $! > {} && wait", pid_file.display());
        let mut child = unsafe {
            Command::new("sh")
                .args(["-c", &cmd])
                .pre_exec(|| {
                    libc::setsid();
                    Ok(())
                })
                .spawn()
                .expect("spawn sh + sleep")
        };
        let shell_pid = child.id();
        // #773: poll until the PID file BOTH exists AND parses as a u32.
        // `pid_file.exists()` flips true the moment the shell opens the
        // file for writing — before `echo $! > file` flushes its content
        // — so the previous `for _ in 0..20 { if pid_file.exists() }`
        // loop could exit early and the subsequent `parse()` panic with
        // `ParseIntError { kind: Empty }` on an empty / partial read.
        // The combined budget (5s = 100 × 50ms) covers both spawn
        // latency AND write-flush race on a contended CI runner.
        let sleep_pid: u32 = {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                if let Ok(content) = std::fs::read_to_string(&pid_file) {
                    if let Ok(pid) = content.trim().parse::<u32>() {
                        break pid;
                    }
                }
                if std::time::Instant::now() >= deadline {
                    let last = std::fs::read_to_string(&pid_file).unwrap_or_default();
                    panic!(
                        "PID file never materialized with parseable content after 5s; \
                         last read: {last:?}"
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        };
        assert!(is_pid_alive(shell_pid), "shell must be alive before kill");
        assert!(
            is_pid_alive(sleep_pid),
            "sleep child must be alive before kill"
        );

        kill_process_tree(shell_pid);
        let _ = child.wait();

        // #934 sibling-fix: same race shape as `sweep_child_tree_body` in
        // src/agent.rs. After `kill_process_tree`, the grandchild `sleep`
        // is re-parented to init / launchd and exists as a zombie until
        // that new parent reaps it. `is_pid_alive` returns true for
        // zombies (libc::kill(pid, 0) succeeds), so a bare assert can
        // see "still alive" under CI scheduler contention. Replace with
        // poll-with-deadline (§3.20 SOP 1). shell_pid: 5s deadline (we
        // wait() directly); sleep_pid: 10s deadline (init reap window).
        assert!(
            crate::admin::cleanup_zombies::poll_until_dead(
                shell_pid,
                std::time::Duration::from_secs(5),
            ),
            "shell must be dead within 5s after kill (we wait() directly)"
        );
        assert!(
            crate::admin::cleanup_zombies::poll_until_dead(
                sleep_pid,
                std::time::Duration::from_secs(10),
            ),
            "sleep grandchild must die within 10s after kill_process_tree \
             (group kill semantics; 10s covers init / launchd reap latency)"
        );
        let _ = std::fs::remove_file(&pid_file);
    }

    #[test]
    fn kill_process_tree_with_pid_zero_is_noop() {
        // Should not panic or kill anything
        kill_process_tree(0);
    }
}
