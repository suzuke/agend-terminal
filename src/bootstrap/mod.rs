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
mod attach_detect;
pub(crate) mod canonical_hygiene;
pub mod daemon_spawn;
pub(crate) mod doctor;
pub(crate) mod doctor_topics;
mod fleet_normalize;
pub mod signals;
mod telegram_init;

pub use agent_resolve::AgentDef;

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// #945 Phase 0 — wrap a bootstrap step with `Instant::now()` + emit a
/// `tracing::info!` line carrying `step` + `elapsed_ms`. Operator post-boot:
///
/// ```bash
/// grep "bootstrap-step" $AGEND_HOME/daemon.log | \
///   grep -oE 'step="[^"]+" elapsed_ms=[0-9]+' | \
///   sort -t= -k2 -n -r | head -10
/// ```
///
/// Surfaces hottest steps for Phase 1+ optimization candidate selection.
/// Pure instrumentation: zero behavior change, single `tracing::info!` per
/// step. Generic over the wrapped fn's return type so it composes with both
/// statement-style sites (`()` return) and expression-style sites (e.g.
/// `Option<AttachedFleet>` from `try_attach`).
pub(crate) fn time_step<T>(name: &'static str, f: impl FnOnce() -> T) -> T {
    let t0 = std::time::Instant::now();
    let result = f();
    // Use default target (crate module path) so `tracing_test::traced_test`
    // captures the event. Operator-side filtering uses the "bootstrap-step"
    // message string + structured `step=` field — both grep-friendly.
    tracing::info!(
        step = name,
        elapsed_ms = t0.elapsed().as_millis() as u64,
        "bootstrap-step"
    );
    result
}

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
/// Some fields are scaffolding for follow-on work — `fleet_path` is retained
/// for diagnostics and operator-facing tools that need to surface the active
/// fleet config; `cookie` is read by tests + may be read by callers that want
/// to avoid re-reading the cookie file per connection. `#[allow(dead_code)]`
/// is applied per-field so genuinely new unused fields still trip `-D warnings`
/// in CI.
pub struct OwnedFleet {
    pub home: PathBuf,
    #[allow(dead_code)] // read by diagnostics + tests
    pub fleet_path: PathBuf,
    pub config: crate::fleet::FleetConfig,
    pub agents: Vec<AgentDef>,
    pub run_dir: PathBuf,
    #[allow(dead_code)] // read by tests; avoids re-reading cookie file
    pub cookie: crate::auth_cookie::Cookie,
    pub telegram: Option<Arc<dyn crate::channel::Channel>>,
    /// Flock guard — drop releases `.daemon.lock`. Kept last so the lock is
    /// released only after every other resource has been dropped.
    #[allow(dead_code)] // RAII guard: drop releases daemon lock
    pub lock: DaemonLock,
}

/// Attached state: an existing daemon owns the run dir. We read its cookie so
/// we can speak the TUI/API protocols but never touch the run dir itself.
///
/// `home` / `fleet_path` / `cookie` are scaffolding — today `BridgeClient`
/// re-derives them per connection, but a future per-pane cache would read
/// these. See OwnedFleet note about why `#[allow(dead_code)]` is per-field.
pub struct AttachedFleet {
    #[allow(dead_code)] // scaffolding for future per-pane cache
    pub home: PathBuf,
    #[allow(dead_code)] // scaffolding for future per-pane cache
    pub fleet_path: PathBuf,
    pub run_dir: PathBuf,
    #[allow(dead_code)] // scaffolding for future per-pane cache
    pub cookie: crate::auth_cookie::Cookie,
    /// PID of the running daemon, parsed from `.daemon`. 0 if unparseable.
    pub daemon_pid: u32,
}

/// Knobs for [`prepare`]. See field docs for semantics.
pub struct PrepareOptions {
    /// If true, fleet.yaml may be rewritten (general auto-create) and
    /// topics.json updated. Set false for read-only contexts like verifier/CI.
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

