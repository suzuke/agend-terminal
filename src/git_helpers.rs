//! Git helper functions — single source of truth for remote/branch detection.

use std::path::Path;
use std::time::Duration;

/// #1897: bound for LOCAL git ops (branch / worktree add / rev-parse / status /
/// remote get-url / log / diff / reset). These don't hit the network; a healthy
/// one is sub-second and the only legitimate slowness is a contended
/// `.git/index.lock` or a large worktree-add checkout. 60s is far above any real
/// local op, so it never false-kills a legit op but still fails fast instead of
/// hanging the daemon forever (the #1893/RC1 root cause).
pub(crate) const LOCAL_GIT_TIMEOUT: Duration = Duration::from_secs(60);

/// #1897: bound for NETWORK git ops (fetch / clone). A large fetch can legitimately
/// take minutes, so this is generous — strictly wider than the prior hard-coded
/// 60s at the fetch sites, so it can only ADD tolerance (never newly false-kills a
/// fetch that passed before) while still capping a wedged network op.
pub(crate) const NETWORK_GIT_TIMEOUT: Duration = Duration::from_secs(300);

/// #781 Piece 6 — daemon-internal `git` subprocess wrapper that always
/// sets `AGEND_GIT_BYPASS=1`. Centralizes the bypass-env contract so
/// adding a new git call cannot silently trip the fleet-managed
/// `git worktree` / `git branch` shim deny.
///
/// Originated in #780 as a private `fn` inside `mcp::handlers::ci::mod`.
/// Promoted to `pub(crate)` in `git_helpers` for #781 so both
/// `handle_checkout_repo` and `dispatch_auto_bind_lease`'s
/// `ensure_branch_exists` extraction share a single bypass-env helper.
///
/// #1897: previously a bare `.output()` (UNBOUNDED) — a git blocked on a
/// contended `.git/index.lock` hung the daemon forever. Now routes through
/// [`git_bypass_timeout`] with the generous [`LOCAL_GIT_TIMEOUT`] so a stuck
/// local git fails fast with a clear `Err(TimedOut)` the caller can handle. The
/// fast path is byte-identical to before (same captured `Output`).
pub(crate) fn git_bypass(cwd: &Path, args: &[&str]) -> std::io::Result<std::process::Output> {
    git_bypass_timeout(cwd, args, LOCAL_GIT_TIMEOUT)
}

/// Like `git_bypass` but with a process-level timeout. Spawns the git
/// subprocess and polls `try_wait` until either the process exits or the
/// deadline is reached, at which point the git process group is killed.
///
/// #1897: two hardening changes (callers' observable contract is unchanged —
/// still `Ok(Output)` on completion, `Err(TimedOut)` on deadline):
///  - git runs in its OWN process group (`process_group(0)` /
///    `CREATE_NEW_PROCESS_GROUP`). This is MANDATORY for the kill below:
///    `kill_process_tree` resolves the pgid via `getpgid`, so without isolation
///    it would resolve to the DAEMON's group and kill the daemon.
///  - on timeout we kill the whole git PROCESS GROUP (git + its sub-helpers like
///    `git-remote-https` / pack) via `process::kill_process_tree`, not just the
///    immediate child — so a wedged fetch's network helper can't orphan.
pub(crate) fn git_bypass_timeout(
    cwd: &Path,
    args: &[&str],
    timeout: Duration,
) -> std::io::Result<std::process::Output> {
    let mut cmd = std::process::Command::new("git");
    cmd.args(args).current_dir(cwd).env("AGEND_GIT_BYPASS", "1");
    spawn_group_bounded(cmd, &format!("git {:?}", &args[..1]), timeout)
}

