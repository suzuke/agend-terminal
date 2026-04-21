//! Bootstrap seam shared by `start` (daemon) and `app` (TUI) entry points.
//!
//! Centralizes every pre-spawn concern behind one call: [`prepare`]. Both
//! daemon and app callers go through the same preflight (run dir, `.daemon`,
//! `api.cookie`, fleet load/normalize, telegram init) so their run-time state
//! cannot diverge.
//!
//! Outcome:
//! - [`BootstrapOutcome::Owned`]: this process is the daemon. Holds the
//!   exclusive `.daemon.lock` flock, owns the run dir, and carries the issued
//!   api.cookie plus fully-resolved agent specs.
//! - [`BootstrapOutcome::Attached`]: another daemon already owns the run dir.
//!   The current process is a client and should not touch run dir ownership.

mod agent_resolve;
pub mod daemon_spawn;
mod fleet_normalize;
pub mod reload;
pub mod signals;
mod telegram_init;

pub(crate) use agent_resolve::resolve_one;
pub use agent_resolve::AgentDef;

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// RAII guard for the daemon-exclusive `.daemon.lock` flock.
///
/// Dropping the struct releases the lock. Keep this alive for the entire
/// lifetime of the owning process.
pub struct DaemonLock {
    _file: std::fs::File,
}

/// Result of [`prepare`] — tells the caller whether this process is the daemon
/// or a client of an existing one.
pub enum BootstrapOutcome {
    Owned(Box<OwnedFleet>),
    Attached(AttachedFleet),
}

/// Owned state: this process is the daemon. Run dir + cookie + lock belong to
/// us. Fleet is normalized and every instance is resolved into a spawn-ready
/// [`AgentDef`].
///
/// Some fields are scaffolding for follow-on work (e.g. hot-reload of
/// fleet.yaml needs `fleet_path`, `cookie` is read by tests + may be read by
/// callers that want to avoid re-reading the cookie file per connection).
/// `#[allow(dead_code)]` is applied per-field so genuinely new unused fields
/// still trip `-D warnings` in CI.
pub struct OwnedFleet {
    pub home: PathBuf,
    #[allow(dead_code)]
    pub fleet_path: PathBuf,
    pub config: crate::fleet::FleetConfig,
    pub agents: Vec<AgentDef>,
    pub run_dir: PathBuf,
    #[allow(dead_code)]
    pub cookie: crate::auth_cookie::Cookie,
    pub telegram: Option<Arc<Mutex<crate::telegram::TelegramState>>>,
    /// Flock guard — drop releases `.daemon.lock`. Kept last so the lock is
    /// released only after every other resource has been dropped.
    #[allow(dead_code)]
    pub lock: DaemonLock,
}

/// Attached state: an existing daemon owns the run dir. We read its cookie so
/// we can speak the TUI/API protocols but never touch the run dir itself.
///
/// `home` / `fleet_path` / `cookie` are scaffolding — today `BridgeClient`
/// re-derives them per connection, but a future per-pane cache would read
/// these. See OwnedFleet note about why `#[allow(dead_code)]` is per-field.
pub struct AttachedFleet {
    #[allow(dead_code)]
    pub home: PathBuf,
    #[allow(dead_code)]
    pub fleet_path: PathBuf,
    pub run_dir: PathBuf,
    #[allow(dead_code)]
    pub cookie: crate::auth_cookie::Cookie,
    /// PID of the running daemon, parsed from `.daemon`. 0 if unparseable.
    pub daemon_pid: u32,
}

/// Knobs for [`prepare`]. See field docs for semantics.
pub struct PrepareOptions {
    /// If true, fleet.yaml may be rewritten (general auto-create, topic_id
    /// backfill). Set false for read-only contexts like verifier/CI.
    pub mutate_fleet_yaml: bool,
    /// If true, initialize Telegram polling when `channel:` is configured.
    /// Set false for tests that don't need real bot traffic.
    pub init_telegram: bool,
    /// If true, resolve every fleet instance into an [`AgentDef`] (creates
    /// worktrees, generates instructions, appends resume/model/Claude flags).
    /// Set false for app mode, where pane_factory spawns on demand from tabs
    /// and the resolve work would duplicate what pane creation does.
    pub resolve_agents: bool,
}

