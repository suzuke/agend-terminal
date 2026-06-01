//! State pattern coverage test — verifies real PTY capture fixtures
//! exist, are non-empty, and the MANIFEST references them all.
//!
//! Sprint 26 PR-A: bundled with parking_lot migration per operator dispatch.
//! Uses existing `tests/fixtures/state-replay/*.raw` (13 real PTY captures).

use std::path::Path;

const EXPECTED_FIXTURES: &[&str] = &[
    "claude-thinking.raw",
    "claude-tooluse.raw",
    "claude-perm.raw",
    "codex-thinking.raw",
    "codex-tooluse.raw",
    "codex-update.raw",
    "codex-perm.raw",
    "gemini-thinking.raw",
    "gemini-tooluse.raw",
    "kiro-thinking.raw",
    "kiro-tooluse.raw",
    "opencode-thinking.raw",
    "opencode-tooluse.raw",
    // #1559 — cross-backend permission-dialog chrome captures (operator
    // recorded 2026-06-01). Replayed by `replay_manifest_regression`.
    "kiro-perm.raw",
    "gemini-perm.raw",
    "opencode-perm.raw",
    // #848 PR-A — classifier root cause fixtures (synthetic from Anthropic
    // docs verbatim strings). The behavior assertion that each fixture
    // produces the right AgentState lives in `src/state.rs::mod tests`
    // (binary-crate-internal types — Sprint 26 PR-A pattern, see
    // `tests/behavioral_shadow.rs` module doc for the lib/bin split).
    "claude-rate-limit-429.raw",
    "claude-server-throttle.raw",
    "claude-overloaded-529.raw",
    "claude-session-limit.raw",
    "claude-discussion-text.raw",
    // #848 PR-B — OpenCode + Gemini + Kiro classifier fixtures (same
    // synthetic-from-docs provenance, same in-process replay coverage
    // via `replay_manifest_regression`).
    "opencode-rate-limit-typical.raw",
    "opencode-usage-limit-typical.raw",
    "opencode-discussion-text.raw",
    "gemini-rate-limit-typical.raw",
    "gemini-discussion-text.raw",
    "gemini-rate-limit-canonical-429.raw",
    "gemini-rate-limit-prose-discussion.raw",
    "kiro-rate-limit-typical.raw",
    "kiro-usage-limit-typical.raw",
    "kiro-discussion-text.raw",
];

#[test]
fn all_fixtures_present_and_nonempty() {
    let dir = Path::new("tests/fixtures/state-replay");
    assert!(dir.exists(), "fixture directory must exist");
    for file in EXPECTED_FIXTURES {
        let path = dir.join(file);
        assert!(path.exists(), "fixture {file} must exist");
        let size = std::fs::metadata(&path).expect("metadata").len();
        assert!(
            size > 100,
            "fixture {file} must be non-trivial ({size} bytes)"
        );
    }
}

#[test]
fn manifest_references_all_fixtures() {
    let manifest = std::fs::read_to_string("tests/fixtures/state-replay/MANIFEST.yaml")
        .expect("read MANIFEST.yaml");
    for file in EXPECTED_FIXTURES {
        assert!(
            manifest.contains(file),
            "MANIFEST.yaml must reference {file}"
        );
    }
}

#[test]
fn fixtures_contain_ansi_escape_sequences() {
    let dir = Path::new("tests/fixtures/state-replay");
    for file in EXPECTED_FIXTURES {
        let data = std::fs::read(dir.join(file)).expect("read fixture");
        let has_esc = data.windows(2).any(|w| w[0] == 0x1b && w[1] == b'[');
        assert!(
            has_esc,
            "fixture {file} must contain ANSI CSI sequences (real PTY capture)"
        );
    }
}
