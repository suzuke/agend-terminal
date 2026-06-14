//! Repro tests (verify-claim-cost batch) attached to `src/claim_verifier.rs`.
//!
//! Two findings on private/`pub(crate)`-surface items of the BINARY crate
//! module `claim_verifier` (a module of `src/main.rs`, NOT reachable from
//! `tests/`). Reached here via `super::`.
//!
//! 1. `find_cargo_test_payload` only inspects the FIRST `cargo test` substring;
//!    a preceding `cargo testbed` suppresses a real later `cargo test foo::bar`
//!    on the same line, letting a hallucinated test name bypass #812.
//! 2. `parse_one`/`parse_claims` reclassifies any prose containing an
//!    `fn name(` token as `Claim::FunctionExists` (and verify() then hard-
//!    rejects), violating the module's 'Unknown phrases pass through' contract.

#![allow(clippy::expect_used)]

use super::{find_cargo_test_payload, parse_claims, Claim};

/// RED until `find_cargo_test_payload` scans ALL `cargo test` occurrences.
///
/// `"cargo testbed && cargo test foo::bar_test"`: the first textual match is
/// inside `cargo testbed` (next char `b`, not whitespace) so the current code
/// bails with `None`, dropping the genuine later invocation. Correct behavior
/// returns the payload of the SECOND, valid occurrence.
#[test]
#[ignore = "verify-claim-cost cargo-testbed-first-match: red until fix; remove #[ignore] after fix to confirm"]
fn find_cargo_test_payload_skips_testbed_and_finds_real_invocation_verify_claim_cost() {
    let line = "cargo testbed && cargo test foo::bar_test";

    let payload = find_cargo_test_payload(line);
    assert_eq!(
        payload,
        Some("foo::bar_test"),
        "find_cargo_test_payload must advance past `cargo testbed` to the real \
         `cargo test foo::bar_test` later on the line, not bail on the first \
         textual match (would fail-open #812 dispatch-name validation)"
    );

    // End-to-end: the public extractor should surface the real test name.
    let names = super::extract_test_invocations(line);
    assert_eq!(
        names,
        vec!["bar_test".to_string()],
        "extract_test_invocations must recover the real test name once the \
         `cargo testbed` prefix no longer suppresses the later invocation"
    );
}

/// Control: a standalone `cargo testing ...` (no real invocation) must still
/// return None. Green now and after the fix — guards against a fix that
/// matches `cargo testing` as a real invocation.
#[test]
fn find_cargo_test_payload_rejects_standalone_testing_verify_claim_cost() {
    assert_eq!(find_cargo_test_payload("cargo testing the waters"), None);
}

/// RED until `fn name(` prose stops being coerced into `Claim::FunctionExists`.
///
/// Free-text describing intended/future work contains the `fn name(` shape;
/// the current last grammar arm reclassifies it as a verifiable claim, and
/// `verify()` then rejects the whole push because the fn is absent. The module
/// contract (header lines 3-4) is that unknown phrases pass through.
#[test]
#[ignore = "verify-claim-cost fn-prose-falsepositive: red until fix; remove #[ignore] after fix to confirm"]
fn prose_mentioning_fn_pattern_stays_unknown_verify_claim_cost() {
    let text = "will add fn new_helper() in a follow-up";
    let claims = parse_claims(text);

    assert_eq!(
        claims,
        vec![Claim::Unknown(text.to_string())],
        "prose merely MENTIONING an `fn name(` pattern must pass through as \
         Claim::Unknown (no false-positive blocking), not be coerced into \
         Claim::FunctionExists and hard-rejected"
    );
}

/// Control: a deliberate, plain `fn name(` assertion is the case the
/// FunctionExists grammar is FOR. We assert only that it is NOT misclassified
/// as Unknown — keeping this test agnostic to the exact post-fix gating verb so
/// it stays green whether the fix keys on 'exists', backticks, etc. The prose
/// test above is what proves the false-positive is gone.
#[test]
fn deliberate_fn_exists_claim_is_not_unknown_verify_claim_cost() {
    let claims = parse_claims("fn parse_claims() exists");
    assert_eq!(claims.len(), 1, "one segment -> one claim");
    assert!(
        !matches!(claims[0], Claim::Unknown(_)),
        "a deliberate `fn name() exists` assertion should remain a verifiable \
         FunctionExists claim, not fall through to Unknown"
    );
}