/// #1897: spawn `cmd` in its OWN process group, capture stdout/stderr, and bound
/// it by `timeout`. On the deadline, kill the whole process group (the child +
/// its sub-helpers) via `process::kill_process_tree` and return `Err(TimedOut)`.
///
/// Extracted from [`git_bypass_timeout`] so the timeout + process-group-kill
/// mechanism is unit-testable with a `sleep` stub (the git path is identical —
/// same `Command` minus the binary). Process-group isolation is MANDATORY:
/// `kill_process_tree` resolves the pgid via `getpgid`, so without isolation the
/// kill would resolve to the DAEMON's group and kill the daemon.
///
/// #1899: `pub(crate)` so NON-bypass prod git sites (ones that deliberately do
/// NOT set `AGEND_GIT_BYPASS`) can pass their own pre-built `Command` to get the
/// same bound + safe process-group kill while preserving their env/bypass
/// behaviour — instead of being forced through [`git_bypass_timeout`] (which
/// always sets the bypass env).
pub(crate) fn spawn_group_bounded(
    mut cmd: std::process::Command,
    label: &str,
    timeout: Duration,
) -> std::io::Result<std::process::Output> {
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // Group isolation only; NOT detached — we keep the captured stdout/stderr.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NEW_PROCESS_GROUP (0x00000200) — group isolation, not DETACHED.
        cmd.creation_flags(0x0000_0200);
    }
    let mut child = cmd.spawn()?;

    // #1897: poll with EXPONENTIAL BACKOFF (1ms → 50ms cap). The previous fixed
    // 200ms poll added up to 200ms of latency per git call vs the old immediate
    // `.output()`, which PERTURBED timing-sensitive concurrent callers (the
    // `checkout_bind_true_concurrent_branch_create_race` test flaked ~50%). A
    // fast git op (the common case — rev-parse / branch / status are sub-100ms)
    // now returns within a couple ms, matching `.output()` latency; a genuinely
    // slow op backs off to a cheap 50ms poll.
    let start = std::time::Instant::now();
    let mut poll = Duration::from_millis(1);
    loop {
        match child.try_wait()? {
            Some(_status) => return child.wait_with_output(),
            None if start.elapsed() >= timeout => {
                crate::process::kill_process_tree(child.id());
                let _ = child.wait();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("{label} timed out after {timeout:?}"),
                ));
            }
            None => {
                std::thread::sleep(poll);
                poll = (poll * 2).min(Duration::from_millis(50));
            }
        }
    }
}

/// Detect the default branch of a repository.
/// Reads `refs/remotes/origin/HEAD` → extracts branch name.
/// Falls back to "main" if detection fails.
pub fn default_branch(repo_dir: &Path) -> String {
    let remote = primary_remote(repo_dir);
    let ref_path = format!("refs/remotes/{remote}/HEAD");
    // #1897: bounded (was an unbounded `.output()`) — a stuck local git falls
    // through to the "main" fallback instead of hanging the daemon.
    match git_bypass_timeout(repo_dir, &["symbolic-ref", &ref_path], LOCAL_GIT_TIMEOUT) {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            let prefix = format!("refs/remotes/{remote}/");
            s.strip_prefix(&prefix).unwrap_or(&s).to_string()
        }
        _ => "main".to_string(),
    }
}

