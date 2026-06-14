//! Cross-platform process utilities.

#[cfg(not(any(unix, windows)))]
compile_error!("process module requires unix or windows");

/// Check if a process with the given PID is alive.
pub fn is_pid_alive(pid: u32) -> bool {
    // #1891 defense-in-depth: pid 0 is never a real tracked process. On Unix
    // `kill(0, 0)` targets the caller's whole process group (always succeeds →
    // false-"alive"); treat 0 as dead so a stray 0 pid can't masquerade as a
    // permanently-live agent. Mirrors the existing `pid == 0` guards below in
    // this module (kill_process_tree / process_group ops).
    if pid == 0 {
        return false;
    }
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

/// Cross-platform process start-time token: an identifier that is stable for
/// the lifetime of a single process but (almost certainly) differs from any
/// later process that the OS happens to recycle the same PID onto. Used to
/// distinguish "the daemon we recorded" from "an innocent process that landed
/// on the recycled PID" so a stale kill can't TOCTOU onto the wrong target
/// (CR-2026-06-14 zombie-kill identity-compare).
///
/// Returns `None` when the value can't be obtained (process gone, syscall /
/// parse failure, unsupported platform). Callers MUST treat `None` as
/// "identity unknown → fail closed" (do not signal). The absolute value is
/// opaque; only equality across two reads of the *same PID* is meaningful.
///
/// - **Linux**: `/proc/<pid>/stat` field 22 (`starttime`, clock ticks since
///   boot). The `comm` field (2) can contain spaces and parens, so parse the
///   tail after the final `)`.
/// - **macOS**: `proc_pidinfo(PROC_PIDTBSDINFO)` → `proc_bsdinfo.pbi_start_*`
///   (process start wall-clock, microsecond resolution).
/// - **Windows**: `GetProcessTimes` creation `FILETIME` (100ns ticks since
///   1601), the canonical per-process start instant.
pub fn process_start_token(pid: u32) -> Option<u64> {
    if pid == 0 {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // `pid (comm) state ppid ...` — comm may embed spaces/parens, so
        // split AFTER the last ')'. The remainder begins at field 3 (state),
        // so `starttime` (field 22) is index 22 - 3 = 19 of the tail.
        let tail = &stat[stat.rfind(')')? + 1..];
        tail.split_whitespace().nth(19)?.parse::<u64>().ok()
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: proc_pidinfo writes at most `size_of::<proc_bsdinfo>()` bytes
        // into our stack buffer; we pass that exact size and check the return.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let n = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                size,
            )
        };
        if n != size {
            return None;
        }
        Some(info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
        use windows_sys::Win32::System::Threading::{
            GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return None;
        }
        let mut creation: FILETIME = unsafe { std::mem::zeroed() };
        let mut exit: FILETIME = unsafe { std::mem::zeroed() };
        let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
        let mut user: FILETIME = unsafe { std::mem::zeroed() };
        let ok =
            unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
        unsafe {
            CloseHandle(handle);
        }
        if ok == 0 {
            return None;
        }
        Some(((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = pid;
        None
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
    fn is_pid_alive_zero_is_false_1891() {
        // #1891: pid 0 must never report alive — `kill(0, 0)` targets the
        // caller's whole process group (always succeeds), so without the guard a
        // stray 0 pid would masquerade as a permanently-live agent.
        assert!(!is_pid_alive(0), "pid 0 must be treated as dead");
        // Sanity: this test process IS alive at its real pid (guard didn't
        // over-reach into rejecting valid pids).
        assert!(
            is_pid_alive(std::process::id()),
            "self pid must report alive"
        );
    }

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

    #[test]
    fn process_start_token_self_is_some_and_stable() {
        // Our own process is alive on a supported OS → token resolves and is
        // stable across reads (the value is constant for a process lifetime).
        let a = process_start_token(std::process::id());
        assert!(
            a.is_some(),
            "self start-token must resolve on a supported OS"
        );
        let b = process_start_token(std::process::id());
        assert_eq!(a, b, "start-token must be stable across reads of same PID");
    }

    #[test]
    fn process_start_token_pid_zero_is_none() {
        // pid 0 is never a real tracked process — mirrors is_pid_alive's guard.
        assert_eq!(process_start_token(0), None);
    }
}
