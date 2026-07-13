//! Source census for #2760/task66's typed code-review ingestion boundary.
//!
//! These assertions deliberately pin the small production call graph. A future
//! semantic text/SHA/name path must not grow a second PR-effect funnel unnoticed.

use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read_source_file(path: impl AsRef<Path>) -> String {
    std::fs::read_to_string(root().join(path))
        .expect("source file")
        .replace("\r\n", "\n")
}

#[test]
fn validated_receipt_is_the_single_pr_effect_funnel_2760() {
    let messaging = read_source_file("src/api/handlers/messaging.rs");
    let pr_state = read_source_file("src/daemon/pr_state/mod.rs");
    let buffer = read_source_file("src/daemon/pr_state/verdict_buffer.rs");
    let auto_release = read_source_file("src/daemon/auto_release.rs");

    assert_eq!(
        messaging.matches("record_validated_receipt(").count(),
        1,
        "messaging must have exactly one typed PR-state ingestion call"
    );
    assert_eq!(
        pr_state.matches("record_validated_receipt(").count(),
        1,
        "pr_state must expose exactly one typed ingestion definition"
    );
    assert!(
        !messaging.contains("record_verdict("),
        "raw name+SHA verdict ingestion must never return to messaging"
    );
    assert!(
        pr_state.contains("#[cfg(test)]\n/// Pre-task66 name+SHA ingestion model")
            && pr_state.contains("pub(crate) fn record_verdict("),
        "legacy raw verdict helper must remain test-only"
    );
    assert_eq!(
        pr_state
            .matches("verdict_buffer::buffer_validated(")
            .count(),
        1,
        "only typed PR ingestion may write the typed buffer"
    );
    assert_eq!(
        buffer.matches("fn buffer_validated(").count(),
        1,
        "one typed buffer writer definition"
    );

    let predicate = auto_release
        .split("pub(crate) fn is_verdict_message")
        .nth(1)
        .and_then(|tail| tail.split("pub(crate) struct AutoReleaseTracker").next())
        .expect("auto-release predicate body");
    for forbidden in ["msg.text", "reviewed_head", "correlation_id"] {
        assert!(
            !predicate.contains(forbidden),
            "auto-release authority must not read {forbidden}"
        );
    }
    assert!(predicate.contains("validated_code_review.is_some()"));

    let bridge = messaging
        .split("fn bridge_verdict_to_review_task")
        .nth(1)
        .and_then(|tail| tail.split("// ── Orchestrator").next())
        .expect("bridge body");
    assert!(bridge.contains("msg.validated_code_review.as_ref()"));
    for forbidden in [
        "detect_verdict",
        "open_review_dispatch_for_reporter",
        "msg.reviewed_head",
    ] {
        assert!(
            !bridge.contains(forbidden),
            "review bridge must not infer authority through {forbidden}"
        );
    }
}