/// Detect the primary remote name.
/// Returns the first remote listed by `git remote`, typically "origin".
/// Falls back to "origin" if detection fails.
pub fn primary_remote(repo_dir: &Path) -> String {
    // #1897: bounded (was an unbounded `.output()`) — a stuck local git falls
    // through to the "origin" fallback instead of hanging the daemon.
    match git_bypass_timeout(repo_dir, &["remote"], LOCAL_GIT_TIMEOUT) {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout);
            s.lines().next().unwrap_or("origin").to_string()
        }
        _ => "origin".to_string(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // #1897 §3.9: the timeout + process-group-kill mechanism, driven by `sleep`
    // stubs (unix). The git path is `spawn_group_bounded` with `git` as the
    // binary, so these exercise the exact prod loop/kill.

    #[test]
    #[cfg(unix)]
    fn spawn_group_bounded_times_out_within_slack_1897() {
        // A slow stub sleeps 30s; a 1s bound must return Err(TimedOut) fast,
        // NOT hang for 30s (the daemon-hang root cause #1893/RC1).
        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("30");
        let start = std::time::Instant::now();
        let res = spawn_group_bounded(cmd, "sleep 30", Duration::from_secs(1));
        let elapsed = start.elapsed();
        let err = res.expect_err("slow stub must time out, not hang");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        // 1s timeout + kill grace (500ms) + poll slack — well under the 30s sleep.
        assert!(
            elapsed < Duration::from_secs(5),
            "must return shortly after the timeout, took {elapsed:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn spawn_group_bounded_fast_command_unaffected_1897() {
        // A fast command completes normally and returns its captured Output —
        // the timeout path must not perturb the success path.
        let cmd = std::process::Command::new("true");
        let out = spawn_group_bounded(cmd, "true", Duration::from_secs(10))
            .expect("fast command must return Ok");
        assert!(out.status.success(), "fast command exits 0 with Output");
    }

    #[test]
    #[cfg(unix)]
    fn spawn_group_bounded_preserves_caller_env_no_bypass_1899() {
        // #1899: NON-bypass prod sites (e.g. ci/mod's cleanup worktree-remove)
        // pass a BARE Command. spawn_group_bounded must NOT inject
        // AGEND_GIT_BYPASS — it only adds the bound + process-group kill, leaving
        // the caller's env/bypass behaviour intact (the bypass env is set ONLY by
        // git_bypass_timeout, never here).
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c")
            .arg("printf 'bypass=[%s]' \"$AGEND_GIT_BYPASS\"");
        let out = spawn_group_bounded(cmd, "sh env-probe", Duration::from_secs(10))
            .expect("fast command must return Ok");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            stdout, "bypass=[]",
            "spawn_group_bounded must not inject AGEND_GIT_BYPASS (caller env preserved)"
        );
    }

    #[test]
    #[cfg(unix)]
    fn spawn_group_bounded_kills_whole_process_group_1897() {
        // The stub backgrounds a grandchild `sleep` (same process group, no
        // setsid), records its pid, then blocks. On timeout the process-group
        // kill must reap the GRANDCHILD too — not just the immediate child.
        let pidfile =
            std::env::temp_dir().join(format!("agend-1897-pgkill-{}.pid", std::process::id()));
        let _ = std::fs::remove_file(&pidfile);
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(format!(
            "sleep 30 & echo $! > {}; sleep 30",
            pidfile.display()
        ));
        let res = spawn_group_bounded(cmd, "sh tree", Duration::from_secs(2));
        assert!(res.is_err(), "blocking stub must time out");
        // kill_process_tree does SIGTERM → 500ms grace → SIGKILL synchronously
        // before returning; a little extra slack lets the OS finish reaping.
        std::thread::sleep(Duration::from_millis(800));
        let gpid: u32 = std::fs::read_to_string(&pidfile)
            .expect("grandchild pid recorded")
            .trim()
            .parse()
            .expect("pid parses");
        assert!(
            !crate::process::is_pid_alive(gpid),
            "grandchild (pid {gpid}) must be reaped by the process-group kill"
        );
        let _ = std::fs::remove_file(&pidfile);
    }

    #[test]
    fn default_branch_fallback_when_no_repo() {
        let fake = std::env::temp_dir().join("no-repo-690");
        std::fs::create_dir_all(&fake).ok();
        assert_eq!(default_branch(&fake), "main");
        std::fs::remove_dir_all(&fake).ok();
    }

    #[test]
    fn primary_remote_fallback_when_no_repo() {
        let fake = std::env::temp_dir().join("no-remote-690");
        std::fs::create_dir_all(&fake).ok();
        assert_eq!(primary_remote(&fake), "origin");
        std::fs::remove_dir_all(&fake).ok();
    }

    #[test]
    fn strip_prefix_preserves_slashes() {
        // Simulate the parsing logic directly
        let output = "refs/remotes/origin/release/2026";
        let prefix = "refs/remotes/origin/";
        let result = output.strip_prefix(prefix).unwrap_or(output);
        assert_eq!(result, "release/2026");
    }
}
