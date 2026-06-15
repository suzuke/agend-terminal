//! Telegram long-poll supervisor health — issue #2200.
//!
//! A cold `api.telegram.org` (firewall / offline / region-block — common on
//! Chinese networks) used to make teloxide's `Dispatcher::dispatch()` PANIC via
//! `.expect("Couldn't prepare dispatching context")` (teloxide-0.17
//! `dispatcher.rs:385`) when the initial `get_me` failed. The old supervisor
//! caught the panic and restarted on a FIXED 5 s loop, so every cycle re-printed
//! teloxide's panic backtrace to stderr and washed the TUI.
//!
//! [`inbound::start_polling`] now drives `try_dispatch_with_listener`, which
//! RETURNS the `get_me` error instead of panicking, and feeds the outcome to the
//! pure state machine here. This module owns ONLY the decision logic
//! (classification + exponential backoff + degraded latch + noise discipline) so
//! it is unit-testable without a network, a runtime, or real sleeps.
//!
//! Noise discipline ([[feedback_system_noise_reduction_priority]]): the fix must
//! not become a new flood. We log ONE WARN on the first failure of an outage,
//! ONE INFO when the channel crosses into "degraded", then stay SILENT until a
//! single INFO on recovery — never per-retry-cycle.

use std::time::Duration;

use teloxide::{ApiError, RequestError};

/// Base backoff after the first transient failure.
pub(super) const POLL_BACKOFF_BASE: Duration = Duration::from_secs(5);
/// Backoff ceiling — exponential growth is clamped here.
pub(super) const POLL_BACKOFF_CAP: Duration = Duration::from_secs(60);
/// Consecutive transient failures before the channel is marked degraded.
pub(super) const POLL_DEGRADE_AFTER: u32 = 3;

/// How a connect failure should be treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConnectErrorClass {
    /// Retry with backoff — a network timeout / flood-wait / I/O blip / unknown
    /// API error. Expected at runtime, never fatal.
    Transient,
    /// A definitively bad / expired bot token — retrying NEVER recovers, so the
    /// supervisor stops polling instead of looping forever on a broken token.
    PermanentAuth,
}

/// Classify the error `try_dispatch_with_listener` returns from its initial
/// `get_me`. ONLY a definitively-permanent auth failure (`InvalidToken`, which
/// teloxide raises for HTTP 401 `Unauthorized` / `Not Found`) stops the
/// supervisor; everything else — including unknown API errors and 5xx — is
/// transient, so a momentary server blip never permanently disables the channel.
pub(super) fn classify_connect_error(err: &RequestError) -> ConnectErrorClass {
    match err {
        RequestError::Api(ApiError::InvalidToken) => ConnectErrorClass::PermanentAuth,
        _ => ConnectErrorClass::Transient,
    }
}

/// Exactly which log line (if any) a transient failure should emit. Fire-once
/// per state EDGE — never per retry cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FailureLog {
    /// First failure of a fresh outage → one WARN carrying the error.
    FirstWarn,
    /// Just crossed [`POLL_DEGRADE_AFTER`] → one INFO "channel offline".
    DegradedEntered,
    /// Already warned / already degraded → emit NOTHING.
    Silent,
}

/// What the supervisor loop should do after one failed attempt.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum FailureOutcome {
    /// Sleep `delay`, emit `log` (if any), then retry.
    Retry { delay: Duration, log: FailureLog },
    /// Permanent auth failure → emit one ERROR, then stop polling.
    Stop,
}

/// Pure backoff + degraded state machine. No I/O, no clock.
#[derive(Debug, Default)]
pub(super) struct PollingHealth {
    consecutive_failures: u32,
    degraded: bool,
}

impl PollingHealth {
    /// Record a failed connect attempt and decide the next action.
    pub(super) fn on_failure(&mut self, class: ConnectErrorClass) -> FailureOutcome {
        if class == ConnectErrorClass::PermanentAuth {
            return FailureOutcome::Stop;
        }
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        let delay = backoff_delay(self.consecutive_failures);
        let log = if self.consecutive_failures == 1 {
            FailureLog::FirstWarn
        } else if self.consecutive_failures == POLL_DEGRADE_AFTER && !self.degraded {
            self.degraded = true;
            FailureLog::DegradedEntered
        } else {
            FailureLog::Silent
        };
        FailureOutcome::Retry { delay, log }
    }

    /// Record a successful connect (`get_me` ok). Returns `true` EXACTLY once
    /// when recovering from a prior outage, so the loop emits a single recovery
    /// INFO; resets the backoff + degraded state either way.
    pub(super) fn on_success(&mut self) -> bool {
        let recovering = self.consecutive_failures > 0 || self.degraded;
        self.consecutive_failures = 0;
        self.degraded = false;
        recovering
    }
}

