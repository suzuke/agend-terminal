//! Pins the post-revert contract that `process_rate_limit_recovery_nudges`
//! (introduced by #841, hotfixed by #846, reverted 2026-05-16 due to
//! classifier false-positive amplification) is NOT wired into the daemon
//! run loop. The feature MUST stay reverted until #848 ships the upstream
//! classifier fix that prevents false-positive RateLimit states from
//! triggering the nudge mechanism in the first place.
//!
//! Failure mode this pin defends against: a future PR re-adds the
//! `process_rate_limit_recovery_nudges` call site to supervisor's
//! `run_loop` without coordinating with #848. Per operator directive
//! 2026-05-16: \"先復原再考慮慢慢修根因\" — revert first, only re-enable
//! after root cause is fixed. This test enforces that workflow gate.
//!
//! When #848 (state classifier fix) ships AND the rate-limit recovery
//! nudge is re-introduced through a properly designed path, this test
//! should be REMOVED in the same PR that re-wires the call site.
//! Removing the test is the explicit signal that operator approved
//! re-enablement.

use std::fs;

#[test]
fn supervisor_run_loop_does_not_call_rate_limit_recovery_nudges() {
    let supervisor = fs::read_to_string("src/daemon/supervisor.rs")
        .expect("src/daemon/supervisor.rs must exist");

    // The revert removed both the function definition and its caller.
    // If either re-appears the test fails — forces re-coordination
    // with #848 + operator approval.
    let call_sites: Vec<_> = supervisor
        .lines()
        .enumerate()
        .filter(|(_, line)| {
            line.contains("process_rate_limit_recovery_nudges")
                && !line.trim_start().starts_with("//")
        })
        .collect();

    assert!(
        call_sites.is_empty(),
        "process_rate_limit_recovery_nudges must stay reverted until #848 \
         ships upstream classifier fix.\n\
         Found references in src/daemon/supervisor.rs:\n{}\n\
         If you are re-introducing the nudge feature, also remove this test \
         file (tests/no_rate_limit_recovery_nudge_invariant.rs) in the same PR.",
        call_sites
            .iter()
            .map(|(n, l)| format!("  line {}: {}", n + 1, l.trim()))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn fleet_yaml_does_not_define_rate_limit_recovery_config() {
    // Companion to the supervisor invariant: the config struct introduced
    // by #841 should also be reverted. If `RateLimitRecoveryConfig` resurfaces
    // in src/fleet.rs without coordinated re-enablement, this test fires.
    let fleet = fs::read_to_string("src/fleet.rs").expect("src/fleet.rs must exist");

    let definitions: Vec<_> = fleet
        .lines()
        .enumerate()
        .filter(|(_, line)| {
            (line.contains("struct RateLimitRecoveryConfig")
                || line.contains("RateLimitRecoveryConfig {"))
                && !line.trim_start().starts_with("//")
        })
        .collect();

    assert!(
        definitions.is_empty(),
        "RateLimitRecoveryConfig must stay reverted until #848 ships.\n\
         Found references in src/fleet.rs:\n{}\n\
         See tests/no_rate_limit_recovery_nudge_invariant.rs header.",
        definitions
            .iter()
            .map(|(n, l)| format!("  line {}: {}", n + 1, l.trim()))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
