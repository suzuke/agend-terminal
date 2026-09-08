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

use crate::admin::orphan_provenance::{self, ProvenanceSupport, ResidualHint};

pub struct StopOptions {
    /// Poll for the daemon process to exit before returning.
    pub wait: bool,
    /// Hard bound on that wait.
    pub timeout: Duration,
}

#[derive(Debug, PartialEq, Eq)]
pub enum StopOutcome {
    /// No active run dir, or the API was unreachable.
    NotRunning,
    /// Request accepted; `--no-wait` so nothing further was observed.
    Initiated,
    /// Request accepted and the daemon process is gone.
    Exited,
    /// Request accepted but the daemon was still alive at the bound.
    TimedOut,
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
        Err(_) => return Ok(StopOutcome::NotRunning),
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

    let alive = || {
        crate::process::is_pid_alive(pid)
            && match token {
                // A different start token on the same pid is a different process.
                Some(recorded) => crate::process::process_start_token(pid) == Some(recorded),
                None => true,
            }
    };
    match wait_for_exit(alive, opts.timeout, POLL) {
        Some(elapsed) => {
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
        None => {
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

/// Poll `alive` until it returns false or `timeout` elapses. `Some(elapsed)`
/// when the process went away, `None` at the bound. Checked once before any
/// sleep so a daemon that is already gone costs no wait.
fn wait_for_exit(
    mut alive: impl FnMut() -> bool,
    timeout: Duration,
    poll: Duration,
) -> Option<Duration> {
    let started = Instant::now();
    loop {
        if !alive() {
            return Some(started.elapsed());
        }
        if started.elapsed() >= timeout {
            return None;
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

    let hinted: Vec<_> = report
        .candidates
        .iter()
        .filter(|c| c.hint.is_some())
        .collect();
    if hinted.is_empty() {
        println!(
            "Residual scan: no reparented process in scope looks agend-related \
             (scoped snapshot, not a global clean result; `agend-terminal doctor` shows the full scope)."
        );
        return;
    }
    println!(
        "Residual scan: {} reparented process(es) look agend-related. UNPROVEN — the daemon \
         did not own them and takes no action:",
        hinted.len()
    );
    for c in hinted {
        println!(
            "  pid={} hint={} elapsed_secs={} argv={} cwd={}",
            c.pid,
            c.hint.map(ResidualHint::label).unwrap_or("none"),
            c.elapsed_secs
                .map(|v| v.to_string())
                .unwrap_or_else(|| "unknown".into()),
            c.argv.as_deref().unwrap_or("unknown"),
            c.cwd.as_deref().unwrap_or("unknown"),
        );
    }
    println!(
        "  Disposition is an operator decision: `agend-terminal doctor` lists every candidate \
         and `doctor orphans preview` starts a manual, confirmed cleanup."
    );
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

    #[test]
    fn wait_for_exit_returns_as_soon_as_the_process_is_gone_3539() {
        let mut polls = 0;
        let elapsed = wait_for_exit(
            || {
                polls += 1;
                polls < 3
            },
            Duration::from_secs(5),
            Duration::from_millis(1),
        );
        assert!(elapsed.is_some(), "must report the exit");
        assert_eq!(polls, 3, "stops polling once alive() is false");
    }

    #[test]
    fn wait_for_exit_never_outstays_its_bound_3539() {
        let started = Instant::now();
        let elapsed = wait_for_exit(|| true, Duration::from_millis(30), Duration::from_millis(1));
        assert_eq!(elapsed, None, "a process that never exits hits the bound");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the bound is honoured"
        );
    }

    #[test]
    fn wait_for_exit_does_not_sleep_when_already_gone_3539() {
        let started = Instant::now();
        let elapsed = wait_for_exit(|| false, Duration::from_secs(5), Duration::from_secs(5));
        assert!(elapsed.is_some());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "no poll interval before the first check"
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
}