    // #945 Phase 0 instrumentation: every reconcile / sweep / migration
    // step in `prepare` + `run_core` is wrapped with `time_step` so the
    // operator can post-boot grep `bootstrap-step` log lines for an
    // empirical timing distribution.
    if let Some(attached) = time_step("try_attach (pre-lock)", || try_attach(home, fleet_path))? {
        return Ok(BootstrapOutcome::Attached(attached));
    }

    let lock = time_step("acquire_daemon_lock", || acquire_daemon_lock(home))?;

    // Re-check after lock acquired: someone may have raced between the early
    // check and the lock grant. If so, release our lock (by dropping) and
    // return Attached — another daemon owns the truth.
    if let Some(attached) = time_step("try_attach (post-lock-TOCTOU)", || {
        try_attach(home, fleet_path)
    })? {
        drop(lock);
        return Ok(BootstrapOutcome::Attached(attached));
    }

    // We hold the exclusive daemon lock, so no one else is attaching or
    // creating run dirs. Sweep any `~/.agend/run/*` left behind by prior
    // daemons whose PIDs have since been recycled — otherwise the first
    // one `find_active_run_dir` visits on a later app launch can lure that
    // process into attaching to a dead daemon (symptom: input lag from 2s
    // port-poll hitting closed sockets).
    time_step("sweep_stale_run_dirs", || {
        crate::daemon::sweep_stale_run_dirs(home);
    });
    // #933: alive-but-stale zombie sweep — complements the dead-PID sweep
    // above. Always-on telemetry log per candidate; destructive kill is
    // env-gated via AGEND_DAEMON_BOOT_SWEEP_AGE_DAYS=N. Reuses #927 PR-B
    // primitives (`admin::cleanup_zombies`).
    time_step("boot_sweep_zombies", || {
        crate::daemon::boot_sweep::boot_sweep_zombies(home);
    });
    // #942/#943: rename legacy-format watch files (DefaultHasher+non-canonical)
    // to canonical+sha256 form. One-shot at boot; idempotent on repeated runs.
    // Active migration (option b from dev-2 cross-audit Pushback 3) prevents
    // the 72h duplicate-notification window across the hash-scheme transition.
    time_step("migrate_legacy_watch_filenames", || {
        crate::daemon::ci_watch::migration::migrate_legacy_watch_filenames(home);
    });
    // #1017: suppress stale pr-state terminal-replay at boot. Without
    // this, a fresh daemon process re-emits [pr-merged] /
    // [pr-closed-unmerged] for every Merged / ClosedUnmerged pr-state
    // file older than `AGEND_PR_STATE_REPLAY_AGE_HOURS` (default 1h)
    // — operator gets a flood of stale notifications. Fresh terminal-
    // state files (post-restart actual merges) still fire normally.
    time_step("pr_state_suppress_stale_replay", || {
        crate::daemon::pr_state::suppress_stale_terminal_replay(home);
    });
    // #704 Phase 1b: warn when AGEND_CAPTURE_FIXTURES=1 is active at
    // boot. The capture sink writes raw PTY bytes — including operator
    // prompts and any tool output — to $AGEND_HOME/captures/. Captures
    // intended for `capture promote` MUST be operator-reviewed before
    // landing in tests/fixtures/state-replay/ because the raw byte
    // stream can contain secrets, API keys echoed during tool calls,
    // path globs containing usernames, etc. Single warn line per boot
    // is sufficient — the env-var lifetime is process-bound, so this
    // fires exactly when the operator opted in.
    time_step("capture_fixtures_privacy_warn", || {
        if std::env::var("AGEND_CAPTURE_FIXTURES").as_deref() == Ok("1") {
            tracing::warn!(
                "#704 AGEND_CAPTURE_FIXTURES=1 active — PTY captures land at \
                 $AGEND_HOME/captures/<agent>/ and may contain secrets / prompts. \
                 Operator MUST review tests/fixtures/state-replay/*.raw before \
                 commit. Promote workflow: see docs/CONTRIBUTING.md \"Capturing a new fixture\"."
            );
        }
    });

    let run_dir = crate::daemon::run_dir(home);
    std::fs::create_dir_all(&run_dir)
        .with_context(|| format!("create run dir {}", run_dir.display()))?;
    crate::daemon::write_daemon_id(&run_dir);