impl Default for PrepareOptions {
    fn default() -> Self {
        Self {
            mutate_fleet_yaml: true,
            init_telegram: true,
            resolve_agents: true,
        }
    }
}

/// Prepare the current process for fleet ownership OR attachment.
///
/// Steps (Owned path):
/// 1. Early Attached check: `find_active_run_dir` — if another daemon is live,
///    read its cookie and return `Attached` without taking any locks.
/// 2. Acquire `.daemon.lock` exclusively. A competing daemon will be rejected
///    here.
/// 3. Re-check Attached after the lock (TOCTOU): another process could have
///    raced and started between step 1 and 2. In that case drop the lock and
///    return Attached.
/// 4. Create the PID-isolated run dir, write `.daemon` identity, issue
///    `api.cookie`.
/// 5. Load fleet.yaml, normalize (auto-create `general`, prune worktrees).
/// 6. Resolve every fleet instance into an [`AgentDef`] (working dir, worktree
///    creation, instructions, resume/model/claude flags).
/// 7. Initialize Telegram if requested and configured.
pub fn prepare(home: &Path, fleet_path: &Path, opts: PrepareOptions) -> Result<BootstrapOutcome> {
    std::fs::create_dir_all(home).with_context(|| format!("create home {}", home.display()))?;

    if let Some(attached) = try_attach(home, fleet_path)? {
        return Ok(BootstrapOutcome::Attached(attached));
    }

    let lock = acquire_daemon_lock(home)?;

    // Re-check after lock acquired: someone may have raced between the early
    // check and the lock grant. If so, release our lock (by dropping) and
    // return Attached — another daemon owns the truth.
    if let Some(attached) = try_attach(home, fleet_path)? {
        drop(lock);
        return Ok(BootstrapOutcome::Attached(attached));
    }

    // We hold the exclusive daemon lock, so no one else is attaching or
    // creating run dirs. Sweep any `~/.agend/run/*` left behind by prior
    // daemons whose PIDs have since been recycled — otherwise the first
    // one `find_active_run_dir` visits on a later app launch can lure that
    // process into attaching to a dead daemon (symptom: input lag from 2s
    // port-poll hitting closed sockets).
    crate::daemon::sweep_stale_run_dirs(home);

    let run_dir = crate::daemon::run_dir(home);
    std::fs::create_dir_all(&run_dir)
        .with_context(|| format!("create run dir {}", run_dir.display()))?;
    crate::daemon::write_daemon_id(&run_dir);

    let cookie = crate::auth_cookie::issue(&run_dir).context("issue api.cookie")?;

    let mut config = crate::fleet::FleetConfig::load(fleet_path)?;
    fleet_normalize::normalize(&mut config, home, opts.mutate_fleet_yaml);
    let agents = if opts.resolve_agents {
        agent_resolve::resolve(&config)
    } else {
        Vec::new()
    };

    let telegram = if opts.init_telegram {
        telegram_init::init(&config, home)
    } else {
        None
    };

    Ok(BootstrapOutcome::Owned(Box::new(OwnedFleet {
        home: home.to_path_buf(),
        fleet_path: fleet_path.to_path_buf(),
        config,
        agents,
        run_dir,
        cookie,
        telegram,
        lock,
    })))
}

