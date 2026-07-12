use serde_json::{json, Value};
use std::path::Path;

/// #2453 Stage R1: explicit active-host restart capability dispatch.
///
/// Replaces the implicit `RUN_CORE_ACTIVE` global proxy with an exhaustive match
/// on the [`crate::api::RestartCapability`] INJECTED at `api::serve` from the
/// composition root (daemon / app / verify) and carried through the
/// `RuntimeContext`. An absent capability (`None` — a standalone bridge call that
/// never traversed the api `mcp_tool` ingress, so has no `RuntimeContext`) is
/// default-DENY. `App` and `Unsupported` are DISTINCT arms even though R1 routes
/// both to a fail-closed response; a future staged app strategy becomes the
/// `App` arm (decision d-20260712012329422433-1) with the compiler forcing every
/// dispatch site to handle it — no god-object switch.
pub(super) fn handle_restart_daemon(
    home: &Path,
    capability: Option<crate::api::RestartCapability>,
    app_restart: Option<crate::api::app_restart::AppRestart>,
    post_flush: Option<crate::api::app_restart::PostFlushSlot>,
) -> Value {
    use crate::api::RestartCapability;
    match capability {
        Some(RestartCapability::Daemon) => daemon_restart_strategy(home),
        Some(RestartCapability::App) => app_restart_strategy(app_restart, post_flush),
        Some(RestartCapability::Unsupported) | None => unsupported_fail_closed(),
    }
}

