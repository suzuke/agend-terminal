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
pub mod signals;
mod telegram_init;

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

    let run_dir = crate::daemon::run_dir(home);
    std::fs::create_dir_all(&run_dir)
        .with_context(|| format!("create run dir {}", run_dir.display()))?;
    crate::daemon::write_daemon_id(&run_dir);

    let cookie = crate::auth_cookie::issue(&run_dir).context("issue api.cookie")?;

    let mut config = crate::fleet::FleetConfig::load(fleet_path)?;
    fleet_normalize::normalize(&mut config, home, opts.mutate_fleet_yaml);
    let agents = if opts.resolve_agents {
        agent_resolve::resolve(&config, home)
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
fn try_attach(home: &Path, fleet_path: &Path) -> Result<Option<AttachedFleet>> {
    let Some(run_dir) = crate::daemon::find_active_run_dir(home) else {
        return Ok(None);
    };
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

    /// Refuse to silently attach to a daemon that has no `api.cookie`. The
    /// alternative — treat cookie-less run dirs as Attached — would mean a
    /// client could land in a state where it thinks a daemon is live but has
    /// no way to authenticate against its API.
    #[test]
    fn attach_fails_when_cookie_missing() {
        let home = tmp_home("no_cookie");
        let fleet = write_minimal_fleet(&home);
        let run = crate::daemon::run_dir(&home);
        std::fs::create_dir_all(&run).expect("mkdir run");
        crate::daemon::write_daemon_id(&run);
        // Deliberately DO NOT call auth_cookie::issue. find_active_run_dir
        // will see the live-looking run dir, try_attach will then bail on
        // read_cookie, and prepare returns Err.

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
        // Simulate a live daemon by creating a run dir with current PID + cookie.
        let home = tmp_home("attached");
        let fleet = write_minimal_fleet(&home);
        let run = crate::daemon::run_dir(&home);
        std::fs::create_dir_all(&run).expect("mkdir run");
        crate::daemon::write_daemon_id(&run);
        let _ = crate::auth_cookie::issue(&run).expect("issue cookie for fake daemon");

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
}
