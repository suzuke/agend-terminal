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

    std::fs::create_dir_all(home)
        .with_context(|| format!("create home {}", home.display()))?;
    let log_path = home.join("daemon.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("open daemon log {}", log_path.display()))?;
    let log_err = log
        .try_clone()
        .context("clone daemon log handle for stderr")?;

    let mut cmd = Command::new(&exe);
    cmd.arg("start");
    if let Some(fp) = fleet_path {
        // Only pass --fleet when caller supplied one; otherwise the child
        // picks up `$AGEND_HOME/fleet.yaml` via its own resolution.
        cmd.arg("--fleet").arg(fp);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));

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

    let child = cmd.spawn().with_context(|| {
        format!("spawn detached daemon: {} start", exe.display())
    })?;
    let spawn_pid = child.id();
    // Forget the handle so the parent does not wait / reap the child at drop.
    // The daemon is now a long-lived background process unrelated to us.
    drop(child);

    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if let Some(run_dir) = crate::daemon::find_active_run_dir(home) {
            let daemon_pid = read_daemon_pid(&run_dir).unwrap_or(spawn_pid);
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

fn read_daemon_pid(run_dir: &Path) -> Option<u32> {
    std::fs::read_to_string(run_dir.join(".daemon"))
        .ok()?
        .trim()
        .split_once(':')
        .and_then(|(pid, _)| pid.parse().ok())
}