/// #2453 Stage R2: the `agend-terminal app` owner-restart strategy (Unix). Restart
/// is a RE-EXEC (same PID → same shell job; job-control proof, R2 PTY harness). TWO
/// PHASES with a transport commit barrier (decision d-20260712034222169749-5):
/// CAS-claim the gate `Serving→Probing`; hand a request to the TUI loop and BLOCK
/// (bounded) for its verdict. On `Prepared` (probe passed — gate STILL `Probing`)
/// register a post-flush commit-permission ack into THIS request's [`PostFlushSlot`]
/// and return the `prepared` JSON (an honest indeterminate ATTEMPT — the reply is
/// generated pre-ack, so it must NOT claim "committing"/"re-exec'ing"); `handle_session`
/// runs that ack ONLY after it writes+flushes the reply, and the TUI (polling
/// non-blockingly) then commits `Probing→Committing` and re-execs. So the reply cannot
/// be lost to a teardown that outran the writer, and a failed flush leaves the gate
/// recoverable at `Probing`. A concurrent duplicate loses the CAS claim → a RETRYABLE
/// "in progress" non-success (never a false ok:true, which the winner's later abort
/// could contradict).
#[cfg(unix)]
fn app_restart_strategy(
    app_restart: Option<crate::api::app_restart::AppRestart>,
    post_flush: Option<crate::api::app_restart::PostFlushSlot>,
) -> Value {
    use crate::api::app_restart::{AppRestartRequest, AppRestartVerdict};
    let Some(ar) = app_restart else {
        return app_no_channel_fail_closed();
    };
    // The two-phase barrier NEEDS this request's slot to arm: the TUI commits + execs
    // only after `handle_session` confirms THIS `prepared` reply flushed. A standalone
    // bridge call (no api mcp_tool ingress → no slot) can't guarantee that handshake,
    // so fail closed rather than risk a lost `prepared` reply.
    let Some(slot) = post_flush else {
        return app_no_channel_fail_closed();
    };
    // Genuine CAS claim: only one concurrent worker enters Probing.
    if !ar.gate.try_begin_probe() {
        // P0 (immediate-retry-before-abort): a CAS loser is NOT a success. The in-flight
        // winner may still ABORT on a flush failure, so a definitive ok:true here would
        // be a false success with no restart. Return a RETRYABLE non-success — the gate
        // is the idempotence authority, so at most one restart commits regardless, and a
        // retry either wins a fresh restart (winner aborted) or loses again (winner
        // committing).
        return json!({
            "ok": false,
            "restart": "in_progress",
            "retryable": true,
            "error": "restart_daemon: another app restart attempt is in progress and NOT yet \
                      committed; this request did not start or confirm a restart. If the app \
                      does not restart shortly, retry."
        });
    }
    // reply: TUI→handler verdict. flush_ack: transport→TUI commit-permission (a `()`
    // on a successful flush of this reply; a DISCONNECT means abort). Both bounded(1).
    let (reply_tx, reply_rx) = crossbeam_channel::bounded::<AppRestartVerdict>(1);
    let (ack_tx, ack_rx) = crossbeam_channel::bounded::<()>(1);
    if ar
        .tx
        .try_send(AppRestartRequest {
            reply: reply_tx,
            flush_ack: ack_rx,
        })
        .is_err()
    {
        ar.gate.abort_to_serving();
        return json!({
            "ok": false,
            "error": "restart_daemon: could not deliver the restart request to the TUI loop; fleet intact"
        });
    }
    // Block (bounded) for the verdict. Probe ≤5s; 7s safety net. restart_daemon's mcp
    // tool timeout is 60s (SLOW) > 7s, so this returns before the worker is abandoned.
    // The gate is NEVER Committing while we are here — the TUI commits only AFTER our
    // `prepared` reply flushes (post-return) — so the timeout branch can only roll
    // Probing→Serving; it can never lie "fleet intact" while the app execs.
    match reply_rx.recv_timeout(std::time::Duration::from_secs(7)) {
        Ok(AppRestartVerdict::Prepared) => {
            // Arm the barrier: register the commit-permission ack into THIS request's
            // slot. Only a successful write+flush of the `prepared` reply runs it (→
            // `ack_tx.send(())` → the TUI, polling `flush_ack` non-blockingly, commits
            // Probing→Committing + re-execs). Any non-success drops the action un-run
            // → `ack_tx` drops → the TUI's `flush_ack` disconnects → it aborts.
            if slot.register(Box::new(move || {
                let _ = ack_tx.send(());
            })) {
                // Honest PRE-ACK wording (codex R3): the reply is generated before the
                // transport ack, and a delivered `prepared` + the TUI's ack observation
                // can straddle the watchdog deadline — so the reply must NOT promise
                // completion. It is an indeterminate ATTEMPT: `prepared`, not
                // "committing"/"re-exec'ing".
                json!({
                    "ok": true,
                    "restart": "prepared",
                    "note": "preflight passed; the restart is prepared but NOT yet committed. \
                             The app will attempt an in-place re-exec after this reply is \
                             delivered; this connection then drops on success. If the app \
                             remains running (no restart), retry."
                })
            } else {
                // Slot already closed/occupied (a timed-out prior worker, or this
                // response already flushed) — cannot guarantee the barrier. Roll the
                // gate back; `ack_tx` just dropped so the TUI's `flush_ack`
                // disconnects and it aborts too.
                ar.gate.abort_to_serving();
                json!({
                    "ok": false,
                    "error": "restart_daemon: could not arm the flush barrier; fleet + TUI intact — no restart"
                })
            }
        }
        Ok(AppRestartVerdict::Aborted(reason)) => json!({
            "ok": false,
            "error": format!("restart_daemon aborted: {reason}. Fleet + TUI intact — no restart.")
        }),
        Err(_) => {
            // Timeout: gate is still Probing (the TUI hasn't committed) → roll back.
            // No lie possible. `ack_tx` drops → TUI's `flush_ack` disconnects → aborts.
            ar.gate.abort_to_serving();
            json!({
                "ok": false,
                "error": "restart_daemon preflight timed out; fleet + TUI intact — no restart"
            })
        }
    }
}

/// #2453 Stage R2: Windows owner-restart is an ISOLATED fail-closed strategy —
/// there is no in-place `exec` on Windows, and ConPTY/Windows-Terminal
/// tab-lifetime on predecessor exit is UNVERIFIED. Fail closed until a Windows
/// console/ConPTY handoff is proven (decision d-20260712012329422433-1).
#[cfg(windows)]
fn app_restart_strategy(
    _app_restart: Option<crate::api::app_restart::AppRestart>,
    _post_flush: Option<crate::api::app_restart::PostFlushSlot>,
) -> Value {
    tracing::warn!(
        target: "handoff",
        event = "fail_closed_app_windows",
        "#2453 restart_daemon refused on Windows app host — owner-restart unsupported (no exec; ConPTY unverified)"
    );
    json!({
        "ok": false,
        "error": "restart_daemon is not supported in `agend-terminal app` on Windows: in-place \
                  re-exec is unavailable and console/ConPTY handoff is unverified. Quit and relaunch \
                  the app."
    })
}

