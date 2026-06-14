//! Worktree opt-out integration tests.
//!
//! §3.5.11 RED: these tests verify fleet.yaml `worktree: false` is parsed
//! correctly by the InstanceConfig schema.

#![allow(clippy::unwrap_used)]

/// Minimal InstanceConfig mirror for testing fleet.yaml parsing.
/// Must match src/fleet.rs::InstanceConfig's worktree field.
#[derive(serde::Deserialize, Debug)]
struct TestInstanceConfig {
    #[allow(dead_code)]
    backend: Option<String>,
    #[serde(default)]
    worktree: Option<bool>,
}

#[derive(serde::Deserialize, Debug)]
struct TestFleetConfig {
    #[serde(default)]
    instances: std::collections::HashMap<String, TestInstanceConfig>,
}

#[test]
fn fleet_yaml_worktree_false_parsed() {
    let yaml = r#"
instances:
  dev-lead:
    backend: claude
    worktree: false
  dev-impl:
    backend: claude
"#;
    let config: TestFleetConfig = serde_yaml_ng::from_str(yaml).unwrap();
    let lead = config.instances.get("dev-lead").unwrap();
    assert_eq!(
        lead.worktree,
        Some(false),
        "worktree: false must parse as Some(false)"
    );
    let impl_inst = config.instances.get("dev-impl").unwrap();
    assert_eq!(
        impl_inst.worktree, None,
        "absent worktree must parse as None (default true)"
    );
}

#[test]
fn fleet_yaml_worktree_true_explicit() {
    let yaml = r#"
instances:
  worker:
    backend: claude
    worktree: true
"#;
    let config: TestFleetConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.instances.get("worker").unwrap().worktree, Some(true));
}

// --- Auto-prune pre-flight check tests (worktree.rs functions) ---
// These test the git-level functions directly since they're in the binary.
// The actual has_uncommitted_changes / remove_worktree are tested via
// the fleet.rs unit tests in the binary crate.

/// Verify git status --porcelain detects uncommitted files.
#[test]
fn git_status_porcelain_detects_dirty() {
    let dir = std::env::temp_dir().join(format!("agend-wt-dirty-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // allow: raw-git-subprocess pre-#821 fixture; properly pins AGEND_GIT_BYPASS+current_dir
    std::process::Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .args(["init", "-b", "main"])
        .current_dir(&dir)
        .output()
        .unwrap();
    // allow: raw-git-subprocess pre-#821 fixture; properly pins AGEND_GIT_BYPASS+current_dir
    std::process::Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .args(["commit", "--allow-empty", "-m", "init"])
        .current_dir(&dir)
        .output()
        .unwrap();
    std::fs::write(dir.join("dirty.txt"), "uncommitted").unwrap();

    // allow: raw-git-subprocess pre-#821 fixture; properly pins AGEND_GIT_BYPASS+current_dir
    let output = std::process::Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .args(["status", "--porcelain"])
        .current_dir(&dir)
        .output()
        .unwrap();
    assert!(
        !output.stdout.is_empty(),
        "git status --porcelain must detect uncommitted file"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Verify clean repo has empty git status --porcelain.
#[test]
fn git_status_porcelain_clean() {
    let dir = std::env::temp_dir().join(format!("agend-wt-clean-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // allow: raw-git-subprocess pre-#821 fixture; properly pins AGEND_GIT_BYPASS+current_dir
    std::process::Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .args(["init", "-b", "main"])
        .current_dir(&dir)
        .output()
        .unwrap();
    // allow: raw-git-subprocess pre-#821 fixture; properly pins AGEND_GIT_BYPASS+current_dir
    std::process::Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .args(["commit", "--allow-empty", "-m", "init"])
        .current_dir(&dir)
        .output()
        .unwrap();

    // allow: raw-git-subprocess pre-#821 fixture; properly pins AGEND_GIT_BYPASS+current_dir
    let output = std::process::Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .args(["status", "--porcelain"])
        .current_dir(&dir)
        .output()
        .unwrap();
    assert!(
        output.stdout.is_empty(),
        "clean repo must have empty git status --porcelain"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// --- E2E auto-prune: covered in-crate, not reachable here ---
//
// The flip `true→false` auto-prune behavior lives in the BIN crate
// (`src/bootstrap/agent_resolve.rs::resolve_one`, which calls
// `worktree::has_uncommitted_changes` + `worktree::remove_worktree`) and is
// NOT reachable from this integration-test crate. The former
// `e2e_clean_worktree_flip_prunes` / `e2e_dirty_worktree_flip_rejected` tests
// here never flipped config or invoked any prune path — each only asserted
// that a dir it had just created still existed (`wt_dir.exists()`), which is
// trivially true and could never catch a prune regression (the names promised
// behavior the bodies did not exercise). The genuine behavior is covered by
// in-crate unit tests:
//   - agent_resolve::tests::resolve_one_worktree_false_prunes_clean_existing_worktree
//   - agent_resolve::tests::resolve_one_worktree_false_skips_worktree_creation
//   - the dirty-reject path via the `has_uncommitted_changes` guard there.
// The fake E2E tests were removed rather than left as false confidence.
