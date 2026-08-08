//! App-owner re-exec requester handoff and restart lifecycle helpers.
//!
//! The successor receives one exact fleet `InstanceId` in argv. This module
//! deliberately performs no name fallback: a missing or changed fleet UUID
//! drops the wake rather than risking a different agent.

#[allow(clippy::wildcard_imports)]
use super::*;
use std::path::Path;
use std::time::Duration;

pub(crate) type RestartSelfKickTarget = (crate::types::InstanceId, String, Duration);

pub(crate) fn resolve_target(
    home: &Path,
    attached_mode: bool,
    requester_id: Option<crate::types::InstanceId>,
) -> Option<RestartSelfKickTarget> {
    if attached_mode {
        return None;
    }
    let requester_id = requester_id?;
    let name = crate::fleet::resolve_name_by_uuid(home, &requester_id.full())?;
    let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)).ok()?;
    let resolved = fleet.resolve_instance(&name)?;
    let timeout = Duration::from_secs(
        resolved
            .backend
            .preset()
            .ready_timeout_secs
            .saturating_add(15),
    );
    Some((requester_id, name, timeout))
}

/// Arm one successor self-kick target. The callback seam makes the exact-once
/// scheduling contract deterministic in tests while production supplies the
/// existing `spawn_self_kick_bootstrap` callback.
pub(crate) fn arm_target_once<F>(target: &mut Option<RestartSelfKickTarget>, arm: F) -> bool
where
    F: FnOnce(crate::types::InstanceId, String, Duration),
{
    let Some((instance_id, name, timeout)) = target.take() else {
        return false;
    };
    arm(instance_id, name, timeout);
    true
}

/// #2453 R2: an in-flight app-restart preflight probe (a direct child running
/// `--restart-probe`), owned + polled non-blocking by the TUI loop. Carries the
/// request's `reply` (TUI→handler verdict) and `flush_ack` (the transport's
/// post-flush commit-permission the TUI waits on before it commits + re-execs).
pub(super) struct RestartProbe {
    pub(super) child: std::process::Child,
    pub(super) reply: crossbeam_channel::Sender<crate::api::app_restart::AppRestartVerdict>,
    pub(super) flush_ack: crossbeam_channel::Receiver<()>,
    pub(super) requester_id: Option<crate::types::InstanceId>,
    pub(super) deadline: std::time::Instant,
}

/// #2453 R2: spawn the read-only preflight probe as a DIRECT child (own process
/// group, no TTY IO). It re-execs THIS binary with `--restart-probe`, which only
/// loads the binary + parses config/fleet.yaml (read-only; it must NOT attach to
/// the live predecessor). Clean exit (0) ⇒ the successor can boot far enough to
/// parse its config; non-zero / crash ⇒ abort the restart with the fleet intact.
pub(super) fn spawn_restart_probe() -> std::io::Result<std::process::Child> {
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("restart-probe")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0); // isolate: a probe kill never signals the TUI's group
    }
    cmd.spawn()
}

/// #2453 R2: the argv the app re-execs into on a committed restart. Preserves
/// the `app` subcommand + the `--fleet <override>` the operator launched with.
/// Factored out so it is unit-testable without performing the irreversible `exec`.
/// Unix-only: the in-place re-exec (`commit_app_restart`) exists only on Unix — its
/// sole caller — so gating this with it keeps the Windows build free of dead_code.
#[cfg(unix)]
pub(super) fn restart_argv(
    fleet_override: Option<&str>,
    requester_id: Option<crate::types::InstanceId>,
) -> Vec<String> {
    let mut argv = vec!["app".to_string()];
    if let Some(f) = fleet_override {
        argv.push("--fleet".to_string());
        argv.push(f.to_string());
    }
    if let Some(id) = requester_id {
        argv.push("--app-restart-requester".to_string());
        argv.push(id.full());
    }
    argv
}

/// #2453 R2: commit the app owner-restart by RE-EXEC (Unix) — replaces this
/// process image in place (same PID → same shell foreground job; a spawned
/// successor would be reclaimed by the shell, PROVEN by the R2 PTY harness). Only
/// returns on failure; by this point agents are stopped, the session is saved and
/// the terminal is restored, so failure is UNRECOVERABLE — log + exit non-zero so
/// the shell surfaces it (accepted residual risk, disclosed in the PR).
#[cfg(unix)]
pub(super) fn commit_app_restart(
    fleet_override: Option<&str>,
    requester_id: Option<crate::types::InstanceId>,
) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(restart_argv(fleet_override, requester_id));
    let err = cmd.exec(); // on success, never returns
    eprintln!(
        "agend-terminal: restart re-exec failed: {err}. Agents were stopped and the \
         session was saved — relaunch `agend-terminal app`."
    );
    crate::logging::flush_app_log();
    std::process::exit(70); // EX_SOFTWARE
}

