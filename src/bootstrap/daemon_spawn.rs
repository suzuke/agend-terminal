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
    // #879 (Bug A fix): pass `--foreground` so the child enters the
    // `cli::start_with_fleet` → `bootstrap::prepare` path that actually
    // publishes the run dir + binds api.port. Without this flag, the
    // child re-enters main.rs's start arm with `force_foreground = false`,
    // calls spawn_detached again, and the recursion never reaches the
    // path that publishes anything. Production via `agend-terminal
    // service install` masked the latent recursion by baking
    // `start --foreground` into the launchd plist /  systemd unit
    // (see `src/service/mod.rs:420` and `src/tray/autostart/macos.rs:52`);
    // shell-invoked `agend-terminal start` and (post-#879) the app's
    // auto-spawn path both surfaced it.
    cmd.arg("--foreground");
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

    let child = cmd
        .spawn()
        .with_context(|| format!("spawn detached daemon: {} start", exe.display()))?;
    let spawn_pid = child.id();
    // Forget the handle so the parent does not wait / reap the child at drop.
    // The daemon is now a long-lived background process unrelated to us.
    drop(child);

    wait_for_daemon_ready(home, log_path, spawn_pid, Instant::now() + STARTUP_TIMEOUT)
}

/// #879 (Bug B fix): poll until the daemon at `home` is **fully ready**
/// — both its run dir is published AND its `api.port` listener is bound
/// (the latter is what `bootstrap::try_attach`'s `probe_api` check
/// requires before it returns `Attached`). Extracted from
/// [`spawn_detached`] so tests can exercise the readiness gate without
/// actually spawning a subprocess: a fixture pre-writes the run dir +
/// `.daemon` + `api.cookie` BUT leaves `api.port` pointing at an
/// unbound port (or omits it entirely), and asserts this helper times
/// out with the expected error shape.
///
/// `spawn_pid` is the fallback PID written into `DaemonHandle.pid` when
/// `.daemon` can't be parsed (e.g. partial write). Tests pass `0` (or
/// any sentinel) since they don't actually spawn anything.
pub fn wait_for_daemon_ready(
    home: &Path,
    log_path: PathBuf,
    spawn_pid: u32,
    deadline: Instant,
) -> Result<DaemonHandle> {
    loop {
        // Pre-fix accepted `find_active_run_dir` presence alone — but
        // the child writes `.daemon` BEFORE it writes `api.port` and
        // binds the listener (per `bootstrap::prepare`'s ordering), so
        // a parent that re-called `prepare()` after `spawn_detached`
        // returned could see the run dir but have `try_attach`'s
        // `probe_api` check fail → fall through to
        // `acquire_daemon_lock` → race with the not-yet-bound child.
        // The new success criterion matches `try_attach`'s own gate,
        // so the handshake is observably complete on the parent side
        // by the time this helper returns Ok.
        if let Some(run_dir) = crate::daemon::find_active_run_dir(home) {
            if crate::ipc::probe_api(&run_dir) {
                let daemon_pid = crate::daemon::read_daemon_pid(&run_dir).unwrap_or(spawn_pid);
                return Ok(DaemonHandle {
                    pid: daemon_pid,
                    run_dir,
                    log_path,
                });
            }
        }
        if Instant::now() >= deadline {
            let run_dir_present = crate::daemon::find_active_run_dir(home).is_some();
            anyhow::bail!(
                "daemon did not become reachable (run_dir_published={}, api_port_listener_bound=false) — check {}",
                run_dir_present,
                log_path.display()
            );
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Info about a successfully spawned detached daemon.
#[derive(Debug)]
pub struct DaemonHandle {
    pub pid: u32,
    pub run_dir: PathBuf,
    pub log_path: PathBuf,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_home(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-879-spawn-{}-{}-{id}",
            std::process::id(),
            tag,
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create tmp_home");
        dir
    }

    /// Pick a TCP port that is NOT currently listening — bind a
    /// listener, read its port, drop the listener so the port is
    /// observably idle. There's a small race window between drop and
    /// the test's `probe_api` call where another process could grab
    /// the port, but for a unit test in CI this is negligible (~50µs
    /// window vs millisecond-scale test runtime, and CI hosts don't
    /// have arbitrary daemons grabbing random loopback ports).
    fn pick_unbound_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
        let port = listener.local_addr().expect("local_addr").port();
        drop(listener);
        port
    }

    /// #879-reopen RED test (Layer 2): `wait_for_daemon_ready` (the
    /// extracted poll loop from `spawn_detached`) must NOT return Ok
    /// when the child has published its run dir but has NOT yet bound
    /// its `api.port` listener. Pre-fix the poll accepted run_dir
    /// presence alone — that's the race window that broke #881 in
    /// production. Fixture: pre-write `.daemon` + `api.cookie` +
    /// `api.port` pointing at a deliberately-unbound port; assert the
    /// helper times out and bails with an error mentioning
    /// `api_port_listener_bound=false`.
    /// Set up `<home>/run/<current_pid>/` populated so that
    /// `find_active_run_dir` accepts it: PID directory name matches
    /// `std::process::id()` (so `is_pid_alive` returns true), `.daemon`
    /// file in `"<pid>:<timestamp>"` format. Caller chooses how to
    /// populate `api.port` (bound vs unbound port).
    fn make_fake_run_dir(home: &Path) -> PathBuf {
        let pid = std::process::id();
        let run_dir = home.join("run").join(pid.to_string());
        std::fs::create_dir_all(&run_dir).expect("create run_dir");
        std::fs::write(run_dir.join(".daemon"), format!("{pid}:0")).expect("write .daemon");
        std::fs::write(run_dir.join("api.cookie"), [0u8; 32]).expect("write api.cookie");
        run_dir
    }

    #[test]
    fn wait_for_daemon_ready_times_out_when_api_port_unbound() {
        let home = tmp_home("api-unbound");
        let run_dir = make_fake_run_dir(&home);
        let unbound_port = pick_unbound_port();
        crate::ipc::write_port(&run_dir, crate::ipc::API_NAME, unbound_port)
            .expect("write api.port");

        let log_path = home.join("daemon.log");
        let started = Instant::now();
        let deadline = started + Duration::from_millis(500);
        let result = wait_for_daemon_ready(&home, log_path, std::process::id(), deadline);
        let elapsed = started.elapsed();

        let err = match result {
            Ok(_) => panic!("must time out when api.port is unbound, got Ok"),
            Err(e) => e,
        };
        assert!(
            elapsed >= Duration::from_millis(450),
            "must poll the full deadline budget (got {elapsed:?})"
        );
        let err_msg = format!("{err:#}");
        assert!(
            err_msg.contains("api_port_listener_bound=false"),
            "error must surface the probe_api failure for operator search: {err_msg}"
        );
        assert!(
            err_msg.contains("run_dir_published=true"),
            "error must distinguish run_dir presence from listener-bind: {err_msg}"
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    /// Positive control: when both run_dir AND a real (bound) api.port
    /// listener are present, `wait_for_daemon_ready` returns Ok
    /// promptly without hitting the deadline.
    #[test]
    fn wait_for_daemon_ready_returns_ok_when_fully_attached() {
        let home = tmp_home("fully-attached");
        let run_dir = make_fake_run_dir(&home);
        // Bind a REAL listener so probe_api succeeds. Hold for the duration
        // of the test so probe_api's connect_timeout succeeds.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        crate::ipc::write_port(&run_dir, crate::ipc::API_NAME, port).expect("write api.port");

        let log_path = home.join("daemon.log");
        let deadline = Instant::now() + Duration::from_secs(2);
        let result = wait_for_daemon_ready(&home, log_path, std::process::id(), deadline);

        match result {
            Ok(handle) => {
                assert_eq!(
                    handle.pid,
                    std::process::id(),
                    "pid must come from .daemon file"
                );
                assert_eq!(handle.run_dir, run_dir);
            }
            Err(e) => panic!("fully-ready daemon must succeed, got Err: {e:#}"),
        }

        drop(listener);
        let _ = std::fs::remove_dir_all(&home);
    }
}