/// #2453 Stage R2: `App` capability but NO injected restart channel (e.g. a
/// standalone bridge call that never traversed the in-process app api ingress).
/// Fail closed — there is no TUI loop to consume a request. Unix-only: its sole
/// caller is the Unix `app_restart_strategy` (the Windows arm fail-closes directly),
/// so gating it keeps the Windows build free of dead_code.
#[cfg(unix)]
fn app_no_channel_fail_closed() -> Value {
    tracing::warn!(
        target: "handoff",
        event = "fail_closed_app_no_channel",
        "#2453 restart_daemon refused: app capability without an injected restart channel"
    );
    json!({
        "ok": false,
        "error": "restart_daemon is not available: no in-process app restart channel is wired \
                  for this request. To restart, quit and relaunch `agend-terminal app`."
    })
}

/// #2453 Stage R1: any other API-server owner (e.g. `verify`) or an ABSENT
/// injected capability — default-DENY. A DISTINCT `RestartCapability` value from
/// `App`; NEVER sets `RESTART_PENDING` and cannot reach the daemon strategy.
fn unsupported_fail_closed() -> Value {
    tracing::warn!(
        target: "handoff",
        event = "fail_closed_unsupported_host",
        "#2453 restart_daemon refused: this API host provides no in-process restart capability"
    );
    json!({
        "ok": false,
        "error": "restart_daemon is not available: this API-server host provides no in-process \
                  restart capability (default-deny). A standalone `agend-terminal start` daemon \
                  restarts in-process as normal."
    })
}

/// #2453 Stage R1: the daemon (`run_core`) host owns the in-process restart. The
/// body below is the pre-Stage-R1 handler VERBATIM (self-respawn default /
/// legacy exit(42) opt-out) — byte-semantically unchanged; only the app-mode
/// gate that used to precede it moved out to the capability dispatch above.
fn daemon_restart_strategy(home: &Path) -> Value {
    // #1814: self-respawn is the DEFAULT (Stage 4). By default the daemon owns
    // its own respawn (spawn successor → Phase-1 health gate → abort-stay-alive
    // on failure), so restart never hinges on an external supervisor being
    // correctly detected (retires the #851/#1812 brick class at the root). Only
    // the explicit opt-out `AGEND_RESTART_HANDOFF=0` takes the legacy supervisor
    // + exit(42) path below (byte-identical to pre-#1814). See
    // `daemon::restart::self_respawn_enabled` + `docs/env-vars.md`.
    // #1814 observability: record which restart path this request takes so the
    // pre-soak operator A/B (legacy exit42 vs self-respawn) is greppable on one
    // tag. `target: "handoff"` + an `event` field make the handoff lifecycle
    // events countable from the rolling log without a metrics framework.
    let self_respawn = crate::daemon::restart::self_respawn_enabled();
    tracing::info!(
        target: "handoff",
        event = "restart_requested",
        path = if self_respawn { "self-respawn" } else { "legacy-exit42" },
        "#1814 restart_daemon path selected"
    );
    if self_respawn {
        return handle_self_respawn(home);
    }

    // #851 fail-closed: refuse the request when no supervisor will
    // respawn the daemon. Without this check, `restart_daemon` on a
    // bare `agend-terminal start` invocation would set
    // `RESTART_PENDING`, the daemon would exit(42) on the next
    // supervisor tick, and operators would see
    // `Resource temporarily unavailable (os error 35)` on every
    // subsequent MCP call until they manually restart.
    if !crate::daemon::restart::is_restart_supervised() {
        // #1812 fail-closed remediation. Two operator situations collapse to
        // the same missing-sentinel state; spell out BOTH rather than try to
        // reliably tell them apart (a stale-install probe would have to parse
        // the on-disk plist/unit for the sentinel — too heavy for the value):
        //   (a) never installed → run `service install` (or use the wrapper).
        //   (b) installed on an earlier build → the AGEND_SUPERVISED sentinel
        //       is new, so an existing macOS/Linux service config predates it;
        //       re-run `service install` + restart once so the daemon relaunches
        //       carrying it. (This is the macOS upgrade migration from the design.)
        return json!({
            "ok": false,
            "error": "restart_daemon requires a supervisor that will respawn the daemon \
                      (launchd / systemd / scripts/agend-wrapper.sh). No supervisor sentinel \
                      (AGEND_SUPERVISED / AGEND_WRAPPED / INVOCATION_ID) was found in the \
                      daemon's environment. Remediation — if you have NOT installed the service: \
                      run `agend-terminal service install` (or launch via scripts/agend-wrapper.sh). \
                      If you installed it on an EARLIER build: the AGEND_SUPERVISED sentinel was \
                      added recently and your existing service config predates it, so re-run \
                      `agend-terminal service install` and restart the daemon once so it relaunches \
                      under the updated config. (Windows Task Scheduler cannot carry the sentinel \
                      yet and stays fail-closed.)"
        });
    }
    crate::daemon::RESTART_PENDING.store(true, std::sync::atomic::Ordering::Release);
    std::fs::write(home.join("restart-requested"), "").ok();
    let _ = crate::api::call(home, &json!({"method": crate::api::method::SHUTDOWN}));
    json!({"ok": true, "restart": "pending", "note": "daemon will exit(42) after graceful shutdown; supervisor restarts"})
}

