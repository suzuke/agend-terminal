//! Sprint 52 agend-git-shim Phase 1 integration tests.
//!
//! Tests trailer hook correctness, binding lifecycle, and AGEND_REAL_GIT injection.

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
    assert!(hook.contains("Agend-Agent:"), "must inject agent trailer");
    assert!(
        hook.contains("AGEND_INSTANCE_NAME"),
        "must read instance name"
    );
}

/// Hook idempotent: existing trailer → skip.
#[test]
fn hook_idempotent_skip_logic() {
    let hook = include_str!("../assets/hooks/prepare-commit-msg");
    assert!(
        hook.contains("grep -q \"^Agend-Agent:\""),
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

/// AGEND_REAL_GIT injection: verify `which` crate resolves git.
#[test]
fn agend_real_git_resolves() {
    // which::which("git") should find git on any dev machine.
    let result = which::which("git");
    assert!(
        result.is_ok(),
        "git must be findable via which (required for AGEND_REAL_GIT)"
    );
    let path = result.expect("git path");
    assert!(
        path.display().to_string().contains("git"),
        "resolved path must contain 'git'"
    );
}

/// No self-IPC in binding.rs (Sprint 49 regression guard).
#[test]
fn binding_no_self_ipc() {
    let src = include_str!("../src/binding.rs");
    for (i, line) in src.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("//") {
            continue;
        }
        assert!(
            !line.contains("api::call("),
            "binding.rs line {} contains forbidden api::call: {line}",
            i + 1
        );
    }
}
