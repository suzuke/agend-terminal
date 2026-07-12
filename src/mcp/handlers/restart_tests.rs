//! #2453 R2 handler App-arm tests, split out of `restart.rs` so the handler
//! implementation file stays under the `file_size_invariant` MAX_LOC (this file's
//! name contains "test" → the invariant skips it). Included from `restart.rs` via
//! `#[cfg(test)] #[path = "restart_tests.rs"] mod restart_tests;`, so these modules
//! are grandchildren of `restart` — hence `use super::super::*` reaches the handler.

/// #2453 R2: the `App` owner-restart strategy — handler-side behavior under the
/// two-phase flush barrier. The TUI loop is stubbed by a thread that consumes the
/// request and replies a verdict, so these exercise the real `handle_restart_daemon`
/// App arm (CAS-claim → bounded oneshot → verdict → register post-flush ack) without
/// a live app. The transport-side commit handshake (flush → ack → TUI commit) is
/// covered by the barrier tests in app_restart.rs / self_respawn_handoff.rs.
#[cfg(all(test, unix))]
mod app_restart_strategy_tests {
    use super::super::*;
    use crate::api::app_restart::{
        AppRestart, AppRestartGate, AppRestartRequest, AppRestartVerdict, PostFlushSlot,
    };
    use crate::api::RestartCapability;
    use std::path::Path;
    use std::time::Duration;

    fn make() -> (AppRestart, crossbeam_channel::Receiver<AppRestartRequest>) {
        let gate = AppRestartGate::new();
        let (tx, rx) = crossbeam_channel::bounded::<AppRestartRequest>(1);
        (AppRestart { tx, gate }, rx)
    }

    /// `App` capability but NO injected channel → fail-closed (no TUI loop).
    #[test]
    fn app_no_channel_fails_closed() {
        let resp =
            handle_restart_daemon(Path::new("/tmp"), Some(RestartCapability::App), None, None);
        assert_eq!(resp["ok"], false);
        assert!(resp["error"]
            .as_str()
            .expect("error is a string")
            .contains("no in-process app restart channel"));
    }

    /// `App` + a channel but NO per-request PostFlushSlot (a standalone bridge call
    /// off the api mcp_tool ingress) → fail closed: the flush barrier cannot be armed,
    /// so committing is refused rather than risk a lost reply. No request is sent.
    #[test]
    fn app_without_flush_slot_fails_closed() {
        let (ar, rx) = make();
        let resp = handle_restart_daemon(
            Path::new("/tmp"),
            Some(RestartCapability::App),
            Some(ar),
            None,
        );
        assert_eq!(resp["ok"], false);
        assert!(resp["error"]
            .as_str()
            .expect("error is a string")
            .contains("no in-process app restart channel"));
        assert!(
            rx.try_recv().is_err(),
            "no request must be sent when the barrier can't be armed"
        );
    }

    /// #2453 R2 P0 (immediate-retry-before-abort): a duplicate/retry arriving while
    /// the gate is already claimed (winner still Probing, NOT yet committed) must get
    /// a RETRYABLE NON-SUCCESS — NOT `ok:true "already in progress"`. Otherwise, if
    /// the winner later ABORTS on a flush failure, this retry received a FALSE success
    /// while no restart happened. It returns `ok:false, restart:"in_progress",
    /// retryable:true` and sends NO request to the loop (the gate is the idempotence
    /// authority). RED against the old `ok:true "already in progress"`.
    #[test]
    fn duplicate_while_in_flight_is_retryable_not_false_success() {
        let (ar, rx) = make();
        assert!(ar.gate.try_begin_probe()); // simulate an in-flight probe holding the claim
        let resp = handle_restart_daemon(
            Path::new("/tmp"),
            Some(RestartCapability::App),
            Some(ar),
            Some(PostFlushSlot::new()),
        );
        assert_eq!(
            resp["ok"], false,
            "a CAS loser must NOT report success (the winner may still abort)"
        );
        assert_eq!(resp["restart"], "in_progress");
        assert_eq!(
            resp["retryable"], true,
            "the loser must be told to retry — the in-flight restart is not yet committed"
        );
        assert!(
            rx.try_recv().is_err(),
            "no request must be sent while a restart is in flight"
        );
    }

