//! Watchdog: classify PTY output into BlockedReason per daemon tick.

use crate::backend::Backend;
use crate::health::HealthTracker;
use crate::state::AgentState;
use std::path::Path;

/// bughunt2: the AgentStates that mean the agent is actively working / ready
/// again — i.e. recovered from a transient rate-limit. Used to auto-clear
/// the set-only RateLimit/QuotaExceeded health latch (mirrors the way the
/// underlying AgentState self-expires when the throttle banner clears).
fn recovered_from_rate_limit(state: AgentState) -> bool {
    matches!(
        state,
        AgentState::Idle | AgentState::Thinking | AgentState::ToolUse
    )
}

/// Parse `AGEND_WATCHDOG_DRY_RUN` env var. Returns true for "1"/"true"/"TRUE"/"True".
pub fn watchdog_dry_run_from_env() -> bool {
    std::env::var("AGEND_WATCHDOG_DRY_RUN")
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "True"))
        .unwrap_or(false)
}

/// Run one watchdog pass for a single agent. Called from the daemon tick loop.
///
/// - Classifies `screen` text against backend-specific error patterns.
/// - `dry_run=true`: logs to event_log only, does not mutate health state.
/// - `dry_run=false`: sets `BlockedReason` on the health tracker.
pub fn run_watchdog_pass(
    home: &Path,
    agent_name: &str,
    backend: &Backend,
    screen: &str,
    health: &mut HealthTracker,
    dry_run: bool,
    current_state: AgentState,
) {
    let reason = crate::state::classify_pty_output(backend, screen);

    // #1634: model-unsupported is HIGH_FP + never-auto-clearing + hang-suppressing.
    // Drive its latch from the RED-ANCHORED live `AgentState` (the StateTracker
    // applied the #919 red gate to reach `ModelUnsupported`), NOT the colorless
    // `classify_pty_output` — a plain-text prose mention of the error wording has
    // no color, so routing the latch through the state inherits the FP boundary.
    let model_unsupported = current_state == AgentState::ModelUnsupported;

    if dry_run {
        // Observability only — never mutate health (no set, no clear).
        if let Some(reason) = reason {
            crate::event_log::log(home, "watchdog_dry_run", agent_name, &format!("{reason:?}"));
        }
        if model_unsupported {
            crate::event_log::log(home, "watchdog_dry_run", agent_name, "ModelUnsupported");
        }
        return;
    }

    // bughunt2 auto-clear: the RateLimit/QuotaExceeded BlockedReason is a
    // SET-ONLY latch — `classify_pty_output` returns `None` once the
    // throttle banner scrolls off, so the old code never cleared it. That
    // latch permanently suppresses hang detection AND blocks task delivery,
    // so an agent stays silently "blocked" forever after a TRANSIENT limit.
    // The underlying AgentState self-expires when the limit lifts; mirror
    // that here by clearing the latch once the agent is actively working /
    // ready again. GUARDED to ONLY the rate-limit/quota latch — never
    // AwaitingOperator / PermissionPrompt / Crash / Hang (operator- or
    // crash-action-required reasons must NOT be auto-cleared; cf. the #1564
    // blocked-reason guard). The set below re-latches if the agent is in
    // fact still throttled (classify still matches).
    if recovered_from_rate_limit(current_state)
        && health
            .current_reason
            .as_ref()
            .is_some_and(|r| r.auto_clears_on(crate::health::RecoverySignal::RateLimitLifted))
    {
        health.clear_blocked_reason();
    }

    // #1634: surface model-unsupported. Transition-gated so the operator notify
    // fires ONCE on entry, not every tick (the red-gated state re-matches while
    // the error is on screen). Handled before the classify latch and returns —
    // ModelUnsupported is the dominant, manual-clear-only reason for this agent.
    if model_unsupported {
        let already = matches!(
            health.current_reason,
            Some(crate::health::BlockedReason::ModelUnsupported)
        );
        health.set_blocked_reason(crate::health::BlockedReason::ModelUnsupported);
        if !already {
            notify_model_unsupported(home, agent_name, backend);
        }
        return;
    }

    if let Some(reason) = reason {
        // #1955 self-poisoning fix (mirrors the #1634 pattern above): drive the
        // QuotaExceeded latch from the GATED live `AgentState`, not the raw
        // colorless `classify_pty_output` — an agent QUOTING the banner in an
        // RCA latched its own `blocked_reason` while `agent_state` was still
        // `thinking` (claude-f9af90, issue #1955). The StateTracker's
        // UsageLimit carries the input-line + tail-position + working-marker
        // gates and the release anchor; only latch quota when IT says so.
        if matches!(reason, crate::health::BlockedReason::QuotaExceeded)
            && current_state != AgentState::UsageLimit
        {
            // prose/quoted mention only — not a live limit
        } else {
            health.set_blocked_reason(reason);
        }
    }
}

