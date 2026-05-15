#![allow(clippy::unwrap_used, clippy::expect_used)]

//! #821 — git subprocess isolation invariant. Test files under `tests/`
//! that shell out to `git` via raw `Command::new("git")` MUST route
//! through `tests/common/git_isolated` to avoid the trap class
//! exposed during #820 prep: the `agend-git` shim routes
//! daemon-process git operations to the bound branch when
//! `AGEND_GIT_BYPASS` is unset, leaking test ops into the host
//! worktree's `.git`.
//!
//! Sister invariant to `tests/test_isolation_invariant.rs` (Sprint 31
//! P0, which enforces `AGEND_TEST_ISOLATION=1` for binary-spawning
//! tests). Different bug class — different env var — different scope.
//!
//! ## What it checks
//!
//! For every `.rs` file under `tests/`, grep for the literal
//! `Command::new("git")`. Each hit must be either:
//!
//! - In the canonical helper module `tests/common/git_isolated.rs`
//!   (exempt — the helper IS the canonical authority).
//! - In this invariant file itself (the lint's own probe fixtures
//!   naturally contain the offending pattern as test data).
//! - On a line carrying `// allow: raw-git-subprocess <rationale>`
//!   (or in a contiguous comment block immediately above).
//!
//! ## Allowlist mechanism
//!
//! Mirrors `tests/anti_pattern_invariant.rs`: line-or-above
//! `// allow: raw-git-subprocess` comment exempts the violation.
//! Always include a rationale next to the marker.
//!
//! ## Grandfathered files (initial v1 allowlist)
//!
//! Pre-#821 test files using raw `Command::new("git")` are
//! grandfathered via per-site `// allow:` comments added when this
//! invariant lands. Follow-up PRs can migrate them to
//! `git_isolated::git()` incrementally.

use std::path::{Path, PathBuf};

const TESTS_DIR: &str = "tests";
const ALLOW_MARKER: &str = "allow: raw-git-subprocess";
const PATTERN: &str = "Command::new(\"git\")";

/// Walk every `.rs` file under `tests/`. Mirrors the helpers used by
/// `tests/anti_pattern_invariant.rs` and `tests/cargo_include_invariant.rs`.
fn rs_files_under(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !root.exists() {
        return out;
    }
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(p);
            }
        }
    }
    walk(root, &mut out);
    out
}

/// True iff this file is exempt from the lint:
/// - The lint itself (`git_subprocess_invariant.rs`) — its probe
///   fixtures naturally contain the offending pattern as test data.
/// - The helper module `tests/common/git_isolated.rs` — the
///   canonical authority is allowed to use the raw subprocess.
fn is_exempt_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if name == "git_subprocess_invariant.rs" {
        return true;
    }
    let parent = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("");
    parent == "common" && name == "git_isolated.rs"
}

/// True iff the violation line (or any contiguous `//` comment line
/// immediately preceding it) carries the allow-marker. Mirrors
/// `anti_pattern_invariant::has_allowlist_marker` semantics.
fn has_allow_marker(lines: &[&str], line_idx: usize) -> bool {
    if lines
        .get(line_idx)
        .map(|l| l.contains(ALLOW_MARKER))
        .unwrap_or(false)
    {
        return true;
    }
    let mut cursor = line_idx;
    while let Some(prev_idx) = cursor.checked_sub(1) {
        let Some(prev_line) = lines.get(prev_idx) else {
            break;
        };
        let trimmed = prev_line.trim_start();
        if !trimmed.starts_with("//") {
            break;
        }
        if trimmed.contains(ALLOW_MARKER) {
            return true;
        }
        cursor = prev_idx;
    }
    false
}

/// One violation entry — `(file, line_number, snippet)`.
#[derive(Debug, PartialEq, Eq)]
pub struct Violation {
    pub file: PathBuf,
    pub line: usize,
    pub snippet: String,
}

/// Scan a directory of test files for raw `Command::new("git")`
/// invocations missing the allow-marker. Returns the violation list
/// (empty when clean). Public-for-tests so the RED/GREEN probes can
/// call the SAME scanner the production invariant calls — no mock
/// branches.
pub fn scan_test_violations(tests_dir: &Path) -> Vec<Violation> {
    let mut violations = Vec::new();
    for file in rs_files_under(tests_dir) {
        if is_exempt_file(&file) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&file) else {
            continue;
        };
        let lines: Vec<&str> = content.lines().collect();
        for (idx, line) in lines.iter().enumerate() {
            if !line.contains(PATTERN) {
                continue;
            }
            if has_allow_marker(&lines, idx) {
                continue;
            }
            violations.push(Violation {
                file: file.clone(),
                line: idx + 1,
                snippet: (*line).to_string(),
            });
        }
    }
    violations
}

