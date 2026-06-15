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

// CR-2026-06-14 F3: the former `NETWORK_GIT_TIMEOUT` (300s) const was consumed
// only by `dispatch_hook::ensure_branch_exists`'s dispatch-time fetches. Those
// now use the tighter `DISPATCH_FETCH_TIMEOUT` (bounded under the 30s `send`
// proxy budget), leaving the 300s const with zero users — removed rather than
// left as dead code. A future network-git op that genuinely needs minutes
// should reintroduce a purpose-named bound at its own call site.

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
///
/// CR-2026-06-14 #5 (deadlock-safety): stdout AND stderr are drained on dedicated
/// SCOPED reader threads CONCURRENTLY with the wait poll. The OS pipe buffer is
/// small and fixed (macOS ~16KB, non-growing); a child that emits more than one
/// buffer's worth (e.g. `gh api .../compare` returning a multi-MB diff JSON,
/// reached via `merge_freshness`/`task_sweep`) blocks in `write()` until a reader
/// drains the pipe. The earlier loop only `try_wait()`d — never reading until
/// after exit — so such a child never exited, hit the deadline, and was
/// false-timeout-killed. The scoped readers keep both pipes drained so the child
/// can always make progress to exit (this is what std's `wait_with_output` does
/// via `read2`, reconstructed here so it composes with the timeout poll). Scoped
/// threads are joined for free at scope close, so they cannot leak — §10.5's
/// graceful-join, compiler-enforced.
pub(crate) fn spawn_group_bounded(
    mut cmd: std::process::Command,
    label: &str,
    timeout: Duration,
) -> std::io::Result<std::process::Output> {
    use std::io::Read;
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
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    // Drain both pipes concurrently with the wait poll using SCOPED threads. A
    // scope is GUARANTEED to join every thread it spawned before it returns —
    // including on early `return` from the closure below — so these readers can
    // never outlive the call or leak (the §10.5 graceful-join contract, here
    // compiler-enforced; scoped `s.spawn` is categorically outside the
    // detached-spawn rule the audit guards). Every non-normal exit path kills the
    // child FIRST so its pipes close → the readers hit EOF → the scope's implicit
    // join completes instead of blocking forever.
    std::thread::scope(|s| {
        let stdout_reader = s.spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = stdout_pipe {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        });
        let stderr_reader = s.spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = stderr_pipe {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        });

        // #1897: poll with EXPONENTIAL BACKOFF (1ms → 50ms cap). The previous
        // fixed 200ms poll added up to 200ms of latency per git call vs the old
        // immediate `.output()`, which PERTURBED timing-sensitive concurrent
        // callers (the `checkout_bind_true_concurrent_branch_create_race` test
        // flaked ~50%). A fast git op (the common case — rev-parse / branch /
        // status are sub-100ms) now returns within a couple ms, matching
        // `.output()` latency; a genuinely slow op backs off to a cheap 50ms poll.
        let start = std::time::Instant::now();
        let mut poll = Duration::from_millis(1);
        loop {
            match child.try_wait() {
                // Normal exit: the child closed its pipe ends, so the readers
                // EOF; join them for the captured output.
                Ok(Some(status)) => {
                    let stdout = stdout_reader.join().unwrap_or_default();
                    let stderr = stderr_reader.join().unwrap_or_default();
                    return Ok(std::process::Output {
                        status,
                        stdout,
                        stderr,
                    });
                }
                Ok(None) if start.elapsed() >= timeout => {
                    // Kill BEFORE returning so the pipes close and the scope's
                    // implicit reader-join can complete (output discarded).
                    crate::process::kill_process_tree(child.id());
                    let _ = child.wait();
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!("{label} timed out after {timeout:?}"),
                    ));
                }
                Ok(None) => {
                    std::thread::sleep(poll);
                    poll = (poll * 2).min(Duration::from_millis(50));
                }
                // try_wait itself failed (rare). Kill first — same reason as the
                // timeout path — then surface the error.
                Err(e) => {
                    crate::process::kill_process_tree(child.id());
                    let _ = child.wait();
                    return Err(e);
                }
            }
        }
    })
}

/// W1.2 (#REFACTOR-PLAN): a structured failure from [`git_cmd`]. Distinguishes a
/// spawn/timeout failure (git never produced a status) from a non-zero exit (git
/// ran and rejected). Carries the exit code + trimmed stderr so a caller that
/// branched on `output.status.code()` / stderr keeps the same information —
/// migration preserves error-branch semantics, only the shape is more structured.
#[derive(Debug)]
pub(crate) enum GitError {
    /// git failed to spawn, or the bounded wait errored (incl. `TimedOut`).
    Spawn(std::io::Error),
    /// git ran but exited non-zero. `code` is `None` for a signal-kill.
    NonZero { code: Option<i32>, stderr: String },
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GitError::Spawn(e) => write!(f, "git spawn failed: {e}"),
            GitError::NonZero { code, stderr } => write!(
                f,
                "git exited {}: {}",
                code.map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".to_string()),
                stderr
            ),
        }
    }
}

impl std::error::Error for GitError {}

