use super::*;

// ── #bughunt-r1 #1: post-grace SIGKILL must not hit clean exits ─────────

#[test]
fn grace_disposition_reaps_clean_exit_does_not_hard_kill() {
    // A child that exited cleanly within the grace window → ReapOnly. This
    // is the crux of the fix: it must NOT escalate to kill_process_tree,
    // whose PID may have been reused by an unrelated process.
    assert_eq!(grace_disposition(false), GraceDisposition::ReapOnly);
    // A holdout still alive after the grace window → HardKill.
    assert_eq!(grace_disposition(true), GraceDisposition::HardKill);
}

/// Source-scan invariant: in `terminate_agents_parallel` (the shared
/// parallel-teardown core, extracted from `shutdown_sequence` for
/// restart-freeze 真嫌#1), `kill_process_tree` must only be reachable via the
/// `GraceDisposition::HardKill` arm — never an unconditional call (the bug).
/// Guards against a regression that bypasses `grace_disposition` and SIGKILLs
/// every child's (possibly-reused) PID.
#[test]
fn shutdown_kill_process_tree_only_in_hard_kill_arm_bughunt_r1() {
    let src = include_str!("mod.rs");
    let start = src
        .find("pub(crate) fn terminate_agents_parallel(")
        .expect("terminate_agents_parallel present");
    let after = &src[start..];
    // Scope to the fn body up to the start of the #[cfg(test)] module.
    let cfg_test = ["#[cfg(", "test)]"].concat();
    let end = after.find(&cfg_test).unwrap_or(after.len());
    let body = &after[..end];

    let hard_kill_arm = body
        .find("GraceDisposition::HardKill =>")
        .expect("shutdown_sequence must branch on GraceDisposition::HardKill");
    let kill_call = body
        .find("kill_process_tree(")
        .expect("shutdown_sequence still calls kill_process_tree for holdouts");
    assert!(
        kill_call > hard_kill_arm,
        "#bughunt-r1 #1: kill_process_tree must appear only inside the \
         GraceDisposition::HardKill arm, never unconditionally (a clean exit \
         during the grace window must be reaped, not SIGKILL'd)"
    );
}

