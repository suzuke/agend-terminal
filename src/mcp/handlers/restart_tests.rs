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
        let resp =
            handle_restart_daemon(Path::new("/tmp"), Some(RestartCapability::App), Some(ar), None);
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

    /// A duplicate request while the gate is already claimed → idempotent
    /// "already in progress", and NO request is sent to the loop.
    #[test]
    fn duplicate_while_in_flight_is_already_in_progress() {
        let (ar, rx) = make();
        assert!(ar.gate.try_begin_probe()); // simulate an in-flight probe holding the claim
        let resp = handle_restart_daemon(
            Path::new("/tmp"),
            Some(RestartCapability::App),
            Some(ar),
            Some(PostFlushSlot::new()),
        );
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["restart"], "already in progress");
        assert!(
            rx.try_recv().is_err(),
            "no request must be sent while a restart is in flight"
        );
    }

    /// Happy path: the stub loop replies `Prepared`; the handler REGISTERS a
    /// post-flush ack into the slot and returns ok:true restart:committing. The stub
    /// uses `recv_timeout` (not blocking `recv`) so a never-sending handler surfaces
    /// as a fast RED rather than a hung test. Proving the barrier is armed: a second
    /// `register` on the same slot is rejected.
    #[test]
    fn prepared_arms_barrier_and_returns_committing() {
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
        assert_eq!(resp["restart"], "committing");
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
        assert!(err.contains("aborted") && err.contains("intact"), "got {err:?}");
        assert!(
            slot.register(Box::new(|| {})),
            "an aborted probe must NOT arm the barrier — the slot stays free"
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
