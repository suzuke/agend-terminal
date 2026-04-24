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
        // First try SIGTERM to the process group
        unsafe {
            libc::kill(-(pid as i32), libc::SIGTERM);
        }
        // Brief grace period, then SIGKILL
        std::thread::sleep(std::time::Duration::from_millis(500));
        if is_pid_alive(pid) {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
    #[cfg(windows)]
    {
        // Windows: TerminateProcess on the leader (process groups work differently)
        terminate(pid);
    }
}