fn tmp_home(name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-daemon-test-{}-{}-{}",
        std::process::id(),
        name,
        id
    ));
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// A1: `write_daemon_id` publishes the `.daemon` identity via
/// `store::atomic_write`, so a concurrent `read_daemon_pid` liveness probe
/// never sees a torn `pid:now:token` record. DISCRIMINATING via a side
/// effect unique to `atomic_write`: it `create_dir_all`s the parent, so a
/// write into a not-yet-created run_dir succeeds and round-trips — the
/// pre-fix plain `std::fs::write` (no parent creation, error swallowed by
/// `let _ =`) would leave no file at all.
#[test]
fn write_daemon_id_atomic_write_roundtrip_a1() {
    let home = tmp_home("a1-daemon-id");
    let run_dir = home.join("run-not-yet-created");
    assert!(!run_dir.exists());
    write_daemon_id(&run_dir);
    assert_eq!(
        read_daemon_pid(&run_dir),
        Some(std::process::id()),
        "atomic_write must create run_dir + publish a complete .daemon record"
    );
    let raw = std::fs::read_to_string(run_dir.join(".daemon")).expect(".daemon present");
    assert_eq!(
        raw.split(':').count(),
        3,
        "complete pid:now:token record expected, got {raw:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #1814 Stage-2 (#t-27) source-scan invariant: the three shared-state
/// GC/migration steps must NOT live in the pre-flock `init_daemon_services`
/// (which runs before the predecessor exits on the handoff path), and the
/// handoff path must invoke `init_daemon_services_post_lock` only AFTER it
/// acquires the flock. Regression-proof: move any of the three back into
/// `init_daemon_services`, or call post_lock before the lock, and this fails.
#[test]
fn handoff_shared_state_init_runs_post_flock_not_pre_t27() {
    let src = include_str!("mod.rs");
    const NEEDLES: [&str; 3] = [
        "migrate_legacy_tasks_json_to_event_log",
        "cleanup_stale_stages",
        "cleanup_tmp_orphans",
    ];

    // Helper: body of a fn from its signature up to the next top-level `fn `.
    let body_of = |sig: &str| -> String {
        let start = src.find(sig).unwrap_or_else(|| panic!("{sig} present"));
        let rest = &src[start..];
        let end = rest[1..].find("\nfn ").map(|i| i + 1).unwrap_or(rest.len());
        rest[..end].to_string()
    };

    // (1) pre-flock init does NOT run any shared-state mutation.
    let pre = body_of("fn init_daemon_services(");
    for n in NEEDLES {
        assert!(
            !pre.contains(n),
            "#t-27: pre-flock init_daemon_services must NOT run `{n}` (escapes minimal pre-lock)"
        );
    }

    // (2) post-lock init owns all three.
    let post = body_of("fn init_daemon_services_post_lock(");
    for n in NEEDLES {
        assert!(
            post.contains(n),
            "#t-27: init_daemon_services_post_lock must run `{n}`"
        );
    }

    // (3) on the handoff path, post_lock is invoked AFTER flock acquisition
    //     (the last post_lock call site is the handoff one; the earlier one
    //     is the normal-boot arm, which already holds the flock).
    let acquire = src
        .find("acquire_daemon_lock_blocking(")
        .expect("handoff acquires the flock");
    let handoff_post = src
        .rfind("init_daemon_services_post_lock(home)")
        .expect("handoff calls post_lock");
    assert!(
        handoff_post > acquire,
        "#t-27: the handoff path must call init_daemon_services_post_lock AFTER \
         acquire_daemon_lock_blocking, never pre-flock"
    );
}

/// #t-27: `init_daemon_services_post_lock` runs end-to-end (all three steps)
/// on a clean home — the legacy migration is a no-op success (no tasks.json),
/// the GC steps are retention-gated. Exercises the real extracted fn.
#[test]
fn post_lock_init_ok_on_clean_home_t27() {
    let home = tmp_home("t27-postlock-clean");
    init_daemon_services_post_lock(&home).expect("post_lock init must succeed on a clean home");
}

#[test]
fn run_dir_contains_pid() {
    let home = tmp_home("run_dir");
    let dir = run_dir(&home);
    let pid = std::process::id().to_string();
    assert!(dir.display().to_string().contains(&pid));
    assert!(dir.ends_with(&pid));
    std::fs::remove_dir_all(&home).ok();
}

/// #1720 app-mode root fix: `register_event_subscribers` is the SINGLE
/// source of truth for the event-bus subscriber list (run_core, app::run_app,
/// and the test harness all route through it). Pin every one of the 12
/// per-pattern registrations so a dropped line — which would silently kill
/// that pattern's delivery in BOTH prod modes at once — fails CI. Source-level
/// pin (cross-platform-safe; survives rustfmt). When adding a pattern, add it
/// here AND to `register_event_subscribers`.
#[test]
fn register_event_subscribers_lists_every_pattern() {
    let src = std::fs::read_to_string("src/daemon/mod.rs")
        .or_else(|_| std::fs::read_to_string("agend-terminal/src/daemon/mod.rs"))
        .expect("source file must be readable from test cwd");
    let start = src
        .find("pub(crate) fn register_event_subscribers")
        .expect("register_event_subscribers must exist");
    let body = &src[start..(start + 1200).min(src.len())];
    for pat in [
        "anti_stall::register_subscriber",
        "decision_timeout::register_subscriber",
        "dispatch_idle::register_subscriber",
        "waiting_on_stale::register_subscriber",
        "helper_staleness_watchdog::register_subscriber",
        "idle_watchdog::register_subscriber",
        "tasks::register_cascade_subscriber",
        "poll_reminder::register_subscriber",
        "cron_tick::register_subscriber",
        "supervisor::register_subscriber",
        "conflict_notify::register_subscriber",
        "ci_watch::register_subscriber",
    ] {
        assert!(
            body.contains(pat),
            "register_event_subscribers must register '{pat}' — a missing \
             pattern silently breaks its delivery in app + daemon mode (#1720)"
        );
    }
}

#[test]
fn run_dir_under_home() {
    let home = tmp_home("run_dir_home");
    let dir = run_dir(&home);
    assert!(dir.starts_with(&home));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn find_active_run_dir_no_run_dir() {
    let home = tmp_home("no_run");
    assert!(find_active_run_dir(&home).is_none());
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn find_active_run_dir_empty_run_dir() {
    let home = tmp_home("empty_run");
    std::fs::create_dir_all(home.join("run")).ok();
    assert!(find_active_run_dir(&home).is_none());
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn find_active_run_dir_stale_pid_cleaned() {
    let home = tmp_home("stale_pid");
    // Use PID 999999 which is very unlikely to be alive
    let stale = home.join("run").join("999999");
    std::fs::create_dir_all(&stale).ok();
    std::fs::write(stale.join(".daemon"), "999999:0").ok();
    assert!(find_active_run_dir(&home).is_none());
    // Stale dir should be cleaned up
    assert!(!stale.exists());
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn find_active_run_dir_current_pid() {
    let home = tmp_home("current_pid");
    let pid = std::process::id();
    let run = home.join("run").join(pid.to_string());
    std::fs::create_dir_all(&run).ok();
    write_daemon_id(&run);
    let found = find_active_run_dir(&home);
    assert!(found.is_some());
    assert_eq!(found.as_deref(), Some(run.as_path()));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn write_daemon_id_format() {
    let home = tmp_home("daemon_id");
    let run = home.join("run").join("test");
    std::fs::create_dir_all(&run).ok();
    write_daemon_id(&run);
    let content = std::fs::read_to_string(run.join(".daemon")).expect("read .daemon");
    let parts: Vec<&str> = content.split(':').collect();
    // CR-2026-06-14: format is now `{pid}:{boot_unix}:{start_token}`.
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[0], std::process::id().to_string());
    // Timestamp should be a positive number
    let ts: u64 = parts[1].parse().expect("parse timestamp");
    assert!(ts > 0);
    // Start-token parses; for our own (alive) PID it resolves non-zero.
    let token: u64 = parts[2].parse().expect("parse start_token");
    assert_eq!(
        token,
        crate::process::process_start_token(std::process::id()).unwrap_or(0),
        "recorded token must equal the live self start-token"
    );
    // The middle-field reader must still parse boot_unix (not "ts:token").
    assert_eq!(read_daemon_boot_unix(&run), Some(ts));
    // The new third-field reader returns the recorded token.
    assert_eq!(read_daemon_start_token(&run), Some(token));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn find_active_run_dir_pid_reuse_detected() {
    let home = tmp_home("pid_reuse");
    let pid = std::process::id();
    let run = home.join("run").join(pid.to_string());
    std::fs::create_dir_all(&run).ok();
    // Write a .daemon file with a DIFFERENT PID (simulates PID reuse)
    std::fs::write(run.join(".daemon"), "12345:0").ok();
    // Should detect PID reuse and clean up
    let found = find_active_run_dir(&home);
    assert!(found.is_none());
    assert!(!run.exists());
    std::fs::remove_dir_all(&home).ok();
}

/// #1814 FIX1 (reviewer race High): a run dir with an alive pid but NO
/// `.daemon` identity file (= a handoff successor that has published its api
/// pre-flock but NOT yet promoted) must NOT be discoverable via
/// `find_active_run_dir`. Otherwise generic CLI/MCP clients could route to a
/// half-promoted, no-agent successor during the overlap window (split-brain).
#[test]
fn find_active_run_dir_skips_dir_without_daemon_identity() {
    let home = tmp_home("no_daemon_identity");
    let pid = std::process::id(); // this test process — guaranteed alive
    let run = home.join("run").join(pid.to_string());
    std::fs::create_dir_all(&run).ok();
    // Successor pre-promote shape: api.port published, but NO `.daemon`.
    crate::ipc::write_port(&run, crate::ipc::API_NAME, 65000).ok();
    assert!(
        find_active_run_dir(&home).is_none(),
        "a run dir without a `.daemon` identity must not be discoverable (pre-promote successor)"
    );
    // The dir must survive (it's a live successor mid-handoff, not stale).
    assert!(
        run.exists(),
        "must NOT delete the un-promoted successor's run dir"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #1814 FIX1 companion: once the `.daemon` identity (matching pid) IS
/// present (= the successor promoted post-flock), the dir becomes
/// discoverable again — the normal-daemon path is unchanged.
#[test]
fn find_active_run_dir_returns_dir_with_valid_daemon_identity() {
    let home = tmp_home("valid_daemon_identity");
    let pid = std::process::id();
    let run = home.join("run").join(pid.to_string());
    std::fs::create_dir_all(&run).ok();
    write_daemon_id(&run); // writes `<pid>:<ts>` for the current (alive) pid
    assert_eq!(
        find_active_run_dir(&home).as_deref(),
        Some(run.as_path()),
        "a run dir with a valid matching `.daemon` must be discoverable"
    );
    std::fs::remove_dir_all(&home).ok();
}

// --- fresh_args ---

#[test]
fn codex_fresh_args_drops_resume() {
    let p = crate::backend::Backend::Codex.preset();
    let fresh = p.fresh_args.expect("codex has fresh_args");
    assert!(!fresh.contains(&"resume"));
    assert!(!fresh.contains(&"--last"));
    assert!(fresh.contains(&"--dangerously-bypass-approvals-and-sandbox"));
}

#[test]
fn claude_fresh_args_same_as_preset() {
    let p = crate::backend::Backend::ClaudeCode.preset();
    assert!(p.fresh_args.is_none());
}

#[test]
fn opencode_fresh_args_same_as_preset() {
    let p = crate::backend::Backend::OpenCode.preset();
    assert!(p.fresh_args.is_none());
}

// ── Clean exit vs crash respawn ──────────────────────────────────
// The earlier trio asserted only language / HashMap discriminant semantics
// (re-implementing the main loop's `match` inline) and never touched
// production, so a regression in the real handlers went uncaught. They
// collapse to two tests that drive the REAL handlers: a clean exit evicts
// the respawn config (so crash-respawn finds nothing to resurrect), and a
// crash's respawn DECISION says "respawn" on a fresh agent. `AgentHandle`
// needs a live PTY and can't be built in a unit test, so the
// registry-removal half of `handle_clean_exit` is reached only via the prod
// dispatch; the config eviction is the unit-testable contract.

#[test]
fn clean_exit_evicts_respawn_config_so_no_resurrect() {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    let home = std::env::temp_dir().join(format!(
        "agend-cleanexit-{}-{}",
        std::process::id(),
        C.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&home).unwrap();

    let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let configs: Arc<Mutex<HashMap<String, AgentConfig>>> = Arc::new(Mutex::new(HashMap::new()));
    configs.lock().insert(
        "agent-3".into(),
        AgentConfig {
            name: "agent-3".into(),
            backend: None,
            backend_command: "claude".into(),
            args: vec![],
            env: None,
            working_dir: None,
            submit_key: "\r".into(),
        },
    );
    assert!(configs.lock().contains_key("agent-3"));

    // Drive the REAL CleanExit handler (no fleet.yaml → resolve_uuid is None,
    // so the registry half is a no-op here; the config eviction must run).
    handle_clean_exit(&home, "agent-3", &registry, &configs);

    assert!(
        !configs.lock().contains_key("agent-3"),
        "clean exit must evict the respawn config so crash-respawn finds nothing to resurrect"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn crash_still_respawns() {
    // The Crash arm respawns iff the health gate says so. Drive the REAL
    // decision (`HealthTracker::record_crash`): a fresh agent's first crash
    // returns respawn=true — the opposite of a clean exit, which never
    // respawns. The max-retries → no-respawn case is covered by
    // `health::tests::test_failed_after_max_retries`.
    let mut health = crate::health::HealthTracker::new();
    let (respawn, _delay, _notify) = health.record_crash();
    assert!(
        respawn,
        "a fresh agent's first crash must respawn (the gate handle_crash_respawn relies on)"
    );
}

// These four drive the REAL production `crate::agent::classify_exit` (now
// pub(crate)) instead of re-implementing its `match` inline, so a refactor
// that reclassifies an exit code is actually caught.
use crate::agent::{classify_exit, ExitKind};

#[test]
fn sigint_130_treated_as_clean_exit() {
    // SIGINT (exit code 130 = 128+2) from /quit in some CLIs must be
    // treated as clean exit, not crash.
    assert_eq!(
        classify_exit(Some(130)),
        ExitKind::UserExit,
        "exit code 130 (SIGINT) must be a user-initiated clean exit"
    );
}

#[test]
fn sigkill_137_not_clean_exit() {
    // SIGKILL (137) is daemon-initiated, not user /exit.
    assert_eq!(
        classify_exit(Some(137)),
        ExitKind::SignalKill,
        "SIGKILL (137) must classify as SignalKill, not a clean UserExit"
    );
}

#[test]
fn sigterm_143_not_clean_exit() {
    // SIGTERM (143) is daemon-initiated, not user /exit.
    assert_eq!(
        classify_exit(Some(143)),
        ExitKind::SignalKill,
        "SIGTERM (143) must classify as SignalKill, not a clean UserExit"
    );
}

#[test]
fn nonzero_exit_is_crash() {
    // Exit code 1 (error) must trigger crash respawn.
    assert_eq!(
        classify_exit(Some(1)),
        ExitKind::Crash,
        "exit code 1 must classify as a crash"
    );
}

// ─────────────────────────────────────────────────────────────
// Sprint 57 Wave 3 PR-2 (#548 Q6) shutdown reason taxonomy +
// payload-shape pins.
// ─────────────────────────────────────────────────────────────

#[test]
fn shutdown_reason_round_trip_preserves_taxonomy() {
    for reason in [
        ShutdownReason::Unknown,
        ShutdownReason::Signal,
        ShutdownReason::ApiShutdown,
        ShutdownReason::Watchdog,
        ShutdownReason::CleanExit,
        // Sprint 60 W1 PR-3 + Sprint 63 W1 PR-3 additions.
        ShutdownReason::OperatorRestart,
        ShutdownReason::SignalSigint,
        ShutdownReason::SignalSigterm,
        ShutdownReason::SignalSighup,
    ] {
        let raw = reason as u8;
        let recovered = ShutdownReason::from_u8(raw);
        assert_eq!(recovered, reason, "round-trip lost taxonomy for {reason:?}");
    }
}

#[test]
fn shutdown_reason_per_signal_taxonomy_strings_pinned() {
    // Sprint 63 W1 PR-3 (Sprint 58 P2 #6): per-signal taxonomy
    // string identifiers are pinned for downstream `daemon_stop`
    // event consumers (greppers / parsers).
    assert_eq!(ShutdownReason::SignalSigint.as_str(), "signal_sigint");
    assert_eq!(ShutdownReason::SignalSigterm.as_str(), "signal_sigterm");
    assert_eq!(ShutdownReason::SignalSighup.as_str(), "signal_sighup");
    // Bundled `Signal` reason still pins to "signal" for backward compat.
    assert_eq!(ShutdownReason::Signal.as_str(), "signal");
}

#[test]
fn shutdown_reason_from_unknown_byte_returns_unknown() {
    // Forward-compat: any out-of-range value decodes to Unknown
    // rather than panicking. Future schema bumps that add more
    // reasons can land without breaking older readers.
    let recovered = ShutdownReason::from_u8(255);
    assert_eq!(recovered, ShutdownReason::Unknown);
    let recovered2 = ShutdownReason::from_u8(99);
    assert_eq!(recovered2, ShutdownReason::Unknown);
}

#[test]
fn shutdown_reason_as_str_matches_audit_taxonomy() {
    // Pin the string identifiers downstream consumers will grep
    // against. Renaming any of these is a downstream-breaking
    // change that needs an explicit migration note.
    assert_eq!(ShutdownReason::Unknown.as_str(), "unknown");
    assert_eq!(ShutdownReason::Signal.as_str(), "signal");
    assert_eq!(ShutdownReason::ApiShutdown.as_str(), "api_shutdown");
    assert_eq!(ShutdownReason::Watchdog.as_str(), "watchdog");
    assert_eq!(ShutdownReason::CleanExit.as_str(), "clean_exit");
}

#[test]
fn record_shutdown_reason_first_write_wins() {
    // Pin the compare_exchange semantic: the FIRST recorded
    // reason wins. A subsequent ctrlc handler trip during an
    // already-in-flight watchdog shutdown must NOT clobber the
    // watchdog's recorded reason.
    SHUTDOWN_REASON.store(0, Ordering::Relaxed); // reset to Unknown for test isolation
    record_shutdown_reason(ShutdownReason::Watchdog);
    record_shutdown_reason(ShutdownReason::Signal); // second write is no-op
    let recovered = ShutdownReason::from_u8(SHUTDOWN_REASON.load(Ordering::Relaxed));
    assert_eq!(
        recovered,
        ShutdownReason::Watchdog,
        "first-write-wins must preserve initial reason against re-entry"
    );
    // Reset for other tests.
    SHUTDOWN_REASON.store(0, Ordering::Relaxed);
}

#[test]
fn daemon_stop_event_payload_carries_reason_and_metrics() {
    // Pin the on-disk shape of the enriched `daemon_stop` event.
    // Build a synthetic ShutdownMetrics + format it the way
    // run_core does, parse the resulting key=value string, and
    // assert each field is present + correct.
    //
    // This is the regression-proof that downstream queries /
    // greps on `reason=...`, `agents_total=...`,
    // `agents_killed_after_grace=...`, `uptime_secs=...` keep
    // working across future Phase 2 IMPL refactors.
    let metrics = ShutdownMetrics {
        reason: ShutdownReason::Signal,
        agents_total: 3,
        agents_killed_after_grace: 1,
        uptime_secs: 123,
        transports_cleaned: 2,
        transports_failed: 0,
    };
    let detail = format!(
        "reason={} agents_total={} agents_killed_after_grace={} transports_cleaned={} transports_failed={} uptime_secs={}",
        metrics.reason.as_str(),
        metrics.agents_total,
        metrics.agents_killed_after_grace,
        metrics.transports_cleaned,
        metrics.transports_failed,
        metrics.uptime_secs
    );
    assert!(detail.contains("reason=signal"), "got: {detail}");
    assert!(detail.contains("agents_total=3"), "got: {detail}");
    assert!(
        detail.contains("agents_killed_after_grace=1"),
        "got: {detail}"
    );
    assert!(detail.contains("transports_cleaned=2"), "got: {detail}");
    assert!(detail.contains("transports_failed=0"), "got: {detail}");
    assert!(detail.contains("uptime_secs=123"), "got: {detail}");
}

#[test]
fn daemon_stop_event_name_unchanged_post_phase_2() {
    // Regression-proof against a future refactor that renames
    // `daemon_stop` to a parallel event name. Phase 1 RCA #554
    // Audit 6 explicitly chose enrich-not-duplicate; this test
    // pins the event-name decision in source text. If a future
    // refactor needs to rename, it must land a deliberate
    // operator-visible CHANGELOG migration note + delete this
    // pin in the same commit.
    //
    // We only check production code by slicing off the tests
    // submodule — including this very test file would self-
    // reference any literal we name in the negative-assertion
    // message.
    let src = include_str!("./mod.rs");
    let prod_end = src.find("\n#[cfg(test)]\nmod tests {").unwrap_or(src.len());
    let prod = &src[..prod_end];
    let count = prod.matches(r#""daemon_stop""#).count();
    assert!(
        count >= 1,
        "the `daemon_stop` event name MUST appear in daemon/mod.rs production \
         code — enrich-not-duplicate semantic per Phase 1 RCA #554 Audit 6"
    );
    // The parallel-event name must not appear ANYWHERE in the
    // production region. Construct the search string without
    // putting the literal into the assertion message so this
    // test's own source doesn't cross-pollute the slice.
    let parallel = [
        'd', 'a', 'e', 'm', 'o', 'n', '_', 's', 'h', 'u', 't', 'd', 'o', 'w', 'n',
    ]
    .iter()
    .collect::<String>();
    let bad_count = prod.matches(&parallel).count();
    assert_eq!(
        bad_count, 0,
        "Phase 1 RCA #554 Audit 6 chose enrich-not-duplicate; \
         a parallel event name appearing in production code would \
         break downstream query / grep paths"
    );
}

#[test]
fn boot_orphan_sweep_runs_once_after_initial_spawn_barrier() {
    let daemon_src = include_str!("./mod.rs");
    let prod_end = daemon_src.find("\nmod tests {").unwrap_or(daemon_src.len());
    let prod = &daemon_src[..prod_end];
    let sweep = "crate::tasks::release_inprogress_orphans_with_live(home, &live);";
    assert_eq!(
        prod.matches(sweep).count(),
        1,
        "the destructive sweep must not also run in recover-as-primary"
    );
    let initial_spawn = prod
        .find("spawn_fleet_agents(home, &agents, &ctx);")
        .expect("initial spawn barrier");
    let sweep_pos = prod.find(sweep).expect("post-spawn orphan sweep");
    let signal_setup = prod
        .find("crate::bootstrap::signals::install(Arc::clone(&ctx.shutdown), shutdown_tx);")
        .expect("post-boot signal setup");
    assert!(
        initial_spawn < sweep_pos && sweep_pos < signal_setup,
        "the sweep must consume the initial full-spawn census before normal serving"
    );

    let bootstrap_src = include_str!("../bootstrap/mod.rs");
    let bootstrap_prod_end = bootstrap_src
        .find("\n#[cfg(test)]\nmod tests {")
        .unwrap_or(bootstrap_src.len());
    assert!(
        !bootstrap_src[..bootstrap_prod_end].contains("release_inprogress_orphans_with_live"),
        "pre-registry bootstrap must never run the destructive task sweep"
    );
}

#[test]
fn shutdown_grace_window_is_2_seconds() {
    // Phase A RCA recommendation: 2s grace window. Long enough
    // for well-behaved agents to honor SIGTERM, short enough to
    // keep total daemon shutdown latency bounded. Pinned so a
    // future refactor doesn't silently drop or stretch the
    // grace window without a CHANGELOG note.
    assert_eq!(
        SHUTDOWN_GRACE,
        std::time::Duration::from_secs(2),
        "Wave 3 PR-2 contract: grace = 2s exactly"
    );
}

/// #t-41673 gap-instrument: the shutdown-complete log MUST carry the new
/// `shutdown_elapsed_ms` field so the shutdown half of the restart freeze is
/// attributable separately from the old-exit→new-launch gap. Pure-tracing
/// instrument — this asserts the field is actually emitted (drop the field →
/// RED). `shutdown_sequence` treats `home` as reserved telemetry (`let _ =
/// home`), so a throwaway path suffices; the empty registry means no agents
/// are spawned/killed (still pays the 2s grace window).
#[cfg(unix)]
#[test]
#[tracing_test::traced_test]
fn shutdown_sequence_emits_shutdown_elapsed_ms() {
    let (registry, _configs, _tx, _rx, _shutdown) = make_test_registry();
    let _ = shutdown_sequence(
        std::path::Path::new("/tmp"),
        &registry,
        std::time::Instant::now(),
    );
    assert!(
        logs_contain("daemon shutdown sequence complete"),
        "gap-instrument: shutdown-complete log should still fire"
    );
    assert!(
        logs_contain("shutdown_elapsed_ms"),
        "gap-instrument: shutdown-complete log must carry shutdown_elapsed_ms"
    );
}

// --- #896 Option D anchor (C0 RED) ---
//
// Locks the boot-time invariant Option D establishes:
// `spawn_and_register_agent` MUST publish the agent's `.port`
// synchronously before returning Ok. Pre-fix the TUI thread does
// the bind+write_port asynchronously after the function returns,
// so app probes between agent spawns see an empty / partial
// `*.port` set (issue #896, race-class regression widened by
// PR #906 daemon api::serve reorder).
//
// The "no sleep, no retry" wording is the contract — the assertion
// is the postcondition at return. Race timing is reviewer-confirmed
// via §3.20 SOP 3 (RED→GREEN protocol on three runs).

#[cfg(unix)]
fn make_shell_agent_def(name: &str) -> crate::bootstrap::AgentDef {
    (
        name.into(),
        "/bin/sh".into(),
        vec!["-c".into(), "sleep 60".into()],
        None,
        None,
        "\r".into(),
        None,
    )
}

#[cfg(unix)]
fn setup_run_dir_with_cookie(home: &Path) -> PathBuf {
    let run = home.join("run").join(std::process::id().to_string());
    std::fs::create_dir_all(&run).expect("create run_dir");
    crate::auth_cookie::issue(&run).expect("issue api.cookie");
    run
}

#[cfg(unix)]
#[allow(clippy::type_complexity)] // test scaffolding tuple; struct would be over-engineering
fn make_test_registry() -> (
    AgentRegistry,
    Arc<Mutex<HashMap<String, AgentConfig>>>,
    crossbeam_channel::Sender<crate::agent::AgentExitEvent>,
    crossbeam_channel::Receiver<crate::agent::AgentExitEvent>,
    Arc<AtomicBool>,
) {
    let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let configs: Arc<Mutex<HashMap<String, AgentConfig>>> = Arc::new(Mutex::new(HashMap::new()));
    let (crash_tx, crash_rx) = crossbeam_channel::unbounded();
    let shutdown = Arc::new(AtomicBool::new(false));
    (registry, configs, crash_tx, crash_rx, shutdown)
}

/// #1441: managed spawns fail-fast unless the instance is in fleet.yaml;
/// seed authoritative ids for the named agents under `home`.
#[cfg(unix)]
fn seed_fleet_ids(home: &std::path::Path, names: &[&str]) {
    let mut yaml = String::from("instances:\n");
    for (i, n) in names.iter().enumerate() {
        yaml.push_str(&format!(
            "  {n}:\n    id: 0d0d0d0d-0000-4000-8000-{:012x}\n",
            i + 1
        ));
    }
    std::fs::write(crate::fleet::fleet_yaml_path(home), yaml).expect("seed fleet.yaml");
}

#[cfg(unix)]
fn kill_registered_child(registry: &AgentRegistry, name: &str) {
    let reg = registry.lock();
    if let Some(handle) = reg.values().find(|h| h.name.as_str() == name) {
        let _ = handle.child.lock().kill();
    }
}

#[cfg(unix)]
#[test]
fn spawn_and_register_agent_publishes_port_synchronously() {
    let home = tmp_home("publish_sync");
    let run_dir = setup_run_dir_with_cookie(&home);
    seed_fleet_ids(&home, &["probe-1"]);
    let (registry, configs, crash_tx, _crash_rx, shutdown) = make_test_registry();
    let def = make_shell_agent_def("probe-1");

    spawn_and_register_agent(&home, &def, &registry, &configs, &crash_tx, &shutdown)
        .expect("spawn ok");

    // CONTRACT: the agent's .port is on disk BEFORE this assertion line.
    // Pre-fix: TUI thread is async, .port may not be written yet (race).
    // Post-fix: prepare_tui_listener_and_publish_port ran synchronously
    // inside spawn_and_register_agent.
    assert!(
        crate::ipc::read_port(&run_dir, "probe-1").is_some(),
        "spawn_and_register_agent must publish .port synchronously before return"
    );

    kill_registered_child(&registry, "probe-1");
    std::fs::remove_dir_all(&home).ok();
}

/// restart-freeze 真嫌#1 (t-…55279): the shared parallel-teardown core must
/// kill and reap N agents in about one grace window, not N times a per-agent
/// sequential wait (the ~6 s app-restart freeze). Spawn several real
/// SIGTERM-responsive shells, drain to `(name, child)` exactly as
/// `shutdown_sequence` and `app_teardown` do, then assert bounded wall time
/// (a per-agent wait would scale with N), zero post-grace SIGKILLs (shells
/// honour SIGTERM), and every child reaped (no zombies).
#[cfg(unix)]
#[test]
fn terminate_agents_parallel_bounded_and_reaps_all() {
    let home = tmp_home("terminate_parallel");
    let _run_dir = setup_run_dir_with_cookie(&home);
    let names = ["t1", "t2", "t3", "t4", "t5", "t6"];
    seed_fleet_ids(&home, &names);
    let (registry, configs, crash_tx, _crash_rx, shutdown) = make_test_registry();
    for n in &names {
        spawn_and_register_agent(
            &home,
            &make_shell_agent_def(n),
            &registry,
            &configs,
            &crash_tx,
            &shutdown,
        )
        .expect("spawn ok");
    }

    // Drain to (name, child) — identical to shutdown_sequence / app_teardown.
    let agents: Vec<(String, ChildHandle)> = {
        let mut reg = registry.lock();
        reg.drain()
            .map(|(_id, h)| (h.name.to_string(), h.child))
            .collect()
    };
    assert_eq!(agents.len(), names.len());
    let children: Vec<ChildHandle> = agents.iter().map(|(_, c)| Arc::clone(c)).collect();

    let start = std::time::Instant::now();
    let killed = terminate_agents_parallel(agents);
    let elapsed = start.elapsed();

    // Bound: one grace window + CI margin. Grace-exiters are reaped in place
    // by `try_wait`, so the reap loop is ~instant; a regression to the
    // is_pid_alive gate (zombie reads alive → kill_process_tree's 500 ms per
    // agent, serialized) would add ~N×0.5 s ≈ 3 s for these 6 agents and
    // blow this bound.
    assert!(
        elapsed < SHUTDOWN_GRACE + std::time::Duration::from_secs(2),
        "parallel teardown of {} agents took {elapsed:?}; expected ≈ one grace window \
         (regression: per-agent kill_process_tree 500ms scaling with N)",
        names.len()
    );
    // `sleep 60` honours SIGTERM (default-terminate) → all exit within grace.
    assert_eq!(
        killed, 0,
        "SIGTERM-responsive shells should exit during grace"
    );
    for c in &children {
        assert!(
            matches!(c.lock().try_wait(), Ok(Some(_))),
            "every child must be reaped after terminate_agents_parallel"
        );
    }

    std::fs::remove_dir_all(&home).ok();
}

/// #1915 boot-path chokepoint: `spawn_and_register_agent` must SKIP an
/// instance that is mid-delete (the boot-stagger resurrection: the loop holds
/// a fleet snapshot, an instance deleted during the stagger must not be
/// re-spawned + have its `workspace/<name>` re-created by skills-install).
/// Also proves the deleting-set does NOT leak the name: after the delete
/// completes (guard drop), a re-create of the SAME name spawns normally.
#[cfg(unix)]
#[test]
fn spawn_and_register_agent_skips_mid_delete_1915() {
    let home = tmp_home("mid_delete");
    let run_dir = setup_run_dir_with_cookie(&home);
    seed_fleet_ids(&home, &["victim"]);
    let (registry, configs, crash_tx, _crash_rx, shutdown) = make_test_registry();
    let def = make_shell_agent_def("victim");

    // Mark victim mid-delete (as full_delete_instance's guard would).
    let guard = crate::agent::deleting::mark_deleting(&home, "victim");

    spawn_and_register_agent(&home, &def, &registry, &configs, &crash_tx, &shutdown)
        .expect("returns Ok (clean skip, not Err)");
    assert!(
        crate::ipc::read_port(&run_dir, "victim").is_none(),
        "#1915: a mid-delete instance must NOT be spawned — no port published"
    );
    assert!(
        registry.lock().is_empty(),
        "#1915: a mid-delete instance must NOT be registered — no resurrection"
    );

    // Delete completes → guard drops → name un-marked → re-create succeeds
    // (deleting-set must not leave the name permanently un-spawnable).
    drop(guard);
    spawn_and_register_agent(&home, &def, &registry, &configs, &crash_tx, &shutdown)
        .expect("re-create after delete spawns");
    assert!(
        crate::ipc::read_port(&run_dir, "victim").is_some(),
        "#1915 no-leak: same name re-creatable once the delete (guard) is done"
    );

    kill_registered_child(&registry, "victim");
    std::fs::remove_dir_all(&home).ok();
}

#[cfg(unix)]
#[test]
fn spawn_and_register_agent_rollback_on_listener_prep_failure() {
    // Force prepare-listener failure by NOT issuing api.cookie in
    // run_dir. `prepare_tui_listener_and_publish_port` reads the
    // cookie first (so it can hand it to the accept loop); a missing
    // cookie file is an Err on the synchronous prep path.
    let home = tmp_home("rollback_prep");
    let run = home.join("run").join(std::process::id().to_string());
    std::fs::create_dir_all(&run).expect("create run_dir");
    // Deliberately skip `auth_cookie::issue` — prep should fail at
    // cookie read.
    let (registry, configs, crash_tx, _crash_rx, shutdown) = make_test_registry();
    let def = make_shell_agent_def("rollback-probe");

    let result = spawn_and_register_agent(&home, &def, &registry, &configs, &crash_tx, &shutdown);

    // CONTRACT (Option D rollback):
    // 1. spawn_and_register_agent returns Err — caller can decide whether
    //    to continue or abort.
    assert!(
        result.is_err(),
        "spawn_and_register_agent must return Err when TUI listener prep fails (got Ok)"
    );
    // 2. Registry MUST NOT contain the agent — caller sees a clean
    //    rollback state, no zombie entries.
    assert!(
        !registry
            .lock()
            .values()
            .any(|h| h.name.as_str() == "rollback-probe"),
        "registry must NOT contain 'rollback-probe' after rollback"
    );
    // 3. AgentConfig MUST NOT contain the agent — configs map mirrors
    //    registry membership.
    assert!(
        configs.lock().get("rollback-probe").is_none(),
        "configs must NOT contain 'rollback-probe' after rollback"
    );
    // 4. .port file MUST NOT be on disk — prep failed before write_port
    //    or the rollback removed it.
    assert!(
        crate::ipc::read_port(&run, "rollback-probe").is_none(),
        "rollback must leave no .port residue"
    );

    std::fs::remove_dir_all(&home).ok();
}

#[cfg(unix)]
#[test]
fn app_attach_during_stagger_window_sees_all_agents() {
    // Behavioral RED: simulates the operator's smoke — multiple agents
    // spawned sequentially with stagger between them. Pre-fix, an "app
    // attach" simulated by `ipc::list_agent_ports` mid-loop sees fewer
    // entries than the loop has produced (TUI threads race). Post-fix,
    // every iteration's port is on disk by the time the next iteration
    // begins, so list_agent_ports == iteration_count holds at each step.
    //
    // #910 PR4 note: post-#910 the app's canonical discovery path is
    // `runtime::list_agents_with_fallback`, NOT bare
    // `ipc::list_agent_ports`. This test still uses the bare fn
    // intentionally — it locks the FILESYSTEM contract (the .port
    // file is present synchronously after spawn returns), which is
    // the worst-case fallback path the helper would expose when the
    // daemon API is briefly unresponsive. Testing the bare fn here
    // covers the helper's degraded mode by construction.
    let home = tmp_home("attach_during_stagger");
    let run_dir = setup_run_dir_with_cookie(&home);
    let agent_names = ["a-1", "a-2", "a-3", "a-4"];
    seed_fleet_ids(&home, &agent_names);
    let (registry, configs, crash_tx, _crash_rx, shutdown) = make_test_registry();
    for (i, name) in agent_names.iter().enumerate() {
        let def = make_shell_agent_def(name);
        spawn_and_register_agent(&home, &def, &registry, &configs, &crash_tx, &shutdown)
            .expect("spawn ok");
        // CONTRACT: every agent spawned so far has its .port on disk.
        // Probe is what an `app` reattach would do.
        let visible = crate::ipc::list_agent_ports(&run_dir);
        for prior in &agent_names[..=i] {
            assert!(
                visible.contains(&prior.to_string()),
                "after spawning {name} (iteration {i}), agent {prior} must have .port on \
                 disk; got {visible:?}"
            );
        }
    }

    for name in &agent_names {
        kill_registered_child(&registry, name);
    }
    std::fs::remove_dir_all(&home).ok();
}

// ── #2935: crash wake must not trigger maintenance pipeline ──────────

fn maintenance_test_cycle() -> (
    crate::daemon::owned_maintenance::OwnedMaintenanceCycle,
    Arc<std::sync::atomic::AtomicUsize>,
) {
    use crate::daemon::owner_services::{
        start_owner_monitoring, start_owner_stream_observers, OwnerMonitoringStarters, OwnerRole,
        OwnerStreamStarters,
    };
    use crate::daemon::per_tick::{PerTickHandler, TickContext};

    struct Counter(Arc<std::sync::atomic::AtomicUsize>);
    impl PerTickHandler for Counter {
        fn name(&self) -> &'static str {
            "counter-2935"
        }
        fn run(&self, _ctx: &TickContext<'_>) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let monitoring = OwnerMonitoringStarters {
        monitor_tick: &|_, _, _| {},
        api_activity_probe: &|_, _| {},
    };
    let streams = OwnerStreamStarters {
        rollout: &|_, _, _| {},
        opencode: &|_, _, _| {},
        kiro: &|_, _, _| {},
        grok: &|_, _, _| {},
    };
    let phase_one = start_owner_monitoring(
        OwnerRole::Owned,
        Path::new("/tmp/agend-2935-red"),
        &registry,
        &monitoring,
    );
    let owner = start_owner_stream_observers(
        OwnerRole::Owned,
        &phase_one,
        Path::new("/tmp/agend-2935-red"),
        &registry,
        &streams,
    );
    let cycle = crate::daemon::owned_maintenance::OwnedMaintenanceCycle::from_parts(
        vec![Box::new(Counter(Arc::clone(&runs)))],
        owner,
        None,
        None,
    );
    (cycle, runs)
}

/// #2935 RED: a crash/exit event must NOT trigger the periodic maintenance
/// pipeline. Before the fix, `serve_loop_post_select` runs `run_once`
/// unconditionally for both tick and crash wakes.
#[test]
fn crash_wake_must_skip_maintenance_2935() {
    let (cycle, runs) = maintenance_test_cycle();
    let home = std::env::temp_dir().join(format!("agend-2935-crash-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let externals: crate::agent::ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
    let configs: crate::api::ConfigRegistry = Arc::new(Mutex::new(HashMap::new()));

    let crash = Some(crate::agent::AgentExitEvent::CleanExit("test-agent".into()));
    let result =
        super::serve_loop_post_select(&cycle, crash, &home, &registry, &externals, &configs);

    assert!(result.is_some(), "crash event must pass through");
    assert_eq!(
        runs.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "#2935 RED: crash wake must NOT trigger maintenance pipeline"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #2935 control: a tick wake (exit_event=None) MUST trigger the maintenance
/// pipeline — this is the desired behavior that must be preserved.
#[test]
fn tick_wake_runs_maintenance_2935() {
    let (cycle, runs) = maintenance_test_cycle();
    let home = std::env::temp_dir().join(format!("agend-2935-tick-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let externals: crate::agent::ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
    let configs: crate::api::ConfigRegistry = Arc::new(Mutex::new(HashMap::new()));

    let result =
        super::serve_loop_post_select(&cycle, None, &home, &registry, &externals, &configs);

    assert!(result.is_none(), "tick returns None");
    assert_eq!(
        runs.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "#2935 control: tick wake MUST trigger maintenance pipeline"
    );
    std::fs::remove_dir_all(&home).ok();
}
