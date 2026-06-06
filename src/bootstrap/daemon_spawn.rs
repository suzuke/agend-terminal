//! Spawn a detached daemon subprocess — the tmux-style background model.
//!
//! [`spawn_detached`] forks the current binary with `start` as its argument,
//! detaches the child from the current shell session / controlling terminal,
//! redirects its stdio to a log file under `$AGEND_HOME`, and waits briefly
//! for the child to publish its run dir so we know it actually started.
//!
//! Used by `agend-terminal start --detached` (daemon runs in the background,
//! parent exits immediately).

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Maximum wait for the child daemon to publish its run dir.
/// 5s is generous: the child just needs to acquire flock, create run dir,
/// issue cookie — all local syscalls.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

/// How often to poll for child readiness during the startup window.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Spawn `{current_exe} start` as a detached background process. The returned
/// `DaemonHandle` carries the child's PID (read from `.daemon` after it
/// publishes its run dir) so the caller can surface it to the user.
///
/// On Unix the child is placed in its own process group via
/// `CommandExt::process_group(0)` — this detaches it from the parent terminal
/// so Ctrl+C in the parent doesn't also kill the daemon. stdio is redirected
/// to `{home}/daemon.log` (appending, so repeated starts keep history).
///
/// On Windows the child gets `CREATE_NEW_PROCESS_GROUP` + `DETACHED_PROCESS`
/// for the equivalent behavior.
pub fn spawn_detached(home: &Path, fleet_path: Option<&Path>) -> Result<DaemonHandle> {
    let exe = std::env::current_exe().context("resolve current_exe for detach spawn")?;

    std::fs::create_dir_all(home).with_context(|| format!("create home {}", home.display()))?;
    // #914: `log_path` is now the symlink the daemon child maintains
    // (Unix) or the operator-tail target. We no longer redirect the
    // child's stdio here — the child's in-process `tracing_appender::rolling`
    // owns all log output. stdio → `/dev/null` so panics-before-tracing-init
    // / third-party direct-stderr writes don't accumulate in an unbounded
    // file. The bridged `panic::set_hook` (installed in
    // `crate::logging::setup_daemon_tracing`) routes panics through tracing
    // into the rotated log files.
    let log_path = home.join("daemon.log");

    let mut cmd = Command::new(&exe);
    cmd.arg("start");
    // P0 hotfix 2026-05-18: child MUST run with --foreground or it re-enters
    // main.rs Start arm's default-detach branch (`force_foreground = false`
    // when no --foreground and no --agents), recursively calling
    // spawn_detached → fork bomb. Witnessed: operator's sandbox produced
    // 355 zombies + 31,980 "daemon did not publish run dir within 5s"
    // log entries in single smoke run. Same RCA as #887 (cheerc). Minimal
    // single-line fix; broader #882 reattempt with safeguards still planned
    // separately (recursion guard env var, RED test for fork bomb).
    cmd.arg("--foreground");
    if let Some(fp) = fleet_path {
        // Only pass --fleet when caller supplied one; otherwise the child
        // picks up `$AGEND_HOME/fleet.yaml` via its own resolution.
        cmd.arg("--fleet").arg(fp);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS (0x00000008) | CREATE_NEW_PROCESS_GROUP (0x00000200)
        cmd.creation_flags(0x00000008 | 0x00000200);
    }

    let child = cmd
        .spawn()
        .with_context(|| format!("spawn detached daemon: {} start", exe.display()))?;
    let spawn_pid = child.id();
    // Forget the handle so the parent does not wait / reap the child at drop.
    // The daemon is now a long-lived background process unrelated to us.
    drop(child);

    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if let Some(run_dir) = crate::daemon::find_active_run_dir(home) {
            let daemon_pid = crate::daemon::read_daemon_pid(&run_dir).unwrap_or(spawn_pid);
            return Ok(DaemonHandle {
                pid: daemon_pid,
                run_dir,
                log_path,
            });
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "daemon did not publish run dir within {}s — check {}",
                STARTUP_TIMEOUT.as_secs(),
                log_path.display()
            );
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Info about a successfully spawned detached daemon.
pub struct DaemonHandle {
    pub pid: u32,
    pub run_dir: PathBuf,
    pub log_path: PathBuf,
}

/// #1814: handle to a spawned successor daemon (self-respawn handoff).
pub struct SuccessorHandle {
    /// The successor daemon's pid. `start --foreground` does NOT re-exec, so
    /// the spawned child IS the daemon — `child.id()` is authoritative.
    pub pid: u32,
    /// The successor's run dir (`home/run/<pid>`) — where the Phase-1 gate
    /// looks for `control-ready` + `api.port` + `api.cookie`.
    pub run_dir: PathBuf,
    /// The live child handle. Held so the Phase-1 gate can `try_wait()` —
    /// detecting a crash-on-launch immediately AND reaping it (vs. `kill(pid,
    /// 0)`, which reports a zombie as still alive). On the COMMIT path the
    /// handle is dropped when the predecessor exits; `std::process::Child::drop`
    /// neither waits nor kills, so the promoted successor keeps running.
    pub child: std::process::Child,
}

/// #1814: spawn a successor daemon for self-respawn handoff. Like
/// [`spawn_detached`] but (a) injects `AGEND_SUCCESSOR_HANDOFF=<value>` so the
/// child takes the minimal pre-lock handoff boot path (bypassing the singleton
/// attach-reject guard, deferring flock + destructive reconciles), and (b)
/// does NOT wait for readiness — the caller (restart handler) runs the Phase-1
/// health gate against the returned run dir, then either commits (signals the
/// predecessor to exit) or aborts (kills this successor). The child inherits
/// the predecessor's env (incl. `AGEND_RESTART_HANDOFF=1`), so the successor will
/// itself self-respawn on a later restart.
pub fn spawn_successor_handoff(home: &Path, handoff_value: &str) -> Result<SuccessorHandle> {
    let exe = std::env::current_exe().context("resolve current_exe for successor spawn")?;
    std::fs::create_dir_all(home).with_context(|| format!("create home {}", home.display()))?;

    let mut cmd = Command::new(&exe);
    cmd.arg("start").arg("--foreground");
    // Explicit set OVERRIDES any inherited stale value (the predecessor may
    // itself have been a successor carrying an old AGEND_SUCCESSOR_HANDOFF).
    cmd.env(crate::daemon::restart::SUCCESSOR_HANDOFF_ENV, handoff_value);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS (0x00000008) | CREATE_NEW_PROCESS_GROUP (0x00000200)
        cmd.creation_flags(0x00000008 | 0x00000200);
    }

    let child = cmd
        .spawn()
        .with_context(|| format!("spawn successor daemon: {} start", exe.display()))?;
    let pid = child.id();
    // The child handle is RETAINED (not dropped) so the Phase-1 gate can
    // `try_wait()` it: fast crash detection + zombie reaping on abort. On the
    // commit path the predecessor's `exit(0)` drops it without killing — the
    // promoted successor lives on (std Child drop is a no-op on the process).
    Ok(SuccessorHandle {
        pid,
        run_dir: crate::daemon::run_dir_for_pid(home, pid),
        child,
    })
}
