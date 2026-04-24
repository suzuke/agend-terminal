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
    #[cfg(unix)]
    {
        let pgid = -(pid as i32);
        // SIGTERM the entire process group
        unsafe {
            libc::kill(pgid, libc::SIGTERM);
        }
        // Grace period, then unconditional SIGKILL (handles grandchildren
        // that ignore SIGTERM even if leader already exited).
        std::thread::sleep(std::time::Duration::from_millis(500));
        unsafe {
            libc::kill(pgid, libc::SIGKILL);
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
        // Spawn a shell in its own process group (like PTY agents do).
        let mut child = unsafe {
            Command::new("sh")
                .args(["-c", "sleep 60 & wait"])
                .pre_exec(|| {
                    libc::setsid();
                    Ok(())
                })
                .spawn()
                .expect("spawn sh + sleep")
        };
        let pid = child.id();
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(is_pid_alive(pid), "shell must be alive before kill");

        kill_process_tree(pid);
        let _ = child.wait();

        assert!(
            !is_pid_alive(pid),
            "shell must be dead after kill_process_tree"
        );
    }
}