/// #2453 R2: Windows never reaches a restart commit — the `App` strategy
/// fail-closes at the handler, so `RunOutcome::RestartRequested` never occurs.
#[cfg(windows)]
pub(super) fn commit_app_restart(
    _fleet_override: Option<&str>,
    _requester_id: Option<crate::types::InstanceId>,
) -> Result<()> {
    unreachable!("app restart commit is Unix-only; Windows fail-closes at the handler")
}

/// #2453 R2: the read-only app-restart preflight (the hidden `restart-probe`
/// subcommand). KISS + read-only: the binary has already loaded (we are running
/// it); confirm AGEND_HOME exists and fleet.yaml — if present — parses, WITHOUT
/// attaching to the live daemon (it never calls `bootstrap::prepare`). Exits 0
/// (ok) / 1 (fail). This proves the successor can boot far enough to parse its
/// config; it does NOT prove flock / api / control-plane readiness.
pub(crate) fn run_restart_probe() -> ! {
    let home = crate::home_dir();
    let code = if !home.exists() {
        1
    } else {
        let fleet_path = crate::fleet::fleet_yaml_path(&home);
        if !fleet_path.exists() {
            0 // a fresh app writes one on boot — absence is fine
        } else {
            match std::fs::read_to_string(&fleet_path)
                .ok()
                .and_then(|s| serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&s).ok())
            {
                Some(_) => 0,
                None => 1,
            }
        }
    };
    std::process::exit(code);
}

/// #2453 R2: the decision the TUI loop reaches when it polls the in-flight restart
/// probe on a tick. Extracted from the loop so the prepare-vs-abort branch — the one
/// gating the IRREVERSIBLE teardown+exec — is unit-testable without driving the full
/// TUI (mirrors `restart_argv`). A passing probe does NOT commit here; it yields
/// `Prepared` (gate still `Probing`) and the loop replies `Prepared`, waits for the
/// transport's post-flush ack, and ONLY THEN CAS `Probing→Committing` + breaks to
/// teardown+exec. `Abort` rolls the gate to `Serving` and keeps serving. Ownership:
/// on any terminal verdict the probe is `take`n out of the slot (killed+reaped on timeout).
pub(super) enum ProbePoll {
    /// Probe still running and within its deadline — keep serving this tick.
    Pending,
    /// Probe passed; gate is STILL `Probing` (NOT yet committed). The loop replies
    /// `Prepared`, then blocks on the `flush_ack` receiver for the transport's
    /// commit-permission; on the ack it CAS `Probing→Committing`, sets
    /// `RestartRequested`, and breaks to teardown+exec. A disconnect/timeout aborts.
    Prepared(
        crossbeam_channel::Sender<crate::api::app_restart::AppRestartVerdict>,
        crossbeam_channel::Receiver<()>,
        Option<crate::types::InstanceId>,
    ),
    /// Probe failed / timed out / errored — the gate was rolled back to `Serving`
    /// and NO restart happens. The loop replies `Aborted(reason)` and keeps
    /// serving; teardown + exec are never reached.
    Abort(
        crossbeam_channel::Sender<crate::api::app_restart::AppRestartVerdict>,
        String,
    ),
}

/// #2453 R2: poll `probe` once (non-blocking). A passing probe yields `Prepared`
/// WITHOUT touching the gate (it stays `Probing`) — the loop commits only after
/// the transport's post-flush ack. Every failure mode (non-zero exit, timeout,
/// wait error) rolls the gate back to `Serving` and yields `Abort`, PROVING a
/// failed preflight performs zero teardown.
pub(super) fn poll_restart_probe(
    probe: &mut Option<RestartProbe>,
    gate: &crate::api::app_restart::AppRestartGate,
) -> ProbePoll {
    if probe.is_none() {
        return ProbePoll::Pending;
    }
    match probe
        .as_mut()
        .expect("probe present (checked above)")
        .child
        .try_wait()
    {
        Ok(Some(status)) => {
            let p = probe.take().expect("probe present (checked above)");
            if status.success() {
                // Probe passed: DO NOT commit here — leave the gate at `Probing`. The
                // loop replies `Prepared` and CAS `Probing→Committing` only after the
                // transport's post-flush ack, so the `prepared` reply can't be lost to
                // a teardown that outran the writer.
                ProbePoll::Prepared(p.reply, p.flush_ack, p.requester_id)
            } else {
                gate.abort_to_serving();
                ProbePoll::Abort(
                    p.reply,
                    format!("preflight failed (exit {:?})", status.code()),
                )
            }
        }
        Ok(None) => {
            if std::time::Instant::now()
                >= probe
                    .as_ref()
                    .expect("probe present (checked above)")
                    .deadline
            {
                let mut p = probe.take().expect("probe present (checked above)");
                let _ = p.child.kill();
                let _ = p.child.wait(); // reap — no zombie
                gate.abort_to_serving();
                ProbePoll::Abort(p.reply, "preflight timed out (5s)".to_string())
            } else {
                ProbePoll::Pending
            }
        }
        Err(e) => {
            let p = probe.take().expect("probe present (checked above)");
            gate.abort_to_serving();
            ProbePoll::Abort(p.reply, format!("preflight wait error: {e}"))
        }
    }
}

