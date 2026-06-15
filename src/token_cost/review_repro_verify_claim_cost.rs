//! Repro tests (verify-claim-cost batch) attached to `src/token_cost.rs`.
//!
//! Finding: `parse_since` multiplies a user-controlled `i64` by a unit
//! constant with a plain `*`. The `since` value flows straight from the MCP
//! `tokens` tool args, so a perfectly parseable-but-absurd value such as
//! `"100000000000000d"` (n = 1e14) makes `n * 86_400_000` overflow `i64`. In
//! the default debug/test profile `overflow-checks` is on, so the multiply
//! PANICS; in release it would silently wrap to a bogus cutoff. The fix is
//! checked arithmetic that fails closed (returns `None`).
//!
//! These private-fn tests live in an in-module submodule because
//! `token_cost` is a module of the BINARY crate (`src/main.rs`), so `parse_since`
//! is unreachable from `tests/` integration tests. Reached here via `super::`.

#![allow(clippy::expect_used)]

use super::parse_since;

/// RED until the multiply is made checked/saturating.
///
/// In debug/test (`overflow-checks = true`) the current `n * 86_400_000`
/// panics for `n = 1e14`. We wrap the call in `catch_unwind` and require it to
/// return `Ok` (no panic). After the fix to `checked_mul` + `checked_sub`, the
/// absurd input resolves to `None` (fail-closed) and no panic occurs.
#[test]
fn parse_since_huge_value_does_not_panic_or_overflow_verify_claim_cost() {
    // `now_ms` near a realistic epoch-ms value so a correct fix's `checked_sub`
    // also has a defined answer.
    let now_ms: i64 = 1_700_000_000_000;

    // `"100000000000000d"` -> num = "100000000000000" (1e14), unit = "d".
    // 1e14 * 86_400_000 == 8.64e21 > i64::MAX (9.22e18) -> overflow.
    let result = std::panic::catch_unwind(|| parse_since(Some("100000000000000d"), now_ms));

    assert!(
        result.is_ok(),
        "parse_since panicked on a parseable-but-huge `since` value \
         (debug overflow-checks); user-controlled MCP input must never panic. \
         Use checked_mul / checked_sub and fail closed (None)."
    );

    // When it does NOT panic, an absurd value must NOT yield a wrapped
    // (possibly negative/future) cutoff — it must fail closed to `None`.
    if let Ok(cutoff) = result {
        assert_eq!(
            cutoff, None,
            "an overflowing `since` must resolve to None (no cutoff), \
             never a wrapped/bogus epoch-ms cutoff"
        );
    }
}

/// Sanity anchor: a normal, in-range `since` still computes correctly. This
/// guards against a fix that over-corrects every value to `None`. Green now
/// and after the fix.
#[test]
fn parse_since_normal_value_still_computes_verify_claim_cost() {
    // 24h before 100_000_000 ms.
    assert_eq!(
        parse_since(Some("24h"), 100_000_000),
        Some(100_000_000 - 24 * 3_600_000)
    );
    assert_eq!(parse_since(Some("all"), 1000), None);
    assert_eq!(parse_since(None, 1000), None);
}