/// Return `Some(AttachedFleet)` if a live daemon owns the run dir, else `None`.
/// Errors if the cookie file is missing — that would mean a running daemon
/// without auth, which we refuse to silently join.
///
/// Stale-run-dir handling: `find_active_run_dir`'s identity check only
/// confirms the `.daemon` file's pid matches the dir name (trivially true).
/// If the original daemon died and Windows recycled its PID for an unrelated
/// process, `is_pid_alive` returns true and we'd attach to a dead daemon —
/// every 2s port poll would then retry dead TCP sockets and stall input.
/// Probe `api.port` to confirm a real daemon is listening.
///
/// Probe failure is NOT sufficient evidence to delete the run dir here:
/// `try_attach` runs *before* the daemon lock is acquired, so a failing
/// probe can mean either (a) the daemon is genuinely dead (PID reused) OR
/// (b) a live daemon is still mid-bootstrap and hasn't bound the port yet.
/// We can't tell the two apart without timing heuristics, and mis-treating
/// (b) as dead would `remove_dir_all` a live daemon's state (issue #7).
/// Return `None` on probe failure; the caller then attempts to acquire the
/// exclusive daemon lock:
///   - If a live daemon holds it, lock acquisition fails and we bail out
///     with a clear "another daemon is running" error — the live daemon's
///     run dir is untouched.
///   - If the daemon is truly dead, lock acquisition succeeds and the
///     caller runs `sweep_stale_run_dirs`, which *under the lock* can
///     safely delete any run dir whose port is unreachable.
fn try_attach(home: &Path, fleet_path: &Path) -> Result<Option<AttachedFleet>> {
    let Some(run_dir) = crate::daemon::find_active_run_dir(home) else {
        return Ok(None);
    };
    if !crate::ipc::probe_api(&run_dir) {
        tracing::debug!(
            path = %run_dir.display(),
            "api.port unreachable — skipping attach (sweep will clean if truly stale)"
        );
        return Ok(None);
    }
    let cookie = crate::auth_cookie::read_cookie(&run_dir)
        .with_context(|| format!("existing daemon at {} has no api.cookie", run_dir.display()))?;
    let daemon_pid = crate::daemon::read_daemon_pid(&run_dir).unwrap_or(0);
    Ok(Some(AttachedFleet {
        home: home.to_path_buf(),
        fleet_path: fleet_path.to_path_buf(),
        run_dir,
        cookie,
        daemon_pid,
    }))
}

