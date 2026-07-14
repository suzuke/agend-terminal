//! #6 review subject/workspace decoupling — RED-first deterministic tests.
//!
//! Three categories:
//!   1. `expected_head` checkout precondition (tests 1-6)
//!   2. review_assignment bind/worktree_binding_required rejection (tests 7-8)
//!   3. `send` schema `bind` parameter exposure (test 9)

use serde_json::json;
use std::path::Path;

// ── Fixtures (reuse the ci/tests.rs pattern) ─────────────────────────

fn tmp_home(suffix: &str) -> std::path::PathBuf {
    let h = std::env::temp_dir().join(format!(
        "agend-rw-{}-{}-{}",
        std::process::id(),
        suffix,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&h).ok();
    h
}

#[cfg(unix)]
fn setup_source_repo(parent: &Path, branch: &str) -> std::path::PathBuf {
    let repo = parent.join("source-repo");
    std::fs::create_dir_all(&repo).ok();
    let bypass = ("AGEND_GIT_BYPASS", "1");
    let _ = std::process::Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(&repo)
        .env(bypass.0, bypass.1)
        .output();
    let _ = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=t@t",
            "commit",
            "--allow-empty",
            "-m",
            "init",
        ])
        .current_dir(&repo)
        .env(bypass.0, bypass.1)
        .output();
    let _ = std::process::Command::new("git")
        .args([
            "remote",
            "add",
            "origin",
            "https://github.com/owner/repo.git",
        ])
        .current_dir(&repo)
        .env(bypass.0, bypass.1)
        .output();
    let _ = std::process::Command::new("git")
        .args(["branch", branch, "main"])
        .current_dir(&repo)
        .env(bypass.0, bypass.1)
        .output();
    repo
}

