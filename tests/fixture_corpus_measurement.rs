//! Fixture corpus measurement harness — top-level integration entry point.
//!
//! Sub-task 5 of `#685` (decision `d-20260514015214320625-1`). This file
//! validates the F685 fixture corpus manifest extension and the byte-level
//! integrity of the new fixtures. The actual `StateTracker` + `check_hang`
//! measurement logic lives in `src/state.rs::tests::corpus_measurement`
//! because it requires access to binary-crate-internal types (lib surface
//! is intentionally minimal — see `src/lib.rs` doc).
//!
//! Pipeline split rationale:
//! - **This file** (integration test): manifest YAML schema validation,
//!   fixture byte-existence check, per-scenario-kind counts, schema_version
//!   tally. No internal API calls.
//! - **`src/state.rs::tests::corpus_measurement`** (unit test): replay
//!   each measurement-labeled fixture through `StateTracker::feed` and
//!   `HealthTracker::check_hang`, classify against
//!   `expected_hung_classification`, aggregate FP/FN counts.
//!
//! See `docs/F685-FIXTURE-CORPUS.md` §F685-CORPUS.4 for measurement
//! methodology and §F685-CORPUS.6 for corpus growth protocol.

// Only the env_gate helper is needed here — avoid pulling the full
// `tests/common/mod.rs` tree (which exposes `harness`) so cargo's
// per-test-binary dead-code lint doesn't flag the unused harness fns.
#[path = "common/env_gate.rs"]
mod env_gate;

use env_gate::with_f9_gate;
use std::path::Path;

/// Minimal manifest subset for schema validation. Mirrors the
/// `ReplayFixture` struct in `src/state.rs::tests` (cannot be re-used
/// directly because that struct lives behind `#[cfg(test)]`). When the
/// upstream struct evolves, this mirror must stay in lock-step OR the
/// schema-validation portion below must be widened.
#[derive(serde::Deserialize)]
struct ManifestSubset {
    fixtures: Vec<FixtureSubset>,
}

#[derive(serde::Deserialize)]
#[allow(dead_code)]
struct FixtureSubset {
    file: String,
    backend: String,
    #[serde(default)]
    scenario_kind: Option<String>,
    #[serde(default)]
    expected_hung_classification: Option<String>,
    #[serde(default)]
    capture_kind: Option<String>,
    #[serde(default = "default_schema_version")]
    schema_version: u32,
}

fn default_schema_version() -> u32 {
    1
}

const FIXTURES_DIR: &str = "tests/fixtures/state-replay";

fn load_manifest() -> ManifestSubset {
    let path = Path::new(FIXTURES_DIR).join("MANIFEST.yaml");
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read MANIFEST.yaml: {e}"));
    serde_yaml_ng::from_str(&raw).unwrap_or_else(|e| panic!("parse MANIFEST.yaml: {e}"))
}

#[test]
fn manifest_parses_with_v2_extension_fields() {
    let manifest = load_manifest();
    assert!(
        !manifest.fixtures.is_empty(),
        "MANIFEST.yaml must list fixtures"
    );
    // Schema-v2 fixtures must be present; existing schema-v1 ones must remain.
    let v2 = manifest
        .fixtures
        .iter()
        .filter(|f| f.schema_version >= 2)
        .count();
    let v1 = manifest
        .fixtures
        .iter()
        .filter(|f| f.schema_version == 1)
        .count();
    assert!(v2 >= 3, "expected at least 3 schema-v2 fixtures, got {v2}");
    assert!(
        v1 >= 10,
        "expected ≥10 schema-v1 (legacy) fixtures (backward-compat baseline), got {v1}"
    );
}

#[test]
fn schema_v2_fixtures_have_measurement_labels() {
    let manifest = load_manifest();
    for f in manifest.fixtures.iter().filter(|f| f.schema_version >= 2) {
        assert!(
            f.scenario_kind.is_some(),
            "v2 fixture {} must have scenario_kind",
            f.file
        );
        assert!(
            f.expected_hung_classification.is_some(),
            "v2 fixture {} must have expected_hung_classification",
            f.file
        );
        assert!(
            f.capture_kind.is_some(),
            "v2 fixture {} must have capture_kind",
            f.file
        );
    }
}

#[test]
fn fixture_files_exist_and_nonempty() {
    let manifest = load_manifest();
    for f in &manifest.fixtures {
        let path = Path::new(FIXTURES_DIR).join(&f.file);
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read fixture {}: {e}", f.file));
        assert!(!bytes.is_empty(), "fixture {} must be non-empty", f.file);
    }
}