fn acquire_daemon_lock(home: &Path) -> Result<DaemonLock> {
    use fs2::FileExt;
    let path = home.join(".daemon.lock");
    let file = std::fs::File::create(&path).with_context(|| format!("open {}", path.display()))?;
    file.try_lock_exclusive().map_err(|e| {
        anyhow!("another agend-terminal daemon is already running (lock held): {e}")
    })?;
    Ok(DaemonLock { _file: file })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_home(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-bootstrap-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn write_minimal_fleet(home: &Path) -> PathBuf {
        let path = home.join("fleet.yaml");
        std::fs::write(
            &path,
            "defaults:\n  backend: claude\ninstances:\n  worker:\n    backend: claude\n",
        )
        .expect("write fleet.yaml");
        path
    }

    /// Stand up a loopback listener and write its port as `api.port` inside
    /// `run_dir`, mimicking a real daemon for `probe_api` purposes. Returns
    /// the listener so the caller keeps it alive for the duration of the test.
    fn fake_api_listener(run_dir: &Path) -> std::net::TcpListener {
        let listener = crate::ipc::bind_loopback().expect("bind");
        let port = crate::ipc::local_port(&listener);
        crate::ipc::write_port(run_dir, crate::ipc::API_NAME, port).expect("write api.port");
        listener
    }

    #[test]
    fn prepare_owned_issues_cookie_and_lock() {
        let home = tmp_home("owned");
        let fleet = write_minimal_fleet(&home);
        let opts = PrepareOptions {
            mutate_fleet_yaml: false,
            init_telegram: false,
            resolve_agents: false,
        };
        let outcome = prepare(&home, &fleet, opts).expect("prepare");
        match outcome {
            BootstrapOutcome::Owned(o) => {
                assert!(o.run_dir.join("api.cookie").exists());
                assert!(o.run_dir.join(".daemon").exists());
                assert_eq!(o.cookie.len(), 32);
                // Lock file should exist now that we took it.
                assert!(home.join(".daemon.lock").exists());
            }
            BootstrapOutcome::Attached(_) => panic!("expected Owned on fresh home"),
        }
        std::fs::remove_dir_all(&home).ok();
    }

    /// Regression: the Telegram-in-app bug was caused by `api::serve` reading
    /// `api.cookie` from the run dir (src/api.rs:123) and aborting when the
    /// file was missing. If bootstrap::prepare ever stopped issuing the cookie
    /// before returning Owned, every `api::call` from `inbox::notify_agent`
    /// would silently fail again. This test binds the invariant: after prepare
    /// returns Owned, `auth_cookie::read_cookie` succeeds with the same bytes.
    #[test]
    fn owned_cookie_is_readable_for_api_serve() {
        let home = tmp_home("api_cookie");
        let fleet = write_minimal_fleet(&home);
        let opts = PrepareOptions {
            mutate_fleet_yaml: false,
            init_telegram: false,
            resolve_agents: false,
        };
        let outcome = prepare(&home, &fleet, opts).expect("prepare");
        let BootstrapOutcome::Owned(o) = outcome else {
            panic!("expected Owned");
        };
        // api::serve calls read_cookie and aborts on Err — this is the exact
        // call path it would use. Must succeed with a 32-byte cookie.
        let read = crate::auth_cookie::read_cookie(&o.run_dir).expect("read_cookie");
        assert_eq!(read.len(), 32);
        assert_eq!(read, o.cookie);
        std::fs::remove_dir_all(&home).ok();
    }

    /// Refuse to silently attach to a daemon that has a live api port but
    /// no `api.cookie`. The alternative — treat cookie-less run dirs as
    /// Attached — would mean a client could land in a state where it thinks
    /// a daemon is live but has no way to authenticate against its API.
    ///
    /// Note: a run dir with no listener on api.port is now treated as stale
    /// and swept, not as a cookie-less daemon (that case is covered by
    /// `prepare_sweeps_run_dir_with_dead_api`).
    #[test]
    fn attach_fails_when_cookie_missing() {
        let home = tmp_home("no_cookie");
        let fleet = write_minimal_fleet(&home);
        let run = crate::daemon::run_dir(&home);
        std::fs::create_dir_all(&run).expect("mkdir run");
        crate::daemon::write_daemon_id(&run);
        let _listener = fake_api_listener(&run);
        // Deliberately DO NOT call auth_cookie::issue. probe_api will pass
        // (listener is alive), try_attach will then bail on read_cookie.

        let opts = PrepareOptions {
            mutate_fleet_yaml: false,
            init_telegram: false,
            resolve_agents: false,
        };
        let err = match prepare(&home, &fleet, opts) {
            Ok(_) => panic!("must not silently Attach or Own when cookie is missing"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("api.cookie"),
            "error should mention api.cookie, got: {msg}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn prepare_attaches_when_run_dir_alive() {
        // Simulate a live daemon: current PID + cookie + bound api port.
        let home = tmp_home("attached");
        let fleet = write_minimal_fleet(&home);
        let run = crate::daemon::run_dir(&home);
        std::fs::create_dir_all(&run).expect("mkdir run");
        crate::daemon::write_daemon_id(&run);
        let _ = crate::auth_cookie::issue(&run).expect("issue cookie for fake daemon");
        let _listener = fake_api_listener(&run);

        let opts = PrepareOptions {
            mutate_fleet_yaml: false,
            init_telegram: false,
            resolve_agents: false,
        };
        let outcome = prepare(&home, &fleet, opts).expect("prepare");
        match outcome {
            BootstrapOutcome::Attached(a) => {
                assert_eq!(a.run_dir, run);
                assert_eq!(a.daemon_pid, std::process::id());
            }
            BootstrapOutcome::Owned(_) => {
                panic!("expected Attached when live daemon owns run_dir")
            }
        }
        std::fs::remove_dir_all(&home).ok();
    }

    /// Regression for the "lag from attached to dead daemon" bug.
    ///
    /// A previous daemon died; Windows recycled its PID for an unrelated
    /// process so `is_pid_alive` keeps returning true. Without a port probe
    /// bootstrap would Attach to a run dir where nothing is listening — the
    /// TUI's 2-second port poll would then spin on failing TCP connects and
    /// starve the input loop.
    ///
    /// Expectation: the stale run dir is swept, `prepare` returns Owned with
    /// a fresh run dir for the current process, and the old `.daemon` /
    /// `api.cookie` files are gone.
    #[test]
    fn prepare_sweeps_run_dir_with_dead_api() {
        let home = tmp_home("dead_api");
        let fleet = write_minimal_fleet(&home);
        // Use a dir name = current pid so is_pid_alive returns true, but
        // never open a listener — probe_api must fail.
        let run = crate::daemon::run_dir(&home);
        std::fs::create_dir_all(&run).expect("mkdir run");
        crate::daemon::write_daemon_id(&run);
        let stale_cookie_bytes = crate::auth_cookie::issue(&run).expect("issue cookie");

        let opts = PrepareOptions {
            mutate_fleet_yaml: false,
            init_telegram: false,
            resolve_agents: false,
        };
        let outcome = prepare(&home, &fleet, opts).expect("prepare");
        match outcome {
            BootstrapOutcome::Owned(o) => {
                // run_dir is pid-keyed so the path may be the same, but the
                // stale state must have been wiped and re-issued fresh.
                assert_ne!(
                    o.cookie, stale_cookie_bytes,
                    "fresh cookie must differ from the stale one we planted"
                );
                let fresh = crate::auth_cookie::read_cookie(&o.run_dir).expect("read");
                assert_eq!(fresh, o.cookie);
            }
            BootstrapOutcome::Attached(_) => {
                panic!("must not Attach to a run dir whose api.port is dead");
            }
        }
        std::fs::remove_dir_all(&home).ok();
    }

    /// Sweep removes a pid-named dir whose `api.port` has no listener, even
    /// when `is_pid_alive` still returns true (PID reuse). Non-pid siblings
    /// (stray files, lock files written later) must be left alone.
    #[test]
    fn sweep_stale_run_dirs_removes_dead_api_entries() {
        let home = tmp_home("sweep");
        let run_base = home.join("run");
        std::fs::create_dir_all(&run_base).expect("mkdir run");

        // Use our own pid for the stale dir name: is_pid_alive returns true
        // (we're running), but no listener is bound so probe_api returns
        // false — the combination that used to leak across sessions.
        let our_pid = std::process::id().to_string();
        let stale = run_base.join(&our_pid);
        std::fs::create_dir_all(&stale).expect("mkdir stale");
        std::fs::write(stale.join(".daemon"), format!("{our_pid}:0")).expect("write .daemon");
        // Non-pid sibling — sweep must leave it alone.
        let non_pid = run_base.join("not-a-pid");
        std::fs::create_dir_all(&non_pid).expect("mkdir non-pid");

        crate::daemon::sweep_stale_run_dirs(&home);

        assert!(
            !stale.exists(),
            "stale pid-named dir with dead api must be swept"
        );
        assert!(
            non_pid.exists(),
            "non-pid-named sibling must be left intact"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Issue #7 regression: when a daemon is mid-bootstrap — `.daemon` and
    /// `api.cookie` written but `api.port` not bound yet — `try_attach` must
    /// NOT `remove_dir_all` the run dir. Prior to this fix a racing `app`
    /// process could clobber a live daemon's state, because `probe_api` fails
    /// during the bootstrap window and the old code treated that as "stale".
    ///
    /// Expectation: `try_attach` returns `None` (port not listening ⇒ not
    /// ready to attach), but all files in the run dir survive untouched, so
    /// the bootstrapping daemon can finish writing its state.
    #[test]
    fn try_attach_preserves_run_dir_when_port_unreachable() {
        let home = tmp_home("mid_bootstrap");
        let fleet = write_minimal_fleet(&home);
        let run = crate::daemon::run_dir(&home);
        std::fs::create_dir_all(&run).expect("mkdir run");
        crate::daemon::write_daemon_id(&run);
        let planted_cookie =
            crate::auth_cookie::issue(&run).expect("issue cookie to simulate bootstrap");
        // Deliberately skip `fake_api_listener` / `write_port` — this is the
        // exact state of a daemon between `auth_cookie::issue` and
        // `api::serve` binding its loopback port.

        let outcome = try_attach(&home, &fleet).expect("try_attach");
        assert!(
            outcome.is_none(),
            "must not attach when api.port is unreachable"
        );
        assert!(
            run.exists(),
            "mid-bootstrap run dir must not be deleted by try_attach (issue #7)"
        );
        assert!(
            run.join(".daemon").exists(),
            ".daemon file must survive try_attach probe failure"
        );
        let surviving = crate::auth_cookie::read_cookie(&run)
            .expect("cookie file must survive try_attach probe failure");
        assert_eq!(
            surviving, planted_cookie,
            "cookie bytes must be unchanged — try_attach must not have rewritten"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
