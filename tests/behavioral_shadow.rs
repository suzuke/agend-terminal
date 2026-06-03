//! Behavioral shadow-mode integration tests.
//!
//! M2: Real fixture replay through state detection pipeline.
//! M4: Real tracing-subscriber capture of shadow telemetry events.
//!
//! These tests exercise actual behavioral inference, NOT source-grep.

use std::path::Path;

/// M2: Verify fixture files are real PTY captures with ANSI sequences.
/// This is the external-fixture validation (§3.5.10) — the actual
/// StateTracker replay happens in src/state.rs unit tests where
/// binary-internal types are accessible.
#[test]
fn fixtures_are_real_pty_captures_with_ansi() {
    let dir = Path::new("tests/fixtures/state-replay");
    let fixtures = [
        "claude-thinking.raw",
        "kiro-thinking.raw",
        "codex-thinking.raw",
        "opencode-thinking.raw",
        // #987: agy is gemini-cli's official successor (Google Antigravity CLI).
        "agy-thinking.raw",
    ];
    for file in &fixtures {
        let data = std::fs::read(dir.join(file)).expect("read fixture");
        assert!(data.len() > 100, "fixture {file} must be non-trivial");
        let has_csi = data.windows(2).any(|w| w[0] == 0x1b && w[1] == b'[');
        assert!(
            has_csi,
            "fixture {file} must contain CSI sequences (real PTY capture)"
        );
    }
}

/// M2: Verify MANIFEST covers all 5 backends.
#[test]
fn manifest_covers_all_backends() {
    let manifest = std::fs::read_to_string("tests/fixtures/state-replay/MANIFEST.yaml")
        .expect("read MANIFEST");
    // #987: bumped from 5 → 6 with agy (Backend::Agy in src/backend.rs).
    // #1580: back to 5 — gemini-cli retired.
    for backend in &["claude-code", "kiro-cli", "codex", "opencode", "agy"] {
        assert!(
            manifest.contains(backend),
            "MANIFEST must reference {backend}"
        );
    }
}