/// Exponential backoff `base * 2^(n-1)`, clamped to [`POLL_BACKOFF_CAP`].
/// Saturating + exponent-capped so a long outage can never overflow.
pub(super) fn backoff_delay(consecutive_failures: u32) -> Duration {
    let exp = consecutive_failures.saturating_sub(1).min(20);
    let mult = 1u32 << exp; // exp <= 20 → no overflow
    POLL_BACKOFF_BASE.saturating_mul(mult).min(POLL_BACKOFF_CAP)
}

#[cfg(test)]
mod tests {
    use super::*;

    // (b) exponential backoff sequence, capped at 60s.
    #[test]
    fn backoff_is_exponential_then_capped() {
        let got: Vec<u64> = (1..=7).map(|n| backoff_delay(n).as_secs()).collect();
        assert_eq!(got, vec![5, 10, 20, 40, 60, 60, 60]);
        // never panics / overflows for a very long outage:
        assert_eq!(backoff_delay(u32::MAX), POLL_BACKOFF_CAP);
    }

    // (c) degraded after exactly N consecutive failures; (d) at most one WARN +
    // one DegradedEntered across a whole outage (no per-cycle flood).
    #[test]
    fn degrades_after_n_and_logs_once_per_edge() {
        let mut h = PollingHealth::default();
        let mut logs = Vec::new();
        for _ in 0..6 {
            match h.on_failure(ConnectErrorClass::Transient) {
                FailureOutcome::Retry { log, .. } => logs.push(log),
                FailureOutcome::Stop => panic!("transient must not stop"),
            }
        }
        assert_eq!(
            logs,
            vec![
                FailureLog::FirstWarn,       // #1
                FailureLog::Silent,          // #2
                FailureLog::DegradedEntered, // #3 == POLL_DEGRADE_AFTER
                FailureLog::Silent,          // #4
                FailureLog::Silent,          // #5
                FailureLog::Silent,          // #6
            ],
            "exactly one WARN + one degraded INFO over an outage; the rest silent"
        );
    }

    // (e) recovery emits the one-shot INFO and resets backoff + degraded.
    #[test]
    fn recovery_logs_once_and_resets() {
        let mut h = PollingHealth::default();
        h.on_failure(ConnectErrorClass::Transient);
        h.on_failure(ConnectErrorClass::Transient);
        h.on_failure(ConnectErrorClass::Transient); // now degraded
        assert!(
            h.on_success(),
            "first success after an outage logs recovery"
        );
        assert!(
            !h.on_success(),
            "a second consecutive success is silent (no repeat recovery flood)"
        );
        // backoff reset to base after recovery:
        assert_eq!(
            h.on_failure(ConnectErrorClass::Transient),
            FailureOutcome::Retry {
                delay: POLL_BACKOFF_BASE,
                log: FailureLog::FirstWarn
            },
            "post-recovery the next outage starts from base backoff + a fresh WARN"
        );
    }

    // (d)/permanent: an invalid token stops; a network error is transient.
    #[test]
    fn classify_invalid_token_permanent_network_transient() {
        assert_eq!(
            classify_connect_error(&RequestError::Api(ApiError::InvalidToken)),
            ConnectErrorClass::PermanentAuth
        );
        assert_eq!(
            classify_connect_error(&RequestError::RetryAfter(
                teloxide::types::Seconds::from_seconds(3)
            )),
            ConnectErrorClass::Transient
        );
        // permanent auth → Stop, regardless of prior state.
        let mut h = PollingHealth::default();
        assert_eq!(
            h.on_failure(ConnectErrorClass::PermanentAuth),
            FailureOutcome::Stop
        );
    }

    // (a) No-panic guarantee — STRUCTURAL. The #2200 panic was teloxide's
    // `dispatch()` → `.expect("Couldn't prepare dispatching context")`; the only
    // network-safe entry is `try_dispatch_with_listener`, which RETURNS the
    // get_me error. A runtime panic-injection would need a real (failing)
    // network call, so we instead pin that `inbound.rs` drives the result-typed
    // API and the supervisor consumes a `Result` (no `.expect()` on the dispatch
    // outcome). A regression to the panicking `dispatch()` would drop these.
    #[test]
    fn inbound_drives_result_typed_dispatch_not_panicking_dispatch() {
        let src = include_str!("inbound.rs");
        let safe_api = "try_dispatch_with_listener";
        let result_boundary = "Result<(), teloxide::RequestError>";
        assert!(
            src.contains(safe_api),
            "inbound.rs must drive the non-panicking `{safe_api}` (not `dispatch()`, \
             whose `.expect(\"Couldn't prepare dispatching context\")` is the #2200 panic)"
        );
        assert!(
            src.contains(result_boundary),
            "the dispatch attempt must surface `{result_boundary}` so the supervisor \
             handles the connect error with backoff instead of panicking"
        );
    }
}
