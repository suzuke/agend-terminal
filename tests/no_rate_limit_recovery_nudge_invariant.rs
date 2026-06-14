//! Pins the contract that `process_rate_limit_recovery_nudges` is NOT wired
//! into the daemon run loop — the feature is currently intentionally reverted.
//!
//! History (this has been re-introduced and re-reverted several times — do not
//! assume a single linear narrative when reading the diff):
//!   #841 introduce → #846 hotfix → #849 revert (2026-05-16, classifier
//!   false-positive amplification) → #886 re-introduce (post-#848 classifier
//!   fix) → reverted again → re-reverted → reverted. Net current state on
//!   `main`: ABSENT. `git log -i --grep rate.limit.*nudge` shows the full chain.
//!
//! Because the feature has oscillated, this pin guards the *current* decision:
//! the recovery nudge stays out of supervisor's `run_loop` unless re-enabled
//! deliberately. The failure mode it defends against is a silent re-add (e.g.
//! an accidental revert-of-a-revert or cherry-pick) of the
//! `process_rate_limit_recovery_nudges` call site without an explicit decision.
//!
//! NOTE: this is a name-based source pin — it catches re-introduction under the
//! same identifier, not a rename. If you are deliberately re-enabling the
//! feature, update/remove this test in the same PR (removal is the explicit
//! signal that re-enablement was approved).

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
    // in src/fleet/mod.rs without coordinated re-enablement, this test fires.
    let fleet = fs::read_to_string("src/fleet/mod.rs").expect("src/fleet/mod.rs must exist");

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
         Found references in src/fleet/mod.rs:\n{}\n\
         See tests/no_rate_limit_recovery_nudge_invariant.rs header.",
        definitions
            .iter()
            .map(|(n, l)| format!("  line {}: {}", n + 1, l.trim()))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
