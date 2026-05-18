//! Spawn a detached daemon subprocess — the tmux-style background model.
//!
//! [`spawn_detached`] forks the current binary with `start` as its argument,
//! detaches the child from the current shell session / controlling terminal,
//! redirects its stdio to a log file under `$AGEND_HOME`, and waits briefly
//! for the child to publish its run dir so we know it actually started.
//!
//! Used by `agend-terminal start --detached` (daemon runs in the background,
//! parent exits immediately) and by `agend-terminal app`'s auto-spawn path
//! when no live daemon is reachable (#879v3 always-Attached architecture).
//!
//! Every legitimate self-spawn site funnels through
//! [`canonical_spawn_daemon`] so the four invariants — `start --foreground`
//! args, `AGEND_SPAWN_DEPTH` increment, detach flags, log redirection —
//! cannot drift across callers.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::spawn_depth;

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
    std::fs::create_dir_all(home).with_context(|| format!("create home {}", home.display()))?;
    let log_path = home.join("daemon.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("open daemon log {}", log_path.display()))?;
    let log_err = log
        .try_clone()
        .context("clone daemon log handle for stderr")?;

    let (mut cmd, exe) = canonical_spawn_daemon(fleet_path)?;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));

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

/// Build a [`Command`] that invokes `{current_exe} start --foreground
/// [--fleet ...]` with the spawn-depth guard checked and the next depth
/// set on the child env, plus platform-appropriate detach flags.
///
/// The single point of truth for "how does agend-terminal spawn agend-terminal
/// as a daemon". All three legitimate self-spawn paths route through here:
///
/// - `agend-terminal app` auto-spawn when no live daemon is found (C2)
/// - `agend-terminal start` (CLI default-detached branch) (`src/main.rs`)
/// - Tray "Start daemon" menu action (`src/tray/mod.rs`)
///
/// Caller is responsible for stdio redirection (the three callers diverge
/// legitimately: detached parent redirects to `daemon.log`; tray inherits
/// the no-stdio profile a menu-spawned child needs; app auto-spawn matches
/// `start` because the daemon will run identically in both).
///
/// Returns the [`Command`] plus the resolved `current_exe()` PathBuf — the
/// latter is handy for the caller's error context. Errors:
/// - `current_exe()` fails (essentially unreachable in production)
/// - [`spawn_depth::check`] bails: caller has reached the fork-bomb threshold
pub fn canonical_spawn_daemon(fleet_path: Option<&Path>) -> Result<(Command, PathBuf)> {
    let exe = std::env::current_exe().context("resolve current_exe for daemon spawn")?;
    // Guard FIRST — bail before we allocate any OS resources / child handles.
    let next_depth = spawn_depth::check().context("AGEND_SPAWN_DEPTH guard")?;
    let mut cmd = Command::new(&exe);
    cmd.arg("start");
    // P0 hotfix 2026-05-18: child MUST run with --foreground or it re-enters
    // main.rs Start arm's default-detach branch (`force_foreground = false`
    // when no --foreground and no --agents), recursively calling
    // spawn_detached → fork bomb. The spawn-depth guard below is the
    // structural backstop if a future caller ever forgets this arg.
    cmd.arg("--foreground");
    if let Some(fp) = fleet_path {
        // Only pass --fleet when caller supplied one; otherwise the child
        // picks up `$AGEND_HOME/fleet.yaml` via its own resolution.
        cmd.arg("--fleet").arg(fp);
    }
    spawn_depth::set_on_child(&mut cmd, next_depth);

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
    Ok((cmd, exe))
}

/// Probe for a live, ready daemon under `home`. Returns the run_dir if both
/// `find_active_run_dir` AND `probe_api` succeed within `timeout`. Tight
/// loop with [`POLL_INTERVAL`] cadence — caller is expected to use this
/// only inside the cold-start window after [`spawn_detached`].
///
/// Used by `app::run` to wait for the auto-spawned daemon to publish its
/// run dir AND bind its API port before attempting to attach (#879v3 C2:
/// closes the spawn → attach race that broke #881).
#[allow(dead_code)] // wired in C2's `app::run` auto-spawn path
pub fn wait_until_ready(home: &Path, timeout: Duration) -> Option<PathBuf> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(run_dir) = crate::daemon::find_active_run_dir(home) {
            if crate::ipc::probe_api(&run_dir) {
                return Some(run_dir);
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn collect_args(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn get_env_value(cmd: &Command, key: &str) -> Option<String> {
        cmd.get_envs()
            .find(|(k, _)| *k == OsStr::new(key))
            .and_then(|(_, v)| v.map(|os| os.to_string_lossy().into_owned()))
    }

    #[test]
    fn canonical_spawn_includes_start_and_foreground() {
        let (cmd, _) = canonical_spawn_daemon(None).expect("guard not tripped");
        let args = collect_args(&cmd);
        assert_eq!(
            args,
            vec!["start".to_string(), "--foreground".to_string()],
            "canonical spawn must always carry `start --foreground` (no --fleet when None)"
        );
    }

    #[test]
    fn canonical_spawn_appends_fleet_path_when_provided() {
        let fleet = std::path::PathBuf::from("/tmp/some/fleet.yaml");
        let (cmd, _) = canonical_spawn_daemon(Some(&fleet)).expect("guard not tripped");
        let args = collect_args(&cmd);
        assert_eq!(
            args,
            vec![
                "start".to_string(),
                "--foreground".to_string(),
                "--fleet".to_string(),
                fleet.display().to_string(),
            ]
        );
    }

    #[test]
    fn canonical_spawn_increments_depth_env_for_child() {
        // Child should get current+1 (test runs at depth 0 → child=1).
        let (cmd, _) = canonical_spawn_daemon(None).expect("guard not tripped");
        let depth = get_env_value(&cmd, spawn_depth::ENV_KEY);
        assert_eq!(
            depth.as_deref(),
            Some("1"),
            "canonical spawn must set AGEND_SPAWN_DEPTH=1 on child (test process is depth 0)"
        );
    }
}
