//! Bootstrap seam shared by `start` (daemon) and `app` (TUI) entry points.
//!
//! Prior to this module, `cli::start_with_fleet` and `app::run` each did their
//! own preflight — create run dir, write `.daemon`, issue api.cookie, normalize
//! fleet.yaml, init telegram — with subtle divergences that caused the
//! `api.cookie missing; aborting serve` regression in app mode. This module
//! centralizes every pre-spawn concern behind one call: [`prepare`].
//!
//! Outcome:
//! - [`BootstrapOutcome::Owned`]: this process is the daemon. Holds the
//!   exclusive `.daemon.lock` flock, owns the run dir, and carries the issued
//!   api.cookie plus fully-resolved agent specs.
//! - [`BootstrapOutcome::Attached`]: another daemon already owns the run dir.
//!   The current process is a client and should not touch run dir ownership.

mod agent_resolve;
mod fleet_normalize;
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
pub struct OwnedFleet {
    pub home: PathBuf,
    pub fleet_path: PathBuf,
    pub config: crate::fleet::FleetConfig,
    pub agents: Vec<AgentDef>,
    pub run_dir: PathBuf,
    pub cookie: crate::auth_cookie::Cookie,
    pub telegram: Option<Arc<Mutex<crate::telegram::TelegramState>>>,
    /// Flock guard — drop releases `.daemon.lock`. Kept last so the lock is
    /// released only after every other resource has been dropped.
    pub lock: DaemonLock,
}

/// Attached state: an existing daemon owns the run dir. We read its cookie so
/// we can speak the TUI/API protocols but never touch the run dir itself.
pub struct AttachedFleet {
    pub home: PathBuf,
    pub fleet_path: PathBuf,
    pub run_dir: PathBuf,
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
pub fn prepare(
    home: &Path,
    fleet_path: &Path,
    opts: PrepareOptions,
) -> Result<BootstrapOutcome> {
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
    write_daemon_identity(&run_dir);

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
    let cookie = crate::auth_cookie::read_cookie(&run_dir).with_context(|| {
        format!(
            "existing daemon at {} has no api.cookie",
            run_dir.display()
        )
    })?;
    let daemon_pid = read_daemon_pid(&run_dir).unwrap_or(0);
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
    let file = std::fs::File::create(&path)
        .with_context(|| format!("open {}", path.display()))?;
    file.try_lock_exclusive()
        .map_err(|e| anyhow!("another agend-terminal daemon is already running (lock held): {e}"))?;
    Ok(DaemonLock { _file: file })
}

fn write_daemon_identity(run_dir: &Path) {
    let pid = std::process::id();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = std::fs::write(run_dir.join(".daemon"), format!("{pid}:{now}"));
}

fn read_daemon_pid(run_dir: &Path) -> Option<u32> {
    let content = std::fs::read_to_string(run_dir.join(".daemon")).ok()?;
    content
        .trim()
        .split_once(':')
        .and_then(|(pid, _)| pid.parse().ok())
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

    #[test]
    fn prepare_attaches_when_run_dir_alive() {
        // Simulate a live daemon by creating a run dir with current PID + cookie.
        let home = tmp_home("attached");
        let fleet = write_minimal_fleet(&home);
        let run = crate::daemon::run_dir(&home);
        std::fs::create_dir_all(&run).expect("mkdir run");
        write_daemon_identity(&run);
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