#[cfg(unix)]
fn get_sha(repo: &Path, refspec: &str) -> String {
    let out = std::process::Command::new("git")
        .args(["rev-parse", refspec])
        .current_dir(repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("git rev-parse");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

// ══════════════════════════════════════════════════════════════════════
// 1. checkout expected_head — fresh branch, match
// ══════════════════════════════════════════════════════════════════════

#[test]
#[cfg(unix)]
fn checkout_expected_head_fresh_branch_match() {
    let home = tmp_home("eh-fresh-match");
    let parent = tmp_home("eh-fresh-match-src");
    let source = setup_source_repo(&parent, "feat/eh-fresh");
    let sha = get_sha(&source, "main");

    let resp = super::handle_checkout_repo(
        &home,
        &json!({
            "repository_path": source.display().to_string(),
            "branch": "feat/eh-fresh",
            "bind": true,
            "from_ref": &sha,
            "expected_head": &sha,
        }),
        "eh-agent-1",
    );

    assert!(
        resp.get("error").is_none(),
        "checkout with matching expected_head must succeed: {resp}"
    );
    assert_eq!(
        resp["expected_head"].as_str(),
        Some(sha.as_str()),
        "response must echo expected_head: {resp}"
    );
    assert_eq!(
        resp["actual_head"].as_str(),
        Some(sha.as_str()),
        "response must echo actual_head: {resp}"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&parent).ok();
}

// ══════════════════════════════════════════════════════════════════════
// 2. checkout expected_head — existing branch, mismatch
// ══════════════════════════════════════════════════════════════════════

#[test]
#[cfg(unix)]
fn checkout_expected_head_existing_branch_mismatch() {
    let home = tmp_home("eh-mismatch");
    let parent = tmp_home("eh-mismatch-src");
    let source = setup_source_repo(&parent, "feat/eh-mismatch");
    let real_sha = get_sha(&source, "feat/eh-mismatch");
    let wrong_sha = "0000000000000000000000000000000000000000";

    // First, verify the branch exists
    assert!(!real_sha.is_empty());
    assert_ne!(real_sha, wrong_sha);

    let resp = super::handle_checkout_repo(
        &home,
        &json!({
            "repository_path": source.display().to_string(),
            "branch": "feat/eh-mismatch",
            "bind": true,
            "expected_head": wrong_sha,
        }),
        "eh-agent-2",
    );

    assert!(
        resp.get("error").is_some(),
        "checkout with mismatched expected_head must error: {resp}"
    );
    assert_eq!(
        resp["code"].as_str(),
        Some("expected_head_mismatch"),
        "error code must be expected_head_mismatch: {resp}"
    );

    // Zero mutation: branch HEAD unchanged
    let after_sha = get_sha(&source, "feat/eh-mismatch");
    assert_eq!(
        real_sha, after_sha,
        "branch HEAD must be unchanged after mismatch rejection"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&parent).ok();
}

// ══════════════════════════════════════════════════════════════════════
// 3. checkout expected_head — idempotent same head
// ══════════════════════════════════════════════════════════════════════

#[test]
#[cfg(unix)]
fn checkout_expected_head_idempotent_same_head() {
    let home = tmp_home("eh-idem");
    let parent = tmp_home("eh-idem-src");
    let source = setup_source_repo(&parent, "feat/eh-idem");
    let sha = get_sha(&source, "main");

    // First checkout with expected_head
    let resp1 = super::handle_checkout_repo(
        &home,
        &json!({
            "repository_path": source.display().to_string(),
            "branch": "feat/eh-idem",
            "bind": true,
            "expected_head": &sha,
        }),
        "eh-agent-3",
    );
    assert!(
        resp1.get("error").is_none(),
        "first checkout must succeed: {resp1}"
    );
    assert_eq!(
        resp1["expected_head"].as_str(),
        Some(sha.as_str()),
        "first response must echo expected_head: {resp1}"
    );

    // Second checkout — same branch, same expected_head — idempotent
    let resp2 = super::handle_checkout_repo(
        &home,
        &json!({
            "repository_path": source.display().to_string(),
            "branch": "feat/eh-idem",
            "bind": true,
            "expected_head": &sha,
        }),
        "eh-agent-3",
    );
    assert!(
        resp2.get("error").is_none(),
        "idempotent second checkout must succeed: {resp2}"
    );
    assert_eq!(
        resp2["expected_head"].as_str(),
        Some(sha.as_str()),
        "idempotent response must echo expected_head: {resp2}"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&parent).ok();
}

// ══════════════════════════════════════════════════════════════════════
// 4. checkout expected_head omitted — preserves current behavior
// ══════════════════════════════════════════════════════════════════════

#[test]
#[cfg(unix)]
fn checkout_expected_head_omitted_preserves_current() {
    let home = tmp_home("eh-omit");
    let parent = tmp_home("eh-omit-src");
    let source = setup_source_repo(&parent, "feat/eh-omit");

    let resp = super::handle_checkout_repo(
        &home,
        &json!({
            "repository_path": source.display().to_string(),
            "branch": "feat/eh-omit",
        }),
        "eh-agent-4",
    );

    assert!(
        resp.get("error").is_none(),
        "checkout without expected_head must succeed: {resp}"
    );
    // No expected_head/actual_head fields in response when omitted
    assert!(
        resp.get("expected_head").is_none(),
        "response must NOT contain expected_head when omitted: {resp}"
    );
    assert!(
        resp.get("actual_head").is_none(),
        "response must NOT contain actual_head when omitted: {resp}"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&parent).ok();
}

// ══════════════════════════════════════════════════════════════════════
// 5. checkout expected_head — partial SHA rejected
// ══════════════════════════════════════════════════════════════════════

#[test]
#[cfg(unix)]
fn checkout_expected_head_partial_sha_rejected() {
    let home = tmp_home("eh-partial");
    let parent = tmp_home("eh-partial-src");
    let source = setup_source_repo(&parent, "feat/eh-partial");

    let resp = super::handle_checkout_repo(
        &home,
        &json!({
            "repository_path": source.display().to_string(),
            "branch": "feat/eh-partial",
            "bind": true,
            "expected_head": "abc123",
        }),
        "eh-agent-5",
    );

    assert!(
        resp.get("error").is_some(),
        "partial SHA expected_head must be rejected: {resp}"
    );
    assert_eq!(
        resp["code"].as_str(),
        Some("invalid_expected_head"),
        "error code must be invalid_expected_head: {resp}"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&parent).ok();
}

// ══════════════════════════════════════════════════════════════════════
// 5b. checkout expected_head — valid 40-hex but nonexistent object
// ══════════════════════════════════════════════════════════════════════

#[test]
#[cfg(unix)]
fn checkout_expected_head_nonexistent_object_refused() {
    let home = tmp_home("eh-noobj");
    let parent = tmp_home("eh-noobj-src");
    let source = setup_source_repo(&parent, "feat/eh-noobj");
    let fake_sha = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";

    let resp = super::handle_checkout_repo(
        &home,
        &json!({
            "repository_path": source.display().to_string(),
            "branch": "feat/eh-noobj",
            "bind": true,
            "expected_head": fake_sha,
        }),
        "eh-agent-5b",
    );

    assert!(
        resp.get("error").is_some(),
        "valid hex SHA for nonexistent object must be refused: {resp}"
    );
    assert_eq!(
        resp["code"].as_str(),
        Some("expected_head_mismatch"),
        "error code must be expected_head_mismatch: {resp}"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&parent).ok();
}

// ══════════════════════════════════════════════════════════════════════
// 6. checkout expected_head correct, from_ref garbage — expected_head
//    is the validation, from_ref is irrelevant for existing branches
// ══════════════════════════════════════════════════════════════════════

#[test]
#[cfg(unix)]
fn checkout_expected_head_correct_head_wrong_from_ref() {
    let home = tmp_home("eh-wrongfr");
    let parent = tmp_home("eh-wrongfr-src");
    let source = setup_source_repo(&parent, "feat/eh-wrongfr");
    let sha = get_sha(&source, "feat/eh-wrongfr");

    let resp = super::handle_checkout_repo(
        &home,
        &json!({
            "repository_path": source.display().to_string(),
            "branch": "feat/eh-wrongfr",
            "bind": true,
            "expected_head": &sha,
            "from_ref": "refs/heads/nonexistent-garbage-ref",
        }),
        "eh-agent-6",
    );

    // expected_head matches the existing branch HEAD, so it should succeed
    // even though from_ref is garbage (from_ref is only used for branch creation)
    assert!(
        resp.get("error").is_none(),
        "checkout with correct expected_head but garbage from_ref must succeed \
         (from_ref is irrelevant for existing branches): {resp}"
    );
    assert_eq!(
        resp["expected_head"].as_str(),
        Some(sha.as_str()),
        "response must echo expected_head: {resp}"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&parent).ok();
}

// ══════════════════════════════════════════════════════════════════════
// 7. review_assignment bind=true rejected before store
// ══════════════════════════════════════════════════════════════════════

#[test]
fn review_assignment_bind_true_rejected_before_store() {
    use crate::identity::Sender;
    use crate::mcp::handlers::comms_gates::DispatchPreChecks;
    use crate::mcp::handlers::comms_gates::ReviewAuthor;
    use crate::mcp::handlers::review_assignment::validate_review_assignment_marker;

    let home = tmp_home("ra-bind");
    let sender = Sender::new("lead").unwrap();
    let args = json!({
        "instance": "reviewer",
        "task_id": "t-test",
        "branch": "feat/x",
        "repository": "owner/repo",
        "pr_number": 42,
        "reviewed_head": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "bind": true,
    });
    let checks = DispatchPreChecks {
        force: false,
        force_reason: None,
        second_reviewer: false,
        plan_ack_required: 0,
        review_assignment: true,
        review_author: Some(ReviewAuthor::Agent("author".into())),
        pr_number: Some(42),
    };

    let result = validate_review_assignment_marker(&home, &sender, "reviewer", &args, &checks);
    assert!(
        result.is_err(),
        "review_assignment with bind=true must be rejected: {result:?}"
    );
    let err = result.unwrap_err();
    assert_eq!(
        err["code"].as_str(),
        Some("review_assignment_bind_rejected"),
        "error code must be review_assignment_bind_rejected: {err}"
    );

    std::fs::remove_dir_all(&home).ok();
}

// ══════════════════════════════════════════════════════════════════════
// 8. review_assignment worktree_binding_required=true rejected
// ══════════════════════════════════════════════════════════════════════

#[test]
fn review_assignment_worktree_binding_required_rejected() {
    use crate::identity::Sender;
    use crate::mcp::handlers::comms_gates::DispatchPreChecks;
    use crate::mcp::handlers::comms_gates::ReviewAuthor;
    use crate::mcp::handlers::review_assignment::validate_review_assignment_marker;

    let home = tmp_home("ra-wbr");
    let sender = Sender::new("lead").unwrap();
    let args = json!({
        "instance": "reviewer",
        "task_id": "t-test",
        "branch": "feat/x",
        "repository": "owner/repo",
        "pr_number": 42,
        "reviewed_head": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "worktree_binding_required": true,
    });
    let checks = DispatchPreChecks {
        force: false,
        force_reason: None,
        second_reviewer: false,
        plan_ack_required: 0,
        review_assignment: true,
        review_author: Some(ReviewAuthor::Agent("author".into())),
        pr_number: Some(42),
    };

    let result = validate_review_assignment_marker(&home, &sender, "reviewer", &args, &checks);
    assert!(
        result.is_err(),
        "review_assignment with worktree_binding_required=true must be rejected: {result:?}"
    );
    let err = result.unwrap_err();
    assert_eq!(
        err["code"].as_str(),
        Some("review_assignment_worktree_binding_rejected"),
        "error code must be review_assignment_worktree_binding_rejected: {err}"
    );

    std::fs::remove_dir_all(&home).ok();
}

// ══════════════════════════════════════════════════════════════════════
// 9. send schema exposes bind parameter
// ══════════════════════════════════════════════════════════════════════

#[test]
fn send_schema_exposes_bind_parameter() {
    let send_def = crate::mcp::tools::def_send();
    let props = &send_def["inputSchema"]["properties"];
    assert!(
        props.get("bind").is_some(),
        "send tool schema must expose `bind` parameter in properties: {}",
        serde_json::to_string_pretty(&props).unwrap_or_default()
    );
}