/// W1.2: THE ergonomic entry point for a daemon-internal LOCAL git command.
/// Always `AGEND_GIT_BYPASS` + bounded by `LOCAL_GIT_TIMEOUT` (both via
/// [`git_bypass`]), returns **trimmed stdout** on success or a structured
/// [`GitError`] on spawn-failure / non-zero exit. A caller routed through this
/// CANNOT forget the bypass env or the timeout — that's what W1.2 makes
/// structurally impossible (enforced by a grep invariant). For a "did it
/// succeed?" check that discards the output, use [`git_ok`].
pub(crate) fn git_cmd(cwd: &Path, args: &[&str]) -> Result<String, GitError> {
    let out = git_bypass(cwd, args).map_err(GitError::Spawn)?;
    if !out.status.success() {
        return Err(GitError::NonZero {
            code: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// W1.2: the boolean form of [`git_cmd`] — `true` iff git spawned AND exited 0.
/// Swallows the output and any spawn/timeout error (→ `false`), matching the
/// ubiquitous daemon idiom
/// `Command::new("git")…output().map(|o| o.status.success()).unwrap_or(false)`
/// that this absorbs (same bypass env, plus the `LOCAL_GIT_TIMEOUT` bound).
pub(crate) fn git_ok(cwd: &Path, args: &[&str]) -> bool {
    git_bypass(cwd, args)
        .map(|o| o.status.success())
        .unwrap_or(false)
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

    /// A real, isolated git repo (one empty commit on `main`), built through the
    /// shim bypass so a fleet-agent test run isn't ChdirPass'd (#1463).
    fn tmp_repo(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static C: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "agend-gitcmd-{}-{}-{}",
            tag,
            std::process::id(),
            C.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for args in [
            vec!["init", "-b", "main"],
            vec![
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ],
        ] {
            std::process::Command::new("git")
                .env("AGEND_GIT_BYPASS", "1")
                .args(&args)
                .current_dir(&dir)
                .output()
                .unwrap();
        }
        dir
    }

    /// W1.2 §3.9: `git_cmd` returns TRIMMED stdout on success, a structured
    /// `GitError::NonZero` (with code + stderr) on a non-zero exit, and never
    /// leaks the bypass-env / timeout wiring to the caller.
    #[test]
    fn git_cmd_trims_stdout_and_structures_errors_w1_2() {
        let repo = tmp_repo("ok");
        // Success → trimmed (no trailing newline).
        let head = git_cmd(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap();
        assert_eq!(head, "main", "stdout must be trimmed, no newline");
        // Non-zero exit → structured NonZero, code present, stderr captured.
        let err = git_cmd(&repo, &["rev-parse", "definitely-no-such-ref"]).unwrap_err();
        match err {
            GitError::NonZero { code, stderr } => {
                assert_eq!(code, Some(128), "git's usage/ref error exit code");
                assert!(!stderr.is_empty(), "stderr captured for the caller");
            }
            GitError::Spawn(e) => panic!("expected NonZero, got spawn error: {e}"),
        }
        std::fs::remove_dir_all(&repo).ok();
    }

    /// W1.2 §3.9: `git_ok` is `true` on exit-0, `false` on non-zero — the
    /// boolean idiom this absorbs.
    #[test]
    fn git_ok_reflects_exit_status_w1_2() {
        let repo = tmp_repo("bool");
        assert!(
            git_ok(&repo, &["rev-parse", "--git-dir"]),
            "valid repo → true"
        );
        assert!(
            !git_ok(&repo, &["rev-parse", "definitely-no-such-ref"]),
            "non-zero exit → false"
        );
        // A non-repo dir → git errors → false (matches the `.unwrap_or(false)` idiom).
        let nonrepo =
            std::env::temp_dir().join(format!("agend-gitcmd-nonrepo-{}", std::process::id()));
        std::fs::create_dir_all(&nonrepo).ok();
        assert!(
            !git_ok(&nonrepo, &["rev-parse", "--git-dir"]),
            "non-repo → false"
        );
        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_dir_all(&nonrepo).ok();
    }

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
    #[cfg(unix)]
    fn spawn_group_bounded_drains_large_output_no_deadlock_cr20260614() {
        // CR-2026-06-14 #5 (r2 reject): a child that writes MORE than one OS pipe
        // buffer (macOS ~16KB, non-growing) must NOT deadlock. With a poll loop
        // that only `try_wait()`s and reads after exit, the child blocks in
        // write() once the buffer fills, never exits, and is false-timeout-killed
        // at the deadline — exactly what would have hung `gh api .../compare`
        // (multi-MB diff JSON) in merge_freshness. Emit 256KB (16× a macOS pipe
        // buffer) and require it ALL back, exit-0, well under the bound.
        let bytes = 256 * 1024;
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c")
            .arg(format!("yes 0123456789abcde | head -c {bytes}"));
        let start = std::time::Instant::now();
        let out = spawn_group_bounded(cmd, "big stdout", Duration::from_secs(10))
            .expect("large-output child must complete, not deadlock + false-timeout");
        assert!(out.status.success(), "child exits 0");
        assert_eq!(
            out.stdout.len(),
            bytes,
            "every byte drained (no truncation)"
        );
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "must finish well under the 10s bound (no deadlock), took {:?}",
            start.elapsed()
        );
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
