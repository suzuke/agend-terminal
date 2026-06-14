//! Repro (daemon-retention batch): the Issue #664 compliance review-verdict
//! gate (`has_review_verdict`) does a bare substring test
//! `pr.body.to_uppercase().contains("VERIFIED")`. "VERIFIED" is a substring of
//! "UNVERIFIED" and "NOT VERIFIED", so a PR whose review explicitly came back
//! UNVERIFIED PASSES the gate — a false-negative for the exact rejected-but-
//! merged case operators most need surfaced. The auto-release path elsewhere
//! deliberately word-anchors (`starts_with("VERIFIED")` vs UNVERIFIED), so this
//! loose substring is a real drift.
//!
//! Behavioral unit test on the private `has_review_verdict` via `super::`
//! (child modules see parent privates). Attaches to src/daemon/task_sweep.rs.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::{has_review_verdict, PrMeta};

/// Build a minimal PrMeta with the given body (mirrors the existing
/// `make_pr_meta` helper in the sibling `tests` mod, replicated here because
/// that helper is not visible from this submodule).
fn pr_with_body(body: &str) -> PrMeta {
    PrMeta {
        number: 1,
        title: "fix: something".to_string(),
        state: "closed".to_string(),
        merged: true,
        merge_commit_sha: Some("abc123".to_string()),
        merged_at: Some("2026-05-12T00:00:00Z".to_string()),
        body: body.to_string(),
        author_login: "test-user".to_string(),
        api_response_hash: "deadbeef".to_string(),
    }
}

#[test]
#[ignore = "daemon-retention review-verdict-substring: red until fix; remove #[ignore] after fix to confirm"]
fn unverified_body_is_not_a_passing_review_verdict_daemon_retention() {
    // A review that came back UNVERIFIED must NOT satisfy the verdict gate.
    let unverified = pr_with_body("Review result: UNVERIFIED by reviewer-codex");
    assert!(
        !has_review_verdict(&unverified),
        "daemon-retention: an UNVERIFIED review body must FAIL the review-verdict gate, \
         but the bare `contains(\"VERIFIED\")` matches UNVERIFIED as a pass — the exact \
         rejected-but-merged false-negative Issue #664 exists to surface."
    );

    // "NOT VERIFIED" likewise must not pass.
    let not_verified = pr_with_body("This PR was NOT VERIFIED before merge.");
    assert!(
        !has_review_verdict(&not_verified),
        "daemon-retention: a 'NOT VERIFIED' review body must FAIL the gate (substring \
         match incorrectly passes it)."
    );

    // Regression guard for the fix: a genuine VERIFIED verdict must still PASS,
    // so a word-anchored fix doesn't over-correct.
    let verified = pr_with_body("Review VERIFIED by reviewer-codex");
    assert!(
        has_review_verdict(&verified),
        "daemon-retention: a genuine VERIFIED verdict must still pass the gate"
    );
}