    /// #2453 R2 P0 (retry-after-abort): once the in-flight winner has ABORTED (its
    /// probe failed / flush dropped → gate rolled back to `Serving`), a retry must be
    /// able to WIN a FRESH restart — it CAS-claims `Serving→Probing` and, on a passing
    /// probe, returns `prepared` (arming a new barrier). Proves the gate is reusable
    /// after abort so a retry is never permanently wedged as a loser.
    #[test]
    fn retry_after_abort_wins_fresh_prepared() {
        let (ar, rx) = make();
        let slot = PostFlushSlot::new();
        // The prior attempt aborted: the gate is back at Serving (its default).
        assert!(ar.gate.is_serving());
        let t = std::thread::spawn(move || {
            let req = rx
                .recv_timeout(Duration::from_secs(2))
                .expect("request delivered to loop");
            req.reply
                .send(AppRestartVerdict::Prepared)
                .expect("verdict reply sent");
        });
        let resp = handle_restart_daemon(
            Path::new("/tmp"),
            Some(RestartCapability::App),
            Some(ar),
            Some(slot),
        );
        t.join().expect("stub loop thread joined");
        assert_eq!(resp["ok"], true, "a retry after abort must win a fresh restart");
        assert_eq!(resp["restart"], "prepared");
    }

    /// Happy path: the stub loop replies `Prepared`; the handler REGISTERS a
    /// post-flush ack into the slot and returns ok:true restart:"prepared" (an honest
    /// indeterminate attempt — the commit happens only after the transport ack, so the
    /// pre-ack reply must NEVER say "committing"/"re-exec'ing"). The stub uses
    /// `recv_timeout` (not blocking `recv`) so a never-sending handler surfaces as a
    /// fast RED rather than a hung test. Proving the barrier is armed: a second
    /// `register` on the same slot is rejected.
    #[test]
    fn prepared_arms_barrier_and_returns_prepared() {
        let (ar, rx) = make();
        let slot = PostFlushSlot::new();
        let t = std::thread::spawn(move || {
            let req = rx
                .recv_timeout(Duration::from_secs(2))
                .expect("request delivered to loop");
            req.reply
                .send(AppRestartVerdict::Prepared)
                .expect("verdict reply sent");
        });
        let resp = handle_restart_daemon(
            Path::new("/tmp"),
            Some(RestartCapability::App),
            Some(ar),
            Some(slot.clone()),
        );
        t.join().expect("stub loop thread joined");
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["restart"], "prepared");
        // The handler armed the barrier — the slot holds an action, so a second
        // register is rejected (the "concurrent response cannot consume the slot"
        // invariant). Running it must not panic (the ack receiver is already gone).
        assert!(
            !slot.register(Box::new(|| {})),
            "handler must have registered the post-flush ack"
        );
        slot.run_after_flush(true);
    }

    /// Abort: the stub loop replies `Aborted` → handler returns ok:false, fleet + TUI
    /// intact, and it does NOT arm the barrier (the slot stays free).
    #[test]
    fn probe_abort_returns_error_fleet_intact() {
        let (ar, rx) = make();
        let slot = PostFlushSlot::new();
        let t = std::thread::spawn(move || {
            let req = rx
                .recv_timeout(Duration::from_secs(2))
                .expect("request delivered to loop");
            req.reply
                .send(AppRestartVerdict::Aborted(
                    "preflight failed (exit Some(1))".into(),
                ))
                .expect("verdict reply sent");
        });
        let resp = handle_restart_daemon(
            Path::new("/tmp"),
            Some(RestartCapability::App),
            Some(ar),
            Some(slot.clone()),
        );
        t.join().expect("stub loop thread joined");
        assert_eq!(resp["ok"], false);
        let err = resp["error"].as_str().expect("error is a string");
        assert!(
            err.contains("aborted") && err.contains("intact"),
            "got {err:?}"
        );
        assert!(
            slot.register(Box::new(|| {})),
            "an aborted probe must NOT arm the barrier — the slot stays free"
        );
    }

    /// #2453 R2 barrier (real-entry): on `Prepared` the handler returns committing and
    /// ARMS the slot, but does NOT itself commit — the gate stays `Probing`, and the
    /// commit-permission is delivered ONLY when the transport runs the slot after a
    /// successful flush. Proves the "commit only after the reply flushes" invariant
    /// (delayed-writer): nothing commits until the exact response flush.
    #[test]
    fn prepared_does_not_commit_until_flush() {
        let (ar, rx) = make();
        let gate = ar.gate.clone();
        let slot = PostFlushSlot::new();
        let t = std::thread::spawn(move || {
            let req = rx
                .recv_timeout(Duration::from_secs(2))
                .expect("request delivered to loop");
            req.reply
                .send(AppRestartVerdict::Prepared)
                .expect("verdict reply sent");
            req.flush_ack // hand the receiver back so the test can observe the ack
        });
        let resp = handle_restart_daemon(
            Path::new("/tmp"),
            Some(RestartCapability::App),
            Some(ar),
            Some(slot.clone()),
        );
        let flush_ack = t.join().expect("stub loop thread joined");
        assert_eq!(resp["restart"], "prepared");
        // BEFORE the flush: the handler must NOT have committed, and no commit-
        // permission has been delivered.
        assert!(
            !gate.is_committing(),
            "the handler must NOT commit before the committing reply flushes"
        );
        assert!(
            flush_ack.try_recv().is_err(),
            "no commit-permission before the flush"
        );
        // The transport flushes THIS committing reply → the commit-permission fires.
        slot.run_after_flush(true);
        assert!(
            flush_ack.recv_timeout(Duration::from_secs(1)).is_ok(),
            "commit-permission is delivered ONLY after the successful flush"
        );
    }

    /// #2453 R2 barrier: if the TUI loop delivers NO verdict (its reply sender drops —
    /// a stuck/gone loop, or equivalently the 7s timeout), the handler rolls the gate
    /// back to `Serving`. It can NEVER be left `Committing`, so it can NEVER report
    /// "fleet intact" while a commit is in flight (the gate reaches `Committing` only
    /// AFTER the flush ack, which is post-return — the timeout branch sees only
    /// `Probing`). Dropping the sender makes this deterministic (no 7s wait).
    #[test]
    fn no_verdict_rolls_gate_to_serving() {
        let (ar, rx) = make();
        let gate = ar.gate.clone();
        let slot = PostFlushSlot::new();
        let t = std::thread::spawn(move || {
            // Receive the request and DROP it without replying → the handler's
            // reply_rx disconnects → its timeout/error branch runs (fast, no 7s wait).
            let _req = rx
                .recv_timeout(Duration::from_secs(2))
                .expect("request delivered to loop");
        });
        let resp = handle_restart_daemon(
            Path::new("/tmp"),
            Some(RestartCapability::App),
            Some(ar),
            Some(slot),
        );
        t.join().expect("stub loop thread joined");
        assert_eq!(resp["ok"], false);
        assert!(
            gate.is_serving(),
            "no verdict must roll the gate back to Serving (fleet intact)"
        );
        assert!(
            !gate.is_committing(),
            "no verdict must NEVER leave the gate Committing"
        );
    }
}

