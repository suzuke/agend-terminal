//! #3539: `agend-terminal stop` — synchronous by default.
//!
//! The daemon's SHUTDOWN API sets a flag and answers `ok:true` BEFORE its
//! shutdown sequence runs (agent SIGTERM → grace → SIGKILL, managed-transport
//! teardown, run-dir removal). `stop` used to print "initiated" on that
//! receipt and return, so `stop && start` raced the still-exiting daemon and
//! an operator could not tell "accepted" from "gone". Now the CLI records the
//! daemon's identity (pid + start token from `run/<pid>/.daemon`) before
//! sending the request, polls liveness with a hard bound, and says which of
//! the two happened. `--no-wait` keeps the receipt-only behaviour.
//!
//! What is left after the daemon has exited is REPORTED, never acted on. The
//! residual section is the #3273 V1 report (`admin::orphan_provenance`)
//! narrowed to rows carrying a display-only [`ResidualHint`]: a cargo test
//! binary of this crate, a debug-profile daemon binary, a cwd under
//! `<home>/worktrees/`. A test runner's child is not daemon-owned — its parent
//! was a `cargo nextest` in some agent's shell — so the shutdown sequence has
//! nothing to reclaim, and consensus `d-20260814214253501210-22` grants no
//! signal authority to argv/cwd/PPID. Disposition goes through
//! `doctor orphans preview|apply`, which this module never names.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::admin::orphan_provenance::{
    self, OrphanCandidate, OrphanReport, ProvenanceSupport, ResidualHint,
};

pub struct StopOptions {
    /// Poll for the daemon process to exit before returning.
    pub wait: bool,
    /// Hard bound on that wait.
    pub timeout: Duration,
}

#[derive(Debug, PartialEq, Eq)]
pub enum StopOutcome {
    /// No active run dir was discoverable.
    NotRunning,
    /// Request accepted; `--no-wait` so nothing further was observed.
    Initiated,
    /// Request accepted and the daemon process is gone.
    Exited,
    /// Request accepted but the daemon was still alive at the bound.
    TimedOut,
    /// Request accepted; at the bound the pid was still alive but its start
    /// token could not be read, so "same daemon or a recycled pid" is unknown.
    /// Never reported as an exit (R1 N1): a false "exited" would let
    /// `stop && start` run against a live daemon.
    Unconfirmed,
}

/// What one liveness probe of the recorded daemon pid established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Liveness {
    /// The pid is gone, or is alive under a DIFFERENT start token (recycled pid).
    Exited,
    /// The pid is alive under the recorded token (or no token was recorded, so
    /// pid liveness is all there is to go on — the legacy `.daemon` case).
    Alive,
    /// The pid is alive but its start token cannot be read right now
    /// (`process_start_token` fails for a live process on macOS when
    /// `proc_pidinfo` returns a short struct, on Linux under `hidepid`, on
    /// Windows without the query right). Not evidence of an exit.
    Unknown,
}

/// "Is the recorded daemon still there?" with the platform probes injected so
/// the token comparison is testable (R1 N1: the previous inline closure was
/// not, and dropping the comparison left every test green).
fn daemon_liveness(
    pid: u32,
    recorded_token: Option<u64>,
    alive: impl Fn(u32) -> bool,
    read_token: impl Fn(u32) -> Option<u64>,
) -> Liveness {
    if !alive(pid) {
        return Liveness::Exited;
    }
    let Some(recorded) = recorded_token else {
        return Liveness::Alive;
    };
    match read_token(pid) {
        Some(current) if current == recorded => Liveness::Alive,
        Some(_) => Liveness::Exited,
        None => Liveness::Unknown,
    }
}

/// Result of polling [`daemon_liveness`] up to a bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitOutcome {
    Exited(Duration),
    /// The bound elapsed; `last` is the final probe (never `Exited`).
    TimedOut {
        last: Liveness,
    },
}

const POLL: Duration = Duration::from_millis(100);