/// #1814 self-respawn orchestration (runs in the daemon's `mcp_tool_*` worker
/// thread, NOT the api accept loop — see `api::serve` thread-per-connection):
/// spawn a successor, gate on its health, and only commit the predecessor's
/// shutdown if the successor is confirmed up. On failure the predecessor is
/// never signalled → it stays fully alive with its agents intact.
///
/// #2098/#2453 precondition: only ever reached via the `Daemon` arm of
/// `handle_restart_daemon`'s capability dispatch (`RestartCapability::Daemon`) —
/// so this never runs in app/owned mode and the `RESTART_PENDING.store(true)` on
/// commit always has a run_core consumer.
fn handle_self_respawn(home: &Path) -> Value {
    let old_pid = std::process::id();
    let handoff_value = crate::daemon::restart::make_handoff_value(old_pid);
    let mut succ = match crate::bootstrap::daemon_spawn::spawn_successor_handoff(
        home,
        &handoff_value,
    ) {
        Ok(s) => s,
        Err(e) => {
            return json!({
                "ok": false,
                "error": format!("self-respawn: failed to spawn successor: {e}; daemon stays alive")
            });
        }
    };
    tracing::info!(
        successor_pid = succ.pid,
        "#1814 self-respawn: successor spawned — running Phase-1 health gate"
    );

    if phase1_gate(&mut succ) {
        // Successor healthy → COMMIT. Set RESTART_PENDING ONLY; do NOT send an
        // api SHUTDOWN. The existing `handle_session` bridge (api/mod.rs: "the
        // restart_daemon MCP handler sets RESTART_PENDING … bridge it here") flips
        // the daemon's shutdown flag on the next api-loop iteration → the main
        // loop breaks → the run_core tail sees RESTART_PENDING + self_respawn and
        // exits(0), releasing the flock so the (blocked) successor promotes.
        //
        // We deliberately AVOID `api::call(home, SHUTDOWN)` here: during the
        // handoff overlap BOTH daemons are alive with a published api.port, so its
        // `find_active_run_dir` is ambiguous and could deliver SHUTDOWN to the
        // freshly-bound SUCCESSOR — which then shuts itself down the instant it
        // promotes (observed). RESTART_PENDING is process-local to THIS
        // predecessor, so there is no cross-daemon misdelivery.
        // #1814 FIX2: park the successor's child handle so the run_core loop can
        // do a FINAL liveness recheck before the irreversible teardown — if the
        // successor dies in the commit→exit window, the loop aborts-stay-alive
        // instead of bricking. Park BEFORE setting RESTART_PENDING (which arms
        // the shutdown bridge) so the handle is always available to the recheck.
        let succ_pid = succ.pid;
        crate::daemon::park_self_respawn_successor(succ.child);
        crate::daemon::RESTART_PENDING.store(true, std::sync::atomic::Ordering::Release);
        std::fs::write(home.join("restart-requested"), "").ok();
        tracing::info!(
            target: "handoff",
            event = "committed",
            successor_pid = succ_pid,
            "#1814 self-respawn: committed — predecessor exiting"
        );
        json!({
            "ok": true,
            "restart": "self-respawn",
            "successor_pid": succ.pid,
            "note": "successor confirmed healthy; predecessor exiting(0) — no external supervisor required"
        })
    } else {
        // ABORT-STAY-ALIVE. Kill the successor + remove its run dir so no stale
        // discovery artifact (api.port / control-ready / .daemon / cookie) lures
        // a client to a half-dead successor. The predecessor was NEVER signalled
        // → it stays fully alive with its agents intact.
        tracing::warn!(
            target: "handoff",
            event = "abort_phase1_failed",
            successor_pid = succ.pid,
            "#1814 self-respawn: successor FAILED Phase-1 gate — aborting; predecessor stays alive"
        );
        crate::process::kill_process_tree(succ.pid);
        let _ = succ.child.wait(); // reap so we don't leave a zombie
        let _ = std::fs::remove_dir_all(&succ.run_dir);
        json!({
            "ok": false,
            "error": "self-respawn aborted: successor failed the Phase-1 health gate. Daemon stays alive (no restart, agents intact). Check daemon.log for the successor's startup failure."
        })
    }
}