    let cookie = crate::auth_cookie::issue(&run_dir).context("issue api.cookie")?;

    let mut config = crate::fleet::FleetConfig::load(fleet_path)?;
    fleet_normalize::normalize(&mut config, home, opts.mutate_fleet_yaml);

    // Doctor pre-flight: detect operator pitfalls in fleet.yaml and emit
    // actionable diagnostics before any agents spawn. Sprint 23 P1 —
    // deferred from Sprint 22 P0 PR #230.
    let diags = doctor::validate_fleet_config(&config, home);
    doctor::emit_diagnostics(&diags);

    let agents = if opts.resolve_agents {
        let fleet_dir = fleet_path.parent().unwrap_or(home);
        time_step("agent_resolve::resolve", || {
            agent_resolve::resolve(&config, fleet_dir, home)
        })
    } else {
        Vec::new()
    };

    let telegram = if opts.init_telegram {
        time_step("telegram_init::init", || telegram_init::init(&config, home))
    } else {
        None
    };

    // agend-git-shim init (shared by daemon + app mode).
    time_step("protocol::extract_default", || {
        crate::protocol::extract_default(home);
    });
    time_step("binding::reconcile_hooks", || {
        crate::binding::reconcile_hooks(home);
    });
    time_step("binding::symlink_shim", || {
        crate::binding::symlink_shim(home);
    });
    time_step("binding::reconcile_orphans", || {
        crate::binding::reconcile_orphans(home);
    });
    time_step("worktree_pool::reconcile_orphan_leases", || {
        crate::worktree_pool::reconcile_orphan_leases(home);
    });
    // #852 PR-C: canonical-repo hygiene. Scan distinct source_repo
    // values from fleet.yaml; for each canonical, auto-switch
    // detached HEAD back to main if the working tree is clean, or
    // warn-log if dirty (operator WIP protection). Pure observation
    // beyond the single `git switch main` mutation when conditions
    // are right. Best-effort — boot continues regardless.
    time_step("canonical_hygiene::run_hygiene", || {
        canonical_hygiene::run_hygiene(&config);
    });
    // #829 Fix A: boot-time orphan-owner sweep. We pass `live = ∅`
    // explicitly because `api::serve` hasn't bound the Unix socket
    // yet (that happens later in `daemon::run_with_prepared`) and no
    // fleet agents have spawned (auto-start runs even later). The
    // pre-Fix A call resolved `live` via `api::call(LIST)` here, which
    // always returned None at this point in bootstrap → guaranteed
    // no-op every boot → ghost owners accumulated until manual
    // cleanup. Strict ghosts (∉ fleet.yaml ∧ ∉ live) are auto-cleared;
    // Soft candidates (∈ fleet.yaml ∧ ∉ live) stay dry-run so the
    // "agent not yet spawned" case isn't over-orphaned. Periodic
    // post-boot callers should keep using `reconcile_orphan_owners`
    // which still fetches `live` from the running daemon.
    time_step("tasks::reconcile_orphan_owners_with_live", || {
        crate::tasks::reconcile_orphan_owners_with_live(home, &std::collections::HashSet::new());
    });

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
/// Probe `api.port` to guard against PID reuse: `is_pid_alive` can't tell
/// a live daemon from a recycled-PID impostor (#6, Windows input-lag bug).
/// Probe failure alone is NOT enough to delete the run dir — `try_attach`
/// runs before the daemon lock, and a failing probe can also mean a live
/// daemon is mid-bootstrap and hasn't bound its port yet; deleting would
/// clobber its state (#7). Return `None` on failure and let the caller's
/// lock acquisition + `sweep_stale_run_dirs` distinguish dead from starting.
/// Sprint 57 Wave 3 PR-2 (#548 Q2 contract pin): daemon discovery is
/// dual-layer — `find_active_run_dir` provides the lockfile/PID
/// evidence and `probe_api` provides the live-TCP evidence. Both
/// must pass before we attach. This is the contract; do NOT
/// degrade to lockfile-only OR API-only without re-validating
/// discovery semantics across PID-recycling, kill -9, and stale
/// rundir scenarios.
fn try_attach(home: &Path, fleet_path: &Path) -> Result<Option<AttachedFleet>> {
    // #969 RC1: bounded-backoff retry closes the App→Daemon race window
    // where `find_active_run_dir` returns None because the daemon's
    // `.daemon` file hasn't landed yet but the process is mid-bootstrap.
    // 3×100ms total budget; on miss the App falls through to Owned mode
    // (and the #984 dedup module catches any wire-layer duplicates).
    let Some(run_dir) = attach_detect::find_active_run_dir_backoff(home) else {
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
    let path = home.join(".daemon.lock");
    let file = std::fs::File::create(&path).with_context(|| format!("open {}", path.display()))?;
    // Explicit trait method: Rust 1.89 stabilized inherent
    // `File::try_lock` shadowing the trait method; current MSRV is 1.87.
    fs4::FileExt::try_lock(&file).map_err(|e| {
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
        let path = crate::fleet::fleet_yaml_path(home);
        std::fs::write(
            &path,
            "defaults:\n  backend: claude\ninstances:\n  worker:\n    backend: claude\n",
        )
        .expect("write fleet.yaml");
        path
    }

    fn write_fleet_with_extra_instructions(home: &Path) -> PathBuf {
        let path = crate::fleet::fleet_yaml_path(home);
        let instructions_dir = home.join("instructions");
        std::fs::create_dir_all(&instructions_dir).expect("mkdir instructions");
        std::fs::write(
            instructions_dir.join("dev.md"),
            "# Extra Instructions\nAlways include rollout checklist.",
        )
        .expect("write instructions file");
        let work_dir = crate::paths::workspace_dir(home).join("worker");
        let yaml = format!(
            "defaults:\n  backend: claude\ninstances:\n  worker:\n    backend: claude\n    working_directory: {}\n    instructions: ./instructions/dev.md\n",
            work_dir.display()
        );
        std::fs::write(&path, yaml).expect("write fleet with instructions");
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

    #[test]
    fn prepare_resolve_agents_applies_extra_instructions_to_generated_file() {
        let home = tmp_home("resolve_extra_instructions");
        let fleet = write_fleet_with_extra_instructions(&home);
        let opts = PrepareOptions {
            mutate_fleet_yaml: false,
            init_telegram: false,
            resolve_agents: true,
        };
        let outcome = prepare(&home, &fleet, opts).expect("prepare");
        let BootstrapOutcome::Owned(_owned) = outcome else {
            panic!("expected Owned for fresh temp home");
        };
        let generated = std::fs::read_to_string(home.join("workspace/worker/.claude/agend.md"))
            .expect("generated .claude/agend.md");
        assert!(
            generated.contains("Always include rollout checklist."),
            "generated instructions must include extra file content"
        );
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

    // ── #945 Phase 0 instrumentation tests ──

    /// Unit: `time_step` calls the wrapped fn exactly once and emits a
    /// `bootstrap-step` log line carrying the expected `step` + `elapsed_ms`
    /// fields.
    #[tracing_test::traced_test]
    #[test]
    fn time_step_emits_log_with_step_name_and_elapsed_ms() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let counter = AtomicU32::new(0);
        let result = time_step("test_step_name", || {
            counter.fetch_add(1, Ordering::Relaxed);
            42
        });
        assert_eq!(result, 42, "wrapped fn return value must pass through");
        assert_eq!(
            counter.load(Ordering::Relaxed),
            1,
            "wrapped fn must be called exactly once"
        );
        assert!(
            logs_contain("bootstrap-step"),
            "log must include the bootstrap-step message"
        );
        assert!(
            logs_contain("test_step_name"),
            "log must include the step name field"
        );
        assert!(
            logs_contain("elapsed_ms"),
            "log must include the elapsed_ms field"
        );
    }

    /// Integration: `prepare` emits at least 13 `bootstrap-step` log lines
    /// covering the canonical reconcile / sweep / migration steps. We don't
    /// pin the exact count (sites may grow over time) but assert every
    /// step name we instrumented is present.
    #[tracing_test::traced_test]
    #[test]
    fn prepare_emits_bootstrap_step_lines_for_every_instrumented_site() {
        let home = tmp_home("945-bootstrap-steps");
        let fleet = write_minimal_fleet(&home);
        // resolve_agents=false keeps the test fast (no agent worktree
        // creation) while still exercising every other step.
        let opts = PrepareOptions {
            mutate_fleet_yaml: false,
            init_telegram: false,
            resolve_agents: false,
        };
        let _ = prepare(&home, &fleet, opts).expect("prepare must succeed");

        // Canonical step names instrumented in `prepare`. New sites added
        // by future PRs should also appear here so the test catches
        // regressions where someone strips the `time_step` wrapper.
        let expected_steps = [
            "try_attach (pre-lock)",
            "acquire_daemon_lock",
            "sweep_stale_run_dirs",
            "boot_sweep_zombies",
            "migrate_legacy_watch_filenames",
            "protocol::extract_default",
            "binding::reconcile_hooks",
            "binding::symlink_shim",
            "binding::reconcile_orphans",
            "worktree_pool::reconcile_orphan_leases",
            "canonical_hygiene::run_hygiene",
            "tasks::reconcile_orphan_owners_with_live",
        ];
        for step in expected_steps {
            assert!(
                logs_contain(step),
                "prepare must emit bootstrap-step for `{step}`"
            );
        }
        assert!(
            logs_contain("bootstrap-step"),
            "logs must include the bootstrap-step message marker"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #704 Phase 1b: `AGEND_CAPTURE_FIXTURES=1` at boot must emit a
    /// single privacy `tracing::warn` line so operators know raw PTY
    /// captures may contain secrets / prompts and require review
    /// before commit.
    #[test]
    #[serial_test::serial]
    #[tracing_test::traced_test]
    fn t704_phase1b_capture_fixtures_active_emits_privacy_warn() {
        // SAFETY: serial-gated; restore at end. The bootstrap step
        // wrapper reads the env var directly so we set/unset around
        // the invocation. Bare `time_step(...)` runs the closure
        // in-band without daemon scaffolding.
        unsafe { std::env::set_var("AGEND_CAPTURE_FIXTURES", "1") };
        let home = tmp_home("capture-warn-active");
        time_step("capture_fixtures_privacy_warn", || {
            if std::env::var("AGEND_CAPTURE_FIXTURES").as_deref() == Ok("1") {
                tracing::warn!(
                    "#704 AGEND_CAPTURE_FIXTURES=1 active — PTY captures may contain \
                     secrets / prompts. Operator MUST review tests/fixtures/state-replay/ \
                     before commit. Promote workflow: see docs/CONTRIBUTING.md."
                );
            }
        });
        assert!(
            logs_contain("AGEND_CAPTURE_FIXTURES=1 active"),
            "privacy warn must fire when env var is active"
        );
        assert!(logs_contain("#704"), "warn must self-identify as #704 hook");
        unsafe { std::env::remove_var("AGEND_CAPTURE_FIXTURES") };
        std::fs::remove_dir_all(&home).ok();
    }

    /// #704 Phase 1b negative pin: no warn when the env var is unset.
    /// Anti-regression — operators not opting in must NOT see noise.
    #[test]
    #[serial_test::serial]
    #[tracing_test::traced_test]
    fn t704_phase1b_no_warn_when_capture_fixtures_unset() {
        unsafe { std::env::remove_var("AGEND_CAPTURE_FIXTURES") };
        let home = tmp_home("capture-warn-inactive");
        time_step("capture_fixtures_privacy_warn", || {
            if std::env::var("AGEND_CAPTURE_FIXTURES").as_deref() == Ok("1") {
                tracing::warn!("#704 AGEND_CAPTURE_FIXTURES=1 active — should NOT fire here");
            }
        });
        assert!(
            !logs_contain("AGEND_CAPTURE_FIXTURES=1 active"),
            "privacy warn MUST NOT fire when env var is unset"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