pub fn run_stop(home: &Path, opts: StopOptions) -> anyhow::Result<StopOutcome> {
    let Some(run_dir) = crate::daemon::find_active_run_dir(home) else {
        return Ok(StopOutcome::NotRunning);
    };
    // Identity is read BEFORE the request: once the daemon exits its run dir
    // is gone, and a recycled pid must not read as "still alive".
    let (pid, token) = daemon_identity(&run_dir);

    let resp = match crate::api::call(
        home,
        &serde_json::json!({"method": crate::api::method::SHUTDOWN}),
    ) {
        Ok(resp) => resp,
        Err(error) => anyhow::bail!("Unable to contact daemon for shutdown request: {error}"),
    };
    if resp["ok"].as_bool() != Some(true) {
        anyhow::bail!("Shutdown request failed.");
    }
    match pid {
        Some(pid) => println!("Daemon shutdown initiated (pid {pid})."),
        None => println!("Daemon shutdown initiated."),
    }
    if !opts.wait {
        return Ok(StopOutcome::Initiated);
    }
    let Some(pid) = pid else {
        // Cannot poll an unknown pid; say so instead of pretending to wait.
        println!(
            "Cannot confirm exit: {} has no readable pid.",
            run_dir.join(".daemon").display()
        );
        return Ok(StopOutcome::Initiated);
    };

    let probe = || {
        daemon_liveness(
            pid,
            token,
            crate::process::is_pid_alive,
            crate::process::process_start_token,
        )
    };
    match wait_for_exit(probe, opts.timeout, POLL) {
        WaitOutcome::Exited(elapsed) => {
            println!(
                "Daemon (pid {pid}) exited after {} ms; run dir {}.",
                elapsed.as_millis(),
                if run_dir.exists() {
                    "still present"
                } else {
                    "removed"
                }
            );
            report_residuals(home);
            Ok(StopOutcome::Exited)
        }
        WaitOutcome::TimedOut {
            last: Liveness::Unknown,
        } => {
            println!(
                "Cannot confirm exit: pid {pid} is still alive after {} s but its start token \
                 could not be read, so it may be the daemon or a recycled pid. \
                 Inspect `agend-terminal doctor` and {}.",
                opts.timeout.as_secs(),
                home.join("daemon.log").display()
            );
            Ok(StopOutcome::Unconfirmed)
        }
        WaitOutcome::TimedOut { .. } => {
            println!(
                "Daemon (pid {pid}) is still running after {} s; shutdown is not complete. \
                 Inspect `agend-terminal doctor` and {}.",
                opts.timeout.as_secs(),
                home.join("daemon.log").display()
            );
            Ok(StopOutcome::TimedOut)
        }
    }
}

/// `run/<pid>/.daemon` is `pid:boot_unix:start_token` (token optional on
/// legacy files). Falls back to the directory name for the pid.
fn daemon_identity(run_dir: &Path) -> (Option<u32>, Option<u64>) {
    let content = std::fs::read_to_string(run_dir.join(".daemon")).unwrap_or_default();
    let mut fields = content.trim().split(':');
    let pid = fields
        .next()
        .and_then(|p| p.parse::<u32>().ok())
        .or_else(|| run_dir.file_name()?.to_str()?.parse().ok());
    let token = fields.nth(1).and_then(|t| t.parse::<u64>().ok());
    (pid, token)
}

/// Poll `probe` until it reports [`Liveness::Exited`] or `timeout` elapses.
/// Checked once before any sleep so a daemon that is already gone costs no
/// wait. An `Unknown` probe keeps polling (a token read can fail transiently
/// while the process is being reaped) and, if it is still `Unknown` at the
/// bound, is reported as such — never as an exit.
fn wait_for_exit(
    mut probe: impl FnMut() -> Liveness,
    timeout: Duration,
    poll: Duration,
) -> WaitOutcome {
    let started = Instant::now();
    loop {
        let last = probe();
        if last == Liveness::Exited {
            return WaitOutcome::Exited(started.elapsed());
        }
        if started.elapsed() >= timeout {
            return WaitOutcome::TimedOut { last };
        }
        std::thread::sleep(poll);
    }
}