/// #1814 Phase-1 health gate: poll (≤30s, 100ms) until the successor's control
/// plane is confirmed — process alive + `control-ready` marker present + a real
/// cookie-authenticated api round-trip succeeds. Returns false on the
/// successor dying, the timeout, or a persistently unreachable api.
fn phase1_gate(succ: &mut crate::bootstrap::daemon_spawn::SuccessorHandle) -> bool {
    let started = std::time::Instant::now();
    let deadline = started + std::time::Duration::from_secs(30);
    let control_ready = succ.run_dir.join(crate::daemon::CONTROL_READY_FILE);
    loop {
        // `try_wait` detects a crash-on-launch immediately AND reaps the child
        // (so it never lingers as a zombie that `kill(pid, 0)` would misreport
        // as alive). `Ok(Some(_))` = exited.
        if matches!(succ.child.try_wait(), Ok(Some(_))) {
            tracing::warn!(
                target: "handoff",
                event = "phase1_failed",
                reason = "successor_exited",
                successor_pid = succ.pid,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "#1814 Phase-1: successor process exited during startup"
            );
            return false;
        }
        if control_ready.exists()
            && crate::api::call_at(
                &succ.run_dir,
                &json!({"method": crate::api::method::STATUS}),
            )
            .is_ok()
        {
            tracing::info!(
                target: "handoff",
                event = "phase1_passed",
                successor_pid = succ.pid,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "#1814 Phase-1: successor control-ready + api healthy"
            );
            return true;
        }
        if std::time::Instant::now() >= deadline {
            tracing::warn!(
                target: "handoff",
                event = "phase1_failed",
                reason = "timeout",
                successor_pid = succ.pid,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "#1814 Phase-1: successor not control-ready within timeout"
            );
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

// #2453 R2: the App-arm handler tests live in the sibling `restart_tests.rs` (its
// name contains "test" so the `file_size_invariant` skips it), keeping this handler
// implementation file under MAX_LOC. `#[path]` + `#[cfg(test)]` so it compiles only
// for tests; the split-out modules use `super::super::*` to reach this handler.
#[cfg(test)]
#[path = "restart_tests.rs"]
mod restart_tests;
