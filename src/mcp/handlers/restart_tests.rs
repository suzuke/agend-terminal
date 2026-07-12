//! #2453 R2 handler App-arm tests, split out of `restart.rs` so the handler
//! implementation file stays under the `file_size_invariant` MAX_LOC (this file's
//! name contains "test" → the invariant skips it). Included from `restart.rs` via
//! `#[cfg(test)] #[path = "restart_tests.rs"] mod restart_tests;`, so these modules
//! are grandchildren of `restart` — hence `use super::super::*` reaches the handler.

/// #2453 R2: the `App` owner-restart strategy — handler-side behavior. The TUI
/// loop is stubbed by a thread that consumes the request and replies, so these
/// exercise the real `handle_restart_daemon` App arm (CAS-claim → bounded
/// oneshot → verdict) without a live app.
#[cfg(all(test, unix))]
mod app_restart_strategy_tests {
    use super::super::*;
    use crate::api::app_restart::{
        AppRestart, AppRestartGate, AppRestartRequest, AppRestartVerdict,
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
        let resp = handle_restart_daemon(Path::new("/tmp"), Some(RestartCapability::App), None);
        assert_eq!(resp["ok"], false);
        assert!(resp["error"]
            .as_str()
            .expect("error is a string")
            .contains("no in-process app restart channel"));
    }

    /// A duplicate request while the gate is already claimed → idempotent
    /// "already in progress", and NO second request is sent to the loop.
    #[test]
    fn duplicate_while_in_flight_is_already_in_progress() {
        let (ar, rx) = make();
        assert!(ar.gate.try_begin_probe()); // simulate an in-flight probe holding the claim
        let resp = handle_restart_daemon(Path::new("/tmp"), Some(RestartCapability::App), Some(ar));
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["restart"], "already in progress");
        assert!(
            rx.try_recv().is_err(),
            "no second request must be sent while a restart is in flight"
        );
    }

    /// Happy path: the stub loop commits + replies `Committing` → handler returns
    /// ok:true restart:committing. The helper uses `recv_timeout` (not blocking
    /// `recv`) so a disabled/never-sending arm surfaces as a fast RED (failed
    /// join) rather than a hung test.
    #[test]
    fn probe_pass_returns_committing() {
        let (ar, rx) = make();
        let gate = ar.gate.clone();
        let t = std::thread::spawn(move || {
            let req = rx
                .recv_timeout(Duration::from_secs(2))
                .expect("request delivered to loop");
            assert!(
                gate.to_committing(),
                "gate must be Probing (handler claimed it)"
            );
            req.reply
                .send(AppRestartVerdict::Committing)
                .expect("verdict reply sent");
        });
        let resp = handle_restart_daemon(Path::new("/tmp"), Some(RestartCapability::App), Some(ar));
        t.join().expect("stub loop thread joined");
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["restart"], "committing");
    }

    /// Abort: the stub loop replies `Aborted` → handler returns ok:false, fleet
    /// + TUI intact.
    #[test]
    fn probe_abort_returns_error_fleet_intact() {
        let (ar, rx) = make();
        let gate = ar.gate.clone();
        let t = std::thread::spawn(move || {
            let req = rx
                .recv_timeout(Duration::from_secs(2))
                .expect("request delivered to loop");
            gate.abort_to_serving();
            req.reply
                .send(AppRestartVerdict::Aborted(
                    "preflight failed (exit Some(1))".into(),
                ))
                .expect("verdict reply sent");
        });
        let resp = handle_restart_daemon(Path::new("/tmp"), Some(RestartCapability::App), Some(ar));
        t.join().expect("stub loop thread joined");
        assert_eq!(resp["ok"], false);
        let err = resp["error"].as_str().expect("error is a string");
        assert!(
            err.contains("aborted") && err.contains("intact"),
            "got {err:?}"
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
        );
        assert_eq!(
            resp["ok"], false,
            "windows app restart must fail closed even with a channel"
        );
    }
}