/// The #3273 V1 report, narrowed to hinted rows. Report-only by construction:
/// same oracle and classifier as bare `doctor`, nothing else.
fn report_residuals(home: &Path) {
    let oracle = crate::daemon::per_tick::tool_call_provenance::PlatformOracle::snapshot();
    let mut report = orphan_provenance::classify(home, &oracle, now_epoch_ms());
    if let ProvenanceSupport::Unsupported { platform, reason } = &report.support {
        println!("Residual scan: not available on {platform} ({reason}).");
        return;
    }
    let pids: Vec<u32> = report.candidates.iter().map(|c| c.pid).collect();
    let cwds = crate::daemon::per_tick::tool_call_provenance::resolve_cwds(&pids);
    for c in &mut report.candidates {
        c.cwd = cwds.get(&c.pid).cloned();
    }
    orphan_provenance::annotate_residual_hints(home, &mut report);

    print!("{}", render_residual_report(&report));
}

/// Render the stop command's narrowed residual view. This is deliberately
/// separate from the process snapshot so the report's classification and
/// formatting can be tested with synthetic candidates without creating or
/// signalling any process.
fn render_residual_report(report: &OrphanReport) -> String {
    if let ProvenanceSupport::Unsupported { platform, reason } = &report.support {
        return format!("Residual scan: not available on {platform} ({reason}).\n");
    }

    let hinted: Vec<&OrphanCandidate> = report
        .candidates
        .iter()
        .filter(|c| c.hint.is_some())
        .collect();
    if hinted.is_empty() {
        return "Residual scan: no reparented process in scope looks agend-related \
             (scoped snapshot, not a global clean result; `agend-terminal doctor` shows the full scope).\n"
            .to_string();
    }
    let mut out = format!(
        "Residual scan: {} reparented process(es) look agend-related. UNPROVEN — the daemon \
         did not own them and takes no action:\n",
        hinted.len()
    );
    for c in hinted {
        out.push_str(&format!(
            "  pid={} hint={} elapsed_secs={} argv={} cwd={}\n",
            c.pid,
            c.hint.map(ResidualHint::label).unwrap_or("none"),
            c.elapsed_secs
                .map(|v| v.to_string())
                .unwrap_or_else(|| "unknown".into()),
            c.argv.as_deref().unwrap_or("unknown"),
            c.cwd.as_deref().unwrap_or("unknown"),
        ));
    }
    out.push_str(
        "  Disposition is an operator decision: `agend-terminal doctor` lists every candidate \
         and `doctor orphans preview` starts a manual, confirmed cleanup.\n",
    );
    out
}

