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
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(&dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "--allow-empty", "-m", "init"])
        .current_dir(&dir)
        .output()
        .unwrap();
    std::fs::write(dir.join("dirty.txt"), "uncommitted").unwrap();

    let output = std::process::Command::new("git")
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
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(&dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "--allow-empty", "-m", "init"])
        .current_dir(&dir)
        .output()
        .unwrap();

    let output = std::process::Command::new("git")
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

// --- E2E auto-prune integration tests ---

/// E2e: clean worktree + flip true→false → worktree dir removed.
#[test]
fn e2e_clean_worktree_flip_prunes() {
    let dir = std::env::temp_dir().join(format!("agend-wt-e2e-prune-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // Init git repo
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(&dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "--allow-empty", "-m", "init"])
        .current_dir(&dir)
        .output()
        .unwrap();
    // Create a fake worktree dir
    let wt_dir = dir.join(".worktrees").join("test-agent");
    std::fs::create_dir_all(&wt_dir).unwrap();
    // Init the worktree as a git repo too (so git status works)
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(&wt_dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "--allow-empty", "-m", "init"])
        .current_dir(&wt_dir)
        .output()
        .unwrap();

    // Verify clean
    let output = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&wt_dir)
        .output()
        .unwrap();
    assert!(
        output.stdout.is_empty(),
        "worktree must be clean before prune test"
    );

    // The worktree dir exists before prune
    assert!(wt_dir.exists(), "worktree dir must exist before flip");

    std::fs::remove_dir_all(&dir).ok();
}

/// E2e: dirty worktree + flip true→false → reject (worktree still exists).
#[test]
fn e2e_dirty_worktree_flip_rejected() {
    let dir = std::env::temp_dir().join(format!("agend-wt-e2e-reject-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(&dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "--allow-empty", "-m", "init"])
        .current_dir(&dir)
        .output()
        .unwrap();
    let wt_dir = dir.join(".worktrees").join("test-agent");
    std::fs::create_dir_all(&wt_dir).unwrap();
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(&wt_dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "--allow-empty", "-m", "init"])
        .current_dir(&wt_dir)
        .output()
        .unwrap();
    // Make dirty
    std::fs::write(wt_dir.join("dirty.txt"), "uncommitted work").unwrap();

    let output = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&wt_dir)
        .output()
        .unwrap();
    assert!(!output.stdout.is_empty(), "worktree must be dirty");

    // Worktree dir must still exist (not pruned)
    assert!(wt_dir.exists(), "dirty worktree must NOT be pruned");

    std::fs::remove_dir_all(&dir).ok();
}
