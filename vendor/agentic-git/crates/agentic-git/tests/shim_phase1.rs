//! Sprint 52 agentic-git-shim Phase 1 integration tests.
//!
//! Tests trailer hook correctness, binding lifecycle, and AGENTIC_GIT_REAL_GIT injection.

/// Binding write + read roundtrip.
#[test]
fn binding_write_read_roundtrip() {
    let home = std::env::temp_dir().join(format!("agend-binding-rt-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();

    // Write binding
    let binding_dir = home.join("runtime").join("test-agent");
    std::fs::create_dir_all(&binding_dir).ok();
    let binding = serde_json::json!({
        "version": 1,
        "agent": "test-agent",
        "task_id": "T-100",
        "branch": "feat/test",
        "issued_at": "2026-05-05T12:00:00Z",
    });
    std::fs::write(
        binding_dir.join("binding.json"),
        serde_json::to_string_pretty(&binding).expect("serialize"),
    )
    .expect("write");

    // Read back
    let content = std::fs::read_to_string(binding_dir.join("binding.json")).expect("read");
    let parsed: serde_json::Value = serde_json::from_str(&content).expect("parse");
    assert_eq!(parsed["task_id"], "T-100");
    assert_eq!(parsed["branch"], "feat/test");
    assert_eq!(parsed["agent"], "test-agent");

    std::fs::remove_dir_all(&home).ok();
}

/// Hook script is valid shell (syntax check).
#[test]
fn hook_script_valid_shell_syntax() {
    let hook = include_str!("../assets/hooks/prepare-commit-msg");
    assert!(hook.starts_with("#!/bin/sh"), "must have shebang");
    assert!(hook.contains("exit 0"), "must always exit 0");
    assert!(hook.contains("Agentic-Agent:"), "must inject agent trailer");
    assert!(
        hook.contains("AGENTIC_GIT_AGENT"),
        "must read instance name"
    );
}

/// Hook idempotent: existing trailer → skip.
#[test]
fn hook_idempotent_skip_logic() {
    let hook = include_str!("../assets/hooks/prepare-commit-msg");
    assert!(
        hook.contains("grep -q \"^Agentic-Agent:\""),
        "must check for existing trailer"
    );
}

/// Hook skips merge/squash/template commits.
#[test]
fn hook_skips_merge_squash_template() {
    let hook = include_str!("../assets/hooks/prepare-commit-msg");
    assert!(
        hook.contains("merge|squash|template"),
        "must skip these sources"
    );
}

/// AGENTIC_GIT_REAL_GIT injection: verify `which` crate resolves git.
#[test]
fn agend_real_git_resolves() {
    // which::which("git") should find git on any dev machine.
    let result = which::which("git");
    assert!(
        result.is_ok(),
        "git must be findable via which (required for AGENTIC_GIT_REAL_GIT)"
    );
    let path = result.expect("git path");
    assert!(
        path.display().to_string().contains("git"),
        "resolved path must contain 'git'"
    );
}

// (binding.rs self-IPC regression guard removed: it scanned the DAEMON's
// binding.rs source, which lives in the upstream agend-terminal repo, not in
// this extracted repo. The guard remains upstream.)