fn now_epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::orphan_provenance::UnprovenReason;

    #[test]
    fn wait_for_exit_returns_as_soon_as_the_process_is_gone_3539() {
        let mut polls = 0;
        let outcome = wait_for_exit(
            || {
                polls += 1;
                if polls < 3 {
                    Liveness::Alive
                } else {
                    Liveness::Exited
                }
            },
            Duration::from_secs(5),
            Duration::from_millis(1),
        );
        assert!(matches!(outcome, WaitOutcome::Exited(_)), "{outcome:?}");
        assert_eq!(polls, 3, "stops polling once the probe says Exited");
    }

    #[test]
    fn wait_for_exit_never_outstays_its_bound_3539() {
        let started = Instant::now();
        let outcome = wait_for_exit(
            || Liveness::Alive,
            Duration::from_millis(30),
            Duration::from_millis(1),
        );
        assert_eq!(
            outcome,
            WaitOutcome::TimedOut {
                last: Liveness::Alive
            },
            "a process that never exits hits the bound"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the bound is honoured"
        );
    }

    #[test]
    fn wait_for_exit_does_not_sleep_when_already_gone_3539() {
        let started = Instant::now();
        let outcome = wait_for_exit(
            || Liveness::Exited,
            Duration::from_secs(5),
            Duration::from_secs(5),
        );
        assert!(matches!(outcome, WaitOutcome::Exited(_)));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "no poll interval before the first check"
        );
    }

    /// R1 N1, test 1: the recorded token no longer matches the live pid — a
    /// recycled pid — and that IS an exit.
    #[test]
    fn a_changed_start_token_on_a_live_pid_is_an_exit_3539() {
        let alive = |_pid: u32| true;
        assert_eq!(
            daemon_liveness(4242, Some(99), alive, |_| Some(100)),
            Liveness::Exited
        );
        assert_eq!(
            daemon_liveness(4242, Some(99), alive, |_| Some(99)),
            Liveness::Alive,
            "same token, same daemon"
        );
        assert_eq!(
            daemon_liveness(4242, None, alive, |_| None),
            Liveness::Alive,
            "legacy .daemon without a token: pid liveness is all there is"
        );
        assert_eq!(
            daemon_liveness(4242, Some(99), |_| false, |_| Some(99)),
            Liveness::Exited,
            "a dead pid is an exit regardless of any token"
        );
    }

    /// R1 N1, test 2: alive but the token cannot be read — NOT an exit. The
    /// waiter must carry that through to the bound instead of reporting a
    /// clean stop, otherwise `stop && start` proceeds against a live daemon.
    #[test]
    fn an_unreadable_start_token_on_a_live_pid_is_never_an_exit_3539() {
        let liveness = daemon_liveness(4242, Some(99), |_| true, |_| None);
        assert_ne!(liveness, Liveness::Exited);
        assert_eq!(liveness, Liveness::Unknown);

        let outcome = wait_for_exit(
            || Liveness::Unknown,
            Duration::from_millis(20),
            Duration::from_millis(1),
        );
        assert_eq!(
            outcome,
            WaitOutcome::TimedOut {
                last: Liveness::Unknown
            },
            "an Unknown probe reaches the bound as Unknown, never as Exited"
        );
    }

    #[test]
    fn daemon_identity_parses_current_and_legacy_files_3539() {
        let dir = std::env::temp_dir().join(format!("agend-stop-identity-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let run = dir.join("4242");
        std::fs::create_dir_all(&run).expect("mkdir");

        std::fs::write(run.join(".daemon"), "4242:1700000000:99").expect("write");
        assert_eq!(daemon_identity(&run), (Some(4242), Some(99)));

        std::fs::write(run.join(".daemon"), "4242:1700000000").expect("write");
        assert_eq!(
            daemon_identity(&run),
            (Some(4242), None),
            "legacy file has no token"
        );

        std::fs::remove_file(run.join(".daemon")).expect("rm");
        assert_eq!(
            daemon_identity(&run),
            (Some(4242), None),
            "pid falls back to the dir name"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn residual_report_formats_hinted_candidates_without_signalling_3539() {
        let home = Path::new("/tmp/agend-stop-report-home");
        let hinted_argv = home
            .join("target/debug/deps/agend_terminal-deadbeef3539")
            .display()
            .to_string();
        let hinted = orphan_provenance::residual_hint(home, Some(&hinted_argv), None);
        let control = orphan_provenance::residual_hint(
            home,
            Some("/tmp/control/plain-sleeper"),
            Some("/tmp/control"),
        );
        assert_eq!(hinted, Some(ResidualHint::TestRunnerBinary));
        assert_eq!(control, None);

        let candidate = |pid: u32, hint: Option<ResidualHint>, argv: &str| OrphanCandidate {
            pid,
            start_token: Some(17),
            lstart_ms: Some(42),
            sid: Some(9),
            pgid: Some(8),
            leader_alive: Some(false),
            argv: Some(argv.to_string()),
            cwd: None,
            elapsed_secs: Some(3),
            cpu_percent: Some(0.0),
            suggested_instance: None,
            unproven_reason: UnprovenReason::NoObservation,
            hint,
        };
        let report = OrphanReport {
            support: ProvenanceSupport::Supported,
            candidates: vec![
                candidate(101, hinted, &hinted_argv),
                candidate(202, control, "/tmp/control/plain-sleeper"),
            ],
            scope: orphan_provenance::ScopeCounts::default(),
        };

        let output = render_residual_report(&report);
        assert!(output.contains("Residual scan: 1 reparented process(es)"));
        assert!(output.contains("pid=101 hint=test-runner-binary"));
        assert!(!output.contains("pid=202 "));
        assert!(output.contains("did not own them and takes no action"));
        assert!(!output.contains("SIGTERM") && !output.contains("SIGKILL"));
    }
}
