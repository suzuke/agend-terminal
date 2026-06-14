//! Repro test (panic_io_extra scope) for the claim_verifier scope-marker
//! parse cross-string byte-offset slice panic.
//!
//! `extract_task_id` computes `idx` from `s.to_lowercase()` and then slices the
//! ORIGINAL `s` with `s[idx..]`. `to_lowercase` is NOT byte-length preserving
//! (e.g. U+0130 'I-with-dot' -> "i\u{0307}", 2 bytes -> 3 bytes), so for
//! non-ASCII input `idx` can land on a non-char-boundary of `s` -> slice panic.
//! Reached from the public `parse_claims` entry via agent/operator-supplied
//! claim text.

#![allow(clippy::unwrap_used)]

use super::{parse_claims, Claim};

/// Triggering input:
/// - `\u{130}` (LATIN CAPITAL LETTER I WITH DOT ABOVE) is 2 bytes in `s` but
///   lowercases to 3 bytes, shifting every byte offset after it by +1 in the
///   lowercased string relative to `s`.
/// - `\u{4e2d}` (a 3-byte CJK char) sits right where the task-id token begins,
///   so the +1 shifted `idx` lands INSIDE that char -> `s[idx..]` is not a char
///   boundary.
///
/// RED now: `parse_claims` -> `parse_one` -> `extract_task_id` panics, so
/// `catch_unwind` returns `Err`.
/// GREEN after fix: a same-string offset / `s.get(idx..)` never slices on a
/// non-boundary; the call returns a `ScopeFollowsDispatchSpec` claim.
#[test]
#[ignore = "claim_verifier-non-ascii-slice: red until fix; remove #[ignore] after fix to confirm"]
fn extract_task_id_non_ascii_prefix_does_not_panic_panic_io_extra() {
    let claim_text = "\u{130} scope follows dispatch spec\u{4e2d}t-1";

    let result = std::panic::catch_unwind(|| parse_claims(claim_text));

    let claims = result.expect(
        "parse_claims must not panic on non-ASCII claim text: extract_task_id slices the \
         original string with a byte offset computed from the lowercased string",
    );

    // Once it no longer panics, the marker branch still classifies it as a
    // scope-follows claim (the parser routes on the lowercased marker, which is
    // present). We do not pin the exact task_id, only the variant — the fix is
    // free to choose a safe same-string slice.
    assert!(
        claims
            .iter()
            .any(|c| matches!(c, Claim::ScopeFollowsDispatchSpec { .. })),
        "expected a ScopeFollowsDispatchSpec claim, got: {claims:?}"
    );
}
