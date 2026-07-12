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
) -> Value {
    use crate::api::RestartCapability;
    match capability {
        Some(RestartCapability::Daemon) => daemon_restart_strategy(home),
        Some(RestartCapability::App) => app_restart_strategy(app_restart),
        Some(RestartCapability::Unsupported) | None => unsupported_fail_closed(),
    }
}

/// #2453 Stage R2: the `agend-terminal app` owner-restart strategy (Unix). The
/// job-control proof (bash+zsh PTY harness) shows a spawned successor is
/// backgrounded when the launching process exits, so restart is a RE-EXEC (same
/// PID → same shell job). Flow: atomically CAS-claim the shared gate; on win hand
/// a request to the TUI loop and BLOCK (bounded) for its verdict — the loop runs a
/// read-only preflight probe and, on PASS, transitions the gate to Committing and
/// replies BEFORE teardown + re-exec (decision d-20260712034222169749-5). A
/// concurrent duplicate loses the CAS claim → idempotent "already in progress".
#[cfg(unix)]
fn app_restart_strategy(app_restart: Option<crate::api::app_restart::AppRestart>) -> Value {
    use crate::api::app_restart::{AppRestartRequest, AppRestartVerdict};
    let Some(ar) = app_restart else {
        return app_no_channel_fail_closed();
    };
    // Genuine CAS claim: only one concurrent worker enters Probing.
    if !ar.gate.try_begin_probe() {
        return json!({
            "ok": true,
            "restart": "already in progress",
            "note": "an app restart is already being processed; this request was a no-op"
        });
    }
    // Hand the request to the TUI loop; block (bounded) for its verdict. Bounded
    // oneshot (capacity 1). The loop runs the probe on its tick (≤5s); wait a
    // little longer as a safety net.
    let (reply_tx, reply_rx) = crossbeam_channel::bounded::<AppRestartVerdict>(1);
    if ar
        .tx
        .try_send(AppRestartRequest { reply: reply_tx })
        .is_err()
    {
        ar.gate.abort_to_serving();
        return json!({
            "ok": false,
            "error": "restart_daemon: could not deliver the restart request to the TUI loop; fleet intact"
        });
    }
    match reply_rx.recv_timeout(std::time::Duration::from_secs(7)) {
        Ok(AppRestartVerdict::Committing) => json!({
            "ok": true,
            "restart": "committing",
            "note": "preflight passed; the app is re-exec'ing in place (this connection will drop)"
        }),
        Ok(AppRestartVerdict::Aborted(reason)) => json!({
            "ok": false,
            "error": format!("restart_daemon aborted: {reason}. Fleet + TUI intact — no restart.")
        }),
        Err(_) => {
            // Safety-net timeout. Best-effort release (CAS from Probing; a no-op if
            // the loop already advanced to Committing).
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
fn app_restart_strategy(_app_restart: Option<crate::api::app_restart::AppRestart>) -> Value {
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
/// Fail closed — there is no TUI loop to consume a request.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    /// Tests touch the process-global `RESTART_PENDING` static and env
    /// state. Serialise via the SINGLE crate-wide
    /// [`crate::daemon::test_env_lock`] (shared with `daemon::restart` +
    /// `per_tick::recovery_dispatcher`) — env mutation races across all
    /// keys, so a module-local mutex wouldn't serialise against those
    /// other modules' env tests (#1812). Also resets `RESTART_PENDING`
    /// after each test so the next caller sees a clean slate.
    fn with_env_and_reset<R>(set: &[(&str, &str)], unset: &[&str], f: impl FnOnce() -> R) -> R {
        let _guard = crate::daemon::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let all_keys: Vec<&str> = set
            .iter()
            .map(|(k, _)| *k)
            .chain(unset.iter().copied())
            .collect();
        let prior: Vec<(String, Option<String>)> = all_keys
            .iter()
            .map(|k| (k.to_string(), std::env::var(k).ok()))
            .collect();
        let prior_restart_pending = crate::daemon::RESTART_PENDING.load(Ordering::Acquire);

        // SAFETY: test-only mutation, serialised via the mutex above.
        unsafe {
            for k in unset {
                std::env::remove_var(k);
            }
            for (k, v) in set {
                std::env::set_var(k, v);
            }
        }
        crate::daemon::RESTART_PENDING.store(false, Ordering::Release);

        let result = f();

        unsafe {
            for (k, v) in &prior {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
        crate::daemon::RESTART_PENDING.store(prior_restart_pending, Ordering::Release);

        result
    }

    /// Contract: when no supervisor signal env is set,
    /// `handle_restart_daemon` must fail-closed — return `ok: false`
    /// with an actionable error, and must NOT set `RESTART_PENDING`
    /// (which would trigger `exit(42)` on the next supervisor tick
    /// and leave the daemon dead with nothing to respawn it).
    #[test]
    fn handle_restart_daemon_fails_closed_when_unsupervised() {
        with_env_and_reset(
            // #1814 Stage 4: self-respawn is now the DEFAULT, so the legacy
            // fail-closed (is_restart_supervised) branch is only reached via the
            // explicit opt-out `AGEND_RESTART_HANDOFF=0`. Set it so this test
            // exercises the legacy supervisor-required path (was: unset, which
            // pre-Stage-4 meant legacy-by-default but now means self-respawn).
            &[("AGEND_RESTART_HANDOFF", "0")],
            &[
                "AGEND_WRAPPED",
                "AGEND_SUPERVISED",
                "INVOCATION_ID",
                "XPC_SERVICE_NAME",
            ],
            || {
                // #2453: inject the Daemon capability so this test drives the
                // daemon strategy — which reaches the legacy supervisor-detection
                // branch under test — with no process-global host state.
                // Manual tempdir — matches the std::env::temp_dir pattern
                // used elsewhere in this crate (e.g. claim_verifier.rs
                // tests). Avoids adding tempfile as a dev-dep just for
                // this one test.
                let tmp = std::env::temp_dir().join(format!(
                    "agend-restart-test-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0),
                ));
                std::fs::create_dir_all(&tmp).expect("create tempdir");
                let response =
                    handle_restart_daemon(&tmp, Some(crate::api::RestartCapability::Daemon), None);
                assert_eq!(
                    response["ok"], false,
                    "unsupervised invocation must return ok:false, got {response}"
                );
                let error = response["error"].as_str().unwrap_or("");
                assert!(
                    error.contains("supervisor"),
                    "error string must name the missing supervisor — got {error:?}"
                );
                assert!(
                    !crate::daemon::RESTART_PENDING.load(Ordering::Acquire),
                    "RESTART_PENDING must NOT be set when fail-closed — \
                     otherwise the next supervisor tick will exit(42) \
                     and leave the daemon dead with no respawn"
                );
                // restart-requested marker file must also NOT be written
                // (it's both a side-effect and an integration signal).
                assert!(
                    !tmp.join("restart-requested").exists(),
                    "restart-requested marker file must NOT exist when fail-closed"
                );
                let _ = std::fs::remove_dir_all(&tmp);
            },
        );
    }

    /// #1812 regression — bare GUI launch on macOS. `XPC_SERVICE_NAME` is
    /// ambient in a macOS GUI login session (set on EVERY process,
    /// including a bare `agend-terminal start` from Terminal.app), so the
    /// pre-#1812 detector treated an UNsupervised daemon as supervised and
    /// `restart_daemon` would exit(42) with nobody to respawn it (#851
    /// defeat). With XPC dropped, the REAL handler must fail-closed: only
    /// `XPC_SERVICE_NAME` set, all our markers unset → `ok: false` and no
    /// `RESTART_PENDING`. Drives the production `handle_restart_daemon`, not
    /// the bare detector.
    #[test]
    fn handle_restart_daemon_fails_closed_on_bare_gui_xpc_only() {
        with_env_and_reset(
            // #1814 Stage 4: opt into the legacy supervisor-detection path
            // (=0) so this exercises the is_restart_supervised XPC false-positive
            // guard — under the new self-respawn default it would otherwise hit
            // the spawn path (which also returns ok:false, masking the intent).
            &[("XPC_SERVICE_NAME", "0"), ("AGEND_RESTART_HANDOFF", "0")],
            &["AGEND_WRAPPED", "AGEND_SUPERVISED", "INVOCATION_ID"],
            || {
                // #2453: inject the Daemon capability so the legacy XPC
                // false-positive guard under test is reached (no host global).
                let tmp = std::env::temp_dir().join(format!(
                    "agend-restart-gui-test-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0),
                ));
                std::fs::create_dir_all(&tmp).expect("create tempdir");
                let response =
                    handle_restart_daemon(&tmp, Some(crate::api::RestartCapability::Daemon), None);
                assert_eq!(
                    response["ok"], false,
                    "ambient XPC_SERVICE_NAME alone must NOT be accepted as a \
                     supervisor (the #851/#1812 macOS-GUI false-positive) — got {response}"
                );
                assert!(
                    !crate::daemon::RESTART_PENDING.load(Ordering::Acquire),
                    "RESTART_PENDING must stay unset on the bare-GUI fail-closed path"
                );
                assert!(
                    !tmp.join("restart-requested").exists(),
                    "restart-requested marker must NOT be written on fail-closed"
                );
                let _ = std::fs::remove_dir_all(&tmp);
            },
        );
    }

    /// #2098 app/owned-mode fail-closed. `agend-terminal app` (combined
    /// TUI+daemon, run_app) never enters run_core, so it has NO RESTART_PENDING
    /// consumer: an in-process self-respawn would set RESTART_PENDING, brick the
    /// api session loop (api/mod.rs), and the held-flock successor would
    /// 30s-time-out and die — latching the brick permanently. With self-respawn
    /// the DEFAULT (#2094: `AGEND_RESTART_HANDOFF=1`), the handler MUST fail-closed
    /// for the App capability — BEFORE selecting any path or spawning a successor,
    /// and WITHOUT setting RESTART_PENDING. Drives the production
    /// `handle_restart_daemon` with the injected `RestartCapability::App` (the real
    /// app-mode state); the §3.9 `app_mode_restart_fails_closed_no_brick_2098`
    /// PTY test in tests/self_respawn_handoff.rs is the end-to-end reproduction.
    #[test]
    fn handle_restart_daemon_fails_closed_in_app_mode_no_run_core() {
        with_env_and_reset(
            // #2094 default: self-respawn ON — the exact brick scenario. The app
            // guard must short-circuit BEFORE this path is even selected.
            &[("AGEND_RESTART_HANDOFF", "1")],
            &[
                "AGEND_WRAPPED",
                "AGEND_SUPERVISED",
                "INVOCATION_ID",
                "XPC_SERVICE_NAME",
            ],
            || {
                // App host: inject the App capability (the real app-mode state)
                // so the handler dispatches to the app fail-close arm.
                let tmp = std::env::temp_dir().join(format!(
                    "agend-restart-appmode-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0),
                ));
                std::fs::create_dir_all(&tmp).expect("create tempdir");
                let response =
                    handle_restart_daemon(&tmp, Some(crate::api::RestartCapability::App), None);
                assert_eq!(
                    response["ok"], false,
                    "app-mode (no run_core consumer) restart must fail-closed, got {response}"
                );
                let error = response["error"].as_str().unwrap_or("");
                assert!(
                    error.contains("app"),
                    "fail-closed error must name `app` mode (actionable) — got {error:?}"
                );
                assert!(
                    !crate::daemon::RESTART_PENDING.load(Ordering::Acquire),
                    "RESTART_PENDING must NOT be set in app mode — with no run_core \
                     consumer it would latch and permanently brick the control plane"
                );
                assert!(
                    !tmp.join("restart-requested").exists(),
                    "restart-requested marker must NOT be written on app-mode fail-closed"
                );
                let _ = std::fs::remove_dir_all(&tmp);
            },
        );
    }

    /// #2453 Stage R1: unique per-test tempdir (matches the std::env::temp_dir
    /// pattern used above; avoids a tempfile dev-dep).
    fn unique_tmp(tag: &str) -> std::path::PathBuf {
        let tmp = std::env::temp_dir().join(format!(
            "agend-restart-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        std::fs::create_dir_all(&tmp).expect("create tempdir");
        tmp
    }

    /// #2453 Stage R1: an INJECTED `RestartCapability::Daemon` reaches the real
    /// restart machinery (the daemon strategy), NOT the app fail-close. With the
    /// legacy path forced (`AGEND_RESTART_HANDOFF=0`) and no supervisor present,
    /// the Daemon arm returns the supervisor-required error — proving dispatch
    /// routed to the daemon strategy (whose error names "supervisor"), not the
    /// app arm (whose error names `agend-terminal app`). No process-global.
    #[test]
    fn daemon_capability_reaches_restart_machinery_not_app() {
        with_env_and_reset(
            &[("AGEND_RESTART_HANDOFF", "0")],
            &[
                "AGEND_WRAPPED",
                "AGEND_SUPERVISED",
                "INVOCATION_ID",
                "XPC_SERVICE_NAME",
            ],
            || {
                let tmp = unique_tmp("daemon-cap");
                let response =
                    handle_restart_daemon(&tmp, Some(crate::api::RestartCapability::Daemon), None);
                assert_eq!(
                    response["ok"], false,
                    "unsupervised daemon restart fails closed on the supervisor check, got {response}"
                );
                let error = response["error"].as_str().unwrap_or("");
                assert!(
                    error.contains("supervisor"),
                    "Daemon capability must REACH the daemon strategy (supervisor-required error) — got {error:?}"
                );
                assert!(
                    !error.contains("agend-terminal app"),
                    "Daemon capability must NOT return the app fail-close message — got {error:?}"
                );
                let _ = std::fs::remove_dir_all(&tmp);
            },
        );
    }

    /// #2453 Stage R1: an INJECTED `RestartCapability::App` fail-closes with the
    /// app-specific message and MUST NOT set `RESTART_PENDING` or write the
    /// restart-requested marker (the successor-spawn path lives ONLY in the
    /// Daemon strategy, unreachable from this arm).
    #[test]
    fn app_capability_fails_closed_leaves_restart_pending_false() {
        with_env_and_reset(&[("AGEND_RESTART_HANDOFF", "1")], &[], || {
            let tmp = unique_tmp("app-cap");
            let response =
                handle_restart_daemon(&tmp, Some(crate::api::RestartCapability::App), None);
            assert_eq!(
                response["ok"], false,
                "app-capability restart must fail closed, got {response}"
            );
            let error = response["error"].as_str().unwrap_or("");
            assert!(
                error.contains("app"),
                "app fail-close error must name `app` mode (actionable) — got {error:?}"
            );
            assert!(
                !crate::daemon::RESTART_PENDING.load(Ordering::Acquire),
                "app capability must NOT set RESTART_PENDING"
            );
            assert!(
                !tmp.join("restart-requested").exists(),
                "app capability must NOT write restart-requested (no successor spawned)"
            );
            let _ = std::fs::remove_dir_all(&tmp);
        });
    }

    /// #2453 Stage R1: an INJECTED `RestartCapability::Unsupported` (e.g. the
    /// `verify` host) is default-deny — fail-closed, no `RESTART_PENDING`, and it
    /// CANNOT reach the daemon strategy. A DISTINCT value/arm from `App` even
    /// though both currently share a fail-closed helper.
    #[test]
    fn unsupported_capability_default_deny_cannot_reach_daemon() {
        with_env_and_reset(&[("AGEND_RESTART_HANDOFF", "1")], &[], || {
            let tmp = unique_tmp("unsupported-cap");
            let response =
                handle_restart_daemon(&tmp, Some(crate::api::RestartCapability::Unsupported), None);
            assert_eq!(
                response["ok"], false,
                "unsupported-host restart must default-deny, got {response}"
            );
            assert!(
                !crate::daemon::RESTART_PENDING.load(Ordering::Acquire),
                "unsupported capability must NOT set RESTART_PENDING"
            );
            assert!(
                !tmp.join("restart-requested").exists(),
                "unsupported capability must NOT write restart-requested"
            );
            let _ = std::fs::remove_dir_all(&tmp);
        });
    }

    /// #2453 Stage R1: an ABSENT capability (`None` — a standalone bridge call
    /// that never traversed the api `mcp_tool` ingress, so carries no
    /// `RuntimeContext`) default-denies exactly like `Unsupported`.
    #[test]
    fn absent_capability_none_default_deny() {
        with_env_and_reset(&[("AGEND_RESTART_HANDOFF", "1")], &[], || {
            let tmp = unique_tmp("none-cap");
            let response = handle_restart_daemon(&tmp, None, None);
            assert_eq!(
                response["ok"], false,
                "absent capability (None) must default-deny, got {response}"
            );
            assert!(
                !crate::daemon::RESTART_PENDING.load(Ordering::Acquire),
                "None capability must NOT set RESTART_PENDING"
            );
            let _ = std::fs::remove_dir_all(&tmp);
        });
    }
}

// #2453 R2: the App-arm handler tests live in the sibling `restart_tests.rs` (its
// name contains "test" so the `file_size_invariant` skips it), keeping this handler
// implementation file under MAX_LOC. `#[path]` + `#[cfg(test)]` so it compiles only
// for tests; the split-out modules use `super::super::*` to reach this handler.
#[cfg(test)]
#[path = "restart_tests.rs"]
mod restart_tests;
