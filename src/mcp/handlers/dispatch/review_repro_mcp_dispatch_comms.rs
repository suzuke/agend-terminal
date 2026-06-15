//! Verification/reproduction test for the `mcp-dispatch-comms` review batch.
//!
//! Attached as an in-module submodule of `dispatch` so the private
//! `parse_duration_secs` is reachable via `super::`.

#![allow(clippy::expect_used)]

/// Finding 5 (low / error-handling): `parse_duration_secs` multiplies an
/// `i64` parsed from an arbitrary digit run by 3600/60 with UNCHECKED
/// arithmetic (`total += n * 3600`). An attacker-controlled `duration`
/// such as `"9999999999999999h"` parses to a valid `i64` (~1e16) whose
/// `* 3600` (~3.6e19) overflows `i64::MAX` (~9.2e18). In debug builds
/// (overflow-checks on, the default dev profile) this PANICS; in release
/// it wraps to a bogus / negative value. It is reachable from
/// `dispatch_watchdog_snooze` via the agent-controlled `duration` arg, and
/// the downstream `.min(MAX_SNOOZE_SECS)` clamp does not prevent the
/// overflow in the multiplication itself.
///
/// CORRECT behaviour: a malformed/oversized duration is REJECTED as
/// invalid (returns `None`), never panicking and never wrapping.
///
/// RED now: in a debug test binary the multiplication overflow panics; the
/// `catch_unwind` below returns `Err`, so the `is_ok()` assert fails.
///
/// GREEN after fix: with checked arithmetic returning `None` on overflow,
/// the call returns `Ok(None)` — no panic — and both asserts pass.
#[test]
fn parse_duration_secs_does_not_overflow_on_huge_input_mcp_dispatch_comms() {
    // Sized so the `i64` parse succeeds but `n * 3600` exceeds i64::MAX.
    let malicious = "9999999999999999h";

    let outcome = std::panic::catch_unwind(|| super::parse_duration_secs(malicious));

    let parsed = outcome.expect(
        "parse_duration_secs must NOT panic on an oversized duration: the unchecked `n * 3600` \
         overflows i64 (debug builds panic). Use checked arithmetic returning None instead.",
    );
    assert_eq!(
        parsed, None,
        "an oversized/overflowing duration must be rejected as invalid (None), \
         not silently wrapped to a bogus value"
    );
}
