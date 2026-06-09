//! Drift-guard for the daemon-boot flake gate (#t-teardown-determinism).
//!
//! Every test binary that boots a REAL daemon via `AgendHarness::spawn*` shares the
//! daemon-boot path whose race (#1909) bit #1914. Those tests are repeated 20x by
//! `.github/workflows/daemon-boot-flake-gate.yml` to catch boot-race flakes
//! pre-merge — but only if they're in `tests/daemon_boot_gate_filter.txt`. A new
//! `AgendHarness` test added WITHOUT being listed there would silently escape the
//! gate — the #1907/#1911 curated-list-drift class.
//!
//! This invariant fails (RED) until every `AgendHarness`-using test binary is in the
//! filter file, forcing the author to add it. (Direct-boot tests — `start
//! --foreground` without AgendHarness — are listed manually in the file; they are
//! not auto-detectable here, so this invariant covers the AgendHarness class, which
//! is the one that grows.)

use std::collections::BTreeSet;
use std::path::Path;

const TESTS_DIR: &str = "tests";
const FILTER_FILE: &str = "tests/daemon_boot_gate_filter.txt";

/// Binary names listed in the gate filter file (comments/blanks stripped).
fn gate_filter_binaries() -> BTreeSet<String> {
    let txt =
        std::fs::read_to_string(FILTER_FILE).unwrap_or_else(|e| panic!("read {FILTER_FILE}: {e}"));
    txt.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

/// Test binaries (tests/<name>.rs stems) that call `AgendHarness::spawn*`. Note
/// `read_dir(TESTS_DIR)` is non-recursive, so the harness DEFINITION in
/// `tests/common/harness.rs` is not scanned — only top-level integration tests that
/// USE it. `"AgendHarness::spawn"` is a prefix of `spawn`/`spawn_with`, so both match.
fn agendharness_user_binaries() -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for entry in std::fs::read_dir(TESTS_DIR)
        .expect("read tests/ dir")
        .flatten()
    {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        // Skip THIS invariant file — its scan code contains the literal
        // "AgendHarness::spawn", which would otherwise self-match (it does not
        // boot a daemon). Mirrors the `is_self` skip in git_subprocess_invariant.
        if path.file_stem().and_then(|s| s.to_str()) == Some("daemon_boot_gate_invariant") {
            continue;
        }
        if std::fs::read_to_string(&path)
            .map(|c| c.contains("AgendHarness::spawn"))
            .unwrap_or(false)
        {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                out.insert(stem.to_string());
            }
        }
    }
    out
}

#[test]
fn agendharness_users_are_in_the_flake_gate_filter() {
    let gated = gate_filter_binaries();
    let users = agendharness_user_binaries();
    assert!(
        !users.is_empty(),
        "sanity: expected to find AgendHarness-using test binaries; found none — \
         did the scan path or the harness API change?"
    );

    let missing: Vec<&String> = users.iter().filter(|b| !gated.contains(*b)).collect();
    assert!(
        missing.is_empty(),
        "daemon-boot flake-gate DRIFT — these AgendHarness-using test binaries are NOT in \
         {FILTER_FILE}, so they'd escape the 20x flake gate. Add each to the file:\n  {missing:#?}\n\
         (curated-list-drift guard — #1907/#1911 class.)"
    );

    // Stale-entry guard: every listed binary must be a real test file.
    for b in &gated {
        assert!(
            Path::new(TESTS_DIR).join(format!("{b}.rs")).exists(),
            "{FILTER_FILE} lists '{b}' but {TESTS_DIR}/{b}.rs does not exist (stale/typo entry)"
        );
    }
}