/// #2453 R2 P0-2: how long the TUI keeps a `prepared` (commit-pending) restart alive
/// waiting for the transport's post-flush ack before a watchdog aborts it. Generous:
/// a successful flush delivers the ack near-instantly, so only a genuinely wedged API
/// writer reaches this. The `prepared` reply is honestly indeterminate (it never
/// promises completion), so a watchdog abort — even after a very-late delivered
/// `prepared` — stays contract-consistent ("if the app is still running, retry").
pub(super) const RESTART_COMMIT_WATCHDOG: std::time::Duration = std::time::Duration::from_secs(10);

/// #2453 R2 P0-2: the TUI's commit-pending restart state. After a passing probe the
/// loop replies `Prepared` and parks THIS (the `flush_ack` receiver + a watchdog
/// `deadline`) instead of BLOCKING — the loop polls it non-blockingly each tick so
/// the UI never freezes on a wedged API writer (codex R3 correction).
pub(super) struct CommitPending {
    pub(super) flush_ack: crossbeam_channel::Receiver<()>,
    pub(super) requester_id: Option<crate::types::InstanceId>,
    pub(super) deadline: std::time::Instant,
}

/// #2453 R2 P0-2: the decision from one non-blocking poll of the commit-pending
/// state — extracted so the "commit ONLY on the transport's post-flush ack, else
/// abort" rule (gating the irreversible teardown+exec) is unit-testable without the
/// TUI. Pure: reads the ack channel + compares `now` to the deadline; the caller
/// owns the gate CAS + teardown.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum CommitPoll {
    /// The post-flush ack arrived — the reply flushed. Caller CAS `Probing→Committing`
    /// and breaks to teardown+exec.
    Commit,
    /// Abort (roll the gate back to `Serving`, keep serving). Either the ack channel
    /// disconnected (the action dropped un-run: write/flush failed or the session
    /// ended) or the watchdog `deadline` passed with no ack. The `&'static str` names
    /// which, for an observable log.
    Abort(&'static str),
    /// No ack yet and still within the deadline — keep serving (UI responsive).
    Pending,
}

/// #2453 R2 P0-2: poll the commit-pending state once (NON-BLOCKING). `try_recv` is
/// checked FIRST, so an already-buffered ack always commits and the watchdog can
/// never preempt an ack the TUI has yet to observe. NOTE (codex R3 truthfulness
/// correction): a delivered `prepared` reply and this ack's visibility can straddle
/// the deadline, so a watchdog `Abort` may occur even after the client received
/// `prepared` — which is why the reply is worded as an indeterminate attempt.
pub(super) fn poll_commit_pending(cp: &CommitPending, now: std::time::Instant) -> CommitPoll {
    // try_recv FIRST: a buffered ack (the `prepared` reply flushed) always commits and
    // the watchdog can never preempt an ack the TUI has yet to observe. Only a
    // genuinely-empty channel is subject to the watchdog — and because the `prepared`
    // reply is an honest indeterminate attempt, aborting the app-intact restart on a
    // disconnect (reply not flushed) or the deadline stays contract-consistent.
    match cp.flush_ack.try_recv() {
        Ok(()) => CommitPoll::Commit,
        Err(crossbeam_channel::TryRecvError::Disconnected) => {
            CommitPoll::Abort("flush_disconnected")
        }
        Err(crossbeam_channel::TryRecvError::Empty) => {
            if now >= cp.deadline {
                CommitPoll::Abort("flush_ack_watchdog")
            } else {
                CommitPoll::Pending
            }
        }
    }
}
