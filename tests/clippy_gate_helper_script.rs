//! Sprint 58 Wave 3 PR-1 (#12) — invariants for the cross-platform
//! clippy gate helper script + lint-discipline doc.
//!
//! Pins the structural contract of `scripts/clippy-all-platforms.sh`
//! and `docs/LINT-DISCIPLINE.md` so future edits don't accidentally
//! break the operator-runnable form (Shape c, passive Q3-aligned).

use std::path::PathBuf;
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn script_path() -> PathBuf {
    repo_root().join("scripts").join("clippy-all-platforms.sh")
}

fn doc_path() -> PathBuf {
    repo_root().join("docs").join("LINT-DISCIPLINE.md")
}

#[test]
fn script_exists_and_is_executable() {
    let p = script_path();
    assert!(p.exists(), "scripts/clippy-all-platforms.sh must exist");
    let meta = std::fs::metadata(&p).expect("metadata");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        assert!(
            mode & 0o111 != 0,
            "script must be executable (mode={:o})",
            mode
        );
    }
    #[cfg(windows)]
    {
        // Windows doesn't have unix-style execute bits; the script is
        // invoked via `bash` directly there. Verify it's a regular
        // file at minimum.
        assert!(meta.is_file(), "script must be a regular file");
    }
}

#[test]
fn script_starts_with_bash_shebang() {
    let content = std::fs::read_to_string(script_path()).expect("read");
    assert!(
        content.starts_with("#!/usr/bin/env bash"),
        "script must start with portable bash shebang"
    );
}

#[test]
fn script_uses_strict_mode() {
    let content = std::fs::read_to_string(script_path()).expect("read");
    assert!(
        content.contains("set -euo pipefail"),
        "script must enable strict mode (set -euo pipefail) — protects against silent failures"
    );
}

#[test]
fn script_defines_all_three_ci_matrix_platforms() {
    // The CI matrix runs ubuntu-latest, macos-latest, windows-latest
    // (see .github/workflows/ci.yml). The helper script must cover
    // all three so local pre-push verification matches the CI surface.
    let content = std::fs::read_to_string(script_path()).expect("read");
    let required_targets = [
        "x86_64-unknown-linux-gnu",
        "x86_64-pc-windows-gnu",
        "x86_64-apple-darwin",
    ];
    for target in &required_targets {
        assert!(
            content.contains(target),
            "script must reference target `{}` (CI matrix coverage)",
            target
        );
    }
}

#[test]
fn script_uses_strict_clippy_invocation() {
    // Pin: the script must invoke clippy with `-D warnings` + `--features tray`
    // to match CI's strictness. Check the ACTUAL invocation — the
    // `DEFAULT_CLIPPY_ARGS` array passed to `cargo clippy` — not a whole-file
    // grep: the script's header comment
    // (`#  cargo clippy --all-targets --features tray -- -D warnings`) already
    // contains every substring, so the old whole-file check would stay green
    // even if the real command were weakened.
    let content = std::fs::read_to_string(script_path()).expect("read");
    let args_block = content
        .split_once("DEFAULT_CLIPPY_ARGS=(")
        .and_then(|(_, rest)| rest.split_once(')'))
        .map(|(block, _)| block)
        .expect("script must define a DEFAULT_CLIPPY_ARGS=( ... ) array");
    assert!(
        args_block.contains("-D") && args_block.contains("warnings"),
        "DEFAULT_CLIPPY_ARGS must run clippy with -D warnings (matches CI strictness): {args_block}"
    );
    assert!(
        args_block.contains("--features") && args_block.contains("tray"),
        "DEFAULT_CLIPPY_ARGS must include the `tray` feature: {args_block}"
    );
    assert!(
        content.contains("cargo clippy") && content.contains("${DEFAULT_CLIPPY_ARGS[@]}"),
        "the script must actually pass DEFAULT_CLIPPY_ARGS to `cargo clippy`"
    );
}

