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
//! Every legitimate self-spawn site sources its args + env from
//! [`super::spawn_depth::canonical_spawn_args`] (the SPEC), then builds
//! its own [`Command`] — this preserves the #548 Q7 tray-separation
//! contract (no `bootstrap::daemon_spawn` import in tray) while still
//! converging the args / env shape across all spawn paths. The shared
//! Command construction lives here in [`spawn_detached`] for the CLI
//! Start + app auto-spawn paths.

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

    let exe = std::env::current_exe().context("resolve current_exe for daemon spawn")?;
    let spec = spawn_depth::canonical_spawn_args(fleet_path).context("AGEND_SPAWN_DEPTH guard")?;
    let mut cmd = Command::new(&exe);
    spec.apply_to(&mut cmd);
    spawn_depth::apply_detach_flags(&mut cmd);
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

/// Best-effort cleanup when a bootstrap-attempting caller decides to bail
/// before it can hold a daemon's run dir alive for the process lifetime.
///
/// Two failure shapes this addresses:
///
/// 1. **Auto-spawn → readiness probe fails** (#879v3 C2 path). We've already
///    forked a child daemon (carrying a PID, having created its run dir,
///    bound its API port), and now we're about to return `Err` from
///    `app::run`. Without this helper the parent exits and the orphan
///    daemon keeps running with no client attached — a new orphan class
///    introduced by the always-Attached architecture.
///
/// 2. **Bootstrap `prepare` Err mid-way** (#879v3 C2.6 path). Some state may
///    be on disk (run dir created but no cookie issued); the next boot's
///    `sweep_stale_run_dirs` would handle it eventually, but proactive
///    removal prevents the misleading "stale-but-recent" rundir from
///    luring a subsequent attach.
///
/// Best-effort throughout: every individual cleanup step is wrapped so a
/// permission error or already-gone path doesn't propagate. Logs at
/// `tracing::warn!` on any per-step failure so post-mortem still has
/// breadcrumbs.
pub fn cleanup_on_bail(spawned_daemon_pid: Option<u32>, run_dir: Option<&Path>) {
    if let Some(pid) = spawned_daemon_pid {
        tracing::warn!(pid, "cleanup_on_bail: SIGTERM auto-spawned daemon");
        crate::process::terminate(pid);
    }
    if let Some(dir) = run_dir {
        // Remove api.port first (probe_api uses it as the liveness signal —
        // its absence on a partial-bail rundir lets a future `try_attach`
        // skip immediately rather than time out).
        crate::ipc::remove_port(dir, crate::ipc::API_NAME);
        // Then the rundir wholesale. Removed-but-already-gone is fine.
        if let Err(e) = std::fs::remove_dir_all(dir) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    error = %e,
                    path = %dir.display(),
                    "cleanup_on_bail: remove_dir_all failed (continuing)"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_tmp_rundir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agend-cleanup-on-bail-test-{}-{}-{}",
            std::process::id(),
            label,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir tmp rundir");
        dir
    }

    /// LOAD-BEARING per #879v3 C2.6: ensures the cleanup helper actually
    /// removes the run-dir port file + the run-dir itself. Reviewer §3.20
    /// SOP 3 RED protocol will revert the body to a no-op and observe
    /// this test FAIL.
    #[test]
    fn cleanup_on_bail_removes_port_and_run_dir() {
        let run_dir = unique_tmp_rundir("port-and-dir");
        crate::ipc::write_port(&run_dir, crate::ipc::API_NAME, 12345).expect("seed api.port");
        let port_file = run_dir.join(format!("{}.port", crate::ipc::API_NAME));
        assert!(
            port_file.exists(),
            "fixture: api.port must exist pre-cleanup"
        );

        // No spawned daemon pid in this test — exercises the "pid=None"
        // branch (bootstrap::prepare Err shape, not auto-spawn shape).
        cleanup_on_bail(None, Some(&run_dir));

        assert!(
            !port_file.exists(),
            "cleanup_on_bail must remove api.port — the absence is the \
             signal probe_api uses to skip stale rundirs without timing out"
        );
        assert!(
            !run_dir.exists(),
            "cleanup_on_bail must remove the run_dir wholesale — \
             leaving it behind would mislead future find_active_run_dir"
        );
    }

    #[test]
    fn cleanup_on_bail_is_idempotent_on_missing_paths() {
        // Run-dir doesn't exist (already cleaned by a prior bail). Helper
        // must not panic / return errors; absence is the expected state.
        let phantom = std::env::temp_dir().join(format!(
            "agend-cleanup-phantom-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        assert!(!phantom.exists(), "fixture: phantom path must not exist");
        // Should not panic.
        cleanup_on_bail(None, Some(&phantom));
        cleanup_on_bail(None, None);
    }
}