/// #1634: one-line operator notice on the transition into `ModelUnsupported`.
/// Mirrors `conflict_notify`: a `System` `notify_agent` surfaces to the
/// operator via the agent's channel binding. The agent itself can't act (it
/// errors every turn) — the operator must change the configured model.
fn notify_model_unsupported(home: &Path, agent_name: &str, backend: &Backend) {
    let text = format!(
        "[model-unsupported] agent `{agent_name}` (backend {}): the configured model is \
         rejected by the provider (e.g. `invalid_request_error` / \"model is not supported\"). \
         The agent will ERROR every turn until you change its model — agent_state stays Idle but \
         it is NOT healthy. Fix the model, then restart the agent (or `health action=clear` once \
         resolved).",
        backend.name()
    );
    let source = crate::inbox::NotifySource::System("model_unsupported");
    crate::inbox::notify_agent(home, agent_name, &source, &text);
    tracing::error!(
        agent = agent_name,
        backend = backend.name(),
        "#1634: ModelUnsupported detected — configured model rejected by provider; operator must change the model"
    );
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::health::BlockedReason;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-watchdog-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn read_event_log(home: &Path) -> String {
        std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default()
    }

    #[test]
    fn test_watchdog_dry_run_env_logs_to_event_log() {
        let home = tmp_home("dry-run");
        let mut health = HealthTracker::new();
        let backend = Backend::KiroCli;

        run_watchdog_pass(
            &home,
            "test-agent",
            &backend,
            "ThrottlingError: Too Many Requests",
            &mut health,
            true, // dry_run
            AgentState::RateLimit,
        );

        assert!(
            health.current_reason.is_none(),
            "dry-run must not set current_reason"
        );
        let log = read_event_log(&home);
        assert!(
            log.contains("watchdog_dry_run"),
            "must log dry-run entry, got: {log}"
        );
        assert!(log.contains("RateLimit"), "must log reason, got: {log}");
        assert!(
            log.contains("test-agent"),
            "must log agent name, got: {log}"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// #1955 self-poisoning pin (claude-f9af90): an agent QUOTING the banner
    /// in its output (writing an RCA) makes `classify_pty_output` return
    /// QuotaExceeded — but the gated live state is `Thinking`, so the latch
    /// must NOT be set. Only a state-machine-confirmed UsageLimit latches.
    #[test]
    fn quoted_banner_while_thinking_does_not_latch_quota_1955() {
        let home = tmp_home("quota-quote-1955");
        let mut health = HealthTracker::new();
        let backend = Backend::ClaudeCode;

        run_watchdog_pass(
            &home,
            "test-agent",
            &backend,
            "writing the RCA: the banner says You've hit your weekly limit · resets 4am",
            &mut health,
            false, // live
            AgentState::Thinking,
        );

        assert!(
            health.current_reason.is_none(),
            "#1955: a quoted banner with gated state Thinking must not latch QuotaExceeded, got: {:?}",
            health.current_reason
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_watchdog_live_env_unset_sets_reason() {
        let home = tmp_home("live");
        let mut health = HealthTracker::new();
        let backend = Backend::KiroCli;

        run_watchdog_pass(
            &home,
            "test-agent",
            &backend,
            "ServiceQuotaExceededException: You have exceeded your quota",
            &mut health,
            false, // live
            // #1955: the QuotaExceeded latch now requires the GATED live state
            // to agree (the raw classify alone self-poisoned on quoted banner
            // text) — so model the realistic shape: the state machine latched
            // UsageLimit from the same banner. Not in the recovered set, so
            // the bughunt2 auto-clear stays inert and the set is observed.
            AgentState::UsageLimit,
        );

        assert!(
            matches!(health.current_reason, Some(BlockedReason::QuotaExceeded)),
            "live mode must set current_reason, got: {:?}",
            health.current_reason
        );
        let log = read_event_log(&home);
        assert!(
            !log.contains("watchdog_dry_run"),
            "live mode must not write dry-run log"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_watchdog_healthy_output_no_action() {
        let home = tmp_home("healthy");
        let mut health = HealthTracker::new();
        let backend = Backend::KiroCli;

        run_watchdog_pass(
            &home,
            "test-agent",
            &backend,
            "Thinking about your request...\n● Read src/main.rs",
            &mut health,
            false,
            AgentState::Thinking,
        );

        assert!(
            health.current_reason.is_none(),
            "healthy output must not set reason"
        );
        let log = read_event_log(&home);
        assert!(
            !log.contains("watchdog"),
            "healthy output must not write watchdog log"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// bughunt2: a rate-limit / quota latch (set by a prior throttled tick)
    /// auto-clears once the agent has recovered (healthy screen + active
    /// AgentState) — restoring hang detection + task delivery.
    #[test]
    fn test_watchdog_autoclear_rate_limit_latch_on_recovery() {
        let backend = Backend::KiroCli;
        let healthy = "Thinking about your request...\n● Read src/main.rs";
        for (latch, state) in [
            (
                BlockedReason::RateLimit {
                    retry_after_secs: Some(30),
                },
                AgentState::Idle,
            ),
            (BlockedReason::QuotaExceeded, AgentState::Idle),
            (
                BlockedReason::RateLimit {
                    retry_after_secs: None,
                },
                AgentState::ToolUse,
            ),
        ] {
            let home = tmp_home("autoclear-rl");
            let mut health = HealthTracker::new();
            health.set_blocked_reason(latch.clone());
            run_watchdog_pass(
                &home,
                "test-agent",
                &backend,
                healthy,
                &mut health,
                false,
                state,
            );
            assert!(
                health.current_reason.is_none(),
                "bughunt2: recovered agent ({state:?}) must auto-clear {latch:?}, got: {:?}",
                health.current_reason
            );
            std::fs::remove_dir_all(&home).ok();
        }
    }

    /// A genuinely still-limited agent (throttle banner present, state
    /// still RateLimit → not recovered) stays latched.
    #[test]
    fn test_watchdog_still_limited_stays_latched() {
        let home = tmp_home("still-limited");
        let mut health = HealthTracker::new();
        let backend = Backend::KiroCli;
        health.set_blocked_reason(BlockedReason::RateLimit {
            retry_after_secs: None,
        });
        run_watchdog_pass(
            &home,
            "test-agent",
            &backend,
            "ThrottlingError: Too Many Requests",
            &mut health,
            false,
            AgentState::RateLimit,
        );
        assert!(
            matches!(health.current_reason, Some(BlockedReason::RateLimit { .. })),
            "still-limited agent must stay latched, got: {:?}",
            health.current_reason
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Guard: the auto-clear is scoped to RateLimit/QuotaExceeded. An
    /// operator-action-required reason (AwaitingOperator) must NOT be
    /// cleared even when the agent looks recovered on screen.
    #[test]
    fn test_watchdog_autoclear_guard_spares_non_rate_limit_reasons() {
        let home = tmp_home("guard");
        let mut health = HealthTracker::new();
        let backend = Backend::KiroCli;
        health.set_blocked_reason(BlockedReason::AwaitingOperator);
        run_watchdog_pass(
            &home,
            "test-agent",
            &backend,
            "Thinking about your request...\n● Read src/main.rs",
            &mut health,
            false,
            AgentState::Idle,
        );
        assert!(
            matches!(health.current_reason, Some(BlockedReason::AwaitingOperator)),
            "bughunt2 guard: non-rate-limit reasons must NOT auto-clear, got: {:?}",
            health.current_reason
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// dry-run never mutates health — it must NOT auto-clear a latch
    /// either (observability mode is read-only).
    #[test]
    fn test_watchdog_dry_run_does_not_autoclear() {
        let home = tmp_home("dry-noclear");
        let mut health = HealthTracker::new();
        let backend = Backend::KiroCli;
        health.set_blocked_reason(BlockedReason::RateLimit {
            retry_after_secs: None,
        });
        run_watchdog_pass(
            &home,
            "test-agent",
            &backend,
            "Thinking about your request...\n● Read src/main.rs",
            &mut health,
            true, // dry_run
            AgentState::Idle,
        );
        assert!(
            matches!(health.current_reason, Some(BlockedReason::RateLimit { .. })),
            "dry-run must not mutate (clear) health, got: {:?}",
            health.current_reason
        );
        std::fs::remove_dir_all(&home).ok();
    }

    static ENV_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    #[test]
    fn test_watchdog_env_true_returns_true() {
        let _guard = ENV_LOCK.lock();
        for val in ["1", "true", "TRUE", "True"] {
            std::env::set_var("AGEND_WATCHDOG_DRY_RUN", val);
            assert!(
                super::watchdog_dry_run_from_env(),
                "AGEND_WATCHDOG_DRY_RUN={val} should return true"
            );
        }
        std::env::remove_var("AGEND_WATCHDOG_DRY_RUN");
    }

    #[test]
    fn test_watchdog_env_false_returns_false() {
        let _guard = ENV_LOCK.lock();
        for val in ["0", "false", "FALSE", "no", ""] {
            std::env::set_var("AGEND_WATCHDOG_DRY_RUN", val);
            assert!(
                !super::watchdog_dry_run_from_env(),
                "AGEND_WATCHDOG_DRY_RUN={val} should return false"
            );
        }
        std::env::remove_var("AGEND_WATCHDOG_DRY_RUN");
    }

    #[test]
    fn test_watchdog_env_unset_returns_false() {
        let _guard = ENV_LOCK.lock();
        std::env::remove_var("AGEND_WATCHDOG_DRY_RUN");
        assert!(
            !super::watchdog_dry_run_from_env(),
            "unset AGEND_WATCHDOG_DRY_RUN should return false"
        );
    }

    /// #1634: a red-gated `AgentState::ModelUnsupported` latches the
    /// `ModelUnsupported` BlockedReason, and — unlike a rate-limit latch — it
    /// must NOT auto-clear when the agent next reads Idle (it never auto-clears;
    /// only an operator model change + respawn `reset()` clears it).
    #[test]
    fn model_unsupported_latches_and_never_auto_clears_1634() {
        let home = tmp_home("model-unsupported");
        let mut health = HealthTracker::new();
        let backend = Backend::Codex;

        // Red-gated state arrives as ModelUnsupported → latch (+ one-time notify).
        run_watchdog_pass(
            &home,
            "dev",
            &backend,
            "screen",
            &mut health,
            false,
            AgentState::ModelUnsupported,
        );
        assert!(
            matches!(health.current_reason, Some(BlockedReason::ModelUnsupported)),
            "ModelUnsupported state must latch the BlockedReason, got {:?}",
            health.current_reason
        );

        // Agent next reads Idle (recovered_from_rate_limit == true). A rate-limit
        // latch would auto-clear here; ModelUnsupported must persist.
        run_watchdog_pass(
            &home,
            "dev",
            &backend,
            "screen",
            &mut health,
            false,
            AgentState::Idle,
        );
        assert!(
            matches!(health.current_reason, Some(BlockedReason::ModelUnsupported)),
            "#1634: ModelUnsupported must persist through Idle (never auto-clears), got {:?}",
            health.current_reason
        );
    }
}
