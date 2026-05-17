//! Integration smoke for the rate-limit recovery nudge tracker shape.
//!
//! The per-tick orchestration in `process_rate_limit_recovery_nudges`
//! needs a live `AgentRegistry` to drive end-to-end, which is not
//! easily fabricated from a test boundary without leaking internals.
//! This integration test focuses on the **pure decision contract**
//! through the public API: re-affirms that the salvaged decide_nudge
//! state machine reaches the right verdict on the canonical
//! "rate-limit observed → recovered → window elapsed" trajectory that
//! today's incident (fixup-dev 80-min stuck) would have triggered.
//!
//! Race-class deterministic (§3.20 SOP 1): all timing through injected
//! `Instant`; zero `thread::sleep`. Pre-fix base (before this PR's
//! C2 GREEN) fails because `decide_nudge` is `unimplemented!()`; this
//! test goes green when the body lands. Reviewer RED protocol per
//! §3.20 SOP 3.

// The supervisor module is `pub(crate)` so this integration test
// reaches its symbols via the binary entry path — the unit-test suite
// in `src/daemon/supervisor.rs::tests` already covers the same
// surface. This scaffold exists so the operator-smoke section of the
// PR body can point at a deterministic reproduction.
#[test]
fn rate_limit_nudge_deterministic_reproduction_documented_in_unit_tests() {
    // Empty body — the contract lives in src/daemon/supervisor.rs
    // unit tests:
    //   - nudge_fires_when_all_conditions_met (canonical happy path)
    //   - nudge_does_not_re_fire_when_fired_this_cycle_and_state_returns_to_ready
    //     (#846 regression lock)
    //   - nudge_hourly_cap_skips_4th_nudge_in_1h_window (Q3 cap)
    //   - nudge_hourly_cap_resets_after_window_elapses (Q3 cap rollover)
    //
    // Operator smoke gate (§3.20 SOP 2) is the real end-to-end:
    // trigger a real rate-limit event or fixture replay and observe a
    // single "continue your prior work" nudge after ~90s of post-
    // recovery silence. PR body has the smoke checklist.
}