#[test]
fn script_help_flag_exits_zero_with_usage() {
    if !is_bash_available() {
        eprintln!("bash unavailable — skipping helper-script invocation test");
        return;
    }
    let out = Command::new("bash")
        .arg(script_path())
        .arg("--help")
        .output()
        .expect("run --help");
    assert!(
        out.status.success(),
        "--help must exit 0; got {:?}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Usage:"),
        "--help output must contain Usage section; got:\n{stdout}"
    );
}

#[test]
fn script_unknown_arg_exits_two_with_stderr() {
    if !is_bash_available() {
        eprintln!("bash unavailable — skipping helper-script invocation test");
        return;
    }
    let out = Command::new("bash")
        .arg(script_path())
        .arg("--definitely-not-a-real-flag")
        .output()
        .expect("run unknown arg");
    let code = out.status.code().unwrap_or(-1);
    assert_eq!(
        code, 2,
        "unknown arg must exit 2 (invocation error); got {}",
        code
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unknown arg"),
        "unknown arg stderr must mention the issue; got: {stderr}"
    );
}

// ─────────────────────────────────────────────────────────────
// docs/LINT-DISCIPLINE.md invariants
// ─────────────────────────────────────────────────────────────

#[test]
fn doc_exists() {
    assert!(
        doc_path().exists(),
        "docs/LINT-DISCIPLINE.md must exist (companion to the helper script)"
    );
}

#[test]
fn doc_references_the_helper_script() {
    let content = std::fs::read_to_string(doc_path()).expect("read");
    assert!(
        content.contains("scripts/clippy-all-platforms.sh"),
        "doc must reference the script — they ship as a pair"
    );
}

#[test]
fn doc_covers_all_seven_recurring_patterns() {
    // The doc is the operator's reference for *why* the script is
    // useful — it captures the patterns from Sprint 56–57 fix-forward
    // cycles. Pin the surface so future edits don't drop coverage.
    let content = std::fs::read_to_string(doc_path()).expect("read");
    let patterns = [
        ("dead_code", "Pattern 1 cfg-gated dead_code"),
        ("fire-and-forget", "Pattern 2 spawn-rationale"),
        ("escap", "Pattern 3 format-aware shell escaping"), // matches "escape" or "escaping"
        ("EXE_SUFFIX", "Pattern 4 Windows .exe handling"),
        ("mtime", "Pattern 5 mtime cross-platform"),
        ("canonicalize", "Pattern 6 path-separator + canonical-path"),
        ("sleep", "Pattern 7 timing-sensitive cross-platform tests"),
    ];
    for (needle, label) in &patterns {
        assert!(
            content.contains(needle),
            "doc must cover {} (looking for `{}`)",
            label,
            needle
        );
    }
}

#[test]
fn doc_documents_workflow_with_script_step() {
    let content = std::fs::read_to_string(doc_path()).expect("read");
    // Pin the operator workflow: the doc must show a numbered checklist
    // that includes the script as one of the steps.
    let workflow_section = content
        .split("workflow")
        .nth(1)
        .or_else(|| content.split("Workflow").nth(1))
        .unwrap_or("");
    assert!(
        workflow_section.contains("scripts/clippy-all-platforms.sh"),
        "operator workflow must reference the script as a step; doc workflow section:\n{workflow_section}"
    );
}

#[test]
fn doc_documents_script_limitations() {
    let content = std::fs::read_to_string(doc_path()).expect("read");
    // The script can't catch link-time / runtime issues — the doc must
    // be honest about that boundary so operators know when CI matrix
    // is still required.
    assert!(
        content.contains("link") || content.contains("Link"),
        "doc must document link-level limitations (the script is lint-only)"
    );
    assert!(
        content.contains("CI matrix") || content.contains("CI"),
        "doc must explain CI matrix is still required for full coverage"
    );
}

// ─────────────────────────────────────────────────────────────
// Helpers.
// ─────────────────────────────────────────────────────────────

fn is_bash_available() -> bool {
    Command::new("bash")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
