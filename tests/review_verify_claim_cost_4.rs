//! Static-invariant repro (verify-claim-cost batch), finding #4.
//!
//! The module header (`src/token_cost.rs` lines 16-18) and the Codex section
//! header (lines ~550-553) both claim `total_token_usage` is session-cumulative
//! 'so the MAX per file is taken, never summed'. The actual implementation
//! (`parse_codex_rows`) emits one Row per `token_count` line using the per-turn
//! `last_token_usage` DELTA and SUMS them (asserted by
//! `codex_delta_sum_equals_cumulative`). The stale comment will mislead a
//! maintainer reasoning about reconciliation.
//!
//! This source-scanning test reads `src/token_cost.rs` as text (the module is
//! binary-internal, but no crate access is needed) and asserts the stale
//! "MAX per file" phrasing is gone. RED now (present), green after the comments
//! are rewritten to describe the delta-sum design.

#![allow(clippy::expect_used)]

use std::path::PathBuf;

#[test]
#[ignore = "verify-claim-cost stale-max-per-file-comment: red until fix; remove #[ignore] after fix to confirm"]
fn token_cost_comment_does_not_claim_max_per_file_verify_claim_cost() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("token_cost.rs");
    let text = std::fs::read_to_string(&path).expect("read src/token_cost.rs");

    // Stale phrasings describing the ABANDONED MAX strategy. The real code sums
    // per-turn deltas, so these must not survive.
    let stale_phrases = ["MAX per file", "is taken, never summed"];

    let mut hits = Vec::new();
    for (i, line) in text.lines().enumerate() {
        for phrase in &stale_phrases {
            if line.contains(phrase) {
                hits.push(format!("{}:{}: {}", path.display(), i + 1, line.trim()));
            }
        }
    }

    assert!(
        hits.is_empty(),
        "stale comment claims a MAX-per-file strategy, but parse_codex_rows sums \
         per-turn `last_token_usage` deltas (Sum(delta) == final cumulative). \
         Update the comments to match the delta-sum implementation:\n{}",
        hits.join("\n")
    );
}