/// #2453 R2: Windows `App` owner-restart is an isolated fail-closed strategy —
/// even WITH an injected channel it must refuse. Runs on CI windows-latest.
#[cfg(all(test, windows))]
mod app_restart_windows_tests {
    use super::super::*;
    use crate::api::app_restart::{AppRestart, AppRestartGate, AppRestartRequest};
    use crate::api::RestartCapability;

    #[test]
    fn app_restart_fails_closed_on_windows() {
        let gate = AppRestartGate::new();
        let (tx, _rx) = crossbeam_channel::bounded::<AppRestartRequest>(1);
        let ar = AppRestart { tx, gate };
        let resp = handle_restart_daemon(
            std::path::Path::new("C:\\tmp"),
            Some(RestartCapability::App),
            Some(ar),
            None,
        );
        assert_eq!(
            resp["ok"], false,
            "windows app restart must fail closed even with a channel"
        );
    }
}

// #2453 R1/R2: the daemon/capability-dispatch handler tests, moved here from
// restart.rs to keep that handler impl file under the file_size_invariant MAX_LOC.
#[cfg(test)]
mod tests {
    use super::super::*;
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
                let response = handle_restart_daemon(
                    &tmp,
                    Some(crate::api::RestartCapability::Daemon),
                    None,
                    None,
                );
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
                let response = handle_restart_daemon(
                    &tmp,
                    Some(crate::api::RestartCapability::Daemon),
                    None,
                    None,
                );
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
                let response = handle_restart_daemon(
                    &tmp,
                    Some(crate::api::RestartCapability::App),
                    None,
                    None,
                );
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
                let response = handle_restart_daemon(
                    &tmp,
                    Some(crate::api::RestartCapability::Daemon),
                    None,
                    None,
                );
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
                handle_restart_daemon(&tmp, Some(crate::api::RestartCapability::App), None, None);
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
            let response = handle_restart_daemon(
                &tmp,
                Some(crate::api::RestartCapability::Unsupported),
                None,
                None,
            );
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
            let response = handle_restart_daemon(&tmp, None, None, None);
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