#[test]
fn corpus_count_report() {
    // Aggregate counts for visibility — not a pass/fail gate. Reports
    // current N per scenario_kind / capture_kind. Promotion criteria
    // (FP < 1% on N ≥ 300; FN < 10% on N ≥ 30 known-stuck) gate on
    // these counts growing over time, NOT on Phase 1 single-PR ship.
    let manifest = load_manifest();
    let v2: Vec<_> = manifest
        .fixtures
        .iter()
        .filter(|f| f.schema_version >= 2)
        .collect();
    let by_scenario = |kind: &str| -> usize {
        v2.iter()
            .filter(|f| f.scenario_kind.as_deref() == Some(kind))
            .count()
    };
    let by_capture = |kind: &str| -> usize {
        v2.iter()
            .filter(|f| f.capture_kind.as_deref() == Some(kind))
            .count()
    };
    let by_hung = |kind: &str| -> usize {
        v2.iter()
            .filter(|f| f.expected_hung_classification.as_deref() == Some(kind))
            .count()
    };
    // Tracing instead of println so test output isn't noisy by default —
    // run with `cargo test -- --nocapture` to surface. The actual report
    // shape is documented in docs/F685-FIXTURE-CORPUS.md §F685-CORPUS.4.
    eprintln!(
        "[F685 corpus report] v2_total={} | scenario: productive_marker_fire={} \
         productive_silence={} silent_stuck={} priority_oscillation={} \
         | capture: real={} synthetic={} synthetic_from_real_template={} \
         | classification: not_hung={} hung={} ambiguous={}",
        v2.len(),
        by_scenario("productive_marker_fire"),
        by_scenario("productive_silence"),
        by_scenario("silent_stuck"),
        by_scenario("priority_oscillation"),
        by_capture("real"),
        by_capture("synthetic"),
        by_capture("synthetic_from_real_template"),
        by_hung("not_hung"),
        by_hung("hung"),
        by_hung("ambiguous"),
    );
    // Sanity assertions on corpus shape — gentle gates that future
    // corpus growth should preserve. Tighten once N grows.
    //
    // #685 PR-3 (corpus expansion via dev-2 cross-audit Pushback 2):
    // bumped silent_stuck from ≥1 → ≥5 after relabelling 7 existing v1
    // `*-thinking.raw` / `*-tooluse.raw` captures as F9 silent_stuck
    // measurement fixtures. productive_marker_fire / productive_silence
    // remain at ≥1 each — real-PTY-captured markers in the recent
    // viewport scope (post-#1013 freshness fix) require operator-side
    // capture work, filed as gap follow-up.
    assert!(
        by_scenario("productive_marker_fire") >= 1,
        "corpus must include at least one productive_marker_fire fixture"
    );
    assert!(
        by_scenario("productive_silence") >= 1,
        "corpus must include at least one productive_silence fixture"
    );
    assert!(
        by_scenario("silent_stuck") >= 5,
        "corpus must include at least 5 silent_stuck fixtures (got {} — see #685 PR-3)",
        by_scenario("silent_stuck")
    );
    // Real-PTY-captured fixtures provide drift-detection beyond what
    // synthetic fixtures can — encode current backend version output
    // exactly. PR-3 raised this from 0 → 7. New real captures (gemini
    // markers, opencode markers) should keep this growing.
    assert!(
        by_capture("real") >= 5,
        "corpus must include at least 5 real-PTY-captured fixtures (got {} — see #685 PR-3)",
        by_capture("real")
    );
    // Backend coverage: relabelled corpus must touch ≥4 distinct
    // backends so per-backend marker behaviour is exercised.
    let distinct_backends: std::collections::HashSet<&str> = v2
        .iter()
        .filter_map(|f| {
            if f.scenario_kind.as_deref() == Some("silent_stuck") {
                Some(f.backend.as_str())
            } else {
                None
            }
        })
        .collect();
    assert!(
        distinct_backends.len() >= 4,
        "silent_stuck fixtures must cover ≥4 distinct backends (got {} — see #685 PR-3)",
        distinct_backends.len()
    );
}

#[test]
fn with_f9_gate_helper_round_trip() {
    // Smoke-check the extracted helper: env var set during closure,
    // unset (or restored) after. Exercises the integration-test
    // copy of with_f9_gate (`tests/common/env_gate.rs`).
    let before = std::env::var("AGEND_PRODUCTIVE_GATE").ok();
    with_f9_gate(true, || {
        assert_eq!(std::env::var("AGEND_PRODUCTIVE_GATE").as_deref(), Ok("1"));
    });
    let after = std::env::var("AGEND_PRODUCTIVE_GATE").ok();
    assert_eq!(
        before, after,
        "with_f9_gate must restore prior AGEND_PRODUCTIVE_GATE state"
    );
}
