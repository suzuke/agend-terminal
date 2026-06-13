use serde_json::{json, Value};
use std::path::Path;

pub(super) fn handle_restart_daemon(home: &Path) -> Value {
    // #1814 Stage 1: flag-gated self-respawn. When AGEND_RESTART_HANDOFF=1 the
    // daemon owns its own respawn (spawn successor → Phase-1 health gate →
    // abort-stay-alive on failure), so restart never hinges on an external
    // supervisor being correctly detected (retires the #851/#1812 brick class
    // at the root). Flag OFF → the legacy supervisor + exit(42) path below runs
    // byte-identically.
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
            &[],
            &[
                // #1964 drive-by (env false-fail): AGEND_RESTART_HANDOFF is now
                // set fleet-wide (#1814 Stage 1 deployed) — without clearing it
                // this test takes the self-respawn branch instead of the
                // fail-closed one and fake-FAILs on every fleet agent.
                "AGEND_RESTART_HANDOFF",
                "AGEND_WRAPPED",
                "AGEND_SUPERVISED",
                "INVOCATION_ID",
                "XPC_SERVICE_NAME",
            ],
            || {
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
                let response = handle_restart_daemon(&tmp);
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
            &[("XPC_SERVICE_NAME", "0")],
            &["AGEND_WRAPPED", "AGEND_SUPERVISED", "INVOCATION_ID"],
            || {
                let tmp = std::env::temp_dir().join(format!(
                    "agend-restart-gui-test-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0),
                ));
                std::fs::create_dir_all(&tmp).expect("create tempdir");
                let response = handle_restart_daemon(&tmp);
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
}