#[test]
fn invariant_passes_on_repo_test_files() {
    // Load-bearing test: actual `tests/` dir of this repo MUST pass
    // once the grandfather allowlist is applied in C3. Pre-C2 this
    // passes trivially (stub returns empty); post-C2+C3 locks the
    // contract.
    let violations = scan_test_violations(Path::new(TESTS_DIR));
    if !violations.is_empty() {
        let summary: Vec<String> = violations
            .iter()
            .map(|v| format!("  {}:{}: {}", v.file.display(), v.line, v.snippet.trim()))
            .collect();
        panic!(
            "#821 invariant FAILED — {} raw `Command::new(\"git\")` site(s) in tests/ \
             missing the `// {}` allow-marker. Either migrate to \
             `tests/common/git_isolated` OR add the marker:\n\n{}",
            violations.len(),
            ALLOW_MARKER,
            summary.join("\n"),
        );
    }
}

#[test]
fn invariant_detects_raw_git_subprocess_in_synthetic_file() {
    // C1 RED test: synthesize a temp `tests/`-style directory with a
    // single .rs file containing a raw `Command::new("git")` call.
    // The scanner must flag it as a violation post-C2.
    let temp = synth_tests_dir("raw_git_red");
    let file = temp.join("offender.rs");
    std::fs::write(
        &file,
        "use std::process::Command;\n\
         fn naughty() {\n    \
             let _ = Command::new(\"git\").args([\"status\"]).output();\n\
         }\n",
    )
    .unwrap();
    let violations = scan_test_violations(&temp);
    assert!(
        !violations.is_empty(),
        "scanner must flag raw Command::new(\"git\") without allow-marker"
    );
    assert!(
        violations.iter().any(|v| v.file.ends_with("offender.rs")),
        "violation list must name offender.rs, got: {violations:?}"
    );
    std::fs::remove_dir_all(&temp).ok();
}

#[test]
fn invariant_passes_when_allow_marker_present() {
    // C2 GREEN: synthesize a file with the allow-marker on the
    // offending line → scanner returns empty violations.
    let temp = synth_tests_dir("allow_marker_green");
    let file = temp.join("clean.rs");
    std::fs::write(
        &file,
        "use std::process::Command;\n\
         fn justified() {\n    \
             // allow: raw-git-subprocess legacy fixture grandfathered per #821\n    \
             let _ = Command::new(\"git\").args([\"status\"]).output();\n\
         }\n",
    )
    .unwrap();
    let violations = scan_test_violations(&temp);
    assert!(
        violations.is_empty(),
        "allow-marker must suppress violation, got: {violations:?}"
    );
    std::fs::remove_dir_all(&temp).ok();
}

#[test]
fn invariant_passes_when_helper_module_used() {
    // C2 GREEN: synthesize a file using `git_isolated::git(...)` →
    // no raw `Command::new("git")` literal → no violation. Pins the
    // intent so a future scanner refactor that probes other patterns
    // doesn't accidentally re-flag helper-routed callers.
    let temp = synth_tests_dir("helper_green");
    let file = temp.join("clean_helper.rs");
    std::fs::write(
        &file,
        "use crate::common::git_isolated;\n\
         fn clean() {\n    \
             let dir = git_isolated::setup_temp_repo(\"t\");\n    \
             git_isolated::git(&dir, &[\"status\"]);\n\
         }\n",
    )
    .unwrap();
    let violations = scan_test_violations(&temp);
    assert!(
        violations.is_empty(),
        "helper-routed callers must surface zero violations, got: {violations:?}"
    );
    std::fs::remove_dir_all(&temp).ok();
}

/// Synthesize a temporary tests-like dir scoped by tag. Used by
/// RED/GREEN tests to avoid touching the real `tests/` dir.
fn synth_tests_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "agend-821-{}-{}-{tag}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

// Suppress unused-fn warnings for helpers reached only at runtime
// (stub doesn't call them; real impl in C2 will).
#[allow(dead_code)]
fn _unused_lints_silencer() {
    let _ = (rs_files_under, has_allow_marker, is_exempt_file, PATTERN);
}
