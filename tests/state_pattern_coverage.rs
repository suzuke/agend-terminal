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
